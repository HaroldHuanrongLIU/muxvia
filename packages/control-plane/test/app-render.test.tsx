import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"

import { App } from "../src/ui/app"
import type { TargetSession } from "../src/control/target-session"
import type { ActionOutcome, TargetAction, TargetView } from "../src/control/types"

const serviceEpoch = "00000000-0000-4000-8000-000000000001"
const snapshotId = "00000000-0000-4000-8000-000000000002"
const snapshotEpoch = "00000000-0000-4000-8000-000000000003"
const credentialSentinel = "provider-secret-must-not-render"

type SaveProviderAction = Extract<TargetAction, { kind: "save-provider" }>
type RecordedAction =
  | Omit<SaveProviderAction, "credential"> & { credentialPresent: boolean }
  | Exclude<TargetAction, SaveProviderAction>

function projectAction(action: TargetAction): RecordedAction {
  if (action.kind !== "save-provider") return action
  return {
    kind: action.kind,
    name: action.name,
    baseUrl: action.baseUrl,
    model: action.model,
    credentialPresent: action.credential.length > 0,
  }
}

function view(overrides: Partial<TargetView> = {}): TargetView {
  return {
    target: "codex",
    managementRevision: 0,
    viewSequence: 0,
    service: { epoch: serviceEpoch, state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    providers: [],
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
  readonly actions: RecordedAction[] = []
  subscribeCalls = 0
  #view: TargetView
  #listeners = new Set<(next: TargetView) => void>()
  #handler: (action: TargetAction) => Promise<ActionOutcome>

  constructor(
    initial: TargetView,
    handler: (action: TargetAction) => Promise<ActionOutcome> = async () => ({
      status: "applied",
      view: initial,
    }),
  ) {
    this.#view = initial
    this.#handler = handler
  }

  get(): Readonly<TargetView> {
    return this.#view
  }

  async act(action: TargetAction): Promise<ActionOutcome> {
    this.actions.push(projectAction(action))
    const outcome = await this.#handler(action)
    this.#view = outcome.view
    return outcome
  }

  subscribe(listener: (next: TargetView) => void): () => void {
    this.subscribeCalls++
    this.#listeners.add(listener)
    return () => this.#listeners.delete(listener)
  }

  push(next: TargetView): void {
    this.#view = next
    for (const listener of this.#listeners) listener(next)
  }

  setAuthoritative(next: TargetView): void {
    this.#view = next
  }

  async close(): Promise<void> {}

  async whenClosed(): Promise<void> {
    return await new Promise(() => {})
  }
}

function expectSecretFree(setup: Awaited<ReturnType<typeof testRender>>, session: MemoryTargetSession): void {
  expect(setup.captureCharFrame()).not.toContain(credentialSentinel)
  expect(JSON.stringify(session.actions)).not.toContain(credentialSentinel)
}

async function flushUi(setup: Awaited<ReturnType<typeof testRender>>): Promise<void> {
  for (let pass = 0; pass < 4; pass++) await Promise.resolve()
  await setup.renderOnce()
}

function meaningfulLines(frame: string): string[] {
  return frame.split("\n").map((line) => line.trim()).filter(Boolean)
}

function expectInOrder(frame: string, expected: string[]): void {
  const lines = meaningfulLines(frame)
  let cursor = 0
  for (const item of expected) {
    const index = lines.findIndex((line, position) => position >= cursor && line.includes(item))
    expect(index).toBeGreaterThanOrEqual(cursor)
    cursor = index + 1
  }
}

async function enterProvider(
  mockInput: Awaited<ReturnType<typeof testRender>>["mockInput"],
  fields = ["Fixture Provider", "https://fixture.example/v1", "gpt-test", credentialSentinel],
): Promise<void> {
  mockInput.pressKey("x", { ctrl: true })
  mockInput.pressKey("p")
  await mockInput.typeText(fields[0]!)
  mockInput.pressTab()
  await mockInput.typeText(fields[1]!)
  mockInput.pressTab()
  await mockInput.typeText(fields[2]!)
  mockInput.pressTab()
  await mockInput.typeText(fields[3]!)
}

test("starts on Home and routes to and from the Codex context", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    const frame = setup.captureCharFrame()
    expectInOrder(frame, [
      "MUXVIA",
      "Codex CLI",
      "Providers, configuration, and routed model access",
      "Claude Code",
      "Selectable context",
      "Choose a target or enter a command",
      "ctrl+p commands",
    ])
    expect(frame).not.toContain("Mode       Unmanaged")
    expect(frame).not.toContain("Overview")
    expect(frame).not.toContain("Providers | Routing")
    expect(frame).not.toContain(credentialSentinel)

    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    const codex = setup.captureCharFrame()
    expectInOrder(codex, [
      "MUXVIA",
      "Codex",
      "Mode       Unmanaged",
      "Current    —",
      "Serving    —",
      "Service    Running",
      "Config     Unmanaged",
      "Snapshot   —",
      "Run a target action",
      "Codex · Control Plane",
    ])
    expect(codex).not.toContain("Overview")
    expect(codex).not.toContain("Providers | Routing")
    expect(codex).not.toContain(credentialSentinel)

    setup.mockInput.pressEscape()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Choose a target or enter a command")

    setup.resize(40, 12)
    await setup.renderOnce()
    const compact = setup.captureCharFrame()
    expect(compact).toContain("MUXVIA")
    expect(compact).toContain("Codex CLI")
    expect(compact).not.toContain("Overview")
  } finally {
    setup.renderer.destroy()
  }
})

test("opens the localized Claude context without operating the Codex session", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} locale="zh-CN" />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("MUXVIA")
    await setup.mockInput.typeText("/claude")
    setup.mockInput.pressEnter()
    const claude = await setup.waitForFrame((frame) => frame.includes("此构建中不提供 Claude Code 管理功能"))
    expect(claude).toContain("使用 Esc 或 /home 返回")
    expect(session.actions).toEqual([])
    expect(session.subscribeCalls).toBe(1)

    await setup.mockInput.typeText("/home")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("选择 Target CLI 或输入命令"))

    setup.mockInput.pressKey("2")
    await setup.waitForFrame((frame) => frame.includes("此构建中不提供 Claude Code 管理功能"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("选择 Target CLI 或输入命令"))
    expect(session.actions).toEqual([])
    expect(session.subscribeCalls).toBe(1)
  } finally {
    setup.renderer.destroy()
  }
})

