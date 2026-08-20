import { expect, spyOn, test } from "bun:test"
import { createTestRenderer, type TestRendererSetup } from "@opentui/core/testing"
import { render } from "@opentui/solid"

import {
  connectTargetSession,
  run,
  type RunPorts,
  type SignalName,
  type SignalSource,
} from "../src/app"
import type { TargetSession } from "../src/control/target-session"
import type { UniversalProviderSession } from "../src/control/universal-provider-session"
import type { ActionOutcome, ClaudePreflightContext, TargetAction, TargetView, UniversalProviderAction, UniversalProviderCatalogView, UniversalProviderOutcome } from "../src/control/types"

const initialView: TargetView = {
  target: "codex",
  managementRevision: 0,
  viewSequence: 0,
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

const credentialSentinel = "provider-secret-must-not-render"

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void
  const promise = new Promise<T>((next) => { resolve = next })
  return { promise, resolve }
}

async function flushMicrotasks(): Promise<void> {
  for (let pass = 0; pass < 8; pass++) await Promise.resolve()
}

class ManualClock {
  #now = 0
  #nextId = 0
  #timers = new Map<number, { at: number; callback: () => void }>()

  now = () => this.#now
  sleep = async (milliseconds: number) => { this.advance(milliseconds) }
  timeout = (milliseconds: number, callback: () => void) => {
    const id = this.#nextId++
    this.#timers.set(id, { at: this.#now + milliseconds, callback })
    return () => { this.#timers.delete(id) }
  }

  advance(milliseconds: number): void {
    this.#now += milliseconds
    const due = [...this.#timers.entries()].filter(([, timer]) => timer.at <= this.#now)
    for (const [id, timer] of due) {
      this.#timers.delete(id)
      timer.callback()
    }
  }

  pendingTimers(): number {
    return this.#timers.size
  }
}

class ManualSignalSource implements SignalSource {
  #handlers = new Map<SignalName, Set<() => void>>()
  unlistenCalls = 0

  listen(name: SignalName, handler: () => void): () => void {
    const handlers = this.#handlers.get(name) ?? new Set()
    handlers.add(handler)
    this.#handlers.set(name, handlers)
    let listening = true
    return () => {
      this.unlistenCalls++
      if (!listening) return
      listening = false
      handlers.delete(handler)
    }
  }

  emit(name: SignalName): void {
    for (const handler of [...(this.#handlers.get(name) ?? [])]) handler()
  }
}

class LifecycleSession implements TargetSession {
  closeCalls = 0
  saveCalls = 0
  credentialPresent = false
  readonly closed = deferred<void>()
  pendingSave?: Promise<ActionOutcome>

  get(): Readonly<TargetView> { return initialView }
  async discoverModels(): Promise<never> { throw new Error("not used by this fixture") }
  async checkReachability(): Promise<never> { throw new Error("not used by this fixture") }
  async previewReconciliation(): Promise<never> { throw new Error("reconciliation not configured in this fixture") }
  async applyReconciliation(): Promise<never> { throw new Error("reconciliation not configured in this fixture") }
  async probeCompatibility(): Promise<never> { throw new Error("compatibility probe not configured in this fixture") }
  async resolveCompatibility(): Promise<never> { throw new Error("compatibility resolution not configured in this fixture") }
  async act(action: TargetAction): Promise<ActionOutcome> {
    if (action.kind === "create-provider") {
      this.saveCalls++
      this.credentialPresent = action.credential.kind === "replace" && action.credential.value.length > 0
      if (this.pendingSave) return await this.pendingSave
    }
    return { status: "applied", view: initialView }
  }
  subscribe(_listener: (next: TargetView) => void): () => void { return () => {} }
  async close(): Promise<void> { this.closeCalls++ }
  whenClosed(): Promise<void> { return this.closed.promise }
}

class LifecycleUniversalSession implements UniversalProviderSession {
  closeCalls = 0
  readonly closed = deferred<void>()
  readonly view: UniversalProviderCatalogView = {
    revision: 0,
    viewSequence: 0,
    providers: [],
    presets: [],
  }
  get(): Readonly<UniversalProviderCatalogView> { return this.view }
  async act(_action: UniversalProviderAction): Promise<UniversalProviderOutcome> {
    return { status: "applied", view: this.view }
  }
  subscribe(_listener: (next: UniversalProviderCatalogView) => void): () => void { return () => {} }
  async close(): Promise<void> { this.closeCalls++ }
  whenClosed(): Promise<void> { return this.closed.promise }
}

async function enterDirtyProvider(setup: TestRendererSetup): Promise<void> {
  setup.mockInput.pressKey("1")
  await setup.renderOnce()
  setup.mockInput.pressKey("x", { ctrl: true })
  setup.mockInput.pressKey("p")
  await setup.waitForFrame((frame) => frame.includes("Blank"))
  setup.mockInput.pressEnter()
  await setup.waitForFrame((frame) => frame.includes("Enter save"))
  await setup.mockInput.typeText("Pending Provider")
  setup.mockInput.pressTab()
  await setup.mockInput.typeText("https://pending.example/v1")
  setup.mockInput.pressTab()
  await setup.mockInput.typeText("pending-model")
  setup.mockInput.pressTab()
  await setup.mockInput.typeText(credentialSentinel)
}

async function rendererFixture(): Promise<{
  setup: TestRendererSetup
  destroyCalls: () => number
  titles: string[]
}> {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  const destroy = setup.renderer.destroy.bind(setup.renderer)
  const setTerminalTitle = setup.renderer.setTerminalTitle.bind(setup.renderer)
  let calls = 0
  const titles: string[] = []
  setup.renderer.destroy = () => {
    calls++
    destroy()
  }
  setup.renderer.setTerminalTitle = (title) => {
    titles.push(title)
    setTerminalTitle(title)
  }
  return { setup, destroyCalls: () => calls, titles }
}

function ports(
  setup: TestRendererSetup,
  session: LifecycleSession,
  overrides: Partial<RunPorts> = {},
): RunPorts {
  return {
    connect: async () => session,
    spawn: () => {},
    createRenderer: async () => setup.renderer,
    render: async (node, renderer) => { await render(node, renderer) },
    signals: new ManualSignalSource(),
    clock: {
      now: () => 0,
      sleep: async () => {},
      timeout: () => () => {},
    },
    ...overrides,
  }
}

const options = {
  servicePath: "/opt/muxvia/muxvia-routing",
  socketPath: "/tmp/operator-home/.muxvia/run/control.sock",
  release: "test-release",
}

test("an available service connects before rendering and never spawns", async () => {
  const { setup, destroyCalls } = await rendererFixture()
  const session = new LifecycleSession()
  const events: string[] = []
  const exitSpy = spyOn(process, "exit")
  const activePorts = ports(setup, session, {
    connect: async () => { events.push("connect"); return session },
    spawn: () => { events.push("spawn") },
    render: async (node, renderer) => { events.push("render"); await render(node, renderer) },
  })
  try {
    const running = run(options, activePorts)
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    setup.mockInput.pressCtrlC()
    await running
    expect(events).toEqual(["connect", "connect", "render"])
    expect(session.closeCalls).toBe(1)
    expect(destroyCalls()).toBe(1)
    expect(exitSpy).not.toHaveBeenCalled()
  } finally {
    exitSpy.mockRestore()
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("startup opens independent Codex and Claude sessions and closes each exactly once", async () => {
  const { setup } = await rendererFixture()
  const codex = new LifecycleSession()
  const claude = new LifecycleSession()
  const targets: string[] = []
  const running = run(options, ports(setup, codex, {
    connect: async (_socketPath, _release, _signal, target) => {
      targets.push(target)
      return target === "codex" ? codex : claude
    },
  }))
  try {
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    setup.mockInput.pressCtrlC()
    await running
    expect(targets).toEqual(["codex", "claude"])
    expect(codex.closeCalls).toBe(1)
    expect(claude.closeCalls).toBe(1)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("startup mounts and closes one independent Universal Provider catalog session", async () => {
  const { setup } = await rendererFixture()
  const target = new LifecycleSession()
  const catalog = new LifecycleUniversalSession()
  let catalogConnections = 0
  let catalogContext: ClaudePreflightContext | undefined
  const running = run(options, ports(setup, target, {
    connectUniversalProviders: async (_socketPath, _release, _signal, claudeContext) => {
      catalogConnections++
      catalogContext = claudeContext
      return catalog
    },
  }))
  try {
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/universal-providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("No Universal Providers"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Codex · Control Plane") && !frame.includes("No Universal Providers"))
    setup.mockInput.pressCtrlC()
    await running
    expect(catalogConnections).toBe(1)
    expect(catalogContext).toMatchObject({
      claudeConfigDir: null,
      selectorState: "unset",
      hostManagedState: "unmanaged",
    })
    expect(catalog.closeCalls).toBe(1)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("one unavailable target remains target-local while its peer renders and closes normally", async () => {
  const { setup } = await rendererFixture()
  const codex = new LifecycleSession()
  const running = run(options, ports(setup, codex, {
    connect: async (_socketPath, _release, _signal, target) => {
      if (target === "claude") throw Object.assign(new Error("Claude unavailable"), { code: "incompatible-target-cli" })
      return codex
    },
  }))
  try {
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    setup.mockInput.pressKey("2")
    await setup.waitForFrame((frame) => frame.includes("Claude Code") && frame.includes("incompatible"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Choose a target"))
    setup.mockInput.pressKey("1")
    await setup.waitForFrame((frame) => frame.includes("Mode") && frame.includes("Codex"))
    setup.mockInput.pressCtrlC()
    await running
    expect(codex.closeCalls).toBe(1)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("a stalled Claude connection times out target-locally after Codex is ready", async () => {
  const { setup } = await rendererFixture()
  const codex = new LifecycleSession()
  const lateClaude = new LifecycleSession()
  const claudeConnection = deferred<TargetSession>()
  const clock = new ManualClock()
  const targets: string[] = []
  const running = run(options, ports(setup, codex, {
    connect: async (_socketPath, _release, _signal, target) => {
      targets.push(target)
      return target === "codex" ? codex : await claudeConnection.promise
    },
    clock,
  }))
  try {
    for (let pass = 0; pass < 20 && targets.length < 2; pass++) await flushMicrotasks()
    expect(targets).toEqual(["codex", "claude"])
    clock.advance(2_000)
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    setup.mockInput.pressKey("2")
    await setup.waitForFrame((frame) => frame.includes("Routing Service is unavailable"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Choose a target"))
    setup.mockInput.pressKey("1")
    await setup.waitForFrame((frame) => frame.includes("Codex · Control Plane"))
    expect(clock.pendingTimers()).toBe(0)
    setup.mockInput.pressCtrlC()
    await running
    expect(codex.closeCalls).toBe(1)

    claudeConnection.resolve(lateClaude)
    await flushMicrotasks()
    expect(lateClaude.closeCalls).toBe(1)
  } finally {
    claudeConnection.resolve(lateClaude)
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    await running.catch(() => {})
  }
})

test("post-open Claude loss becomes unavailable without ending the Codex session", async () => {
  const { setup } = await rendererFixture()
  const codex = new LifecycleSession()
  const claude = new LifecycleSession()
  const running = run(options, ports(setup, codex, {
    connect: async (_socketPath, _release, _signal, target) => target === "codex" ? codex : claude,
  }))
  try {
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    claude.closed.resolve()
    await flushMicrotasks()
    setup.mockInput.pressKey("2")
    await setup.waitForFrame((frame) => frame.includes("Routing Service is unavailable"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Choose a target"))
    setup.mockInput.pressKey("1")
    await setup.waitForFrame((frame) => frame.includes("Codex · Control Plane"))
    expect(codex.closeCalls).toBe(0)
    setup.mockInput.pressCtrlC()
    await running
    expect(codex.closeCalls).toBe(1)
    expect(claude.closeCalls).toBe(1)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("an unavailable socket directly spawns the absolute sidecar with its Muxvia Home", async () => {
  const { setup } = await rendererFixture()
  const session = new LifecycleSession()
  const attempts: string[] = []
  const spawns: Array<{ path: string; args: string[]; shell: boolean }> = []
  let connects = 0
  const running = run(options, ports(setup, session, {
    connect: async (socketPath, release) => {
      attempts.push(`${socketPath}:${release}`)
      connects++
      if (connects === 1) throw Object.assign(new Error("missing"), { code: "ENOENT" })
      return session
    },
    spawn: (path, args, spawnOptions) => { spawns.push({ path, args, shell: spawnOptions.shell }) },
  }))
  try {
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    setup.mockInput.pressCtrlC()
    await running
    expect(attempts).toHaveLength(3)
    expect(spawns).toEqual([{
      path: "/opt/muxvia/muxvia-routing",
      args: ["--home", "/tmp/operator-home/.muxvia"],
      shell: false,
    }])
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
  }
})

test("a relative sidecar path rejects before spawn and restores the renderer", async () => {
  const { setup, destroyCalls } = await rendererFixture()
  const session = new LifecycleSession()
  let spawnCalls = 0
  await expect(run(
    { ...options, servicePath: "muxvia-routing" },
    ports(setup, session, {
      connect: async () => { throw Object.assign(new Error("missing"), { code: "ENOENT" }) },
      spawn: () => { spawnCalls++ },
    }),
  )).rejects.toThrow("absolute")
  expect(spawnCalls).toBe(0)
  expect(session.closeCalls).toBe(0)
  expect(destroyCalls()).toBe(1)
})

test("a protocol connection failure never starts a second service", async () => {
  const { setup, destroyCalls } = await rendererFixture()
  const session = new LifecycleSession()
  let spawnCalls = 0
  let now = 0
  await expect(run(options, ports(setup, session, {
    connect: async () => { throw Object.assign(new Error("handshake closed"), { code: "connection-closed" }) },
    spawn: () => { spawnCalls++ },
    clock: {
      now: () => now,
      sleep: async (milliseconds) => { now += milliseconds },
      timeout: () => () => {},
    },
  }))).rejects.toMatchObject({ code: "connection-closed" })
  expect(spawnCalls).toBe(0)
  expect(destroyCalls()).toBe(1)
})

test("readiness timeout reports service-unavailable and restores the renderer", async () => {
  const { setup, destroyCalls } = await rendererFixture()
  const session = new LifecycleSession()
  let now = 0
  const failure = run(options, ports(setup, session, {
    connect: async () => { throw Object.assign(new Error("refused"), { code: "ECONNREFUSED" }) },
    clock: {
      now: () => now,
      sleep: async (milliseconds) => { now += milliseconds },
      timeout: () => () => {},
    },
  }))
  await expect(failure).rejects.toMatchObject({ code: "service-unavailable" })
  expect(session.closeCalls).toBe(0)
  expect(destroyCalls()).toBe(1)
})

test("a never-resolving initial connection is bounded and a late session is closed", async () => {
  const { setup, destroyCalls } = await rendererFixture()
  const late = new LifecycleSession()
  const connection = deferred<TargetSession>()
  const clock = new ManualClock()
  let spawnCalls = 0
  const running = run(options, ports(setup, late, {
    connect: () => connection.promise,
    spawn: () => { spawnCalls++ },
    clock,
  }))
  try {
    await flushMicrotasks()
    clock.advance(2_000)
    await expect(running).rejects.toMatchObject({ code: "service-unavailable" })
    expect(spawnCalls).toBe(0)
    expect(destroyCalls()).toBe(1)
    expect(clock.pendingTimers()).toBe(0)

    connection.resolve(late)
    await flushMicrotasks()
    expect(late.closeCalls).toBe(1)
  } finally {
    connection.resolve(late)
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    await running.catch(() => {})
  }
})

test("a never-resolving post-spawn connection shares the deadline and closes a late session", async () => {
  const { setup, destroyCalls } = await rendererFixture()
  const late = new LifecycleSession()
  const connection = deferred<TargetSession>()
  const clock = new ManualClock()
  let connects = 0
  let spawnCalls = 0
  const running = run(options, ports(setup, late, {
    connect: () => {
      connects++
      if (connects === 1) return Promise.reject(Object.assign(new Error("missing"), { code: "ENOENT" }))
      return connection.promise
    },
    spawn: () => { spawnCalls++ },
    clock,
  }))
  try {
    await flushMicrotasks()
    expect(connects).toBe(3)
    clock.advance(2_000)
    await expect(running).rejects.toMatchObject({ code: "service-unavailable" })
    expect(spawnCalls).toBe(1)
    expect(destroyCalls()).toBe(1)
    expect(clock.pendingTimers()).toBe(0)

    connection.resolve(late)
    await flushMicrotasks()
    expect(late.closeCalls).toBe(1)
  } finally {
    connection.resolve(late)
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    await running.catch(() => {})
  }
})

test("the production connector closes its control client when opening the Target fails", async () => {
  const failure = Object.assign(new Error("Target rejected"), {
    code: "incompatible-target-cli",
    detail: "preserve-me",
  })
  let closeCalls = 0
  const control = {
    openTarget: async () => { throw failure },
    close: async () => {
      closeCalls++
      throw new Error("close also failed")
    },
  }

  await expect(connectTargetSession("/tmp/control.sock", "test", new AbortController().signal, async () => control)).rejects.toBe(failure)
  expect(closeCalls).toBe(1)
})

test("the production connector aborts a control whose open Target never settles", async () => {
  const controller = new AbortController()
  const opening = deferred<TargetSession>()
  const lateSession = new LifecycleSession()
  let closeCalls = 0
  let openCalls = 0
  const control = {
    openTarget: () => {
      openCalls++
      return opening.promise
    },
    close: async () => { closeCalls++ },
  }

  const connecting = connectTargetSession(
    "/tmp/control.sock",
    "test",
    controller.signal,
    async (_socketPath, _release, signal) => {
      expect(signal).toBe(controller.signal)
      return control
    },
  )
  await flushMicrotasks()
  expect(openCalls).toBe(1)
  controller.abort()
  await expect(connecting).rejects.toMatchObject({ code: "service-unavailable" })
  expect(closeCalls).toBe(1)

  opening.resolve(lateSession)
  await flushMicrotasks()
  expect(closeCalls).toBe(1)
})

test("Ctrl+C, session closure, and render failure each close and destroy exactly once", async () => {
  for (const exit of ["ctrl-c", "session-close", "render-error"] as const) {
    const { setup, destroyCalls } = await rendererFixture()
    const session = new LifecycleSession()
    const activePorts = ports(setup, session, exit === "render-error" ? {
      render: async () => { throw new Error("render failed") },
    } : {})
    try {
      const running = run(options, activePorts)
      if (exit === "render-error") {
        await expect(running).rejects.toThrow("render failed")
      } else {
        await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
        if (exit === "ctrl-c") setup.mockInput.pressCtrlC()
        else session.closed.resolve()
        await running
      }
      expect(session.closeCalls).toBe(1)
      expect(destroyCalls()).toBe(1)
    } finally {
      if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    }
  }
})

test.each(["SIGHUP", "SIGINT", "SIGTERM"] as const)(
  "%s emitted twice converges on one cleanup without exiting or stopping the service",
  async (signal) => {
    const { setup, destroyCalls, titles } = await rendererFixture()
    const session = new LifecycleSession()
    const signals = new ManualSignalSource()
    const exitSpy = spyOn(process, "exit")
    let spawnCalls = 0
    try {
      const running = run(options, ports(setup, session, {
        signals,
        spawn: () => { spawnCalls++ },
      }))
      await setup.waitForFrame((frame) => frame.includes("MUXVIA"))

      signals.emit(signal)
      signals.emit(signal)
      await running

      expect(signals.unlistenCalls).toBe(3)
      expect(session.closeCalls).toBe(1)
      expect(titles.at(-1)).toBe("")
      expect(destroyCalls()).toBe(1)
      expect(spawnCalls).toBe(0)
      expect(exitSpy).not.toHaveBeenCalled()
    } finally {
      exitSpy.mockRestore()
      if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    }
  },
)

test("a startup signal finishes cleanup without waiting for a stalled connection", async () => {
  const { setup, destroyCalls, titles } = await rendererFixture()
  const session = new LifecycleSession()
  const signals = new ManualSignalSource()
  const connection = deferred<TargetSession>()
  const clock = new ManualClock()
  const connectorSignals: AbortSignal[] = []
  let renderCalls = 0
  let spawnCalls = 0
  const running = run(options, ports(setup, session, {
    signals,
    connect: async (_socketPath, _release, signal) => {
      connectorSignals.push(signal)
      return await connection.promise
    },
    spawn: () => { spawnCalls++ },
    render: async () => { renderCalls++ },
    clock,
  }))
  try {
    await flushMicrotasks()
    expect(clock.pendingTimers()).toBe(2)
    signals.emit("SIGTERM")
    await Promise.race([
      running,
      Bun.sleep(100).then(() => { throw new Error("startup signal did not finish cleanup") }),
    ])

    expect(renderCalls).toBe(0)
    expect(spawnCalls).toBe(0)
    expect(session.closeCalls).toBe(0)
    expect(signals.unlistenCalls).toBe(3)
    expect(titles.at(-1)).toBe("")
    expect(destroyCalls()).toBe(1)
    expect(connectorSignals).toHaveLength(2)
    expect(connectorSignals.every((signal) => signal.aborted)).toBeTrue()
    expect(clock.pendingTimers()).toBe(0)

    connection.resolve(session)
    await flushMicrotasks()
    expect(session.closeCalls).toBe(1)
  } finally {
    connection.resolve(session)
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    await running.catch(() => {})
  }
})

test("a renderer destroyed during creation needs no destroy event subscription to finish cleanup", async () => {
  const { setup, destroyCalls, titles } = await rendererFixture()
  const session = new LifecycleSession()
  const signals = new ManualSignalSource()
  let connectCalls = 0
  let renderCalls = 0
  await run(options, ports(setup, session, {
    signals,
    createRenderer: async () => {
      setup.renderer.destroy()
      return setup.renderer
    },
    connect: async () => { connectCalls++; return session },
    render: async () => { renderCalls++ },
  }))

  expect(connectCalls).toBe(0)
  expect(renderCalls).toBe(0)
  expect(session.closeCalls).toBe(0)
  expect(signals.unlistenCalls).toBe(3)
  expect(titles.at(-1)).toBe("")
  expect(destroyCalls()).toBe(1)
})

test("a mounted render rejection rethrows the same error after one cleanup", async () => {
  const { setup, destroyCalls, titles } = await rendererFixture()
  const session = new LifecycleSession()
  const signals = new ManualSignalSource()
  const failure = new Error("render failed after mount")
  const running = run(options, ports(setup, session, {
    signals,
    render: async (node, renderer) => {
      await render(node, renderer)
      await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
      throw failure
    },
  }))
  try {
    await expect(running).rejects.toBe(failure)
    expect(signals.unlistenCalls).toBe(3)
    expect(session.closeCalls).toBe(1)
    expect(titles.at(-1)).toBe("")
    expect(destroyCalls()).toBe(1)
  } finally {
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    await running.catch(() => {})
  }
})

test("dirty confirmation completes run cleanup once before a pending save settles", async () => {
  const { setup, destroyCalls } = await rendererFixture()
  const session = new LifecycleSession()
  const save = deferred<ActionOutcome>()
  session.pendingSave = save.promise
  const running = run(options, ports(setup, session))
  try {
    await setup.waitForFrame((frame) => frame.includes("MUXVIA"))
    await enterDirtyProvider(setup)
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.saveCalls === 1)
    setup.mockInput.pressCtrlC()
    await setup.waitForFrame((frame) => frame.includes("Discard Provider draft?"))
    setup.mockInput.pressEnter()
    await running

    expect(session.credentialPresent).toBeTrue()
    expect(session.closeCalls).toBe(1)
    expect(destroyCalls()).toBe(1)
    expect(setup.captureCharFrame()).not.toContain(credentialSentinel)

    save.resolve({ status: "applied", view: initialView })
    await flushMicrotasks()
    expect(session.closeCalls).toBe(1)
    expect(destroyCalls()).toBe(1)
  } finally {
    save.resolve({ status: "applied", view: initialView })
    if (!setup.renderer.isDestroyed) setup.renderer.destroy()
    await running.catch(() => {})
  }
})
