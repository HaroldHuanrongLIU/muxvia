import { afterEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createServer, type Server, type Socket } from "node:net"

import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { RpcClient } from "../src/control/rpc-client"
import type {
  ClientFrame,
  ClaudePreflightContext,
  CompatibilityProbe,
  ReconciliationPreview,
  ServerFrame,
  TargetView,
} from "../src/control/types"

const roots: string[] = []

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

const serviceEpoch = "00000000-0000-4000-8000-000000000001"

function viewAtRevision(revision: number, sequence = revision, target: TargetView["target"] = "codex"): TargetView {
  return {
    target,
    managementRevision: revision,
    viewSequence: sequence,
    service: { epoch: serviceEpoch, state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: revision === 0 ? [] : [{
      id: `provider-${revision}`,
      position: 0,
      providerRevision: 1,
      name: `Provider ${revision}`,
      baseUrl: "https://provider.example/v1",
      model: `model-${revision}`,
      protocol: target === "claude" ? "anthropic-messages" : "openai-responses",
      authentication: target === "claude" ? "anthropic-api-key" : "openai-bearer",
      routingRequirement: "direct-compatible",
      credential: "present",
      completeness: "complete",
      missingFields: [],
      provenance: null,
      generated: false,
      activeReferences: [],
    }],
    providerPresets: [],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    problems: [],
  }
}

function reconciliationPreview(
  target: TargetView["target"] = "codex",
  strategy: ReconciliationPreview["strategy"] = "reapply",
): ReconciliationPreview {
  return {
    observationToken: "00000000-0000-4000-8000-000000000701",
    target,
    strategy,
    managementRevision: 1,
    compatibility: {
      version: target === "claude" ? "2.1.0" : "1.2.0",
      classification: "tested",
      acknowledgementRequired: false,
    },
    shadowSources: [],
    changes: [{ field: "provider", state: "changed" }],
    providerEffect: "keep-current",
    restartRequired: true,
    unobservableRuntimeBoundary: true,
  }
}

class ScriptedServer {
  readonly frames: ClientFrame[] = []
  #server: Server
  #socket?: Socket
  #waiters: Array<() => void> = []

  private constructor(server: Server) {
    this.#server = server
  }

  static async start(): Promise<{ server: ScriptedServer; path: string }> {
    const root = await mkdtemp(join(tmpdir(), "muxvia-target-session-"))
    roots.push(root)
    const path = join(root, "control.sock")
    let scripted!: ScriptedServer
    const server = createServer((socket) => scripted.#accept(socket))
    scripted = new ScriptedServer(server)
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject)
      server.listen(path, resolve)
    })
    return { server: scripted, path }
  }

  #accept(socket: Socket): void {
    this.#socket = socket
    const decoder = new FrameDecoder()
    socket.on("data", (chunk) => {
      if (typeof chunk === "string") throw new Error("unexpected text chunk")
      for (const value of decoder.push(chunk)) {
        const frame = value as ClientFrame
        this.frames.push(frame)
        if (frame.type === "hello") {
          this.send({
            type: "hello-ack",
            rpc: { major: 1, minor: 0 },
            release: "routing-test",
            serviceEpoch,
            frameLimit: 1_048_576,
          })
        }
        for (const wake of this.#waiters.splice(0)) wake()
      }
    })
  }

  async waitForRequests(count: number): Promise<void> {
    while (this.requests().length < count) {
      await new Promise<void>((resolve) => this.#waiters.push(resolve))
    }
  }

  async waitForFrames(count: number): Promise<void> {
    while (this.frames.length < count) {
      await new Promise<void>((resolve) => this.#waiters.push(resolve))
    }
  }

  requests(): Extract<ClientFrame, { type: "request" }>[] {
    return this.frames.filter((frame): frame is Extract<ClientFrame, { type: "request" }> => frame.type === "request")
  }

  receivedActionCount(): number {
    return this.requests().filter((frame) => frame.operation.kind === "act").length
  }

  cancels(): Extract<ClientFrame, { type: "cancel" }>[] {
    return this.frames.filter(
      (frame): frame is Extract<ClientFrame, { type: "cancel" }> => frame.type === "cancel",
    )
  }

  replyOpen(index: number, view: TargetView): void {
    const frame = this.requests()[index]!
    this.send({ type: "response", requestId: frame.requestId, result: { kind: "target-view", view } })
  }

  replyApplied(view: TargetView): void {
    const frame = this.requests().filter((request) => request.operation.kind === "act").at(-1)!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "action-outcome", outcome: { status: "applied", view } },
    })
    this.push(view)
  }

  replyStale(view: TargetView, code = "stale-revision"): void {
    const frame = this.requests().filter((request) => request.operation.kind === "act").at(-1)!
    this.send({
      type: "error",
      requestId: frame.requestId,
      problem: { code, message: "Target state changed" },
      authoritativeView: view,
    })
  }

  replyDiscovery(index: number, models: string[]): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: {
        kind: "model-discovery",
        result: {
          status: "success",
          models: models.map((id) => ({ id, displayName: null })),
          attempts: 1,
          elapsedMs: 1,
          endpointOrigin: "https://draft.example",
        },
      },
    })
  }

  replyPreview(index: number, preview: ReconciliationPreview): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "reconciliation-preview", preview },
    })
  }

  replyCompatibilityProbe(index: number, probe: CompatibilityProbe): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "compatibility-probe", probe },
    })
  }

  replyWithTargetView(index: number, view: TargetView): void {
    const frame = this.requests()[index]!
    this.send({ type: "response", requestId: frame.requestId, result: { kind: "target-view", view } })
  }

  push(view: TargetView): void {
    this.send({ type: "target-view", view })
  }

  send(frame: ServerFrame): void {
    this.#socket!.write(encodeFrame(frame))
  }

  async close(): Promise<void> {
    this.#socket?.destroy()
    await new Promise<void>((resolve) => this.#server.close(() => resolve()))
  }
}

