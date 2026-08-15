import { expect, test } from "bun:test"

import {
  assertControlledSecretSource,
  assertSecretFreeStructured,
  auditSecretFreeActions,
  auditSecretFreeActivities,
  auditSecretFreeDiagnostic,
  auditSecretFreeFrame,
  auditSecretFreeView,
  waitForSecretFreeCondition,
  waitForSecretFreeFrame,
} from "./secret-audit"

test("Claude Direct audits reject every contaminated surface with fixed secret-free diagnostics", () => {
  const secret = "controlled-claude-direct-secret-must-not-escape"
  const cases = [
    ["frame", () => auditSecretFreeFrame(`frame:${secret}`, [secret], "mutation")],
    ["action", () => auditSecretFreeActions([{ kind: "activate-provider", additive: secret }], [secret], "mutation")],
    ["activity", () => auditSecretFreeActivities([{ messageKey: "activity.direct.applied", additive: secret }], [secret], "mutation")],
    ["view", () => auditSecretFreeView({ target: "claude", additive: secret }, [secret], "mutation")],
    ["diagnostic", () => auditSecretFreeDiagnostic(new Error(`raw:${secret}`), [secret], "mutation")],
  ] as const

  for (const [kind, audit] of cases) {
    let diagnostic = ""
    try {
      audit()
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe(`secret-scan-failed:mutation-${kind}`)
    expect(diagnostic.includes(secret)).toBeFalse()
  }
})

test("Claude Direct renderer waits and structured assertions collapse raw failures", async () => {
  const secret = "controlled-claude-direct-wait-secret"
  const contaminated = {
    waitForFrame: async (predicate: (frame: string) => boolean) => {
      predicate(`secret frame ${secret}`)
      throw new Error(`lastFrame: secret frame ${secret}`)
    },
  }
  let frameDiagnostic = ""
  try {
    await waitForSecretFreeFrame(contaminated, () => false, [secret], "controlled-wait")
  } catch (error) {
    frameDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(frameDiagnostic).toBe("secret-scan-failed:controlled-wait-frame")
  expect(frameDiagnostic.includes(secret)).toBeFalse()

  const action = { kind: "activate-provider", providerId: "actual", mode: "direct" }
  let assertionDiagnostic = ""
  try {
    assertSecretFreeStructured("action", action, [secret], "controlled-action", (safeAction) => {
      expect(safeAction).toEqual({ kind: "activate-provider", providerId: "expected", mode: "direct" })
    })
  } catch (error) {
    assertionDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(assertionDiagnostic).toBe("structured-assertion-failed:controlled-action-action")
  expect(assertionDiagnostic.includes(JSON.stringify(action))).toBeFalse()
})

test("Claude Direct controlled sources must contain every declared sentinel", () => {
  const credential = "controlled-credential-source"
  const backend = "controlled-backend-source"
  const settings = "controlled-settings-source"

  expect(() => assertControlledSecretSource(
    { credential, backend: new Error(backend), settings: { raw: settings } },
    [credential, backend, settings],
    "controlled-source",
  )).not.toThrow()

  let diagnostic = ""
  try {
    assertControlledSecretSource({ credential }, [credential, backend, settings], "controlled-source")
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("controlled-secret-source-missing:controlled-source")
  expect(diagnostic.includes(credential)).toBeFalse()
  expect(diagnostic.includes(backend)).toBeFalse()
  expect(diagnostic.includes(settings)).toBeFalse()
})

test("Claude Direct condition waits scan structured surfaces before predicates and redact timeouts", async () => {
  const secret = "controlled-claude-direct-condition-secret"
  const contaminatedAction = { kind: "activate-provider", additive: secret }
  const setup = {
    waitFor: async (predicate: () => boolean) => {
      predicate()
      throw new Error(`raw timeout ${secret}`)
    },
  }
  let scanDiagnostic = ""
  try {
    await waitForSecretFreeCondition(
      setup,
      () => false,
      () => auditSecretFreeActions([contaminatedAction], [secret], "controlled-condition"),
      "secret-scan-failed:controlled-condition-action",
      "controlled-condition",
    )
  } catch (error) {
    scanDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(scanDiagnostic).toBe("secret-scan-failed:controlled-condition-action")
  expect(scanDiagnostic.includes(secret)).toBeFalse()

  let timeoutDiagnostic = ""
  try {
    await waitForSecretFreeCondition(setup, () => false, () => {}, "unused", "controlled-timeout")
  } catch (error) {
    timeoutDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(timeoutDiagnostic).toBe("condition-wait-failed:controlled-timeout")
  expect(timeoutDiagnostic.includes(secret)).toBeFalse()
})

test("Claude Direct audits fail before a secret-bearing opposite branch can produce a raw diff", async () => {
  const secret = "controlled-opposite-branch-secret"
  const takeoverAction = {
    kind: "activate-provider",
    providerId: "claude-provider",
    mode: "takeover",
    additiveDiagnostic: secret,
  }
  let actionDiagnostic = ""
  try {
    assertSecretFreeStructured("action", takeoverAction, [secret], "opposite-branch", (safeAction) => {
      expect(safeAction).toMatchObject({
        kind: "activate-provider",
        providerId: "claude-provider",
        mode: "direct",
      })
    })
  } catch (error) {
    actionDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(actionDiagnostic).toBe("secret-scan-failed:opposite-branch-action")
  expect(actionDiagnostic.includes(secret)).toBeFalse()
  expect(actionDiagnostic.includes(JSON.stringify(takeoverAction))).toBeFalse()

  const frameSetup = {
    waitForFrame: async (predicate: (frame: string) => boolean) => {
      const frame = `Applying Target Takeover… ${secret}`
      predicate(frame)
      throw new Error(`lastFrame:${frame}`)
    },
  }
  let frameDiagnostic = ""
  try {
    await waitForSecretFreeFrame(
      frameSetup,
      (frame) => frame.includes("Applying Direct Activation…"),
      [secret],
      "opposite-branch",
    )
  } catch (error) {
    frameDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(frameDiagnostic).toBe("secret-scan-failed:opposite-branch-frame")
  expect(frameDiagnostic.includes(secret)).toBeFalse()
})
