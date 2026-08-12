import { expect, spyOn, test } from "bun:test"
import { createTestRenderer, type TestRendererSetup } from "@opentui/core/testing"
import { render } from "@opentui/solid"

import { run, type RunPorts } from "../src/app"
import type { TargetSession } from "../src/control/target-session"
import type { ActionOutcome, TargetAction, TargetView } from "../src/control/types"

const initialView: TargetView = {
  target: "codex",
  managementRevision: 0,
  viewSequence: 0,
  service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
  mode: "unmanaged",
  takeover: { state: "inactive", endpoint: null },
  providers: [],
  currentProviderId: null,
  servingProviderId: null,
  managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
  recovery: { intentId: null, state: "clean" },
  activatedSnapshot: null,
  problems: [],
}

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void
  const promise = new Promise<T>((next) => { resolve = next })
  return { promise, resolve }
}

class LifecycleSession implements TargetSession {
  closeCalls = 0
  readonly closed = deferred<void>()

  get(): Readonly<TargetView> { return initialView }
  async act(_action: TargetAction): Promise<ActionOutcome> { return { status: "applied", view: initialView } }
  subscribe(_listener: (next: TargetView) => void): () => void { return () => {} }
  async close(): Promise<void> { this.closeCalls++ }
  whenClosed(): Promise<void> { return this.closed.promise }
}

async function rendererFixture(): Promise<{
  setup: TestRendererSetup
  destroyCalls: () => number
}> {
  const setup = await createTestRenderer({ width: 80, height: 24, useThread: false })
  const destroy = setup.renderer.destroy.bind(setup.renderer)
  let calls = 0
  setup.renderer.destroy = () => {
    calls++
    destroy()
  }
  return { setup, destroyCalls: () => calls }
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
    clock: {
      now: () => 0,
      sleep: async () => {},
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
    setup.mockInput.pressKey("q")
    await running
    expect(events).toEqual(["connect", "render"])
    expect(session.closeCalls).toBe(1)
    expect(destroyCalls()).toBe(1)
    expect(exitSpy).not.toHaveBeenCalled()
  } finally {
    exitSpy.mockRestore()
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
    expect(attempts).toHaveLength(2)
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
    },
  }))
  await expect(failure).rejects.toMatchObject({ code: "service-unavailable" })
  expect(session.closeCalls).toBe(0)
  expect(destroyCalls()).toBe(1)
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
