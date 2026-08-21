import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"
import { createSignal } from "solid-js"

import type { TargetSession } from "../src/control/target-session"
import type {
  ActionOutcome,
  CompatibilityProbe,
  RequestRecordDetail,
  RequestRecordPage,
  RequestRecordSummary,
  Target,
  TargetAction,
  TargetView,
} from "../src/control/types"
import { App } from "../src/ui/app"
import {
  assertControlledSecretSource,
  auditSecretFreeFrame,
  waitForSecretFreeFrame,
} from "./secret-audit"

const credentialSecret = "ACTIVITY_CREDENTIAL_SECRET_15001"
const configSecret = "ACTIVITY_CONFIG_SECRET_15002"
const backendSecret = "ACTIVITY_BACKEND_SECRET_15003"
const settingsSecret = "ACTIVITY_SETTINGS_SECRET_15004"
const privateSecrets = [credentialSecret, configSecret, backendSecret, settingsSecret] as const
const retainedPayload = "SANITIZED_FAILURE_PAYLOAD_15005"
const retainedPayloadAtLimit = retainedPayload + "x".repeat(65_536 - retainedPayload.length)

function targetView(target: Target): TargetView {
  return {
    target,
    managementRevision: 1,
    viewSequence: 1,
    service: { epoch: "00000000-0000-4000-8000-000000001501", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: [],
    providerPresets: [],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    failover: { draftRevision: 1, draftMembers: [], activePlan: null },
    problems: [],
  }
}

function requestRecord(
  target: Target,
  overrides: Partial<RequestRecordSummary> = {},
): RequestRecordSummary {
  return {
      id: "00000000-0000-4000-8000-000000001502",
      planId: "00000000-0000-4000-8000-000000001503",
      planEpoch: "00000000-0000-4000-8000-000000001504",
      providerId: "00000000-0000-4000-8000-000000001505",
      providerName: "Activity Primary",
      model: target === "codex" ? "gpt-5.6" : "claude-sonnet-4-5",
      protocol: target === "codex" ? "openai-responses" : "anthropic-messages",
      startedAtUnixMs: 1_700_000_000_000,
      finishedAtUnixMs: 1_700_000_000_125,
      latencyMs: 125,
      outcome: "success",
      httpStatus: 200,
      usage: {
        inputTokens: 12,
        cachedInputTokens: 2,
        cacheCreationInputTokens: 0,
        outputTokens: 7,
      },
      estimatedCostNanoUsd: 42,
      hasErrorPayload: false,
      errorPayloadTruncated: false,
      ...overrides,
  }
}

function requestPage(target: Target): RequestRecordPage {
  return {
    target,
    records: [requestRecord(target)],
    nextCursor: null,
  }
}

class ActivitySession implements TargetSession {
  readonly listCalls: Array<{ limit: number; beforeCursor?: string }> = []
  readonly inspectCalls: string[] = []
  readonly #view: TargetView
  listHandler: (
    input: { limit: number; beforeCursor?: string },
    signal?: AbortSignal,
  ) => Promise<RequestRecordPage>
  inspectHandler: (recordId: string, signal?: AbortSignal) => Promise<RequestRecordDetail>

  constructor(target: Target) {
    this.#view = targetView(target)
    this.listHandler = async () => requestPage(target)
    this.inspectHandler = async () => { throw new Error("request detail not configured") }
  }
  get(): Readonly<TargetView> { return this.#view }
  async listRequestRecords(
    input: { limit: number; beforeCursor?: string },
    signal?: AbortSignal,
  ): Promise<RequestRecordPage> {
    this.listCalls.push(structuredClone(input))
    return await this.listHandler(input, signal)
  }
  async listUsageActivity(
    input: { limit: number; beforeCursor?: string },
    signal?: AbortSignal,
  ) {
    const page = await this.listRequestRecords(input, signal)
    return {
      target: page.target,
      entries: page.records.map((record) => ({ kind: "request-record" as const, record })),
      nextCursor: page.nextCursor,
      detailedRetentionDays: 30,
      catalogVersion: "fixture",
    }
  }
  async refreshNativeUsage() {
    return { target: this.#view.target, importedRecords: 0, scannedFiles: 0 }
  }
  async setUsageRetention(detailedRetentionDays: number) {
    return { target: this.#view.target, detailedRetentionDays, rolledUpDays: 0, prunedRequestRecords: 0, prunedNativeUsageRecords: 0 }
  }
  async clearUsage() {
    return { target: this.#view.target, clearedRequestRecords: 0, clearedNativeUsageRecords: 0, clearedDailyRollups: 0, clearedImportCursors: 0 }
  }
  async updatePricingCatalog() {
    return { target: this.#view.target, catalogVersion: "fixture", source: "models.dev", backfilledRequestRecords: 0, backfilledNativeUsageRecords: 0 }
  }
  async inspectRequestRecord(recordId: string, signal?: AbortSignal): Promise<RequestRecordDetail> {
    this.inspectCalls.push(recordId)
    return await this.inspectHandler(recordId, signal)
  }
  async act(_action: TargetAction): Promise<ActionOutcome> { throw new Error("not used") }
  async discoverModels(): Promise<never> { throw new Error("not used") }
  async checkReachability(): Promise<never> { throw new Error("not used") }
  async previewReconciliation(): Promise<never> { throw new Error("not used") }
  async applyReconciliation(): Promise<never> { throw new Error("not used") }
  async probeCompatibility(): Promise<CompatibilityProbe> { throw new Error("not used") }
  async resolveCompatibility(): Promise<ActionOutcome> { throw new Error("not used") }
  async previewProviderImport(): Promise<never> { throw new Error("not used") }
  async confirmProviderImport(): Promise<never> { throw new Error("not used") }
  async exportProviderConfiguration(): Promise<never> { throw new Error("not used") }
  subscribe(): () => void { return () => {} }
  async close(): Promise<void> {}
  async whenClosed(): Promise<void> { return await new Promise(() => {}) }
}

test.each(["codex", "claude"] as const)(
  "/activity opens one Target-bound newest-first history overlay for %s",
  async (target) => {
    const session = new ActivitySession(target)
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 24,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/activity")
      setup.mockInput.pressEnter()
      for (let pass = 0; pass < 4; pass++) await Promise.resolve()
      await setup.renderOnce()

      expect(session.listCalls).toEqual([{ limit: 20 }])
      const frame = setup.captureCharFrame()
      auditSecretFreeFrame(frame, privateSecrets, `activity-open-${target}`)
      expect(frame).toContain("Request History")
      expect(frame).toContain("Activity Primary")
      expect(frame).toContain("In 12 · Out 7")
      expect(frame).toContain("Estimated $0.000000042")
    } finally {
      setup.renderer.destroy()
    }
  },
)

test("failed detail loads only after selection and renders its sensitivity and truncation boundary", async () => {
  const session = new ActivitySession("codex")
  const failed = requestRecord("codex", {
    id: "00000000-0000-4000-8000-000000001510",
    providerName: "Activity Fallback",
    model: "fallback-model",
    finishedAtUnixMs: 1_700_000_000_225,
    outcome: "upstream-error",
    httpStatus: 429,
    usage: null,
    estimatedCostNanoUsd: null,
    hasErrorPayload: true,
    errorPayloadTruncated: true,
  })
  session.listHandler = async () => ({
    target: "codex",
    records: [requestRecord("codex"), failed],
    nextCursor: null,
  })
  session.inspectHandler = async (recordId) => ({
    target: "codex",
    record: failed,
    pricingSnapshot: null,
    errorPayload: recordId === failed.id ? retainedPayloadAtLimit : null,
    errorPayloadSensitive: true,
  })
  const setup = await testRender(() => <App session={session} />, {
    width: 80,
    height: 30,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    const listing = await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("Activity Fallback"),
      privateSecrets,
      "activity-failed-list",
    )
    expect(listing).not.toContain(retainedPayload)
    expect(listing).toContain("Unpriced")

    setup.mockInput.pressKey("down")
    await setup.renderOnce()
    const selectedLine = setup.captureCharFrame().split("\n").find((line) => line.includes("Activity Fallback"))
    expect(selectedLine).toContain(">")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.inspectCalls.length === 1)
    const detail = await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes(retainedPayload),
      privateSecrets,
      "activity-failed-detail",
    )
    expect(session.inspectCalls).toEqual([failed.id])
    expect(detail).toContain("sensitive request")
    expect(detail).toContain("truncated at the retention limit")
    expect(detail).toContain("Display clipped to this terminal")
  } finally {
    setup.renderer.destroy()
  }
})

test("a payload-free failed record remains inspectable and is not labelled successful", async () => {
  const session = new ActivitySession("codex")
  const failed = requestRecord("codex", {
    id: "00000000-0000-4000-8000-000000001511",
    outcome: "route-unavailable",
    httpStatus: null,
    usage: null,
    estimatedCostNanoUsd: null,
    hasErrorPayload: false,
  })
  session.listHandler = async () => ({ target: "codex", records: [failed], nextCursor: null })
  session.inspectHandler = async () => ({
    target: "codex",
    record: failed,
    pricingSnapshot: null,
    errorPayload: null,
    errorPayloadSensitive: false,
  })
  const setup = await testRender(() => <App session={session} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("Route unavailable"),
      privateSecrets,
      "activity-payload-free-failure-list",
    )
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.inspectCalls.length === 1)
    const detail = await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("No retained failure payload."),
      privateSecrets,
      "activity-payload-free-failure-detail",
    )
    expect(detail).not.toContain("Successful records retain no response payload")
    expect(detail).not.toContain("sensitive request")
  } finally {
    setup.renderer.destroy()
  }
})

