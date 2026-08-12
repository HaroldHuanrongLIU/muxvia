import { expect, test } from "bun:test"
import { readFile } from "node:fs/promises"
import { fileURLToPath } from "node:url"

import { FrameDecoder, encodeFrame } from "../src/control/framing"
import {
  parseClientFrame,
  parseServerFrame,
  parseTargetAction,
  parseTargetView,
} from "../src/control/types"

const fixtures = new URL("../../../protocol/fixtures/", import.meta.url)

async function readFixture(name: string): Promise<unknown> {
  return JSON.parse(await readFile(fileURLToPath(new URL(name, fixtures)), "utf8"))
}

test.each([
  ["hello.json", parseClientFrame],
  ["initial-target-view.json", parseTargetView],
  ["save-provider.json", parseTargetAction],
] as const)("round-trips %s as its protocol type", async (name, parse) => {
  const value = await readFixture(name)
  expect(JSON.parse(JSON.stringify(parse(value)))).toEqual(value)
})

test("decodes a frame split across arbitrary chunks", () => {
  const frame = encodeFrame({ type: "hello", rpc: { major: 1, minor: 0 }, release: "test" })
  const decoder = new FrameDecoder()
  expect(decoder.push(frame.subarray(0, 2))).toEqual([])
  expect(decoder.push(frame.subarray(2, 7))).toEqual([])
  expect(decoder.push(frame.subarray(7))).toEqual([
    { type: "hello", rpc: { major: 1, minor: 0 }, release: "test" },
  ])
})

test("rejects the advertised length before allocating an oversized body", () => {
  const decoder = new FrameDecoder()
  const prefix = new Uint8Array([0, 16, 0, 1])
  expect(() => decoder.push(prefix)).toThrow("frame-too-large")
})

test("ignores unknown additive fields in an action envelope", () => {
  expect(parseClientFrame({
    type: "request",
    requestId: "request-1",
    operation: {
      kind: "act",
      target: "codex",
      actionId: "00000000-0000-4000-8000-000000000005",
      expectedRevision: 0,
      action: { kind: "save-provider" },
      futureField: "ignored",
    },
  })).toEqual({
    type: "request",
    requestId: "request-1",
    operation: {
      kind: "act",
      target: "codex",
      actionId: "00000000-0000-4000-8000-000000000005",
      expectedRevision: 0,
      action: { kind: "save-provider" },
    },
  })
})

test("rejects an action identifier that is not a UUID", () => {
  expect(() => parseClientFrame({
    type: "request",
    requestId: "request-1",
    operation: {
      kind: "act",
      target: "codex",
      actionId: "not-a-uuid",
      expectedRevision: 0,
      action: { kind: "save-provider" },
    },
  })).toThrow()
})

test("rejects a hello acknowledgement outside RPC 1.0 or the fixed frame limit", () => {
  const helloAck = {
    type: "hello-ack",
    rpc: { major: 1, minor: 0 },
    release: "test",
    serviceEpoch: "00000000-0000-4000-8000-000000000001",
    frameLimit: 1_048_576,
  } as const

  expect(parseServerFrame(helloAck)).toEqual(helloAck)
  expect(() => parseServerFrame({ ...helloAck, rpc: { major: 1, minor: 1 } })).toThrow()
  expect(() => parseServerFrame({ ...helloAck, frameLimit: 1_048_575 })).toThrow()
})

test("drops additive secret fields from typed Target View projections", async () => {
  const view = await readFixture("initial-target-view.json") as Record<string, unknown>
  view.activatedSnapshot = {
    id: "00000000-0000-4000-8000-000000000002",
    providerId: "00000000-0000-4000-8000-000000000003",
    model: "gpt-test",
    epoch: "00000000-0000-4000-8000-000000000004",
    providerCredential: "provider-secret-must-not-escape",
    routingCredential: "routing-secret-must-not-escape",
    authorization: "Bearer provider-secret-must-not-escape",
    recovery: { raw: "routing-secret-must-not-escape" },
  }
  view.problems = [{
    code: "invalid-action",
    message: "The action cannot be completed.",
    providerCredential: "provider-secret-must-not-escape",
    routingCredential: "routing-secret-must-not-escape",
  }]
  view.providerCredential = "provider-secret-must-not-escape"

  const serialized = JSON.stringify(parseTargetView(view))
  expect(serialized).not.toContain("provider-secret-must-not-escape")
  expect(serialized).not.toContain("routing-secret-must-not-escape")
  expect(JSON.parse(serialized).activatedSnapshot).toEqual({
    id: "00000000-0000-4000-8000-000000000002",
    providerId: "00000000-0000-4000-8000-000000000003",
    model: "gpt-test",
    epoch: "00000000-0000-4000-8000-000000000004",
  })
})

test("encodes the frame length in big-endian order", () => {
  const frame = encodeFrame({ type: "hello" })
  expect([...frame.subarray(0, 4)]).toEqual([0, 0, 0, 16])
})

test("rejects invalid UTF-8 and JSON frame bodies", () => {
  const invalidUtf8 = new FrameDecoder()
  expect(() => invalidUtf8.push(new Uint8Array([0, 0, 0, 1, 0xff]))).toThrow("invalid-utf8")

  const invalidJson = new FrameDecoder()
  expect(() => invalidJson.push(new Uint8Array([0, 0, 0, 1, 0x7b]))).toThrow("invalid-json")
})

test("rejects a partial frame at end of stream", () => {
  const decoder = new FrameDecoder() as unknown as { push(chunk: Uint8Array): unknown[]; finish(): void }
  decoder.push(new Uint8Array([0, 0, 0, 4, 0x7b, 0x7d]))
  expect(() => decoder.finish()).toThrow("unexpected-eof")
})