async function openScriptedSession(initial: TargetView) {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const opening = client.openTarget(initial.target)
  await server.waitForRequests(1)
  server.replyOpen(0, initial)
  const session = await opening
  return { session, server }
}

test("a Claude session captures its target and preflight context for gap refresh", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const claudeContext = {
    claudeConfigDir: null,
    selectorState: "disabled" as const,
    hostManagedState: "unmanaged" as const,
    cwd: "/tmp/project",
  }
  const opening = client.openTarget("claude", claudeContext)
  await server.waitForRequests(1)
  server.replyOpen(0, viewAtRevision(0, 0, "claude"))
  const session = await opening
  const action = session.act({
    kind: "create-provider", name: "Claude", baseUrl: "https://api.anthropic.com/v1", model: "claude-test",
    credential: { kind: "replace", value: "not-serialized" }, presetKey: "anthropic-api-messages",
  })
  await server.waitForRequests(2)
  expect(server.requests().map((request) => request.operation.target)).toEqual(["claude", "claude"])
  server.replyApplied(viewAtRevision(1, 1, "claude"))
  await action
  server.push(viewAtRevision(3, 3, "claude"))
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toEqual({
    kind: "open-target",
    target: "claude",
    claudeContext,
  })
  server.replyOpen(2, viewAtRevision(3, 3, "claude"))
  await session.close()
  await server.close()
})

test("a target session serializes actions and replaces stale state", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(0))
  const ordering: string[] = []
  session.subscribe(() => ordering.push("push"))
  const first = session.act({
    kind: "create-provider",
    name: "P",
    baseUrl: "https://p.test/v1",
    model: "m",
    credential: { kind: "replace", value: "s" },
    presetKey: null,
  }).then((outcome) => {
    ordering.push("resolved")
    return outcome
  })
  const second = session.act({ kind: "activate-provider", providerId: "p", mode: "takeover" })
  await server.waitForRequests(2)
  expect(server.receivedActionCount()).toBe(1)
  server.replyApplied(viewAtRevision(1))
  await first
  await Bun.sleep(10)
  expect(ordering).toEqual(["resolved", "push"])
  await server.waitForRequests(3)
  expect(server.receivedActionCount()).toBe(2)
  const actions = server.requests().filter((request) => request.operation.kind === "act")
  expect(actions[0]!.operation).toMatchObject({ expectedRevision: 0 })
  expect(actions[1]!.operation).toMatchObject({ expectedRevision: 1 })
  expect((actions[0]!.operation as { actionId: string }).actionId).not.toBe(
    (actions[1]!.operation as { actionId: string }).actionId,
  )
  server.replyStale(viewAtRevision(2))
  await expect(second).rejects.toMatchObject({ code: "stale-revision", retryable: true })
  expect(session.get().managementRevision).toBe(2)
  expect(server.receivedActionCount()).toBe(2)

  await session.close()
  await server.close()
})

