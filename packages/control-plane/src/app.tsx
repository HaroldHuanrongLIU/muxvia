import { createCliRenderer, type CliRenderer } from "@opentui/core"
import { render, type JSX } from "@opentui/solid"
import { spawn } from "node:child_process"
import { dirname, isAbsolute } from "node:path"

import { RpcClient, ControlError } from "./control/rpc-client"
import type { TargetSession } from "./control/target-session"
import { App } from "./ui/app"

export interface RunOptions {
  servicePath: string
  socketPath: string
  release: string
}

export interface Clock {
  now(): number
  sleep(milliseconds: number): Promise<void>
  timeout(milliseconds: number, callback: () => void): () => void
}

interface SpawnOptions {
  shell: false
}

export interface RunPorts {
  connect(socketPath: string, release: string, signal: AbortSignal): Promise<TargetSession>
  spawn(path: string, args: string[], options: SpawnOptions): void
  createRenderer(): Promise<CliRenderer>
  render(node: () => JSX.Element, renderer: CliRenderer): Promise<void>
  clock: Clock
}

const readinessTimeoutMs = 2_000
const retryIntervalMs = 50

interface TargetControl {
  openTarget(target: "codex"): Promise<TargetSession>
  close(): Promise<void>
}

type ControlConnector = (
  socketPath: string,
  release: string,
  signal: AbortSignal,
) => Promise<TargetControl>

export async function connectTargetSession(
  socketPath: string,
  release: string,
  signal: AbortSignal,
  connect: ControlConnector = RpcClient.connect,
): Promise<TargetSession> {
  if (signal.aborted) throw new ConnectionDeadlineError()

  let control: TargetControl | undefined
  let closing: Promise<void> | undefined
  const closeControl = (): Promise<void> => {
    if (!control) return Promise.resolve()
    closing ??= control.close().catch(() => {})
    return closing
  }
  let rejectAborted!: (error: ConnectionDeadlineError) => void
  const aborted = new Promise<never>((_, reject) => {
    rejectAborted = reject
  })
  const onAbort = () => {
    void closeControl()
    rejectAborted(new ConnectionDeadlineError())
  }
  signal.addEventListener("abort", onAbort, { once: true })

  const connected = connect(socketPath, release, signal).then(async (next) => {
    control = next
    if (!signal.aborted) return next
    await closeControl()
    throw new ConnectionDeadlineError()
  })

  try {
    control = await Promise.race([connected, aborted])
    const opening = control.openTarget("codex").then(async (session) => {
      if (!signal.aborted) return session
      await closeControl()
      throw new ConnectionDeadlineError()
    })
    return await Promise.race([opening, aborted])
  } catch (error) {
    await closeControl()
    throw error
  } finally {
    signal.removeEventListener("abort", onAbort)
  }
}

const productionPorts: RunPorts = {
  connect: connectTargetSession,
  spawn: (path, args) => {
    const child = spawn(path, args, { shell: false, detached: true, stdio: "ignore" })
    child.unref()
  },
  createRenderer: () => createCliRenderer({
    exitOnCtrlC: false,
    useKittyKeyboard: {},
    autoFocus: false,
  }),
  render: async (node, renderer) => { await render(node, renderer) },
  clock: {
    now: () => Date.now(),
    sleep: async (milliseconds) => { await Bun.sleep(milliseconds) },
    timeout: (milliseconds, callback) => {
      const timer = setTimeout(callback, milliseconds)
      return () => clearTimeout(timer)
    },
  },
}

function socketUnavailable(error: unknown): boolean {
  if (typeof error !== "object" || error === null || !("code" in error)) return false
  return ["ENOENT", "ECONNREFUSED", "service-unavailable"].includes(String(error.code))
}

function muxviaHomeForSocket(socketPath: string): string {
  return dirname(dirname(socketPath))
}

class ConnectionDeadlineError extends ControlError {
  constructor() {
    super("service-unavailable", "Routing Service did not become ready")
  }
}

async function connectBeforeDeadline(
  options: RunOptions,
  ports: RunPorts,
  deadline: number,
): Promise<TargetSession> {
  const remaining = deadline - ports.clock.now()
  if (remaining <= 0) throw new ConnectionDeadlineError()

  let expired = false
  const controller = new AbortController()
  let cancelTimeout = () => {}
  const timeout = new Promise<never>((_, reject) => {
    cancelTimeout = ports.clock.timeout(remaining, () => {
      expired = true
      controller.abort()
      reject(new ConnectionDeadlineError())
    })
  })
  const connection = Promise.resolve()
    .then(() => ports.connect(options.socketPath, options.release, controller.signal))
    .then(async (session) => {
      if (!expired) return session
      try {
        await session.close()
      } catch {
        // A timed-out connection is abandoned; cleanup failure cannot replace the deadline result.
      }
      throw new ConnectionDeadlineError()
    })

  try {
    return await Promise.race([connection, timeout])
  } finally {
    if (!expired) cancelTimeout()
  }
}

async function connectOrStart(options: RunOptions, ports: RunPorts): Promise<TargetSession> {
  const deadline = ports.clock.now() + readinessTimeoutMs
  try {
    return await connectBeforeDeadline(options, ports, deadline)
  } catch (error) {
    if (error instanceof ConnectionDeadlineError) throw error
    if (!socketUnavailable(error)) throw error
  }

  if (!isAbsolute(options.servicePath)) {
    throw new Error("Routing Service path must be absolute")
  }
  ports.spawn(options.servicePath, ["--home", muxviaHomeForSocket(options.socketPath)], { shell: false })

  while (ports.clock.now() < deadline) {
    try {
      return await connectBeforeDeadline(options, ports, deadline)
    } catch (error) {
      if (error instanceof ConnectionDeadlineError) throw error
      if (!socketUnavailable(error)) throw error
    }
    await ports.clock.sleep(retryIntervalMs)
  }
  throw new ControlError("service-unavailable", "Routing Service did not become ready")
}

export async function run(options: RunOptions, ports: RunPorts = productionPorts): Promise<void> {
  const renderer = await ports.createRenderer()
  const destroyed = new Promise<void>((resolve) => renderer.once("destroy", resolve))
  let session: TargetSession | undefined
  try {
    session = await connectOrStart(options, ports)
    await ports.render(() => <App session={session!} />, renderer)
    const sessionClosed = session.whenClosed().then(
      () => { if (!renderer.isDestroyed) renderer.destroy() },
      () => { if (!renderer.isDestroyed) renderer.destroy() },
    )
    await Promise.race([destroyed, sessionClosed])
  } finally {
    try {
      renderer.setTerminalTitle("")
      if (session) await session.close()
    } finally {
      if (!renderer.isDestroyed) renderer.destroy()
      await destroyed
    }
  }
}
