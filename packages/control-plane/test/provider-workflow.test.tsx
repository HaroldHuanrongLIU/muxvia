import { expect, test } from "bun:test"
import type { InputRenderable } from "@opentui/core"
import { testRender } from "@opentui/solid"

import { MuxviaKeymapProvider, useMuxviaKeymap } from "../src/commands/keymap"
import type { TargetSession } from "../src/control/target-session"
import type { ActionOutcome, TargetAction, TargetView } from "../src/control/types"
import { createTranslator } from "../src/i18n"
import { App } from "../src/ui/app"
import { OverlayProvider } from "../src/ui/overlay-stack"
import { ProviderForm, type ProviderFormResult } from "../src/ui/provider-form"

const credentialSecret = "provider-secret-must-not-render"
const credentialUuid = "00000000-0000-4000-8000-000000000099"

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void
  let reject!: (reason?: unknown) => void
  const promise = new Promise<T>((next, fail) => {
    resolve = next
    reject = fail
  })
  return { promise, resolve, reject }
}

function provider(overrides: Partial<TargetView["providers"][number]>): TargetView["providers"][number] {
  return {
    id: "00000000-0000-4000-8000-000000000011",
    position: 0,
    providerRevision: 1,
    name: "First Provider",
    baseUrl: "https://first.example/v1",
    model: "first-model",
    protocol: "openai-responses",
    authentication: "openai-bearer",
    routingRequirement: "direct-compatible",
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
    routeHealth: { state: "unobserved" },
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

type RecordedTargetAction<T extends TargetAction = TargetAction> = T extends { credential: infer Credential }
  ? Omit<T, "credential"> & {
    credential: Credential extends { kind: "replace" }
      ? { kind: "replace"; valuePresent: boolean }
      : Credential
  }
  : T

function projectAction(action: TargetAction): RecordedTargetAction {
  if ("credential" in action && action.credential.kind === "replace") {
    return {
      ...action,
      credential: { kind: "replace", valuePresent: action.credential.value.length > 0 },
    } as RecordedTargetAction
  }
  return action as RecordedTargetAction
}

class MemoryTargetSession implements TargetSession {
  readonly actions: RecordedTargetAction[] = []
  readonly #listeners = new Set<(next: TargetView) => void>()
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
    this.actions.push(projectAction(action))
    const outcome = await this.#handler(action)
    this.#view = outcome.view
    return outcome
  }
  setView(next: TargetView): void { this.#view = next }
  pushView(next: TargetView): void {
    this.#view = next
    for (const listener of this.#listeners) listener(next)
  }
  async discoverModels(): Promise<never> { throw new Error("not used") }
  async checkReachability(): Promise<never> { throw new Error("not used") }
  subscribe(listener: (next: TargetView) => void): () => void {
    this.#listeners.add(listener)
    return () => this.#listeners.delete(listener)
  }
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

test("/providers renders provenance kinds and generated state with secret-free selected Provider detail", async () => {
  const first = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Generated Provider",
    provenance: { kind: "universal-provider", key: "00000000-0000-4000-8000-000000000010" },
    generated: true,
    activeReferences: ["current", "activated-snapshot"],
  })
  const second = provider({
    id: "00000000-0000-4000-8000-000000000012",
    position: 1,
    name: "Preset Provider",
    provenance: { kind: "preset", key: "openai-api-responses" },
  })
  const third = provider({
    id: "00000000-0000-4000-8000-000000000013",
    position: 2,
    name: "Ordinary Provider",
  })
  const fourth = provider({
    id: "00000000-0000-4000-8000-000000000014",
    position: 3,
    name: "Future Provider",
    provenance: { kind: "future-kind-must-not-render", key: "future-key-must-not-render" },
  })
  const session = new MemoryTargetSession(view({
    providers: [first, second, third, fourth],
    currentProviderId: first.id,
    activatedSnapshot: {
      id: "00000000-0000-4000-8000-000000000002",
      providerId: first.id,
      model: first.model,
      protocol: "openai-responses",
      authentication: "openai-bearer",
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
    expectInOrder(frame, ["Generated Provider", "Preset Provider", "Ordinary Provider", "Future Provider"])
    expect(frame).toContain("Complete")
    expect(frame).toContain("Universal Provider")
    expect(frame).toContain("Preset")
    expect(frame).toContain("Ordinary")
    expect(frame).toContain("Other provenance")
    expect(frame).toContain("Generated")
    expect(frame).toContain("Credential Reference present")
    expect(frame).toContain("Current")
    expect(frame).toContain("Activated Snapshot")
    expect(frame).not.toContain(credentialSecret)
    expect(frame).not.toContain(credentialUuid)
    expect(frame).not.toContain("future-kind-must-not-render")
    expect(frame).not.toContain("future-key-must-not-render")

    setup.mockInput.pressKey("down")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Preset Provider")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Edit Provider")
    expect(setup.captureCharFrame()).toContain("Preset Provider")
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

    resolve({ status: "applied", view: view({ providers: [{ ...second }, { ...first }, { ...third }], viewSequence: 2 }) })
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

test("a failed credential replacement keeps dirty fields but retries an edit with Keep", async () => {
  const savedSecret = "replacement-secret-must-not-render"
  const selected = provider({ credential: "missing", completeness: "incomplete", missingFields: ["credential"] })
  let attempts = 0
  let sawReplacementSecret = false
  const session = new MemoryTargetSession(view({ providers: [selected] }), async (action) => {
    attempts++
    if (action.kind === "update-provider" && action.credential.kind === "replace") {
      sawReplacementSecret = action.credential.value === savedSecret
    }
    if (attempts === 1) throw { code: "invalid-provider", message: "server-message-must-not-render" }
    return { status: "applied", view: view({ providers: [selected], viewSequence: 2 }) }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    await setup.mockInput.typeText(" dirty")
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(savedSecret)
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    await setup.renderOnce()

    expect(setup.captureCharFrame()).toContain("First Provider dirty")
    expect(setup.captureCharFrame()).not.toContain(savedSecret)
    expect(setup.captureCharFrame()).not.toContain("server-message-must-not-render")
    expect(session.actions[0]).toMatchObject({
      kind: "update-provider",
      credential: { kind: "replace", valuePresent: true },
    })
    expect(sawReplacementSecret).toBeTrue()
    const persistedReplacementSecret = JSON.stringify(session.actions).includes(savedSecret)
    expect(persistedReplacementSecret).toBeFalse()

    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "update-provider",
      name: "First Provider dirty",
      credential: { kind: "keep" },
    })
    expect(JSON.stringify(session.actions[1])).not.toContain(savedSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("a failed explicit credential removal retries an edit with Remove", async () => {
  const results: ProviderFormResult[] = []
  let keymap!: ReturnType<typeof useMuxviaKeymap>
  function Harness() {
    keymap = useMuxviaKeymap()
    return <ProviderForm
      target="codex"
      mode="edit"
      initialDraft={{
        name: "First Provider",
        baseUrl: "https://first.example/v1",
        model: "first-model",
        providerId: "00000000-0000-4000-8000-000000000011",
        providerRevision: 1,
      }}
      credentialPresence="present"
      pending={false}
      t={createTranslator("en")}
      onDirtyChange={() => {}}
      onCancel={() => {}}
      onSave={async (result) => {
        results.push(result)
        return false
      }}
    />
  }
  const setup = await testRender(() => (
    <MuxviaKeymapProvider><OverlayProvider><Harness /></OverlayProvider></MuxviaKeymapProvider>
  ), { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    keymap.dispatchCommand("provider.credential.remove")
    await setup.renderOnce()
    keymap.dispatchCommand("provider.save")
    await setup.waitFor(() => results.length === 1)
    await setup.renderOnce()

    expect(results[0]).toMatchObject({ kind: "update-provider", credential: { kind: "remove" } })

    keymap.dispatchCommand("provider.save")
    await setup.waitFor(() => results.length === 2)
    expect(results[1]).toMatchObject({ kind: "update-provider", credential: { kind: "remove" } })
  } finally {
    setup.renderer.destroy()
  }
})

test("a successful delete selects a remaining Provider for the next named action", async () => {
  const first = provider({ id: "00000000-0000-4000-8000-000000000011", name: "Deleted Provider" })
  const second = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Remaining Provider", model: "remaining-model" })
  const afterDelete = view({ providers: [second], viewSequence: 2 })
  const session = new MemoryTargetSession(view({ providers: [first, second] }), async (action) => {
    if (action.kind === "delete-provider") return { status: "applied", view: afterDelete }
    return { status: "applied", view: afterDelete }
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
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => session.actions.length === 1)
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()

    expect(setup.captureCharFrame()).toContain("Remaining Provider")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Edit Provider")
    expect(setup.captureCharFrame()).toContain("remaining-model")
  } finally {
    setup.renderer.destroy()
  }
})

test("a stale Provider edit refreshes its revision before the next save", async () => {
  const stale = provider({ providerRevision: 1 })
  const authoritative = provider({ providerRevision: 2, name: "Authoritative Provider" })
  const authoritativeView = view({ providers: [{ ...authoritative }], viewSequence: 2 })
  let attempts = 0
  const session = new MemoryTargetSession(view({ providers: [stale] }), async (action) => {
    attempts++
    if (attempts === 1) {
      session.setView(authoritativeView)
      throw { code: "stale-provider-revision", message: "stale-server-message-must-not-render" }
    }
    return { status: "applied", view: view({ providers: [{ ...authoritative }], viewSequence: 3 }) }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    await setup.mockInput.typeText(" dirty")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    await setup.renderOnce()

    expect(setup.captureCharFrame()).toContain("First Provider dirty")
    expect(setup.captureCharFrame()).not.toContain("stale-server-message-must-not-render")
    expect(session.actions[0]).toMatchObject({ kind: "update-provider", providerRevision: 1 })

    await setup.mockInput.typeText(" again")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "update-provider",
      name: "First Provider dirty again",
      providerRevision: 2,
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("blank edits Keep credentials for either saved presence while creates start with Remove", async () => {
  for (const credential of ["present", "missing"] as const) {
    const selected = provider({ credential })
    const session = new MemoryTargetSession(view({ providers: [selected] }))
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.renderOnce()
      setup.mockInput.pressEnter()
      await setup.renderOnce()
      setup.mockInput.pressEnter()
      await setup.waitFor(() => session.actions.length === 1)
      expect(session.actions[0]).toMatchObject({ kind: "update-provider", credential: { kind: "keep" } })
    } finally {
      setup.renderer.destroy()
    }
  }

  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    await setup.waitForFrame((frame) => frame.includes("Blank"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter save"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toMatchObject({ kind: "create-provider", credential: { kind: "remove" } })
  } finally {
    setup.renderer.destroy()
  }
})

test("Preset source selection copies an ordinary draft without discovery and saves provenance", async () => {
  const session = new MemoryTargetSession(view({
    providerPresets: [{
      key: "openai-api-responses",
      baseUrl: "https://api.openai.com/v1",
      model: "",
      protocol: "openai-responses",
      authentication: "openai-bearer",
    }],
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    const sources = await setup.waitForFrame((frame) => frame.includes("Blank"))
    expect(sources).toContain("OpenAI API (Responses)")

    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    const editor = await setup.waitForFrame((frame) => frame.includes("https://api.openai.com/v1"))
    expect(editor).toContain("API credential")
    expect(editor).not.toContain(credentialSecret)

    await setup.mockInput.typeText("Official Copy")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions).toEqual([{
      kind: "create-provider",
      name: "Official Copy",
      baseUrl: "https://api.openai.com/v1",
      model: "",
      authentication: "openai-bearer",
      credential: { kind: "remove" },
      presetKey: "openai-api-responses",
    }])
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude Provider editor selects Bearer authentication and dispatches only its Target session", async () => {
  const codex = new MemoryTargetSession(view())
  const claudeInitial = view({
    target: "claude",
    providerPresets: [{
      key: "anthropic-api-messages",
      baseUrl: "https://api.anthropic.com/v1",
      model: "",
      protocol: "anthropic-messages",
      authentication: "anthropic-api-key",
    }],
  })
  const claude = new MemoryTargetSession(claudeInitial, async () => ({ status: "applied", view: claudeInitial }))
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API (Messages)"))
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API key"))
    await setup.renderOnce()

    await setup.mockInput.typeText("Claude Bearer")
    setup.mockInput.pressTab()
    await setup.renderOnce()
    setup.mockInput.pressTab()
    await setup.renderOnce()
    await setup.mockInput.typeText("claude-test")
    setup.mockInput.pressTab()
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("h")
    setup.mockInput.pressTab()
    await setup.renderOnce()
    await setup.mockInput.typeText(credentialSecret)

    const safeFrame = setup.captureCharFrame()
    expect(safeFrame).not.toContain(credentialSecret)
    expect(safeFrame).toContain("Anthropic Bearer token")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.actions.length === 1)
    expect(claude.actions).toEqual([{
      kind: "create-provider",
      name: "Claude Bearer",
      baseUrl: "https://api.anthropic.com/v1",
      model: "claude-test",
      authentication: "anthropic-bearer",
      credential: { kind: "replace", valuePresent: true },
      presetKey: "anthropic-api-messages",
    }])
    expect(codex.actions).toEqual([])
    expect(JSON.stringify(claude.actions)).not.toContain(credentialSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude owns edit reorder duplicate and delete actions without touching Codex", async () => {
  const first = provider({
    id: "00000000-0000-4000-8000-000000000071",
    name: "Claude First",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
    routingRequirement: "takeover-required",
  })
  const second = provider({
    id: "00000000-0000-4000-8000-000000000072",
    position: 1,
    name: "Claude Second",
    protocol: "anthropic-messages",
    authentication: "anthropic-bearer",
    routingRequirement: "takeover-required",
  })
  let nextView = view({ target: "claude", providers: [first, second] })
  const codex = new MemoryTargetSession(view())
  const claude = new MemoryTargetSession(nextView, async (action) => {
    const providers = [...nextView.providers]
    if (action.kind === "reorder-providers") {
      nextView = view({
        target: "claude",
        viewSequence: nextView.viewSequence + 1,
        providers: action.providerIds.map((id, position) => ({ ...providers.find((item) => item.id === id)!, position })),
      })
    } else if (action.kind === "duplicate-provider") {
      const duplicate = provider({
        id: "00000000-0000-4000-8000-000000000073",
        position: providers.length,
        name: action.name,
        baseUrl: action.baseUrl,
        model: action.model,
        protocol: "anthropic-messages",
        authentication: "anthropic-api-key",
        routingRequirement: "takeover-required",
        credential: "missing",
      })
      nextView = view({ target: "claude", viewSequence: nextView.viewSequence + 1, providers: [...providers, duplicate] })
    } else if (action.kind === "update-provider") {
      nextView = view({
        target: "claude",
        viewSequence: nextView.viewSequence + 1,
        providers: providers.map((item) => item.id === action.providerId ? { ...item, name: action.name, providerRevision: item.providerRevision + 1 } : item),
      })
    } else if (action.kind === "delete-provider") {
      nextView = view({
        target: "claude",
        viewSequence: nextView.viewSequence + 1,
        providers: providers.filter((item) => item.id !== action.providerId).map((item, position) => ({ ...item, position })),
      })
    } else {
      throw new Error(`unexpected ${action.kind}`)
    }
    return { status: "applied", view: nextView }
  })
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude First"))

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("n")
    await setup.waitFor(() => claude.actions.length === 1)
    expect(claude.actions[0]).toEqual({ kind: "reorder-providers", providerIds: [second.id, first.id] })

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    await setup.waitForFrame((frame) => frame.includes("Reuse Credential Reference?"))
    setup.mockInput.pressKey("n")
    await setup.waitForFrame((frame) => frame.includes("Claude First Copy"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.actions.length === 2)
    expect(claude.actions[1]).toMatchObject({ kind: "duplicate-provider", sourceProviderId: first.id, credential: { kind: "without" } })

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude Second"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Edit Provider"))
    await setup.mockInput.typeText(" Updated")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.actions.length === 3)
    expect(claude.actions[2]).toMatchObject({ kind: "update-provider", providerId: first.id, name: "Claude First Updated" })

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude First Updated"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Claude First Updated") && !frame.includes("Delete Provider?"))
    expect(setup.renderer.currentFocusedRenderable?.isDestroyed).toBeFalse()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => claude.actions.length === 4)
    expect(claude.actions[3]).toMatchObject({ kind: "delete-provider", providerId: first.id })
    expect(codex.actions).toEqual([])
    expect(JSON.stringify(claude.actions)).not.toContain(credentialSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("duplicate credential confirmation keeps cancel without and reuse distinct and replacement wins", async () => {
  const source = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Source Provider",
    credential: "present",
  })
  const tail = provider({
    id: "00000000-0000-4000-8000-000000000012",
    position: 1,
    name: "Tail Provider",
  })
  const createdIds = [
    "00000000-0000-4000-8000-000000000021",
    "00000000-0000-4000-8000-000000000022",
    "00000000-0000-4000-8000-000000000023",
  ]
  let sawDuplicateReplacementSecret = false
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(view({ providers: [source, tail] }), async (action): Promise<ActionOutcome> => {
    if (action.kind !== "duplicate-provider") throw new Error("unexpected action")
    if (action.credential.kind === "replace") {
      sawDuplicateReplacementSecret = action.credential.value === credentialSecret
    }
    const duplicate = provider({
      id: createdIds[session.actions.length - 1]!,
      position: 1,
      name: action.name,
      baseUrl: action.baseUrl,
      model: action.model,
      credential: action.credential.kind === "without" ? "missing" : "present",
    })
    return {
      status: "applied",
      view: view({
        viewSequence: session.actions.length + 1,
        providers: [source, duplicate, { ...tail, position: 2 }],
      }),
    }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })

  const openPicker = async () => {
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·") && frame.includes("Source Provider"))
  }
  const requestDuplicate = async () => {
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    return await setup.waitForFrame((frame) => frame.includes("Credential Reference"))
  }

  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await openPicker()

    const confirmation = await requestDuplicate()
    expect(confirmation).toContain("Source Provider")
    expect(confirmation).toContain("Credential Reference present")
    expect(confirmation).not.toContain(credentialUuid)
    expect(confirmation).not.toContain(credentialSecret)
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Providers") && !frame.includes("Reuse Credential Reference?"))
    expect(session.actions).toEqual([])

    await requestDuplicate()
    setup.mockInput.pressKey("n")
    const withoutEditor = await setup.waitForFrame((frame) => frame.includes("Source Provider Copy"))
    expect(withoutEditor).toContain("https://first.example/v1")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toEqual({
      kind: "duplicate-provider",
      sourceProviderId: source.id,
      sourceProviderRevision: source.providerRevision,
      name: "Source Provider Copy",
      baseUrl: source.baseUrl,
      model: source.model,
      credential: { kind: "without" },
    })

    await openPicker()
    await requestDuplicate()
    setup.mockInput.pressKey("y")
    await setup.waitForFrame((frame) => frame.includes("Source Provider Copy"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "duplicate-provider",
      sourceProviderId: source.id,
      sourceProviderRevision: source.providerRevision,
      credential: { kind: "reuse-source" },
    })

    await openPicker()
    await requestDuplicate()
    setup.mockInput.pressKey("y")
    await setup.waitForFrame((frame) => frame.includes("Source Provider Copy"))
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(credentialSecret)
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 3)
    expect(session.actions[2]).toMatchObject({
      kind: "duplicate-provider",
      credential: { kind: "replace", valuePresent: true },
    })
    expect(sawDuplicateReplacementSecret).toBeTrue()
    const persistedReplacementSecret = JSON.stringify(session.actions).includes(credentialSecret)
    expect(persistedReplacementSecret).toBeFalse()
    expect(setup.captureCharFrame()).not.toContain(credentialSecret)

    await openPicker()
    const rows = setup.captureCharFrame()
    expectInOrder(rows, ["Source Provider ·", "Source Provider Copy ·", "Tail Provider ·"])
  } finally {
    setup.renderer.destroy()
  }
})

test("duplicate without a source credential opens directly with an explicit without intent", async () => {
  const source = provider({ credential: "missing" })
  const session = new MemoryTargetSession(view({ providers: [source] }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·") && frame.includes("First Provider"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    const editor = await setup.waitForFrame((frame) => frame.includes("First Provider Copy"))
    expect(editor).not.toContain("Reuse Credential Reference?")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toMatchObject({
      kind: "duplicate-provider",
      credential: { kind: "without" },
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("Provider Direct Activation defaults to Current and dispatches the selected row exact identity", async () => {
  const first = provider({ id: "00000000-0000-4000-8000-000000000011", name: "First Provider" })
  const second = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Second Provider" })
  for (const testCase of [
    { currentProviderId: second.id, moveDown: false, expectedId: second.id },
    { currentProviderId: first.id, moveDown: true, expectedId: second.id },
  ]) {
    const initial = view({ providers: [first, second], currentProviderId: testCase.currentProviderId })
    const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: initial }))
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes("First Provider") && frame.includes("Second Provider"))
      if (testCase.moveDown) setup.mockInput.pressKey("down")
      setup.mockInput.pressKey("x", { ctrl: true })
      setup.mockInput.pressKey("a")
      await setup.waitFor(() => session.actions.length === 1)

      expect(session.actions).toEqual([{
        kind: "activate-provider",
        providerId: testCase.expectedId,
        mode: "direct",
      }])
      expect(setup.captureCharFrame()).not.toContain(credentialSecret)
      expect(JSON.stringify(session.actions)).not.toContain(credentialSecret)
    } finally {
      setup.renderer.destroy()
    }
  }
})

for (const action of ["activate", "edit"] as const) {
  test(`authoritative removal synchronizes the visible Provider fallback before ${action}`, async () => {
    const fallback = provider({ id: "00000000-0000-4000-8000-000000000011", name: "Fallback Provider" })
    const removed = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Removed Provider" })
    const initial = view({ providers: [fallback, removed], currentProviderId: removed.id })
    const authoritative = view({
      ...initial,
      managementRevision: 2,
      viewSequence: 2,
      providers: [fallback],
      currentProviderId: null,
    })
    const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: authoritative }))
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes("Removed Provider"))

      session.pushView(authoritative)
      const fallbackFrame = await setup.waitForFrame((frame) => frame.includes("Fallback Provider") && !frame.includes("Removed Provider"))
      expect(fallbackFrame).toContain("Fallback Provider")

      if (action === "activate") {
        setup.mockInput.pressKey("x", { ctrl: true })
        setup.mockInput.pressKey("a")
        for (let pass = 0; pass < 4; pass++) await Promise.resolve()
        expect(session.actions).toEqual([{
          kind: "activate-provider",
          providerId: "00000000-0000-4000-8000-000000000011",
          mode: "direct",
        }])
      } else {
        setup.mockInput.pressEnter()
        const editor = await setup.waitForFrame((frame) => frame.includes("Edit Provider"))
        expect(editor).toContain("Fallback Provider")
      }
    } finally {
      setup.renderer.destroy()
    }
  })
}

test("Provider Direct Activation disables picker actions while pending then closes onto restart guidance", async () => {
  const selected = provider({ id: "00000000-0000-4000-8000-000000000011", name: "Pending Direct Provider" })
  const initial = view({ providers: [selected] })
  const applied = view({
    ...initial,
    managementRevision: 2,
    viewSequence: 2,
    mode: "direct",
    currentProviderId: selected.id,
    managedConfiguration: { state: "managed", path: "/tmp/home/.codex/config.toml", restartRequired: true },
    activatedSnapshot: {
      id: "00000000-0000-4000-8000-000000000021",
      providerId: selected.id,
      model: selected.model,
      protocol: "openai-responses",
      authentication: "openai-bearer",
      epoch: "00000000-0000-4000-8000-000000000022",
    },
  })
  const pending = deferred<ActionOutcome>()
  const session = new MemoryTargetSession(initial, async () => await pending.promise)
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Pending Direct Provider"))
    setup.mockInput.pressEscape()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitFor(() => session.actions.length === 1)
    const pendingFrame = await setup.waitForFrame((frame) => frame.includes("Applying Direct Activation…"))
    expect(pendingFrame).toContain("Providers")

    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    setup.mockInput.pressKey("p", { ctrl: true })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const guardedFrame = setup.captureCharFrame()
    expect(guardedFrame).toContain("Applying Direct Activation…")
    expect(guardedFrame).toContain("Providers")
    expect(guardedFrame).not.toContain("Search commands")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Applying Direct Activation…")
    expect(setup.captureCharFrame()).not.toContain("Delete Provider?")
    expect(setup.captureCharFrame()).not.toContain("Edit Provider")
    expect(session.actions).toHaveLength(1)

    pending.resolve({ status: "applied", view: applied })
    const completed = await setup.waitForFrame((frame) =>
      frame.includes("Direct Activation applied: Pending Direct Provider")
      && frame.includes("Restart Codex to use the managed configuration.")
    )
    expect(completed).toContain("Mode       Direct")
    expect(completed).not.toContain("Providers")
    expect(completed).not.toContain(credentialSecret)
    expect(session.actions).toEqual([{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "direct",
    }])
  } finally {
    pending.resolve({ status: "applied", view: applied })
    setup.renderer.destroy()
  }
})

test("picker Direct failures close onto localized stable guidance without raw backend text", async () => {
  const cases = [
    {
      selected: provider({
        id: "00000000-0000-4000-8000-000000000011",
        name: "Incomplete Picker Provider",
        credential: "missing",
        completeness: "incomplete",
        missingFields: ["credential"],
      }),
      expected: "Complete the required Provider fields and retry.",
      backend: false,
    },
    {
      selected: provider({
        id: "00000000-0000-4000-8000-000000000012",
        name: "Active Takeover Provider",
      }),
      expected: "Disable Target Takeover before using Direct Activation.",
      backend: true,
    },
  ] as const

  for (const testCase of cases) {
    const initial = view({ providers: [testCase.selected] })
    let session!: MemoryTargetSession
    session = new MemoryTargetSession(initial, async () => {
      session.setView(view({
        ...initial,
        managementRevision: 2,
        viewSequence: 2,
        providers: [{ ...testCase.selected, name: "Authoritative Active Provider" }],
      }))
      throw { code: "takeover-active", message: "backend-picker-secret-must-not-render" }
    })
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes(testCase.selected.name))
      setup.mockInput.pressKey("x", { ctrl: true })
      setup.mockInput.pressKey("a")
      const failure = await setup.waitForFrame((frame) => frame.includes(testCase.expected))

      expect(failure).not.toContain("Providers")
      expect(failure).not.toContain("backend-picker-secret-must-not-render")
      expect(failure).not.toContain(credentialSecret)
      expect(session.actions).toHaveLength(testCase.backend ? 1 : 0)
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("a Takeover-required Provider offers only Takeover or cancel with scoped single dispatch", async () => {
  const selected = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Bridge Provider",
    routingRequirement: "takeover-required",
  })
  const initial = view({ providers: [selected], currentProviderId: selected.id })
  const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: initial }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Bridge Provider"))
    const pickerFocus = setup.renderer.currentFocusedRenderable as InputRenderable

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Enable Target Takeover?"))
    expect(confirmation).toContain("Bridge Provider requires Target Takeover for activation.")
    expect(confirmation).toContain("Enable Takeover")
    expect(confirmation).toContain("Cancel")
    expect(confirmation).not.toContain(credentialSecret)
    expect(session.actions).toEqual([])

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    expect(session.actions).toEqual([])
    expect(setup.captureCharFrame()).toContain("Providers")
    const restoredFocus = setup.renderer.currentFocusedRenderable as InputRenderable
    expect(pickerFocus.isDestroyed).toBeTrue()
    expect(restoredFocus.isDestroyed).toBeFalse()
    expect(restoredFocus.placeholder).toBe("Navigate Providers")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitForFrame((frame) => frame.includes("Enable Target Takeover?"))
    setup.mockInput.pressKey("y")
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => session.actions.length === 1)
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    setup.mockInput.pressKey("y")
    await setup.renderOnce()

    expect(session.actions).toEqual([{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "takeover",
    }])
    expect(JSON.stringify(session.actions)).not.toContain(credentialSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("authoritative takeover-required preserves the pending picker and restores it after cancel", async () => {
  const selected = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Changed Requirement Provider",
    routingRequirement: "direct-compatible",
  })
  const authoritative = view({
    managementRevision: 2,
    viewSequence: 2,
    providers: [{ ...selected, routingRequirement: "takeover-required" }],
    currentProviderId: selected.id,
  })
  const pendingDirect = deferred<ActionOutcome>()
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(view({ providers: [selected] }), async () => await pendingDirect.promise)
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Changed Requirement Provider"))
    const pickerFocus = setup.renderer.currentFocusedRenderable as InputRenderable
    setup.mockInput.pressEscape()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitFor(() => session.actions.length === 1)
    await setup.waitForFrame((frame) => frame.includes("Applying Direct Activation…"))

    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    setup.mockInput.pressKey("p", { ctrl: true })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    session.setView(authoritative)
    pendingDirect.reject({ code: "takeover-required", message: "backend-takeover-secret-must-not-render" })
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Enable Target Takeover?"))

    expect(session.actions).toEqual([{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "direct",
    }])
    expect(confirmation).not.toContain("backend-takeover-secret-must-not-render")
    expect(confirmation).not.toContain("Action failed")
    expect(confirmation).not.toContain(credentialSecret)

    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const restored = setup.captureCharFrame()
    expect(restored).toContain("Providers")
    expect(restored).not.toContain("Commands")
    const restoredFocus = setup.renderer.currentFocusedRenderable as InputRenderable
    expect(pickerFocus.isDestroyed).toBeTrue()
    expect(restoredFocus.isDestroyed).toBeFalse()
    expect(restoredFocus.placeholder).toBe("Navigate Providers")
    expect(session.actions).toHaveLength(1)
    expect(JSON.stringify(session.actions)).not.toContain("backend-takeover-secret-must-not-render")
  } finally {
    pendingDirect.reject({ code: "takeover-required" })
    setup.renderer.destroy()
  }
})