test("a sequence gap issues one refresh and only complete increasing views notify", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1, 1))
  const received: number[] = []
  const unsubscribe = session.subscribe((view) => received.push(view.viewSequence))

  server.push(viewAtRevision(1, 1))
  server.push(viewAtRevision(2, 0))
  server.push(viewAtRevision(4, 4))
  server.push(viewAtRevision(5, 5))
  await server.waitForRequests(2)
  expect(server.requests().filter((request) => request.operation.kind === "open-target")).toHaveLength(2)
  expect(received).toEqual([])

  server.replyOpen(1, viewAtRevision(5, 5))
  await Bun.sleep(10)
  expect(session.get().viewSequence).toBe(5)
  expect(received).toEqual([5])

  server.push(viewAtRevision(6, 6))
  await Bun.sleep(10)
  expect(received).toEqual([5, 6])
  unsubscribe()
  server.push(viewAtRevision(7, 7))
  await Bun.sleep(10)
  expect(received).toEqual([5, 6])

  await session.close()
  await server.close()
})

test("close is idempotent, removes subscriptions, and closes the socket", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(0))
  let notifications = 0
  session.subscribe(() => notifications++)

  await session.close()
  await session.close()
  server.push(viewAtRevision(1))
  await Bun.sleep(10)
  expect(notifications).toBe(0)
  await expect(session.act({
    kind: "create-provider", name: "P", baseUrl: "https://p.test/v1", model: "m", credential: { kind: "replace", value: "s" }, presetKey: null,
  })).rejects.toMatchObject({ code: "connection-closed" })

  await server.close()
})

test("aborted discovery sends one cancel, ignores a late result, and preserves a newer push", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1, 1))
  const controller = new AbortController()
  const discovery = session.discoverModels({
    kind: "draft",
    baseUrl: "https://draft.example/v1",
    authentication: "openai-bearer",
    credentialSource: { kind: "ephemeral", value: "ephemeral-test-value" },
  }, controller.signal)
  await server.waitForRequests(2)
  const discoveryRequest = server.requests()[1]!

  server.push(viewAtRevision(2, 2))
  await Bun.sleep(10)
  controller.abort()
  await expect(discovery).rejects.toMatchObject({ code: "cancelled" })
  await server.waitForFrames(4)
  expect(server.cancels()).toEqual([{ type: "cancel", requestId: discoveryRequest.requestId }])

  server.replyDiscovery(1, ["late-model"])
  await Bun.sleep(10)
  expect(session.get()).toEqual(viewAtRevision(2, 2))
  expect(server.cancels()).toHaveLength(1)

  await session.close()
  await server.close()
})

test("explicit refresh accepts unsaved Blank and Preset drafts without Provider identity", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(0))
  const blank = session.discoverModels({
    kind: "draft",
    baseUrl: "",
    authentication: "openai-bearer",
    credentialSource: { kind: "missing" },
  })
  const preset = session.discoverModels({
    kind: "draft",
    baseUrl: "https://api.openai.com/v1",
    authentication: "openai-bearer",
    credentialSource: { kind: "ephemeral", value: "preset-test-value" },
  })
  await server.waitForRequests(3)

  const inspections = server.requests().slice(1)
  expect(inspections.map((request) => request.operation)).toEqual([
    {
      kind: "discover-models",
      target: "codex",
      source: {
        kind: "draft",
        baseUrl: "",
        authentication: "openai-bearer",
        credentialSource: { kind: "missing" },
      },
    },
    {
      kind: "discover-models",
      target: "codex",
      source: {
        kind: "draft",
        baseUrl: "https://api.openai.com/v1",
        authentication: "openai-bearer",
        credentialSource: { kind: "ephemeral", value: "preset-test-value" },
      },
    },
  ])
  expect(JSON.stringify(inspections.map((request) => request.operation))).not.toContain("providerId")

  server.replyDiscovery(1, [])
  server.replyDiscovery(2, ["model-a"])
  expect((await blank).status).toBe("success")
  expect((await preset).status).toBe("success")
  expect(session.get()).toEqual(viewAtRevision(0))

  await session.close()
  await server.close()
})

