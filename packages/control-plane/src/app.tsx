import { createCliRenderer, type CliRenderer } from "@opentui/core"
import { render, type JSX } from "@opentui/solid"
import { spawn } from "node:child_process"
import { dirname, isAbsolute } from "node:path"
import { createSignal } from "solid-js"

import { RpcClient, ControlError } from "./control/rpc-client"
import type { TargetSession } from "./control/target-session"
import type { ClaudePreflightContext, Target } from "./control/types"
import { resolveLocale } from "./i18n"
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

export type SignalName = "SIGHUP" | "SIGINT" | "SIGTERM"

export interface SignalSource {
  listen(name: SignalName, handler: () => void): () => void
}

interface SpawnOptions {
  shell: false
}

export interface RunPorts {
  connect(
    socketPath: string,
    release: string,
    signal: AbortSignal,
    target: Target,
    claudeContext?: ClaudePreflightContext,
  ): Promise<TargetSession>
  spawn(path: string, args: string[], options: SpawnOptions): void
  createRenderer(): Promise<CliRenderer>
  render(node: () => JSX.Element, renderer: CliRenderer): Promise<void>
  signals: SignalSource
  clock: Clock
}

const readinessTimeoutMs = 2_000
const retryIntervalMs = 50

interface TargetControl {
  openTarget(target: Target, claudeContext?: ClaudePreflightContext): Promise<TargetSession>
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
  targetOrConnect: Target | ControlConnector = "codex",
  connectOverride?: ControlConnector,
  claudeContext?: ClaudePreflightContext,
): Promise<TargetSession> {
  const target = typeof targetOrConnect === "function" ? "codex" : targetOrConnect
  const connect = typeof targetOrConnect === "function" ? targetOrConnect : connectOverride ?? RpcClient.connect
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
    const opening = control.openTarget(target, target === "claude" ? claudeContext : undefined).then(async (session) => {
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

export function createProductionRenderer(): Promise<CliRenderer> {
  return createCliRenderer({
    exitOnCtrlC: false,
    useKittyKeyboard: {},
    autoFocus: false,
  })
}

const productionPorts: RunPorts = {
  connect: (socketPath, release, signal, target, claudeContext) =>
    connectTargetSession(socketPath, release, signal, target, undefined, claudeContext),
  spawn: (path, args) => {
    const child = spawn(path, args, { shell: false, detached: true, stdio: "ignore" })
    child.unref()
  },
  createRenderer: createProductionRenderer,
  render: async (node, renderer) => { await render(node, renderer) },
  signals: {
    listen: (name, handler) => {
      process.on(name, handler)
      return () => process.off(name, handler)
    },
  },
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
  cancellation: AbortSignal,
  target: Target,
  claudeContext?: ClaudePreflightContext,
  closeSession: (session: TargetSession) => Promise<void> = (session) => session.close(),
): Promise<TargetSession> {
  if (cancellation.aborted) throw new ConnectionCancelledError()
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
  let removeCancellation = () => {}
  const cancelled = new Promise<never>((_, reject) => {
    const onCancel = () => {
      cancelTimeout()
      controller.abort()
      reject(new ConnectionCancelledError())
    }
    cancellation.addEventListener("abort", onCancel, { once: true })
    removeCancellation = () => cancellation.removeEventListener("abort", onCancel)
    if (cancellation.aborted) onCancel()
  })
  const connection = Promise.resolve()
    .then(() => ports.connect(options.socketPath, options.release, controller.signal, target, claudeContext))
    .then(async (session) => {
      if (cancellation.aborted) {
        try {
          await closeSession(session)
        } catch {
          // A cancelled startup owns the late session, but cleanup failure must not resume startup.
        }
        throw new ConnectionCancelledError()
      }
      if (!expired) return session
      try {
        await closeSession(session)
      } catch {
        // A timed-out connection is abandoned; cleanup failure cannot replace the deadline result.
      }
      throw new ConnectionDeadlineError()
    })

  try {
    return await Promise.race([connection, timeout, cancelled])
  } finally {
    removeCancellation()
    cancelTimeout()
  }
}

class ConnectionCancelledError extends Error {}

async function connectOrStart(
  options: RunOptions,
  ports: RunPorts,
  cancellation: AbortSignal,
  target: Target,
  spawnState: { started: boolean },
  claudeContext?: ClaudePreflightContext,
  closeSession?: (session: TargetSession) => Promise<void>,
): Promise<TargetSession | undefined> {
  const deadline = ports.clock.now() + readinessTimeoutMs
  try {
    return await connectBeforeDeadline(options, ports, deadline, cancellation, target, claudeContext, closeSession)
  } catch (error) {
    if (cancellation.aborted) return undefined
    if (error instanceof ConnectionCancelledError) return undefined
    if (error instanceof ConnectionDeadlineError) throw error
    if (!socketUnavailable(error)) throw error
  }

  if (cancellation.aborted) return undefined

  if (!isAbsolute(options.servicePath)) {
    throw new Error("Routing Service path must be absolute")
  }
  if (!spawnState.started) {
    spawnState.started = true
    ports.spawn(options.servicePath, ["--home", muxviaHomeForSocket(options.socketPath)], { shell: false })
  }

  while (ports.clock.now() < deadline) {
    if (cancellation.aborted) return undefined
    try {
      return await connectBeforeDeadline(options, ports, deadline, cancellation, target, claudeContext, closeSession)
    } catch (error) {
      if (cancellation.aborted) return undefined
      if (error instanceof ConnectionCancelledError) return undefined
      if (error instanceof ConnectionDeadlineError) throw error
      if (!socketUnavailable(error)) throw error
    }
    if (cancellation.aborted) return undefined
    await ports.clock.sleep(retryIntervalMs)
  }
  throw new ControlError("service-unavailable", "Routing Service did not become ready")
}

export async function run(options: RunOptions, ports: RunPorts = productionPorts): Promise<void> {
  const locale = resolveLocale(process.env)
  const renderer = await ports.createRenderer()
  const destroyed = renderer.isDestroyed
    ? Promise.resolve()
    : new Promise<void>((resolve) => renderer.once("destroy", resolve))
  const startup = new AbortController()
  void destroyed.then(() => startup.abort())
  const stopListening = (["SIGHUP", "SIGINT", "SIGTERM"] as const).map((name) =>
    ports.signals.listen(name, () => {
      if (!renderer.isDestroyed) renderer.destroy()
    }),
  )
  const [sessions, setSessions] = createSignal<Partial<Record<Target, TargetSession>>>({})
  const [unavailable, setUnavailable] = createSignal<Partial<Record<Target, string>>>({})
  const closedSessions = new Set<TargetSession>()
  const closeSessionOnce = async (session: TargetSession): Promise<void> => {
    if (closedSessions.has(session)) return
    closedSessions.add(session)
    await session.close()
  }
  const spawnState = { started: false }
  const claudeContext = claudePreflightContext(process.env)
  let firstConnectionFailure: unknown
  try {
    if (renderer.isDestroyed) return
    await Promise.all((["codex", "claude"] as const).map(async (target) => {
      try {
        const session = await connectOrStart(
          options,
          ports,
          startup.signal,
          target,
          spawnState,
          target === "claude" ? claudeContext : undefined,
          closeSessionOnce,
        )
        if (session) setSessions((current) => ({ ...current, [target]: session }))
      } catch (error) {
        firstConnectionFailure ??= error
        setUnavailable((current) => ({
          ...current,
          [target]: typeof error === "object" && error !== null && "code" in error
            ? String(error.code)
            : "service-unavailable",
        }))
      }
    }))
    if (renderer.isDestroyed) return
    if (Object.keys(sessions()).length === 0) throw firstConnectionFailure ?? new ConnectionDeadlineError()
    await ports.render(() => <App sessions={sessions} unavailable={unavailable} locale={locale} />, renderer)
    const opened = sessions()
    const uniqueSessions = [...new Set(Object.values(opened))]
    for (const target of ["codex", "claude"] as const) {
      const targetSession = opened[target]
      if (!targetSession) continue
      void targetSession.whenClosed().then(
        () => markTargetUnavailable(target, targetSession, closeSessionOnce, setSessions, setUnavailable),
        () => markTargetUnavailable(target, targetSession, closeSessionOnce, setSessions, setUnavailable),
      )
    }
    const allSessionsClosed = Promise.all(uniqueSessions.map((session) => session.whenClosed().catch(() => {})))
      .then(() => { if (!renderer.isDestroyed) renderer.destroy() })
    await Promise.race([destroyed, allSessionsClosed])
  } finally {
    for (const stop of stopListening) stop()
    try {
      renderer.setTerminalTitle("")
      await Promise.all([...new Set(Object.values(sessions()))].map((session) => closeSessionOnce(session).catch(() => {})))
    } finally {
      if (!renderer.isDestroyed) renderer.destroy()
      await destroyed
    }
  }
}

function markTargetUnavailable(
  target: Target,
  closedSession: TargetSession,
  closeSessionOnce: (session: TargetSession) => Promise<void>,
  setSessions: (update: (current: Partial<Record<Target, TargetSession>>) => Partial<Record<Target, TargetSession>>) => void,
  setUnavailable: (update: (current: Partial<Record<Target, string>>) => Partial<Record<Target, string>>) => void,
): void {
  void closeSessionOnce(closedSession).catch(() => {})
  setSessions((current) => {
    if (current[target] !== closedSession) return current
    const next = { ...current }
    delete next[target]
    return next
  })
  setUnavailable((current) => ({ ...current, [target]: "service-unavailable" }))
}

function claudePreflightContext(environment: NodeJS.ProcessEnv): ClaudePreflightContext {
  const selectorNames = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
  ] as const
  const normalized = selectorNames.map((name) => normalizeSelector(environment[name]))
  const blockingSelector = selectorNames.find((_, index) => (
    normalized[index] === "enabled" || normalized[index] === "unknown-nonempty"
  )) ?? null
  const selectorState = normalized.includes("enabled")
    ? "enabled"
    : normalized.includes("unknown-nonempty")
      ? "unknown-nonempty"
      : normalized.every((state) => state === "unset") ? "unset" : "disabled"
  return {
    claudeConfigDir: environment.CLAUDE_CONFIG_DIR ?? null,
    selectorState,
    blockingSelector,
    hostManagedState: (() => {
      const state = normalizeSelector(environment.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST)
      if (state === "enabled") return "managed"
      if (state === "unknown-nonempty") return "unknown"
      return "unmanaged"
    })(),
    cwd: process.cwd(),
  }
}

function normalizeSelector(value: string | undefined): "unset" | "disabled" | "enabled" | "unknown-nonempty" {
  if (value === undefined || value === "") return "unset"
  if (value === "0" || value === "false") return "disabled"
  if (value === "1" || value === "true") return "enabled"
  return "unknown-nonempty"
}
