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

interface Clock {
  now(): number
  sleep(milliseconds: number): Promise<void>
}

interface SpawnOptions {
  shell: false
}

export interface RunPorts {
  connect(socketPath: string, release: string): Promise<TargetSession>
  spawn(path: string, args: string[], options: SpawnOptions): void
  createRenderer(): Promise<CliRenderer>
  render(node: () => JSX.Element, renderer: CliRenderer): Promise<void>
  clock: Clock
}

const readinessTimeoutMs = 2_000
const retryIntervalMs = 50

const productionPorts: RunPorts = {
  connect: async (socketPath, release) => {
    const control = await RpcClient.connect(socketPath, release)
    return await control.openTarget("codex")
  },
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
  },
}

function socketUnavailable(error: unknown): boolean {
  if (typeof error !== "object" || error === null || !("code" in error)) return false
  return ["ENOENT", "ECONNREFUSED", "service-unavailable"].includes(String(error.code))
}

function muxviaHomeForSocket(socketPath: string): string {
  return dirname(dirname(socketPath))
}

async function connectOrStart(options: RunOptions, ports: RunPorts): Promise<TargetSession> {
  try {
    return await ports.connect(options.socketPath, options.release)
  } catch (error) {
    if (!socketUnavailable(error)) throw error
  }

  if (!isAbsolute(options.servicePath)) {
    throw new Error("Routing Service path must be absolute")
  }
  ports.spawn(options.servicePath, ["--home", muxviaHomeForSocket(options.socketPath)], { shell: false })

  const deadline = ports.clock.now() + readinessTimeoutMs
  while (ports.clock.now() < deadline) {
    try {
      return await ports.connect(options.socketPath, options.release)
    } catch (error) {
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