test("Request History appends one opaque page and restores exact focus when cancelled", async () => {
  const session = new ActivitySession("codex")
  session.listHandler = async (input) => input.beforeCursor
    ? {
        target: "codex",
        records: [requestRecord("codex", {
          id: "00000000-0000-4000-8000-000000001520",
          providerName: "Older Provider",
        })],
        nextCursor: null,
      }
    : {
        target: "codex",
        records: [requestRecord("codex")],
        nextCursor: "opaque-target-cursor",
      }
  const setup = await testRender(() => <App session={session} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    const originFocus = setup.renderer.currentFocusedRenderable
    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    await waitForSecretFreeFrame(setup, (frame) => frame.includes("Activity Primary"), privateSecrets, "activity-page-one")
    setup.mockInput.pressKey("m")
    await waitForSecretFreeFrame(setup, (frame) => frame.includes("Older Provider"), privateSecrets, "activity-page-two")
    expect(session.listCalls).toEqual([
      { limit: 20 },
      { limit: 20, beforeCursor: "opaque-target-cursor" },
    ])
    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("Request History")
    expect(setup.renderer.currentFocusedRenderable).toBe(originFocus)
  } finally {
    setup.renderer.destroy()
  }
})

test("closing a pending Target history aborts it and a late result cannot cross into its peer", async () => {
  let resolveCodex!: (page: RequestRecordPage) => void
  let codexSignal: AbortSignal | undefined
  const codex = new ActivitySession("codex")
  codex.listHandler = async (_input, signal) => {
    codexSignal = signal
    return await new Promise<RequestRecordPage>((resolve) => { resolveCodex = resolve })
  }
  const claude = new ActivitySession("claude")
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => codex.listCalls.length === 1)
    setup.mockInput.pressEscape()
    await setup.waitFor(() => codexSignal?.aborted === true)
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()

    setup.mockInput.pressEscape()
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    const claudeFrame = await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("Claude Code Request History"),
      privateSecrets,
      "activity-target-isolation",
    )
    expect(claudeFrame).toContain("sonnet-4-5")
    resolveCodex({
      target: "codex",
      records: [requestRecord("codex", { providerName: "Late Codex Provider" })],
      nextCursor: null,
    })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const afterLate = setup.captureCharFrame()
    auditSecretFreeFrame(afterLate, privateSecrets, "activity-target-isolation-late")
    expect(afterLate).not.toContain("Late Codex Provider")
    expect(afterLate).toContain("Claude Code Request History")
  } finally {
    setup.renderer.destroy()
  }
})

