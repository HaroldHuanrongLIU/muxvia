import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"

import { App } from "../src/ui/app"
import type { TargetSession } from "../src/control/target-session"
import type { ActionOutcome, TargetAction, TargetView } from "../src/control/types"

const serviceEpoch = "00000000-0000-4000-8000-000000000001"
const snapshotId = "00000000-0000-4000-8000-000000000002"
const snapshotEpoch = "00000000-0000-4000-8000-000000000003"
const credentialSentinel = "provider-secret-must-not-render"
const problemMessageSentinel = "backend-problem-secret-must-not-render"

type CreateProviderAction = Extract<TargetAction, { kind: "create-provider" }>
type RecordedAction =
  | Omit<CreateProviderAction, "credential"> & { credentialPresent: boolean }
  | Exclude<TargetAction, CreateProviderAction>

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void
  const promise = new Promise<T>((next) => { resolve = next })
  return { promise, resolve }
}

function projectAction(action: TargetAction): RecordedAction {
  if (action.kind !== "create-provider") return action
  return {
    kind: action.kind,
    name: action.name,
    baseUrl: action.baseUrl,
    model: action.model,
    credentialPresent: action.credential.kind === "replace" && action.credential.value.length > 0,
    presetKey: action.presetKey ?? null,
  }
}

