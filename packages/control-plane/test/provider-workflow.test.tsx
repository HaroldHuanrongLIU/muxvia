import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"

import type { TargetSession } from "../src/control/target-session"
import type { ActionOutcome, TargetAction, TargetView } from "../src/control/types"
import { App } from "../src/ui/app"

const credentialSecret = "provider-secret-must-not-render"
const credentialUuid = "00000000-0000-4000-8000-000000000099"

function provider(overrides: Partial<TargetView["providers"][number]>): TargetView["providers"][number] {
  return {
    id: "00000000-0000-4000-8000-000000000011",
    position: 0,
    providerRevision: 1,
    name: "First Provider",
    baseUrl: "https://first.example/v1",
    model: "first-model",
    protocol: "openai-responses",
    credential: "present",
    completeness: "complete",
    missingFields: [],
    provenance: null,
    generated: false,
    activeReferences: [],
    ...overrides,
  }
}

function view(overrides: Partial<TargetView> = {}): TargetView {
  return {
    target: "codex",
    managementRevision: 1,
    viewSequence: 1,
    service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    providers: [],
    providerPresets: [],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    problems: [],
    ...overrides,
  }
}

class MemoryTargetSession implements TargetSession {
  readonly actions: TargetAction[] = []
  #view: TargetView
  #handler: (action: TargetAction) => Promise<ActionOutcome>

  constructor(
    initial: TargetView,
    handler: (action: TargetAction) => Promise<ActionOutcome> = async () => ({ status: "applied", view: initial }),
  ) {
    this.#view = initial
    this.#handler = handler
  }

  get(): Readonly<TargetView> { return this.#view }
  async act(action: TargetAction): Promise<ActionOutcome> {
    this.actions.push(action)
    const outcome = await this.#handler(action)
    this.#view = outcome.view
    return outcome
  }
  setView(next: TargetView): void { this.#view = next }
  async discoverModels(): Promise<never> { throw new Error("not used") }
  async checkReachability(): Promise<never> { throw new Error("not used") }
  subscribe(): () => void { return () => {} }
  async whenClosed(): Promise<void> { return await new Promise(() => {}) }
  async close(): Promise<void> {}
}

function expectInOrder(frame: string, names: readonly string[]): void {
  let cursor = 0
  for (const name of names) {
    const next = frame.indexOf(name, cursor)
    expect(next ?? -1).toBeGreaterThanOrEqual(cursor)
    cursor = (next ?? 0) + name.length
  }
}

test("/providers opens a top-only picker with secret-free selected Provider detail", async () => {
  const first = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "First Provider",
    provenance: { kind: "preset", key: "openai-api-responses" },
    generated: true,
    activeReferences: ["current", "activated-snapshot"],
  })
  const second = provider({
    id: "00000000-0000-4000-8000-000000000012",
    position: 1,
    name: "Second Provider",
    model: "second-model",
    credential: "missing",
    completeness: "incomplete",
    missingFields: ["credential"],
  })
  const third = provider({
    id: "00000000-0000-4000-8000-000000000013",
    position: 2,
    name: "Third Provider",
  })
  const session = new MemoryTargetSession(view({
    providers: [first, second, third],
    currentProviderId: first.id,
    activatedSnapshot: {
      id: "00000000-0000-4000-8000-000000000002",
      providerId: first.id,
      model: first.model,
      epoch: "00000000-0000-4000-8000-000000000003",
    },
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    const frame = setup.captureCharFrame()

    expect(frame).toContain("Providers")
    expectInOrder(frame, ["First Provider", "Second Provider", "Third Provider"])
    expect(frame).toContain("Complete")
    expect(frame).toContain("Preset")
    expect(frame).toContain("Generated")
    expect(frame).toContain("Credential Reference present")
    expect(frame).toContain("Current")
    expect(frame).toContain("Activated Snapshot")
    expect(frame).not.toContain(credentialSecret)
    expect(frame).not.toContain(credentialUuid)

    setup.mockInput.pressKey("down")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("second-model")
    expect(setup.captureCharFrame()).toContain("Incomplete")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Edit Provider")
    expect(setup.captureCharFrame()).toContain("second-model")
  } finally {
    setup.renderer.destroy()
  }
})

test("Provider picker remains renderable at every required terminal size", async () => {
  const session = new MemoryTargetSession(view({ providers: [provider({})] }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    for (const [width, height] of [[1, 1], [2, 2], [20, 5], [40, 10], [80, 24], [121, 30]] as const) {
      setup.resize(width, height)
      await setup.renderOnce()
      expect(() => setup.captureCharFrame()).not.toThrow()
    }
  } finally {
    setup.renderer.destroy()
  }
})

test("provider move commands send an exact identity permutation and wait for the action outcome", async () => {
  let resolve!: (outcome: ActionOutcome) => void
  const pending = new Promise<ActionOutcome>((next) => { resolve = next })
  const first = provider({ id: "00000000-0000-4000-8000-000000000011", name: "First Provider" })
  const second = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Second Provider" })
  const third = provider({ id: "00000000-0000-4000-8000-000000000013", position: 2, name: "Third Provider" })
  const initial = view({ providers: [first, second, third] })
  const session = new MemoryTargetSession(initial, async () => await pending)
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    setup.resize(121, 30)
    await setup.renderOnce()
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("u")
    await setup.renderOnce()
    expect(session.actions).toEqual([])

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("n")
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions).toEqual([{
      kind: "reorder-providers",
      providerIds: [second.id, first.id, third.id],
    }])
    expectInOrder(setup.captureCharFrame(), ["First Provider ·", "Second Provider ·", "Third Provider ·"])

    resolve({ status: "applied", view: view({ providers: [second, first, third], viewSequence: 2 }) })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    expectInOrder(setup.captureCharFrame(), ["Second Provider ·", "First Provider ·", "Third Provider ·"])
  } finally {
    setup.renderer.destroy()
  }
})

test("deletion confirms before dispatching and keeps the picker open with authoritative active references", async () => {
  const first = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Referenced Provider",
    activeReferences: ["current", "activated-snapshot"],
  })
  const initial = view({ providers: [first] })
  const session = new MemoryTargetSession(initial, async () => {
    session.setView(initial)
    throw { code: "stale-revision", message: "backend-message-must-not-render" }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    expect(confirmation).toContain("Referenced Provider")
    expect(session.actions).toEqual([])

    setup.mockInput.pressEscape()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Providers")
    expect(session.actions).toEqual([])

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await Promise.resolve()
    await setup.renderOnce()
    expect(session.actions).toEqual([{
      kind: "delete-provider",
      providerId: first.id,
      providerRevision: first.providerRevision,
    }])
    const picker = setup.captureCharFrame()
    expect(picker).toContain("Current")
    expect(picker).toContain("Activated Snapshot")
    expect(picker).not.toContain("backend-message-must-not-render")
  } finally {
    setup.renderer.destroy()
  }
})