test("Request History renders only a fixed localized error from secret-bearing diagnostics", async () => {
  const session = new ActivitySession("codex")
  const error = Object.assign(new Error(backendSecret), {
    code: "request-history-unavailable",
    credential: credentialSecret,
    config: configSecret,
    settings: settingsSecret,
  })
  assertControlledSecretSource(error, privateSecrets, "activity-error-source")
  session.listHandler = async () => { throw error }
  const setup = await testRender(() => <App session={session} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    const frame = await waitForSecretFreeFrame(
      setup,
      (current) => current.includes("Request History is unavailable"),
      privateSecrets,
      "activity-fixed-error",
    )
    expect(frame).not.toContain("internal-failure")
  } finally {
    setup.renderer.destroy()
  }
})

test("a late list cannot cross a same-Target session replacement or overlay generation", async () => {
  let rejectOld!: (error: unknown) => void
  const oldSession = new ActivitySession("codex")
  oldSession.listHandler = async () => await new Promise<RequestRecordPage>((_resolve, reject) => {
    rejectOld = reject
  })
  const replacement = new ActivitySession("codex")
  replacement.listHandler = async () => ({
    target: "codex",
    records: [requestRecord("codex", { providerName: "Replacement Session Provider" })],
    nextCursor: null,
  })
  const [sessions, setSessions] = createSignal<Partial<Record<Target, TargetSession>>>({ codex: oldSession })
  const setup = await testRender(() => <App sessions={sessions} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => oldSession.listCalls.length === 1)
    setSessions({ codex: replacement })
    rejectOld(Object.assign(new Error(backendSecret), { code: "request-history-unavailable" }))
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const replacedFrame = setup.captureCharFrame()
    auditSecretFreeFrame(replacedFrame, privateSecrets, "activity-session-replacement-error")
    expect(replacedFrame).not.toContain("Request History is unavailable")
    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()

    await setup.mockInput.typeText("/activity")
    setup.mockInput.pressEnter()
    await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("Replacement Session Provider"),
      privateSecrets,
      "activity-session-replacement",
    )
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const frame = setup.captureCharFrame()
    auditSecretFreeFrame(frame, privateSecrets, "activity-session-replacement-late")
    expect(frame).toContain("Replacement Session Provider")
    expect(replacement.listCalls).toEqual([{ limit: 20 }])
  } finally {
    setup.renderer.destroy()
  }
})