function provider(
  overrides: Partial<TargetView["providers"][number]> = {},
): TargetView["providers"][number] {
  return {
    id: "provider-1",
    position: 0,
    providerRevision: 1,
    name: "Fixture Provider",
    baseUrl: "https://fixture.example/v1",
    model: "gpt-test",
    protocol: "openai-responses",
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
    providerPresets: overrides.providerPresets ?? [],
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

  async discoverModels(): Promise<never> { throw new Error("not used by this fixture") }
  async checkReachability(): Promise<never> { throw new Error("not used by this fixture") }

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

async function fillProvider(
  mockInput: Awaited<ReturnType<typeof testRender>>["mockInput"],
  fields = ["Fixture Provider", "https://fixture.example/v1", "gpt-test", credentialSentinel],
): Promise<void> {
  await mockInput.typeText(fields[0]!)
  mockInput.pressTab()
  await mockInput.typeText(fields[1]!)
  mockInput.pressTab()
  await mockInput.typeText(fields[2]!)
  mockInput.pressTab()
  await mockInput.typeText(fields[3]!)
}

async function enterProvider(
  setup: Awaited<ReturnType<typeof testRender>>,
  fields = ["Fixture Provider", "https://fixture.example/v1", "gpt-test", credentialSentinel],
): Promise<void> {
  setup.mockInput.pressKey("x", { ctrl: true })
  setup.mockInput.pressKey("p")
  await setup.waitForFrame((frame) => frame.includes("Blank"))
  setup.mockInput.pressEnter()
  await setup.waitForFrame((frame) => frame.includes("Enter save"))
  await fillProvider(setup.mockInput, fields)
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
      "Current Target Provider  —",
      "Serving Provider  —",
      "Routing Service  Running",
      "Managed Configuration  Unmanaged",
      "Activated Snapshot  —",
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

test("renders every visible Provider editor string through the Chinese catalog", async () => {
  const pending = deferred<ActionOutcome>()
  const initial = view()
  const session = new MemoryTargetSession(initial, async () => await pending.promise)
  const setup = await testRender(() => <App session={session} locale="zh-CN" />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    await setup.waitForFrame((frame) => frame.includes("空白"))
    setup.mockInput.pressEnter()
    const editor = await setup.waitForFrame((frame) => frame.includes("Enter 保存 · Esc 取消"))

    expectInOrder(editor, [
      "Provider",
      "名称",
      "示例 Provider",
      "基础 URL",
      "https://provider.example/v1",
      "模型",
      "gpt-model",
      "凭据",
      "API 凭据",
      "Enter 保存 · Esc 取消",
    ])

    await fillProvider(setup.mockInput)
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(await setup.waitForFrame((frame) => frame.includes("正在保存…"))).not.toContain("Saving…")
  } finally {
    pending.resolve({ status: "applied", view: initial })
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

test("maps known and unknown problems without rendering backend messages", async () => {
  const session = new MemoryTargetSession(view({
    providers: [provider({
      id: "provider-without-credential",
      name: "Credential-free Provider",
      baseUrl: "https://fixture.example/v1",
      model: "gpt-test",
      credential: "missing",
      completeness: "incomplete",
      missingFields: ["credential"],
    })],
    problems: [
      {
        code: "untested-target-cli",
        message: `${problemMessageSentinel}-known`,
      },
      {
        code: "future-problem",
        message: `${problemMessageSentinel}-unknown`,
      },
    ],
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    const frame = setup.captureCharFrame()
    expect(frame).toContain("This Target CLI version is untested")
    expect(frame).not.toContain("untested-target-cli")
    expect(frame).toContain("Action failed (future-problem)")
    expect(frame).toContain("Credential  Absent")
    expect(frame).not.toContain(problemMessageSentinel)
    expect(frame).not.toContain(credentialSentinel)
  } finally {
    setup.renderer.destroy()
  }
})

test("renders canonical Chinese status concepts and the managed configuration state", async () => {
  const session = new MemoryTargetSession(view({
    managedConfiguration: { state: "managed", path: "/tmp/home/.codex/config.toml", restartRequired: false },
  }))
  const setup = await testRender(() => <App session={session} locale="zh-CN" />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    const frame = await setup.waitForFrame((next) => next.includes("受管理配置"))
    expect(frame).toContain("当前 Target Provider")
    expect(frame).toContain("服务中 Provider")
    expect(frame).toContain("路由服务")
    expect(frame).toContain("受管理配置")
    expect(frame).toContain("受管理")
    expect(frame).toContain("已激活快照")
    expect(frame).not.toContain("未知（managed）")
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
    await enterProvider(setup)
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
    await enterProvider(setup)
    await setup.renderOnce()
    const maskedBefore = (setup.captureCharFrame().match(/•/g) ?? []).length

    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))
    setup.mockInput.pressKey("n")
    await flushUi(setup)
    const draft = await setup.waitForFrame((frame) => frame.includes("Fixture Provider") && !frame.includes("Discard Provider draft?"))
    expect((draft.match(/•/g) ?? []).length).toBe(maskedBefore)
    await setup.mockInput.typeText(" Restored")
    const restored = await setup.waitForFrame((frame) => frame.includes("Fixture Provider Restored"))
    expect((restored.match(/•/g) ?? []).length).toBe(maskedBefore)
    expect(setup.renderer.isDestroyed).toBeFalse()
    expectSecretFree(setup, session)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("closing dirty exit disposes its command layer before later Provider save and cancel keys", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup)

    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))
    setup.mockInput.pressKey("n")
    await flushUi(setup)
    await setup.waitForFrame((frame) => frame.includes("Fixture Provider") && !frame.includes("Discard Provider draft?"))

    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    await setup.waitForFrame((frame) => frame.includes("Provider saved: Fixture Provider"))

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    await setup.waitForFrame((frame) => frame.includes("Blank"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter save"))
    setup.mockInput.pressEscape()
    await flushUi(setup)
    expect(setup.captureCharFrame()).toContain("Run a target action")
    expect(session.actions).toHaveLength(1)
  } finally {
    setup.renderer.destroy()
  }
})

test("Esc declines dirty exit and keeps the Provider editor", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup)
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
    await enterProvider(setup)
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
    await enterProvider(setup)
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
    providers: [provider({
      id: "provider-1",
      name: "Fixture Provider",
      baseUrl: "https://fixture.example/v1",
      model: "gpt-test",
      credential: "present",
    })],
  })
  const session = new MemoryTargetSession(initial, async (action) => ({
    status: "applied",
    view: action.kind === "create-provider" ? saved : saved,
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup)
    await setup.renderOnce()
    const draftFrame = setup.captureCharFrame()
    expect(draftFrame).toContain("Credential")
    expect(draftFrame).toContain("••••")
    expect(draftFrame).not.toContain(credentialSentinel)

    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    const savedFrame = await setup.waitForFrame((frame) => frame.includes("Credential  Present"))
    expect(savedFrame).toContain("Recent activity")
    expect(savedFrame).toContain("Provider saved: Fixture Provider")
    expect(session.actions[0]).toEqual({
      kind: "create-provider",
      name: "Fixture Provider",
      baseUrl: "https://fixture.example/v1",
      model: "gpt-test",
      credentialPresent: true,
      presetKey: null,
    })
    expect(setup.captureCharFrame()).not.toContain(credentialSentinel)

    session.push(saved)
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("Target state updated.")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitFor(() => session.actions.length === 2)
    const appliedFrame = await setup.waitForFrame((frame) => frame.includes("Target Takeover applied: Fixture Provider"))
    expect(appliedFrame.match(/Target Takeover applied/g)?.length).toBe(1)
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
    await setup.waitForFrame((frame) => frame.includes("Mode       Takeover") && frame.includes("Current Target Provider  Fixture Provider"))

    session.push({ ...active, viewSequence: 3, servingProviderId: "provider-1" })
    const served = await setup.waitForFrame((frame) => frame.includes("Serving Provider  Fixture Provider"))
    expect(served).toContain("Restart Codex")
    expectInOrder(served, [
      "Provider saved: Fixture Provider",
      "Target Takeover applied: Fixture Provider",
      "Target state updated.",
      "Target state updated.",
    ])
    expect(served.match(/Target state updated\./g)?.length).toBe(2)
    expect(served).not.toContain(credentialSentinel)
  } finally {
    setup.renderer.destroy()
  }
})

test("Codex Direct Activation chooses Current or first and renders one restart-guided success", async () => {
  const first = provider({ id: "provider-first", name: "First Provider" })
  const current = provider({ id: "provider-current", position: 1, name: "Current Provider" })
  for (const testCase of [
    { currentProviderId: current.id, expected: current, invocation: "slash" },
    { currentProviderId: null, expected: first, invocation: "leader" },
  ] as const) {
    const initial = view({
      managementRevision: 1,
      viewSequence: 1,
      providers: [first, current],
      currentProviderId: testCase.currentProviderId,
    })
    const direct = view({
      ...initial,
      managementRevision: 2,
      viewSequence: 2,
      mode: "direct",
      currentProviderId: testCase.expected.id,
      managedConfiguration: { state: "managed", path: "/tmp/home/.codex/config.toml", restartRequired: true },
      activatedSnapshot: {
        id: snapshotId,
        providerId: testCase.expected.id,
        model: testCase.expected.model,
        epoch: snapshotEpoch,
      },
    })
    const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: direct }))
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.renderOnce()
      if (testCase.invocation === "slash") {
        await setup.mockInput.typeText("/direct")
        setup.mockInput.pressEnter()
      } else {
        setup.mockInput.pressKey("x", { ctrl: true })
        setup.mockInput.pressKey("d")
      }

      await setup.waitFor(() => session.actions.length === 1)
      const frame = await setup.waitForFrame((next) => next.includes(`Direct Activation applied: ${testCase.expected.name}`))
      expect(session.actions).toEqual([{
        kind: "activate-provider",
        providerId: testCase.expected.id,
        mode: "direct",
      }])
      expect(frame.match(/Direct Activation applied:/g)?.length).toBe(1)
      expect(frame).toContain("Mode       Direct")
      expect(frame).toContain("Restart Codex to use the managed configuration.")
      expect(frame).not.toContain(credentialSentinel)
      expect(frame).not.toContain(problemMessageSentinel)
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("a pending Direct Activation is labeled as Direct rather than Takeover", async () => {
  const pending = deferred<ActionOutcome>()
  const initial = view({ providers: [provider()] })
  const session = new MemoryTargetSession(initial, async () => await pending.promise)
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/direct")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    const frame = await setup.waitForFrame((next) => next.includes("Applying Direct Activation…"))

    expect(frame).not.toContain("Applying Target Takeover…")
    expect(frame).not.toContain(credentialSentinel)
  } finally {
    pending.resolve({ status: "applied", view: initial })
    setup.renderer.destroy()
  }
})

test("Direct preflight requires a complete Provider without sending an action", async () => {
  for (const testCase of [
    {
      initial: view(),
      expected: "Create a Provider before applying a managed configuration.",
    },
    {
      initial: view({
        providers: [provider({
          credential: "missing",
          completeness: "incomplete",
          missingFields: ["credential"],
        })],
      }),
      expected: "Complete the required Provider fields and retry.",
    },
  ]) {
    const session = new MemoryTargetSession(testCase.initial)
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/direct")
      setup.mockInput.pressEnter()
      const frame = await setup.waitForFrame((next) => next.includes(testCase.expected))
      expect(session.actions).toEqual([])
      expect(frame).not.toContain(credentialSentinel)
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("a failed Direct Activation installs the authoritative view and localizes stable codes", async () => {
  const initialProvider = provider({ id: "provider-1", name: "Before" })
  const authoritative = view({
    managementRevision: 2,
    viewSequence: 2,
    providers: [{ ...initialProvider, name: "Authoritative Provider" }],
    currentProviderId: initialProvider.id,
  })
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(view({
    managementRevision: 1,
    viewSequence: 1,
    providers: [initialProvider],
  }), async () => {
    session.setAuthoritative(authoritative)
    throw { code: "takeover-active", message: problemMessageSentinel }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/direct")
    setup.mockInput.pressEnter()
    const frame = await setup.waitForFrame((next) => next.includes("Disable Target Takeover before using Direct Activation."))

    expect(frame).toContain("Current Target Provider  Authoritative Provider")
    expect(frame.match(/Disable Target Takeover before using Direct Activation\./g)?.length).toBe(1)
    expect(frame).not.toContain(problemMessageSentinel)
    expect(frame).not.toContain(credentialSentinel)
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
    await enterProvider(setup)
    setup.mockInput.pressEnter()
    const frame = await setup.waitForFrame((next) => next.includes("Check the fields and retry"))
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
  const before = provider({
    id: "provider-1",
    name: "Before",
    baseUrl: "https://fixture.example/v1",
    model: "gpt-test",
    credential: "present",
  })
  const initial = view({ managementRevision: 1, viewSequence: 1, providers: [before] })
  const authoritative = view({
    managementRevision: 2,
    viewSequence: 2,
    providers: [{ ...before, name: "Authoritative Provider" }],
    currentProviderId: "provider-1",
  })
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(initial, async () => {
    session.setAuthoritative(authoritative)
    throw Object.assign(new Error(problemMessageSentinel), { code: "stale-revision" })
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    const frame = await setup.waitForFrame((next) => next.includes("Retry the action"))
    expect(frame).toContain("Current Target Provider  Authoritative Provider")
    expect(frame).toContain("Target state changed")
    expect(frame.match(/Target state changed\. Retry the action\./g)?.length).toBe(1)
    expect(frame).not.toContain(problemMessageSentinel)
  } finally {
    setup.renderer.destroy()
  }
})

test("a replayed Provider save appends one success activity", async () => {
  const saved = view({
    managementRevision: 1,
    viewSequence: 1,
    providers: [provider({
      id: "provider-1",
      name: "Fixture Provider",
      baseUrl: "https://fixture.example/v1",
      model: "gpt-test",
      credential: "present",
    })],
  })
  const session = new MemoryTargetSession(view(), async () => ({ status: "replayed", view: saved }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await enterProvider(setup)
    setup.mockInput.pressEnter()
    const frame = await setup.waitForFrame((next) => next.includes("Provider saved: Fixture Provider"))
    expect(frame.match(/Provider saved: Fixture Provider/g)?.length).toBe(1)
    expect(frame).not.toContain("Target state updated.")
    expectSecretFree(setup, session)
  } finally {
    setup.renderer.destroy()
  }
})

test("an unavailable Codex command appends one localized error activity", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.waitForFrame((frame) => frame.includes("Mode       Unmanaged"))
    await setup.mockInput.typeText("/not-a-command")
    setup.mockInput.pressEnter()
    const frame = await setup.waitForFrame((next) => next.includes("Unknown or unavailable command"))
    expect(frame).toContain("Recent activity")
    expect(frame.match(/Unknown or unavailable command: \/not-a-command/g)?.length).toBe(1)
  } finally {
    setup.renderer.destroy()
  }
})

test("subscribed Target View activity keeps only the latest 50 entries", async () => {
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 100, useThread: false })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.waitForFrame((frame) => frame.includes("Mode       Unmanaged"))
    await setup.mockInput.typeText("/drop-this-oldest-entry")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("/drop-this-oldest-entry"))
    for (let sequence = 1; sequence <= 50; sequence++) {
      session.push(view({ viewSequence: sequence }))
    }
    await setup.renderOnce()
    const frame = setup.captureCharFrame()
    expect(frame.match(/Target state updated\./g)?.length).toBe(50)
    expect(frame).not.toContain("drop-this-oldest-entry")
  } finally {
    setup.renderer.destroy()
  }
})
