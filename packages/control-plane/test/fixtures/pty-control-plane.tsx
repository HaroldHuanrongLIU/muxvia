/** @jsxImportSource @opentui/solid */
import { render } from "@opentui/solid"

import {
  createProductionRenderer,
  run,
  type RunPorts,
  type SignalName,
} from "../../src/app"
import type { TargetSession } from "../../src/control/target-session"
import type { ActionOutcome, TargetAction, TargetView } from "../../src/control/types"

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

function deferred<T>() {
  let resolvePromise!: (value: T | PromiseLike<T>) => void
  const promise = new Promise<T>((resolve) => { resolvePromise = resolve })
  return { promise, resolve: resolvePromise }
}

const crash = deferred<void>()
let exceptional = false
process.on("message", (message) => {
  if (typeof message === "object" && message !== null && "type" in message && message.type === "crash") {
    exceptional = true
    crash.resolve()
  }
})

class FixtureSession implements TargetSession {
  #closed = false
  readonly #whenClosed = deferred<void>()

  get(): Readonly<TargetView> { return initialView }
  async discoverModels(): Promise<never> { throw new Error("not used by this fixture") }
  async checkReachability(): Promise<never> { throw new Error("not used by this fixture") }
  async previewReconciliation(): Promise<never> { throw new Error("reconciliation not configured in this fixture") }
  async applyReconciliation(): Promise<never> { throw new Error("reconciliation not configured in this fixture") }
  async probeCompatibility(): Promise<never> { throw new Error("compatibility probe not configured in this fixture") }
  async resolveCompatibility(): Promise<never> { throw new Error("compatibility resolution not configured in this fixture") }
  async listRequestRecords(): Promise<never> { throw new Error("request history not configured in this fixture") }
  async listUsageActivity(): Promise<never> { throw new Error("usage activity not configured in this fixture") }
  async refreshNativeUsage(): Promise<never> { throw new Error("usage activity not configured in this fixture") }
  async setUsageRetention(): Promise<never> { throw new Error("usage activity not configured in this fixture") }
  async clearUsage(): Promise<never> { throw new Error("usage activity not configured in this fixture") }
  async updatePricingCatalog(): Promise<never> { throw new Error("usage activity not configured in this fixture") }
  async inspectRequestRecord(): Promise<never> { throw new Error("request history not configured in this fixture") }
  async previewProviderImport(): Promise<never> { throw new Error("provider transfer not configured in this fixture") }
  async confirmProviderImport(): Promise<never> { throw new Error("provider transfer not configured in this fixture") }
  async exportProviderConfiguration(): Promise<never> { throw new Error("provider transfer not configured in this fixture") }
  async act(_action: TargetAction): Promise<ActionOutcome> {
    return { status: "applied", view: initialView }
  }
  subscribe(_listener: (next: TargetView) => void): () => void { return () => {} }
  async close(): Promise<void> {
    if (this.#closed) return
    this.#closed = true
    process.send?.({ type: "session-closed" })
  }
  whenClosed(): Promise<void> { return this.#whenClosed.promise }
}

const session = new FixtureSession()
const ports: RunPorts = {
  connect: async () => session,
  spawn: () => { throw new Error("fixture must not spawn a Routing Service") },
  createRenderer: createProductionRenderer,
  render: async (node, renderer) => {
    await render(node, renderer)
    process.send?.({ type: "ready", screenMode: renderer.screenMode })
    await Promise.race([
      crash.promise.then(() => { throw new Error("injected-render-failure") }),
      renderer.isDestroyed
        ? Promise.resolve()
        : new Promise<void>((resolve) => renderer.once("destroy", resolve)),
    ])
  },
  signals: {
    listen: (name: SignalName, handler: () => void) => {
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

try {
  await run({
    servicePath: "/fixture/muxvia-routing",
    socketPath: "/fixture/.muxvia/run/control.sock",
    release: "pty-fixture",
    serviceRelease: "0.1.0",
  }, ports)
} catch (error) {
  if (!exceptional || !(error instanceof Error) || error.message !== "injected-render-failure") throw error
  process.exitCode = 70
} finally {
  process.disconnect?.()
}
