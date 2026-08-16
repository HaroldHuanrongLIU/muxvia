import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"

import type { TargetSession } from "../src/control/target-session"
import type { ActionOutcome, CompatibilityProbe, ReconciliationPreview, ReconciliationStrategy, TargetAction, TargetView } from "../src/control/types"
import { App } from "../src/ui/app"

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
    problems: [],
  }
}

class StaticTargetSession implements TargetSession {
  readonly #view: TargetView

  constructor(
    target: "codex" | "claude" = "codex",
    problem?: "configuration-drift" | "compatibility-acknowledgement-required",
  ) {
    this.#view = {
      ...view(),
      target,
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
