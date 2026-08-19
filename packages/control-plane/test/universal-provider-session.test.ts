import { afterEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createServer, type Server, type Socket } from "node:net"

import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { RpcClient } from "../src/control/rpc-client"
import type {
  ClientFrame,
  ServerFrame,
  UniversalProviderAction,
  UniversalProviderCatalogView,
} from "../src/control/types"

const roots: string[] = []
const serviceEpoch = "00000000-0000-4000-8000-000000000901"

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

function catalogAt(revision: number, viewSequence = revision): UniversalProviderCatalogView {
  return {
    revision,
    viewSequence,
    providers: [],
    presets: [],
  }
}

class CatalogServer {
  readonly frames: ClientFrame[] = []
  readonly #server: Server
  #socket?: Socket
  #waiters: Array<() => void> = []

  private constructor(server: Server) {
    this.#server = server
  }

  static async start(): Promise<{ server: CatalogServer; path: string }> {
    const root = await mkdtemp(join(tmpdir(), "muxvia-universal-session-"))
    roots.push(root)
    const path = join(root, "control.sock")
    let scripted!: CatalogServer
    const server = createServer((socket) => scripted.#accept(socket))
    scripted = new CatalogServer(server)
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

  requests(): Extract<ClientFrame, { type: "request" }>[] {
    return this.frames.filter(
      (frame): frame is Extract<ClientFrame, { type: "request" }> => frame.type === "request",
    )
  }

  async waitForRequests(count: number): Promise<void> {
    while (this.requests().length < count) {
      await new Promise<void>((resolve) => this.#waiters.push(resolve))
    }
  }

  send(frame: ServerFrame): void {
    this.#socket!.write(encodeFrame(frame))
  }

  replyOpen(index: number, view = catalogAt(0)): void {
    const request = this.requests()[index]!
    this.send({
      type: "response",
      requestId: request.requestId,
      result: { kind: "universal-provider-catalog", view },
    })
  }

  replyApplied(view: UniversalProviderCatalogView): void {
    const request = this.requests().filter(
      (frame) => frame.operation.kind === "universal-provider-act",
    ).at(-1)!
    this.send({
      type: "response",
      requestId: request.requestId,
      result: {
        kind: "universal-provider-outcome",
        outcome: { status: "applied", view },
      },
    })
    this.push(view)
  }

  replyStale(): void {
    const request = this.requests().filter(
      (frame) => frame.operation.kind === "universal-provider-act",
    ).at(-1)!
    this.send({
      type: "error",
      requestId: request.requestId,
      problem: {
        code: "stale-universal-catalog-revision",
        message: "Universal Provider catalog changed",
      },
    })
  }

  push(view: UniversalProviderCatalogView): void {
    this.send({ type: "universal-provider-view", view })
  }

  async close(): Promise<void> {
    this.#socket?.destroy()
    await new Promise<void>((resolve) => this.#server.close(() => resolve()))
  }
}

test("a Universal Provider session opens an independent catalog view", async () => {
  const { server, path } = await CatalogServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const opening = client.openUniversalProviders()
  await server.waitForRequests(1)
  expect(server.requests()[0]!.operation).toEqual({ kind: "open-universal-providers" })
  server.replyOpen(0)
  const session = await opening
  expect(session.get()).toEqual(catalogAt(0))

  await session.close()
  await session.close()
  await expect(
    session.act(createUniversalProvider("Closed", "CLOSED_SESSION_CREDENTIAL")),
  ).rejects.toMatchObject({ code: "connection-closed" })
  await server.close()
})

function createUniversalProvider(
  name: string,
  credential: string,
): Extract<UniversalProviderAction, { kind: "create-universal-provider" }> {
  return {
    kind: "create-universal-provider",
    name,
    baseUrl: "https://shared.example/v1",
    credential: { kind: "replace", value: credential },
    presetKey: null,
    targets: [{
      target: "codex",
      enabled: true,
      model: "shared-model",
      authentication: "openai-bearer",
      routingRequirement: "direct-compatible",
    }],
  }
}

function assertCapturedUniversalCreate(
  frame: Extract<ClientFrame, { type: "request" }> | undefined,
  expected: { revision: number; name: string; credential: string },
): void {
  const operation = frame?.operation
  if (operation?.kind !== "universal-provider-act") {
    throw new Error("queued Universal Provider action did not match captured input")
  }
  const action = operation.action as UniversalProviderAction
  const matches = operation.expectedRevision === expected.revision
    && action.kind === "create-universal-provider"
    && action.name === expected.name
    && action.credential.kind === "replace"
    && action.credential.value === expected.credential
  if (!matches) {
    throw new Error("queued Universal Provider action did not match captured input")
  }
}

test("catalog actions capture caller input, serialize revisions, and refresh stale state", async () => {
  const { server, path } = await CatalogServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const opening = client.openUniversalProviders()
  await server.waitForRequests(1)
  server.replyOpen(0)
  const session = await opening
  const ordering: string[] = []
  session.subscribe(() => ordering.push("push"))

  const first = session.act(createUniversalProvider("First", "FIRST_CAPTURED_CREDENTIAL"))
    .then((outcome) => {
      ordering.push("resolved")
      return outcome
    })
  const mutable = createUniversalProvider("Captured", "ORIGINAL_NESTED_CREDENTIAL")
  const queued = session.act(mutable)
  mutable.name = "Mutated"
  if (mutable.credential.kind === "replace") {
    mutable.credential.value = "MUTATED_NESTED_CREDENTIAL"
  }

  await server.waitForRequests(2)
  expect(server.requests().filter(
    (request) => request.operation.kind === "universal-provider-act",
  )).toHaveLength(1)
  server.replyApplied(catalogAt(1, 1))
  await first
  await Bun.sleep(10)
  expect(ordering).toEqual(["resolved", "push"])

  await server.waitForRequests(3)
  const actions = server.requests().filter(
    (request) => request.operation.kind === "universal-provider-act",
  )
  expect(actions).toHaveLength(2)
  expect(actions[0]!.operation).toMatchObject({ expectedRevision: 0 })
  assertCapturedUniversalCreate(actions[1], {
    revision: 1,
    name: "Captured",
    credential: "ORIGINAL_NESTED_CREDENTIAL",
  })
  const firstOperation = actions[0]!.operation
  const secondOperation = actions[1]!.operation
  if (
    firstOperation.kind !== "universal-provider-act"
    || secondOperation.kind !== "universal-provider-act"
  ) {
    throw new Error("catalog action requests did not retain their operation kind")
  }
  expect(firstOperation.actionId).not.toBe(secondOperation.actionId)
  server.replyApplied(catalogAt(2, 2))
  await queued

  const stale = session.act(createUniversalProvider("Stale", "STALE_CREDENTIAL"))
  await server.waitForRequests(4)
  server.replyStale()
  await server.waitForRequests(5)
  expect(server.requests()[4]!.operation).toEqual({ kind: "open-universal-providers" })
  server.replyOpen(4, catalogAt(4, 4))
  await expect(stale).rejects.toMatchObject({
    code: "stale-universal-catalog-revision",
  })
  expect(session.get()).toEqual(catalogAt(4, 4))

  await session.close()
  await server.close()
})

test("a catalog push gap performs one refresh and ignores incomplete sequences", async () => {
  const { server, path } = await CatalogServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const opening = client.openUniversalProviders()
  await server.waitForRequests(1)
  server.replyOpen(0, catalogAt(1, 1))
  const session = await opening
  const notified: number[] = []
  session.subscribe((view) => notified.push(view.viewSequence))

  server.push(catalogAt(4, 4))
  server.push(catalogAt(5, 5))
  await server.waitForRequests(2)
  expect(server.requests().filter(
    (request) => request.operation.kind === "open-universal-providers",
  )).toHaveLength(2)
  expect(notified).toEqual([])
  server.replyOpen(1, catalogAt(5, 5))
  await Bun.sleep(10)
  expect(session.get()).toEqual(catalogAt(5, 5))
  expect(notified).toEqual([5])

  await session.close()
  await server.close()
})
