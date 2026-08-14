import { afterEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createServer, type Server, type Socket } from "node:net"
import { EventEmitter } from "node:events"

import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { RpcClient } from "../src/control/rpc-client"
import type { TargetAction } from "../src/control/types"

const roots: string[] = []

test("a missing control socket is classified as service-unavailable", async () => {
  const root = await mkdtemp(join(tmpdir(), "muxvia-missing-control-"))
  roots.push(root)
  await expect(RpcClient.connect(join(root, "missing.sock"), "control-test")).rejects.toMatchObject({
    code: "service-unavailable",
  })
})

test("aborting a stalled handshake destroys the underlying socket", async () => {
  class StalledSocket extends EventEmitter {
    destroyCalls = 0
    write(): boolean { return true }
    destroy(): this {
      this.destroyCalls++
      return this
    }
  }
  const socket = new StalledSocket()
  const controller = new AbortController()
  const connecting = RpcClient.connect(
    "/tmp/stalled.sock",
    "control-test",
    controller.signal,
    () => socket as unknown as Socket,
  )
  controller.abort()
  await expect(connecting).rejects.toMatchObject({ code: "service-unavailable" })
  expect(socket.destroyCalls).toBe(1)
})
const servers: Server[] = []

afterEach(async () => {
  for (const server of servers.splice(0)) server.close()
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

async function listen(onSocket: (socket: Socket) => void): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "muxvia-rpc-client-"))
  roots.push(root)
  const path = join(root, "control.sock")
  const server = createServer(onSocket)
  servers.push(server)
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject)
    server.listen(path, resolve)
  })
  return path
}

test("connect negotiates and a socket close rejects every pending operation", async () => {
  const path = await listen((socket) => {
    const decoder = new FrameDecoder()
    socket.on("data", (chunk) => {
      if (typeof chunk === "string") throw new Error("unexpected text chunk")
      for (const value of decoder.push(chunk)) {
        const frame = value as { type: string }
        if (frame.type === "hello") {
          socket.write(encodeFrame({
            type: "hello-ack",
            rpc: { major: 1, minor: 0 },
            release: "routing-test",
            serviceEpoch: "00000000-0000-4000-8000-000000000001",
            frameLimit: 1_048_576,
          }))
        } else {
          socket.destroy()
        }
      }
    })
  })

  const client = await RpcClient.connect(path, "control-test")
  const outcomes = await Promise.allSettled([
    client.openTarget("codex"),
    client.openTarget("codex"),
  ])
  expect(outcomes).toMatchObject([
    { status: "rejected", reason: { code: "connection-closed" } },
    { status: "rejected", reason: { code: "connection-closed" } },
  ])
})

test("connect rejects a bounded-decoder violation", async () => {
  const path = await listen((socket) => {
    socket.once("data", () => socket.write(Uint8Array.from([0, 16, 0, 1])))
  })

  await expect(RpcClient.connect(path, "control-test")).rejects.toMatchObject({
    code: "frame-too-large",
  })
})

test("a generic server frame error rejects pending operations before socket close", async () => {
  const path = await listen((socket) => {
    const decoder = new FrameDecoder()
    socket.on("data", (chunk) => {
      if (typeof chunk === "string") throw new Error("unexpected text chunk")
      for (const value of decoder.push(chunk)) {
        const frame = value as { type: string }
        if (frame.type === "hello") {
          socket.write(encodeFrame({
            type: "hello-ack",
            rpc: { major: 1, minor: 0 },
            release: "routing-test",
            serviceEpoch: "00000000-0000-4000-8000-000000000001",
            frameLimit: 1_048_576,
          }))
        } else {
          socket.end(encodeFrame({
            type: "error",
            requestId: null,
            problem: { code: "frame-invalid", message: "Control frame is invalid" },
          }))
        }
      }
    })
  })

  const client = await RpcClient.connect(path, "control-test")
  await expect(client.openTarget("codex")).rejects.toMatchObject({ code: "frame-invalid" })
})

test("a generic server frame error rejects the handshake before socket close", async () => {
  const path = await listen((socket) => {
    socket.once("data", () => socket.end(encodeFrame({
      type: "error",
      requestId: null,
      problem: { code: "frame-invalid", message: "Control frame is invalid" },
    })))
  })

  await expect(RpcClient.connect(path, "control-test")).rejects.toMatchObject({
    code: "frame-invalid",
  })
})

test("sends revision-guarded reorder and delete actions unchanged over the control socket", async () => {
  const received: unknown[] = []
  const path = await listen((socket) => {
    const decoder = new FrameDecoder()
    socket.on("data", (chunk) => {
      if (typeof chunk === "string") throw new Error("unexpected text chunk")
      for (const value of decoder.push(chunk)) {
        const frame = value as { type: string; requestId?: string; operation?: { action?: unknown } }
        if (frame.type === "hello") {
          socket.write(encodeFrame({
            type: "hello-ack",
            rpc: { major: 1, minor: 0 },
            release: "routing-test",
            serviceEpoch: "00000000-0000-4000-8000-000000000001",
            frameLimit: 1_048_576,
          }))
          continue
        }
        received.push(frame.operation?.action)
        socket.write(encodeFrame({
          type: "error",
          requestId: frame.requestId,
          problem: { code: "stale-revision", message: "refresh" },
        }))
      }
    })
  })
  const client = await RpcClient.connect(path, "control-test")
  const actions = [
    {
      kind: "reorder-providers",
      providerIds: [
        "00000000-0000-4000-8000-000000000103",
        "00000000-0000-4000-8000-000000000101",
        "00000000-0000-4000-8000-000000000102",
      ],
    },
    {
      kind: "delete-provider",
      providerId: "00000000-0000-4000-8000-000000000101",
      providerRevision: 7,
    },
  ] satisfies TargetAction[]

  for (const action of actions) {
    await expect(client.request({
      kind: "act",
      target: "codex",
      actionId: crypto.randomUUID(),
      expectedRevision: 9,
      action,
    })).rejects.toMatchObject({ code: "stale-revision" })
  }

  expect(received).toEqual(actions)
  await client.close()
})
