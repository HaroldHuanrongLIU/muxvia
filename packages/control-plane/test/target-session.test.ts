import { afterEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createServer, type Server, type Socket } from "node:net"

import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { RpcClient } from "../src/control/rpc-client"
import type { ClientFrame, ServerFrame, TargetView } from "../src/control/types"

const roots: string[] = []

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

const serviceEpoch = "00000000-0000-4000-8000-000000000001"

function viewAtRevision(revision: number, sequence = revision): TargetView {
  return {
    target: "codex",
    managementRevision: revision,
    viewSequence: sequence,
    service: { epoch: serviceEpoch, state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    providers: revision === 0 ? [] : [{
      id: `provider-${revision}`,
      position: 0,
      providerRevision: 1,
      name: `Provider ${revision}`,
      baseUrl: "https://provider.example/v1",
      model: `model-${revision}`,
      protocol: "openai-responses",
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

  replyStale(view: TargetView): void {
    const frame = this.requests().filter((request) => request.operation.kind === "act").at(-1)!
    this.send({
      type: "error",
      requestId: frame.requestId,
      problem: { code: "stale-revision", message: "Target state changed" },
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
  const opening = client.openTarget("codex")
  await server.waitForRequests(1)
  server.replyOpen(0, initial)
  const session = await opening
  return { session, server }
}

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
    credentialSource: { kind: "missing" },
  })
  const preset = session.discoverModels({
    kind: "draft",
    baseUrl: "https://api.openai.com/v1",
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
        credentialSource: { kind: "missing" },
      },
    },
    {
      kind: "discover-models",
      target: "codex",
      source: {
        kind: "draft",
        baseUrl: "https://api.openai.com/v1",
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
