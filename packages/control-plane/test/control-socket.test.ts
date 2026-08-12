import { afterEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createServer, type Server, type Socket } from "node:net"

import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { RpcClient } from "../src/control/rpc-client"

const roots: string[] = []
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
