import { expect, test } from "bun:test"
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
  const session = new MemoryTargetSession(view({ providers: [selected] }), async (action) => {
    attempts++
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
      credential: { kind: "replace", value: savedSecret },
    })

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

    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "update-provider",
      name: "First Provider dirty",
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
      credential: { kind: "remove" },
      presetKey: "openai-api-responses",
    }])
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
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(view({ providers: [source, tail] }), async (action): Promise<ActionOutcome> => {
    if (action.kind !== "duplicate-provider") throw new Error("unexpected action")
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
      credential: { kind: "replace", value: credentialSecret },
    })
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
