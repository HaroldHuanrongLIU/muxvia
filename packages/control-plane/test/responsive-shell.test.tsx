import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"

import { MuxviaKeymapProvider } from "../src/commands/keymap"
import type { TargetSession } from "../src/control/target-session"
import type { ActionOutcome, CompatibilityProbe, ReconciliationPreview, ReconciliationStrategy, RequestRecordPage, TargetAction, TargetView } from "../src/control/types"
import { createTranslator } from "../src/i18n"
import { App } from "../src/ui/app"
import { OverlayProvider } from "../src/ui/overlay-stack"
import { ProviderForm } from "../src/ui/provider-form"

const sizes = [[1, 1], [2, 2], [20, 5], [40, 10], [80, 24], [120, 30], [121, 30]] as const

function view(): TargetView {
  return {
    target: "codex",
    managementRevision: 7,
    viewSequence: 11,
    service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
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

class StaticTargetSession implements TargetSession {
  readonly #view: TargetView

  constructor(
    target: "codex" | "claude" = "codex",
    problem?: "configuration-drift" | "compatibility-acknowledgement-required",
    withFailover = false,
    activeTakeover = false,
  ) {
    const routeProvider: TargetView["providers"][number] = {
      id: "00000000-0000-4000-8000-000000000011",
      position: 0,
      providerRevision: 1,
      name: "Responsive Primary",
      baseUrl: "https://responsive.example/v1",
      model: "responsive-model",
      protocol: target === "codex" ? "openai-responses" : "anthropic-messages",
      authentication: target === "codex" ? "openai-bearer" : "anthropic-api-key",
      routingRequirement: "direct-compatible",
      credential: "present",
      completeness: "complete",
      missingFields: [],
      provenance: null,
      generated: false,
      universalProviderId: null,
      synchronization: null,
      ownership: {
        name: "target-provider", baseUrl: "target-provider", model: "target-provider",
        protocol: "target-fixed", authentication: "target-provider",
        routingRequirement: "target-provider", credential: "target-provider",
      },
      routeHealth: { state: "healthy" },
      activeReferences: ["current", "activated-route-plan"],
    }
    this.#view = {
      ...view(),
      target,
      ...(activeTakeover ? {
        mode: "takeover" as const,
        takeover: { state: "active" as const, endpoint: "http://127.0.0.1:43123/v1" },
        managedConfiguration: {
          state: "managed" as const,
          path: target === "codex" ? "/tmp/.codex/config.toml" : "/tmp/.claude/settings.json",
          restartRequired: false,
        },
      } : {}),
      ...(withFailover ? {
        providers: [routeProvider],
        currentProviderId: routeProvider.id,
        servingProviderId: routeProvider.id,
        failover: {
          draftRevision: 1,
          draftMembers: [{ providerId: routeProvider.id, providerRevision: 1 }],
          activePlan: {
            id: "00000000-0000-4000-8000-000000000012",
            epoch: "00000000-0000-4000-8000-000000000013",
            members: [{
              position: 0,
              providerId: routeProvider.id,
              providerRevision: 1,
              name: routeProvider.name,
              model: routeProvider.model,
              protocol: routeProvider.protocol,
              authentication: routeProvider.authentication,
            }],
          },
        },
      } : {}),
      problems: problem ? [{ code: problem, message: "must-not-render" }] : [],
    }
  }

  get(): Readonly<TargetView> {
    return this.#view
  }

  async discoverModels(): Promise<never> { throw new Error("not used by this fixture") }
  async checkReachability(): Promise<never> { throw new Error("not used by this fixture") }
  async previewReconciliation(strategy: ReconciliationStrategy): Promise<ReconciliationPreview> {
    return {
      observationToken: "00000000-0000-4000-8000-000000000090",
      target: this.#view.target,
      strategy,
      managementRevision: this.#view.managementRevision,
      compatibility: { version: "9.9.9", classification: "tested", acknowledgementRequired: false },
      shadowSources: [],
      changes: [{ field: "provider", state: "changed" }],
      providerEffect: "keep-current",
      restartRequired: true,
      unobservableRuntimeBoundary: true,
    }
  }
  async applyReconciliation(): Promise<never> { throw new Error("reconciliation not configured in this fixture") }
  async probeCompatibility(): Promise<CompatibilityProbe> {
    return {
      target: this.#view.target,
      managementRevision: this.#view.managementRevision,
      compatibility: {
        version: `${this.#view.target}-tested-responsive`,
        classification: "tested",
        acknowledgementRequired: false,
      },
    }
  }
  async resolveCompatibility(): Promise<never> { throw new Error("compatibility resolution not configured in this fixture") }
  async listRequestRecords(): Promise<RequestRecordPage> {
    return {
      target: this.#view.target,
      records: [{
        id: "00000000-0000-4000-8000-000000000095",
        planId: "00000000-0000-4000-8000-000000000096",
        planEpoch: "00000000-0000-4000-8000-000000000097",
        providerId: null,
        providerName: "Responsive History",
        model: "responsive-history-model",
        protocol: this.#view.target === "codex" ? "openai-responses" : "anthropic-messages",
        startedAtUnixMs: 1_700_000_000_000,
        finishedAtUnixMs: 1_700_000_000_010,
        latencyMs: 10,
        outcome: "success",
        httpStatus: 200,
        usage: null,
        estimatedCostNanoUsd: null,
        hasErrorPayload: false,
        errorPayloadTruncated: false,
      }],
      nextCursor: null,
    }
  }
  async inspectRequestRecord(): Promise<never> { throw new Error("request history not configured in this fixture") }

  async act(_action: TargetAction): Promise<ActionOutcome> {
    return { status: "applied", view: this.#view }
  }

  subscribe(_listener: (next: TargetView) => void): () => void {
    return () => {}
  }

  async close(): Promise<void> {}

  async whenClosed(): Promise<void> {
    return await new Promise(() => {})
  }
}

function expectExcludedChrome(frame: string): void {
  expect(frame).not.toContain("Terminal too small")
  expect(frame).not.toContain("Overview")
  expect(frame).not.toContain("Providers | Routing")
  expect(frame).not.toContain("Global status")
}

async function expectSizeMatrix(setup: Awaited<ReturnType<typeof testRender>>): Promise<void> {
  for (const [width, height] of sizes) {
    setup.resize(width, height)
    await setup.renderOnce()
    expect(() => setup.captureCharFrame()).not.toThrow()
    expectExcludedChrome(setup.captureCharFrame())
  }
}

test("Home renders the exact extreme-size matrix without excluded application chrome", async () => {
  const setup = await testRender(() => <App session={new StaticTargetSession()} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    await expectSizeMatrix(setup)
  } finally {
    setup.renderer.destroy()
  }
})

test("Subscription Bridge risk disclosure renders across the exact extreme-size matrix", async () => {
  const setup = await testRender(() => <MuxviaKeymapProvider><OverlayProvider><ProviderForm
    target="claude"
    mode="create"
    initialDraft={{
      name: "Subscription Route",
      baseUrl: "https://chatgpt.com/backend-api/codex",
      model: "gpt-5.6",
      presetKey: "codex-subscription-bridge",
      authentication: "codex-subscription",
    }}
    credentialPresence="missing"
    pending={false}
    t={createTranslator("en")}
    onDirtyChange={() => {}}
    onCancel={() => {}}
    onSave={async () => true}
  /></OverlayProvider></MuxviaKeymapProvider>, {
    width: 100,
    height: 38,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    for (const [width, height] of sizes) {
      setup.resize(width, height)
      await setup.renderOnce()
      expect(() => setup.captureCharFrame()).not.toThrow()
    }
    setup.resize(100, 38)
    await setup.renderOnce()
    const wide = setup.captureCharFrame()
    expect(wide).toContain("Undocumented ChatGPT Codex interface")
    expect(wide).toContain("Compatibility Deviations")
    expect(wide).not.toContain("API credential")
  } finally {
    setup.renderer.destroy()
  }
})

test("Codex renders the exact extreme-size matrix and folds its contextual sidebar", async () => {
  const setup = await testRender(() => <App session={new StaticTargetSession()} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.waitForFrame((frame) => frame.includes("Mode       Unmanaged"))

    await expectSizeMatrix(setup)

    setup.resize(80, 24)
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("Target context")

    setup.resize(121, 30)
    await setup.renderOnce()
    const wide = setup.captureCharFrame()
    expect(wide).toContain("Mode       Unmanaged")
    expect(wide).toContain("Target context")
    expect(wide).toContain("Management revision")
    expect(wide).toContain("View sequence")
    expect(wide).toContain("Takeover endpoint")
    expect(wide).toContain("Recovery")
    expect(wide).not.toContain("Unknown (clean)")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("b")
    const toggled = await setup.waitForFrame((frame) => !frame.includes("Target context"))
    expect(toggled).toContain("Mode       Unmanaged")
    expect(toggled).toContain("Activated Snapshot  —")
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude renders the exact extreme-size matrix without excluded application chrome", async () => {
  const setup = await testRender(() => <App sessions={{
    codex: new StaticTargetSession(),
    claude: new StaticTargetSession("claude"),
  }} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.waitForFrame((frame) => frame.includes("Claude · Control Plane"))
    await expectSizeMatrix(setup)

    setup.resize(120, 30)
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("Target context")

    setup.resize(121, 30)
    const wide = await setup.waitForFrame((frame) => frame.includes("Target context"))
    expect(wide).toContain("Route Health  Unobserved")
    expect(wide).toContain("Claude · Control Plane")
  } finally {
    setup.renderer.destroy()
  }
})

test.each(["codex", "claude"] as const)(
  "Takeover disable confirmation renders the exact extreme-size matrix for %s",
  async (target) => {
    const session = new StaticTargetSession(target, undefined, false, true)
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 24,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/disable-takeover")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes("Disable Target Takeover?"))
      await expectSizeMatrix(setup)
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each(["codex", "claude"] as const)(
  "Failover route overlay renders the exact extreme-size matrix for %s",
  async (target) => {
    const session = new StaticTargetSession(target, undefined, true)
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 24,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/route")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes("Failover Route"))
      await expectSizeMatrix(setup)
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each(["codex", "claude"] as const)(
  "Reconciliation modal renders the exact extreme-size matrix for %s",
  async (target) => {
    const session = new StaticTargetSession(target, "configuration-drift")
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 24,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.waitForFrame((frame) => frame.includes("Reconcile Managed Configuration"))
      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes("Adopt observed configuration"))
      for (const [width, height] of [[1, 1], [2, 2], [20, 5], [40, 10], [80, 24], [121, 30]] as const) {
        setup.resize(width, height)
        await setup.renderOnce()
        expect(() => setup.captureCharFrame()).not.toThrow()
        expectExcludedChrome(setup.captureCharFrame())
      }
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each(["codex", "claude"] as const)(
  "Compatibility Probe overlay keeps the tested resolution boundary renderable at every size for %s",
  async (target) => {
    const session = new StaticTargetSession(target, "compatibility-acknowledgement-required")
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 24,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes(`${target}-tested-responsive`))
      for (const [width, height] of sizes) {
        setup.resize(width, height)
        await setup.renderOnce()
        expect(() => setup.captureCharFrame()).not.toThrow()
        expectExcludedChrome(setup.captureCharFrame())
      }
      setup.resize(80, 24)
      await setup.renderOnce()
      const regular = setup.captureCharFrame()
      expect(regular).toContain("Command-line flags and resumed sessions may still")
      expect(regular).toContain("Y resolve tested version")
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each([
  ["codex", "en"],
  ["claude", "en"],
  ["codex", "zh-CN"],
  ["claude", "zh-CN"],
] as const)(
  "Request History remains renderable for %s in %s at every required terminal size",
  async (target, locale) => {
    const session = new StaticTargetSession(target)
    const setup = await testRender(() => <App sessions={{ [target]: session }} locale={locale} />, {
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
      await setup.waitForFrame((frame) => frame.includes("Responsive History"))
      for (const [width, height] of [[1, 1], [2, 2], [20, 5], [40, 10], [80, 24], [121, 30]] as const) {
        setup.resize(width, height)
        await setup.renderOnce()
        expect(() => setup.captureCharFrame()).not.toThrow()
        expectExcludedChrome(setup.captureCharFrame())
      }
    } finally {
      setup.renderer.destroy()
    }
  },
)