test("reconciliation preview captures target and Claude context without waiting for queued actions", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const claudeContext: ClaudePreflightContext = {
    claudeConfigDir: "/tmp/claude-home",
    selectorState: "disabled" as const,
    hostManagedState: "unmanaged" as const,
    cwd: "/tmp/project",
  }
  const capturedContext = structuredClone(claudeContext)
  const opening = client.openTarget("claude", claudeContext)
  await server.waitForRequests(1)
  server.replyOpen(0, viewAtRevision(1, 1, "claude"))
  const session = await opening
  claudeContext.claudeConfigDir = "/tmp/mutated-claude-home"
  claudeContext.cwd = "/tmp/mutated-project"

  const action = session.act({ kind: "activate-provider", providerId: "provider-1", mode: "direct" })
  const preview = session.previewReconciliation("reapply")
  await server.waitForRequests(3)
  const previewIndex = server.requests().findIndex(
    (request) => request.operation.kind === "preview-reconciliation",
  )
  expect(server.requests()[previewIndex]!.operation).toEqual({
    kind: "preview-reconciliation",
    target: "claude",
    strategy: "reapply",
    claudeContext: capturedContext,
  })

  const expected = reconciliationPreview("claude")
  server.replyPreview(previewIndex, expected)
  expect(await preview).toEqual(expected)
  server.replyApplied(viewAtRevision(2, 2, "claude"))
  await action
  await session.close()
  await server.close()
})

test("compatibility probe and resolution use the public target-scoped workflow", async () => {
  const initial = viewAtRevision(1)
  initial.problems = [{
    code: "compatibility-acknowledgement-required",
    message: "Acknowledge the exact version",
  }]
  const { session, server } = await openScriptedSession(initial)

  const inspecting = session.probeCompatibility()
  await server.waitForRequests(2)
  expect(server.requests()[1]!.operation).toEqual({
    kind: "probe-compatibility",
    target: "codex",
  })
  const probe: CompatibilityProbe = {
    target: "codex",
    managementRevision: 1,
    compatibility: {
      version: "codex-unknown-8.1",
      classification: "unknown-compatible",
      acknowledgementRequired: true,
    },
  }
  server.replyCompatibilityProbe(1, probe)
  expect(await inspecting).toEqual(probe)

  const resolving = session.resolveCompatibility("codex-unknown-8.1")
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toMatchObject({
    kind: "act",
    target: "codex",
    expectedRevision: 1,
    action: {
      kind: "resolve-compatibility",
      version: "codex-unknown-8.1",
    },
  })
  const acknowledged = viewAtRevision(1, 2)
  server.replyApplied(acknowledged)
  expect((await resolving).view).toEqual(acknowledged)
  expect(session.get()).toEqual(acknowledged)

  await session.close()
  await server.close()
})

test("reconciliation preview supports abort and validates its response kind", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const controller = new AbortController()
  const aborted = session.previewReconciliation("adopt", controller.signal)
  await server.waitForRequests(2)
  const request = server.requests()[1]!
  controller.abort()
  await expect(aborted).rejects.toMatchObject({ code: "cancelled" })
  await server.waitForFrames(4)
  expect(server.cancels()).toContainEqual({ type: "cancel", requestId: request.requestId })

  const invalid = session.previewReconciliation("restore")
  await server.waitForRequests(3)
  server.replyWithTargetView(2, viewAtRevision(2))
  await expect(invalid).rejects.toMatchObject({
    code: "invalid-response",
    message: "Reconciliation preview response did not match request",
  })
  expect(session.get().managementRevision).toBe(1)

  await session.close()
  await server.close()
})

test("reconciliation preview rejects a response for another target with a fixed diagnostic", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const pending = session.previewReconciliation("restore")
  await server.waitForRequests(2)
  const wrongTarget = reconciliationPreview("claude", "restore")
  server.replyPreview(1, wrongTarget)

  await expect(pending).rejects.toMatchObject({
    code: "invalid-response",
    message: "Reconciliation preview response did not match request",
  })
  expect(session.get().managementRevision).toBe(1)
  await session.close()
  await server.close()
})

