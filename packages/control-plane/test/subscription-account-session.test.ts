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
  SubscriptionAccountCatalogView,
} from "../src/control/types"

const roots: string[] = []

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

const catalog = (revision: number): SubscriptionAccountCatalogView => ({
  revision,
  viewSequence: revision,
  defaultAccountId: revision === 0 ? null : "account-primary",
  accounts: revision === 0 ? [] : [{
    accountId: "account-primary",
    email: "operator@example.test",
    authenticatedAt: 1,
    state: "authorized",
    default: true,
  }],
  bindings: [],
  recovery: { state: "clean" },
})

class AccountServer {
  readonly frames: ClientFrame[] = []
  readonly #server: Server
  #socket?: Socket
  #waiters: Array<() => void> = []

  private constructor(server: Server) {
    this.#server = server
  }

  static async start(): Promise<{ server: AccountServer; path: string }> {
    const root = await mkdtemp(join(tmpdir(), "muxvia-account-session-"))
    roots.push(root)
    const path = join(root, "control.sock")
    let scripted!: AccountServer
    const server = createServer((socket) => scripted.#accept(socket))
    scripted = new AccountServer(server)
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
            serviceEpoch: "00000000-0000-4000-8000-000000001121",
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

  async waitForCancel(): Promise<void> {
    while (!this.frames.some((frame) => frame.type === "cancel")) {
      await new Promise<void>((resolve) => this.#waiters.push(resolve))
    }
  }

  send(frame: ServerFrame): void {
    this.#socket!.write(encodeFrame(frame))
  }

  reply(index: number, result: Extract<ServerFrame, { type: "response" }>["result"]): void {
    this.send({
      type: "response",
      requestId: this.requests()[index]!.requestId,
      result,
    })
  }

  async close(): Promise<void> {
    this.#socket?.destroy()
    await new Promise<void>((resolve) => this.#server.close(() => resolve()))
  }
}

test("Subscription Account session owns its catalog, device flow, and queued actions", async () => {
  const { server, path } = await AccountServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const opening = client.openSubscriptionAccounts()
  await server.waitForRequests(1)
  expect(server.requests()[0]!.operation).toEqual({ kind: "open-subscription-accounts" })
  server.reply(0, { kind: "subscription-account-catalog", view: catalog(0) })
  const session = await opening

  const starting = session.startDeviceAuthorization()
  await server.waitForRequests(2)
  server.reply(1, {
    kind: "device-authorization-challenge",
    challenge: {
      flowId: "00000000-0000-4000-8000-000000001122",
      userCode: "ABCD-EFGH",
      verificationUrl: "https://auth.openai.com/codex/device",
      expiresInSeconds: 900,
      pollIntervalSeconds: 11,
    },
  })
  const challenge = await starting
  expect(challenge.pollIntervalSeconds).toBe(11)

  const cancelled = new AbortController()
  const pending = session.pollDeviceAuthorization(challenge.flowId, cancelled.signal)
  await server.waitForRequests(3)
  cancelled.abort()
  await expect(pending).rejects.toMatchObject({ code: "cancelled" })
  await server.waitForCancel()
  expect(server.frames.some((frame) => frame.type === "cancel")).toBeTrue()

  const resumed = session.pollDeviceAuthorization(challenge.flowId)
  await server.waitForRequests(4)
  server.reply(3, {
    kind: "device-authorization-poll",
    poll: { status: "authorized", accountId: "account-primary" },
  })
  expect(await resumed).toEqual({ status: "authorized", accountId: "account-primary" })

  const mutable = {
    kind: "delete-account" as const,
    accountId: "account-primary",
  }
  const applied = session.act(mutable)
  mutable.accountId = "caller-mutated"
  await server.waitForRequests(5)
  const operation = server.requests()[4]!.operation
  expect(operation.kind).toBe("subscription-account-act")
  if (operation.kind !== "subscription-account-act") throw new Error("wrong account operation")
  expect(operation.expectedRevision).toBe(0)
  expect(operation.action).toEqual({ kind: "delete-account", accountId: "account-primary" })
  server.reply(4, {
    kind: "subscription-account-outcome",
    outcome: { status: "applied", view: catalog(1) },
  })
  expect((await applied).view).toEqual(catalog(1))
  server.send({ type: "subscription-account-view", view: catalog(1) })
  await Bun.sleep(0)
  expect(session.get()).toEqual(catalog(1))

  const refreshed = new Promise<void>((resolveRefresh) => {
    const unsubscribe = session.subscribe((view) => {
      if (view.viewSequence === 3) {
        unsubscribe()
        resolveRefresh()
      }
    })
  })
  server.send({ type: "subscription-account-view", view: catalog(3) })
  await server.waitForRequests(6)
  expect(server.requests()[5]!.operation).toEqual({ kind: "open-subscription-accounts" })
  server.reply(5, { kind: "subscription-account-catalog", view: catalog(3) })
  await refreshed
  expect(session.get()).toEqual(catalog(3))

  const firstQueued = session.act({ kind: "delete-account", accountId: "account-first" })
  const secondQueued = session.act({ kind: "delete-account", accountId: "account-second" })
  await server.waitForRequests(7)
  const firstQueuedOperation = server.requests()[6]!.operation
  expect(firstQueuedOperation.kind).toBe("subscription-account-act")
  if (firstQueuedOperation.kind !== "subscription-account-act") throw new Error("wrong queued account operation")
  expect(firstQueuedOperation.expectedRevision).toBe(3)
  server.reply(6, {
    kind: "subscription-account-outcome",
    outcome: { status: "applied", view: catalog(4) },
  })
  await firstQueued
  await server.waitForRequests(8)
  const secondQueuedOperation = server.requests()[7]!.operation
  expect(secondQueuedOperation.kind).toBe("subscription-account-act")
  if (secondQueuedOperation.kind !== "subscription-account-act") throw new Error("wrong queued account operation")
  expect(secondQueuedOperation.expectedRevision).toBe(4)
  server.reply(7, {
    kind: "subscription-account-outcome",
    outcome: { status: "applied", view: catalog(5) },
  })
  await secondQueued
  server.send({ type: "subscription-account-view", view: catalog(2) })
  await Bun.sleep(0)
  expect(session.get()).toEqual(catalog(5))

  await session.close()
  await expect(session.act({ kind: "delete-account", accountId: "closed" }))
    .rejects.toMatchObject({ code: "connection-closed" })
  await server.close()
})