test("submitting the global quit command from Home exits without an unknown-command notice", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    await setup.mockInput.typeText("/quit")
    setup.mockInput.pressEnter()
    if (!setup.renderer.isDestroyed) await setup.renderOnce()

    expect(setup.captureCharFrame()).not.toContain("Unknown or unavailable command")
    expect(setup.renderer.isDestroyed).toBeTrue()
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("renders a persisted compatibility warning without exposing Provider credentials", async () => {
  const session = new MemoryTargetSession(view({
    problems: [{
      code: "untested-target-cli",
      message: "Codex CLI 99.0.0 is untested; required capabilities were detected",
    }],
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    const frame = setup.captureCharFrame()
    expect(frame).toContain("Codex CLI 99.0.0 is untested")
    expect(frame).toContain("target-cli")
    expect(frame).not.toContain(credentialSentinel)
  } finally {
    setup.renderer.destroy()
  }
})

test("dirty Provider fields require localized confirmation before Ctrl+C exits", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup.mockInput)
    setup.mockInput.pressCtrlC()
    const frame = await setup.waitForFrame((next) => next.includes("Discard Provider draft?"))
    expect(frame).toContain("Unsaved Provider fields will be lost.")
    expect(setup.renderer.isDestroyed).toBeFalse()
    expectSecretFree(setup, session)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("declining dirty exit keeps the Provider draft and restores editor focus", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup.mockInput)
    await setup.renderOnce()
    const maskedBefore = (setup.captureCharFrame().match(/•/g) ?? []).length

    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))
    setup.mockInput.pressKey("n")
    await flushUi(setup)
    const draft = await setup.waitForFrame((frame) => frame.includes("Fixture Provider") && !frame.includes("Discard Provider draft?"))
    expect((draft.match(/•/g) ?? []).length).toBe(maskedBefore)
    expect(setup.renderer.currentFocusedRenderable).toBeTruthy()
    expect(setup.renderer.isDestroyed).toBeFalse()
    expectSecretFree(setup, session)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("Esc declines dirty exit and keeps the Provider editor", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup.mockInput)
    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))
    setup.mockInput.pressEscape()
    await flushUi(setup)
    await setup.waitForFrame((frame) => frame.includes("Fixture Provider") && !frame.includes("Discard Provider draft?"))
    expect(setup.renderer.isDestroyed).toBeFalse()
    expectSecretFree(setup, session)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("exit confirmation blocks hidden credential key, backspace, and paste mutation", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup.mockInput)
    await setup.renderOnce()
    const maskedBefore = (setup.captureCharFrame().match(/•/g) ?? []).length
    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))

    setup.mockInput.pressKey("z")
    setup.mockInput.pressBackspace()
    await setup.mockInput.pasteBracketedText("overlay-text-must-not-enter-credential")
    await flushUi(setup)
    const frame = setup.captureCharFrame()
    expect(frame).toContain("Discard Provider draft?")
    expect((frame.match(/•/g) ?? []).length).toBe(maskedBefore)
    expectSecretFree(setup, session)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("reopening dirty exit after cancel confirms exactly once without retaining Provider credentials", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  const destroy = setup.renderer.destroy.bind(setup.renderer)
  let destroyCalls = 0
  setup.renderer.destroy = () => {
    destroyCalls++
    destroy()
  }
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup.mockInput)
    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))
    setup.mockInput.pressKey("n")
    await flushUi(setup)
    await setup.waitForFrame((frame) => frame.includes("Fixture Provider") && !frame.includes("Discard Provider draft?"))
    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => setup.renderer.isDestroyed)
    expect(destroyCalls).toBe(1)
    expectSecretFree(setup, session)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("clean Ctrl+C exits immediately without confirmation", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressCtrlC()
    await setup.waitFor(() => setup.renderer.isDestroyed)
    expect(setup.captureCharFrame()).not.toContain("Discard Provider draft?")
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("saves a masked Provider, applies its visible identity, and follows pushed views", async () => {
  const initial = view()
  const saved = view({
    managementRevision: 1,
    viewSequence: 1,
    providers: [{
      id: "provider-1",
      name: "Fixture Provider",
      baseUrl: "https://fixture.example/v1",
      model: "gpt-test",
      credential: "present",
    }],
  })
  const session = new MemoryTargetSession(initial, async (action) => ({
    status: "applied",
    view: action.kind === "save-provider" ? saved : saved,
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup.mockInput)
    await setup.renderOnce()
    const draftFrame = setup.captureCharFrame()
    expect(draftFrame).toContain("Credential")
    expect(draftFrame).toContain("••••")
    expect(draftFrame).not.toContain(credentialSentinel)

    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    await setup.waitForFrame((frame) => frame.includes("Credential  Present"))
    expect(session.actions[0]).toEqual({
      kind: "save-provider",
      name: "Fixture Provider",
      baseUrl: "https://fixture.example/v1",
      model: "gpt-test",
      credentialPresent: true,
    })
    expect(setup.captureCharFrame()).not.toContain(credentialSentinel)

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toEqual({
      kind: "activate-provider",
      providerId: "provider-1",
      mode: "takeover",
    })

    const active = view({
      ...saved,
      managementRevision: 2,
      viewSequence: 2,
      mode: "takeover",
      takeover: { state: "active", endpoint: "http://127.0.0.1:43123/v1" },
      currentProviderId: "provider-1",
      managedConfiguration: { state: "managed", path: "/tmp/home/.codex/config.toml", restartRequired: true },
      activatedSnapshot: { id: snapshotId, providerId: "provider-1", model: "gpt-test", epoch: snapshotEpoch },
    })
    session.push(active)
    await setup.waitForFrame((frame) => frame.includes("Mode       Takeover") && frame.includes("Current    Fixture Provider"))

    session.push({ ...active, viewSequence: 3, servingProviderId: "provider-1" })
    const served = await setup.waitForFrame((frame) => frame.includes("Serving    Fixture Provider"))
    expect(served).toContain("Restart Codex")
    expect(served).not.toContain(credentialSentinel)
  } finally {
    setup.renderer.destroy()
  }
})

test("invalid Provider guidance clears the credential without echoing it", async () => {
  const initial = view()
  const session = new MemoryTargetSession(initial, async () => {
    const error = Object.assign(new Error("Base URL must use HTTPS or loopback HTTP"), {
      code: "invalid-provider",
    })
    throw error
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup.mockInput)
    setup.mockInput.pressEnter()
    const frame = await setup.waitForFrame((next) => next.includes("Check the Provider fields and try again"))
    expect(frame).toContain("Provider details are invalid")
    expect(frame).toContain("Fixture Provider")
    expect(frame).not.toContain(credentialSentinel)
    expect(frame).not.toContain("••••")
    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((next) => next.includes("Discard Provider draft?"))
    expect(setup.renderer.isDestroyed).toBeFalse()
    expectSecretFree(setup, session)
  } finally {
    setup.renderer.destroy()
  }
})

test("a stale action installs the authoritative Target View and asks for an explicit retry", async () => {
  const provider = {
    id: "provider-1",
    name: "Before",
    baseUrl: "https://fixture.example/v1",
    model: "gpt-test",
    credential: "present" as const,
  }
  const initial = view({ managementRevision: 1, viewSequence: 1, providers: [provider] })
  const authoritative = view({
    managementRevision: 2,
    viewSequence: 2,
    providers: [{ ...provider, name: "Authoritative Provider" }],
    currentProviderId: "provider-1",
  })
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(initial, async () => {
    session.setAuthoritative(authoritative)
    throw Object.assign(new Error("Target state changed"), { code: "stale-revision" })
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    const frame = await setup.waitForFrame((next) => next.includes("Retry the action"))
    expect(frame).toContain("Current    Authoritative Provider")
    expect(frame).toContain("Target state changed")
  } finally {
    setup.renderer.destroy()
  }
})