test("reconciliation preview rejects another strategy with a fixed diagnostic", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const pending = session.previewReconciliation("reapply")
  await server.waitForRequests(2)
  const wrongStrategy = reconciliationPreview("codex", "adopt")
  server.replyPreview(1, wrongStrategy)

  await expect(pending).rejects.toMatchObject({
    code: "invalid-response",
    message: "Reconciliation preview response did not match request",
  })
  expect(session.get().managementRevision).toBe(1)
  await session.close()
  await server.close()
})

test("reconciliation preview is immutable and independent from later authoritative views", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const pending = session.previewReconciliation("reapply")
  await server.waitForRequests(2)
  server.replyPreview(1, reconciliationPreview())
  const preview = await pending

  expect(Object.isFrozen(preview)).toBe(true)
  expect(Object.isFrozen(preview.compatibility)).toBe(true)
  expect(Object.isFrozen(preview.changes)).toBe(true)
  expect(Object.isFrozen(preview.changes[0]!)).toBe(true)
  server.push(viewAtRevision(2, 2))
  await Bun.sleep(10)
  expect(preview.managementRevision).toBe(1)
  expect(preview.changes).toEqual([{ field: "provider", state: "changed" }])

  await session.close()
  await server.close()
})

test("reconciliation apply shares action serialization and replaces an authoritative stale view", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const first = session.act({ kind: "activate-provider", providerId: "provider-1", mode: "direct" })
  const apply = session.applyReconciliation({
    strategy: "reapply",
    observationToken: "00000000-0000-4000-8000-000000000701",
  })
  await server.waitForRequests(2)
  expect(server.receivedActionCount()).toBe(1)

  server.replyApplied(viewAtRevision(2, 2))
  await first
  await server.waitForRequests(3)
  const actions = server.requests().filter((request) => request.operation.kind === "act")
  expect(actions[1]!.operation).toMatchObject({
    target: "codex",
    expectedRevision: 2,
    action: {
      kind: "reconcile",
      strategy: "reapply",
      observationToken: "00000000-0000-4000-8000-000000000701",
    },
  })
  expect((actions[1]!.operation as { actionId: string }).actionId).not.toBe(
    (actions[0]!.operation as { actionId: string }).actionId,
  )
  server.replyStale(viewAtRevision(3, 3), "stale-reconciliation-preview")
  await expect(apply).rejects.toMatchObject({ code: "stale-reconciliation-preview" })
  expect(session.get().managementRevision).toBe(3)

  await session.close()
  await server.close()
})

test("reconciliation methods reject after close", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  await session.close()

  await expect(session.previewReconciliation("restore")).rejects.toMatchObject({ code: "connection-closed" })
  await expect(session.applyReconciliation({
    strategy: "restore",
    observationToken: "00000000-0000-4000-8000-000000000701",
  })).rejects.toMatchObject({ code: "connection-closed" })
  expect(server.requests()).toHaveLength(1)
  await server.close()
})

test("a Claude reconciliation push gap refresh retains its captured context", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const claudeContext: ClaudePreflightContext = {
    claudeConfigDir: null,
    selectorState: "disabled" as const,
    hostManagedState: "unmanaged" as const,
    cwd: "/tmp/reconciliation-project",
  }
  const capturedContext = structuredClone(claudeContext)
  const opening = client.openTarget("claude", claudeContext)
  await server.waitForRequests(1)
  server.replyOpen(0, viewAtRevision(1, 1, "claude"))
  const session = await opening
  claudeContext.claudeConfigDir = "/tmp/mutated-gap-home"
  claudeContext.cwd = "/tmp/mutated-gap-project"

  const apply = session.applyReconciliation({
    strategy: "adopt",
    observationToken: "00000000-0000-4000-8000-000000000701",
    acknowledgeVersion: "2.1.0",
  })
  await server.waitForRequests(2)
  expect(server.requests()[1]!.operation).toMatchObject({
    target: "claude",
    expectedRevision: 1,
    action: {
      kind: "reconcile",
      strategy: "adopt",
      observationToken: "00000000-0000-4000-8000-000000000701",
      acknowledgeVersion: "2.1.0",
    },
  })
  server.replyApplied(viewAtRevision(2, 2, "claude"))
  await apply
  server.push(viewAtRevision(4, 4, "claude"))
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toEqual({
    kind: "open-target",
    target: "claude",
    claudeContext: capturedContext,
  })
  server.replyOpen(2, viewAtRevision(4, 4, "claude"))

  await session.close()
  await server.close()
})
