/** @jsxImportSource @opentui/solid */
import { Database } from "bun:sqlite"
import { afterEach, expect, test } from "bun:test"
import { TestRecorder } from "@opentui/core/testing"
import { testRender } from "@opentui/solid"
import { createHash } from "node:crypto"
import { spawn } from "node:child_process"
import { createConnection, createServer as createTcpServer } from "node:net"
import { createServer as createHttpServer, request as httpRequest } from "node:http"
import { chmod, mkdir, mkdtemp, readFile, readdir, readlink, rm, stat, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { finished } from "node:stream/promises"

import { RpcClient } from "../src/control/rpc-client"
import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { App } from "../src/ui/app"
import type { TargetSession } from "../src/control/target-session"
import type { UniversalProviderSession } from "../src/control/universal-provider-session"
import {
  parseClientFrame,
  type OrdinaryTargetAction,
  type TargetView,
} from "../src/control/types"
import { CLAUDE_SSE_BYTES, SSE_BYTES, startFakeUpstream, type CapturedRequest } from "../../../tests/e2e/fake-upstream"

const providerSecret = "provider-secret-must-not-escape"
const wrongRoutingSecret = "routing-secret-must-not-escape"
const authSentinel = "auth-sentinel-must-not-escape"
const repoRoot = resolve(import.meta.dir, "../../..")
const serviceBinary = resolve(repoRoot, "target/debug/muxvia-routing")
const controlPlaneEntry = resolve(repoRoot, "packages/control-plane/src/index.tsx")
const fakeCodex = resolve(repoRoot, "tests/e2e/fixtures/fake-codex")
const fakeClaude = resolve(repoRoot, "tests/e2e/fixtures/fake-claude")
const deadlineMs = 10_000
const roots: string[] = []

if (
  process.env.MUXVIA_DIRECT_RESTRICTIVE_UMASK_CHILD === "1"
  || process.env.MUXVIA_CLAUDE_RESTRICTIVE_UMASK_CHILD === "1"
) process.umask(0o077)

afterEach(async () => {
  for (const root of roots.splice(0)) await rm(root, { recursive: true, force: true })
})

async function controlledTreeFingerprint(path: string): Promise<string> {
  try {
    const own = await stat(path)
    const metadata = [own.mode, own.size, own.mtimeMs]
    if (!own.isDirectory()) {
      return JSON.stringify([metadata, sensitiveDigest(await readFile(path))])
    }
    const children = await Promise.all((await readdir(path)).sort().map(async (name) => [
      name,
      await controlledTreeFingerprint(join(path, name)),
    ]))
    return JSON.stringify([metadata, children])
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return "missing"
    throw error
  }
}

async function assertControlledTreeUnchanged(
  path: string,
  expectedFingerprint: string,
  diagnostic: string,
): Promise<void> {
  if (await controlledTreeFingerprint(path) !== expectedFingerprint) {
    throw new Error(diagnostic)
  }
}

async function safeFileFingerprint(path: string): Promise<{
  digest: string
  mode: number
  mtimeMs: number
  size: number
}> {
  const metadata = await stat(path)
  return {
    digest: sensitiveDigest(await readFile(path))!,
    mode: metadata.mode & 0o777,
    mtimeMs: metadata.mtimeMs,
    size: metadata.size,
  }
}

async function waitFor(predicate: () => boolean | Promise<boolean>, label: string): Promise<void> {
  const deadline = Date.now() + deadlineMs
  while (!(await predicate())) {
    if (Date.now() >= deadline) throw new Error(`Timed out waiting for ${label}`)
    await Bun.sleep(10)
  }
}

async function eventBarrier<T>(event: Promise<T>, label: string): Promise<T> {
  return await new Promise<T>((resolveEvent, reject) => {
    const timeout = setTimeout(() => reject(new Error(`event-barrier-failed:${label}`)), deadlineMs)
    event.then((value) => {
      clearTimeout(timeout)
      resolveEvent(value)
    }, (error) => {
      clearTimeout(timeout)
      if (error && typeof error === "object" && "code" in error) {
        reject(error)
        return
      }
      const message = error instanceof Error ? error.message : ""
      reject(new Error(message.startsWith("held-request-completed-before-upstream:")
        ? message
        : `event-barrier-failed:${label}`))
    })
  })
}

type InboundResultExpectation =
  | { resultKind: "model-discovery" | "reachability" }
  | { errorCode: string }
type WaitForCondition = (
  predicate: () => boolean | Promise<boolean>,
  label: string,
) => Promise<void>

async function waitForInboundResult(
  frames: readonly unknown[],
  start: number,
  expectation: InboundResultExpectation,
  label: string,
  waitForCondition: WaitForCondition = waitFor,
): Promise<void> {
  try {
    await waitForCondition(() => frames.slice(start).some((frame) => {
      if (typeof frame !== "object" || frame === null) return false
      const candidate = frame as {
        type?: unknown
        requestId?: unknown
        result?: { kind?: unknown }
        problem?: { code?: unknown }
      }
      if ("errorCode" in expectation) {
        return candidate.type === "error"
          && typeof candidate.requestId === "string"
          && candidate.problem?.code === expectation.errorCode
      }
      return candidate.type === "response" && candidate.result?.kind === expectation.resultKind
    }), `inbound ${label}`)
  } catch {
    throw new Error(`inbound-result-wait-failed:${label}`)
  }
}

async function waitForSession(
  session: TargetSession,
  predicate: (view: Readonly<TargetView>) => boolean,
  label: string,
): Promise<void> {
  if (predicate(session.get())) return
  await new Promise<void>((resolveWait, reject) => {
    let unsubscribe = () => {}
    const timeout = setTimeout(() => {
      unsubscribe()
      reject(new Error(`Timed out waiting for ${label}`))
    }, deadlineMs)
    const finish = () => {
      clearTimeout(timeout)
      unsubscribe()
      resolveWait()
    }
    unsubscribe = session.subscribe((view) => {
      if (predicate(view)) finish()
    })
    if (predicate(session.get())) finish()
  })
}

async function chunkedPost(endpoint: string, credential: string) {
  const url = new URL(`${endpoint}/responses`)
  return await new Promise<{ status: number; headers: Record<string, string | string[] | undefined>; body: string }>((resolveResponse, reject) => {
    const request = httpRequest({
      hostname: url.hostname,
      port: url.port,
      path: url.pathname,
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-test-preserved": "preserved",
        "x-muxvia-routing-credential": credential,
        "authorization": "Bearer incoming-must-be-replaced",
      },
    }, (response) => {
      const chunks: Buffer[] = []
      response.on("data", (chunk) => chunks.push(Buffer.from(chunk)))
      response.on("end", () => resolveResponse({
        status: response.statusCode ?? 0,
        headers: response.headers,
        body: Buffer.concat(chunks).toString("utf8"),
      }))
    })
    request.on("error", reject)
    request.write('{"model":"gpt-test",')
    request.end('"input":"hello"}')
  })
}

async function claudePost(
  endpoint: string,
  credential: string,
  path: string,
  body: Record<string, unknown>,
  headers: Record<string, string | readonly string[]> = {},
): Promise<{ status: number; headers: Record<string, string | string[] | undefined>; body: string }> {
  const url = new URL(`${endpoint}${path}`)
  return await new Promise((resolveResponse, reject) => {
    const request = httpRequest({
      hostname: url.hostname,
      port: url.port,
      path: `${url.pathname}${url.search}`,
      method: "POST",
      headers: {
        authorization: `Bearer ${credential}`,
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
        ...headers,
      },
    }, (response) => {
      const chunks: Buffer[] = []
      response.on("data", (chunk) => chunks.push(Buffer.from(chunk)))
      response.on("end", () => resolveResponse({
        status: response.statusCode ?? 0,
        headers: response.headers,
        body: Buffer.concat(chunks).toString("utf8"),
      }))
    })
    request.on("error", reject)
    request.end(JSON.stringify(body))
  })
}

async function cancelRoutedPost(
  target: "codex" | "claude",
  endpoint: string,
  credential: string,
  requestBodySecret: string,
): Promise<number> {
  const path = target === "codex" ? "/responses" : "/v1/messages"
  const url = new URL(`${endpoint}${path}`)
  return await new Promise<number>((resolveCancelled, reject) => {
    let settled = false
    const finish = (status: number) => {
      if (settled) return
      settled = true
      resolveCancelled(status)
    }
    const request = httpRequest({
      hostname: url.hostname,
      port: url.port,
      path: url.pathname,
      method: "POST",
      headers: target === "codex"
        ? {
            "content-type": "application/json",
            "x-muxvia-routing-credential": credential,
          }
        : {
            "content-type": "application/json",
            authorization: `Bearer ${credential}`,
            "anthropic-version": "2023-06-01",
          },
    }, (response) => {
      response.once("data", () => {
        const status = response.statusCode ?? 0
        response.destroy()
        request.destroy()
        finish(status)
      })
      response.once("error", () => finish(response.statusCode ?? 0))
    })
    request.once("error", (error) => {
      if (!settled) reject(error)
    })
    request.end(JSON.stringify(target === "codex"
      ? { model: "ignored", input: requestBodySecret }
      : { model: "ignored", messages: [{ role: "user", content: requestBodySecret }], stream: true }))
  })
}

type HeldReconciliationTarget = "codex" | "claude"

type FailoverFixtureBehavior = "success" | "retryable-failure"

async function startFailoverFixtureUpstream(
  target: HeldReconciliationTarget,
  credential: string,
) {
  const calls: CapturedRequest[] = []
  const waiters = new Set<() => void>()
  let behavior: FailoverFixtureBehavior = "success"
  let held: {
    behavior: FailoverFixtureBehavior
    started: () => void
    release: Promise<void>
    releaseNow: () => void
  } | undefined
  const server = createHttpServer(async (request, response) => {
    const body: Buffer[] = []
    for await (const chunk of request) body.push(Buffer.from(chunk))
    const captured: CapturedRequest = {
      authorization: typeof request.headers.authorization === "string" ? request.headers.authorization : null,
      headers: Object.fromEntries(Object.entries(request.headers).map(([name, value]) => [name, value ?? null])),
      contentType: typeof request.headers["content-type"] === "string" ? request.headers["content-type"] : null,
      method: request.method ?? "",
      testHeader: typeof request.headers["x-test-preserved"] === "string" ? request.headers["x-test-preserved"] : null,
      body: Buffer.concat(body).toString("utf8"),
      path: request.url ?? "",
    }
    if (request.method === "GET") {
      response.writeHead(401, { "x-fixture-reachability": "reachable" }).end()
      return
    }
    calls.push(captured)
    for (const notify of waiters) notify()
    const expectedPath = target === "codex" ? "/v1/responses" : "/v1/messages"
    if (captured.path.split("?", 1)[0] !== expectedPath || captured.authorization !== `Bearer ${credential}`) {
      response.writeHead(403, { "content-type": "application/json" }).end('{"error":"fixture-rejected"}')
      return
    }
    let selected = behavior
    if (held) {
      const current = held
      selected = current.behavior
      current.started()
      await current.release
      if (held === current) held = undefined
    }
    if (selected === "retryable-failure") {
      response.writeHead(503, { "content-type": "application/json" })
      response.end('{"error":"fixture-unavailable"}')
      return
    }
    response.writeHead(target === "codex" ? 201 : 200, { "content-type": "text/event-stream" })
    for (const part of target === "codex" ? SSE_BYTES : CLAUDE_SSE_BYTES) response.write(part)
    response.end()
  })
  await new Promise<void>((resolveListen, reject) => {
    server.once("error", reject)
    server.listen(0, "127.0.0.1", resolveListen)
  })
  const address = server.address()
  if (!address || typeof address === "string") throw new Error("failover-upstream-address-missing")
  return {
    target,
    credential,
    calls,
    baseUrl: `http://127.0.0.1:${address.port}/v1`,
    setBehavior(next: FailoverFixtureBehavior) { behavior = next },
    holdNext(next: FailoverFixtureBehavior) {
      if (held) throw new Error("failover-upstream-hold-already-armed")
      let started!: () => void
      let release!: () => void
      const startedPromise = new Promise<void>((resolveStarted) => { started = resolveStarted })
      const releasePromise = new Promise<void>((resolveRelease) => { release = resolveRelease })
      held = { behavior: next, started, release: releasePromise, releaseNow: release }
      return { started: startedPromise, release }
    },
    async waitForCallCount(count: number) {
      if (calls.length >= count) return
      await new Promise<void>((resolveCount, reject) => {
        const timeout = setTimeout(() => {
          waiters.delete(notify)
          reject(new Error(`failover-upstream-call-timeout:${target}:${count}`))
        }, deadlineMs)
        const notify = () => {
          if (calls.length < count) return
          clearTimeout(timeout)
          waiters.delete(notify)
          resolveCount()
        }
        waiters.add(notify)
      })
    },
    releaseHeld() {
      const current = held
      held = undefined
      current?.releaseNow()
    },
    async stop() {
      held?.releaseNow()
      held = undefined
      server.closeAllConnections()
      await new Promise<void>((resolveClose) => server.close(() => resolveClose()))
    },
  }
}

type RequestRecordFixtureBehavior =
  | { kind: "success"; bodySecret: string; headerSecret: string }
  | { kind: "retryable-failure" }
  | { kind: "retained-failure"; payload: string }
  | { kind: "cancelled-stream" }

async function startRequestRecordFixtureUpstream(
  target: HeldReconciliationTarget,
  credential: string,
) {
  const calls: CapturedRequest[] = []
  const waiters = new Set<() => void>()
  let behavior: RequestRecordFixtureBehavior = {
    kind: "success",
    bodySecret: "request-record-default-body",
    headerSecret: "request-record-default-header",
  }
  let cancellationStarted = () => {}
  let cancellationStartedPromise = new Promise<void>((resolveStarted) => {
    cancellationStarted = resolveStarted
  })
  const server = createHttpServer(async (request, response) => {
    const body: Buffer[] = []
    for await (const chunk of request) body.push(Buffer.from(chunk))
    const captured: CapturedRequest = {
      authorization: typeof request.headers.authorization === "string" ? request.headers.authorization : null,
      apiKey: typeof request.headers["x-api-key"] === "string" ? request.headers["x-api-key"] : null,
      headers: Object.fromEntries(Object.entries(request.headers).map(([name, value]) => [name, value ?? null])),
      contentType: typeof request.headers["content-type"] === "string" ? request.headers["content-type"] : null,
      method: request.method ?? "",
      testHeader: typeof request.headers["x-test-preserved"] === "string" ? request.headers["x-test-preserved"] : null,
      body: Buffer.concat(body).toString("utf8"),
      path: request.url ?? "",
    }
    calls.push(captured)
    for (const notify of waiters) notify()
    const expectedPath = target === "codex" ? "/v1/responses" : "/v1/messages"
    if (
      captured.path.split("?", 1)[0] !== expectedPath
      || captured.authorization !== `Bearer ${credential}`
    ) {
      response.writeHead(403, { "content-type": "application/json" }).end('{"error":"fixture-rejected"}')
      return
    }
    const selected = behavior
    if (selected.kind === "retryable-failure") {
      response.writeHead(503, { "content-type": "application/json" })
      response.end('{"error":"retryable-fixture-unavailable"}')
      return
    }
    if (selected.kind === "retained-failure") {
      response.writeHead(429, { "content-type": "application/json" })
      response.end(selected.payload)
      return
    }
    if (selected.kind === "cancelled-stream") {
      response.writeHead(200, { "content-type": "text/event-stream" })
      response.write(target === "codex"
        ? 'event: response.output_text.delta\ndata: {"delta":"cancel-started"}\n\n'
        : 'event: message_start\ndata: {"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}\n\n')
      cancellationStarted()
      await new Promise<void>((resolveClosed) => response.once("close", resolveClosed))
      return
    }
    response.writeHead(target === "codex" ? 201 : 200, {
      "content-type": "text/event-stream",
      "x-request-record-success": selected.headerSecret,
    })
    const chunks = target === "codex"
      ? [
          `event: response.output_text.delta\ndata: {"delta":${JSON.stringify(selected.bodySecret)}}\n\n`,
          'event: response.completed\ndata: {"type":"response.completed","response":{"usage":{"input_tokens":120,"input_tokens_details":{"cached_tokens":20},"output_tokens":30}}}\n\n',
        ]
      : [
          'event: message_start\ndata: {"type":"message_start","message":{"usage":{"input_tokens":12,"output_tokens":0,"cache_read_input_tokens":2,"cache_creation_input_tokens":1}}}\n\n',
          `event: content_block_delta\ndata: {"type":"content_block_delta","delta":{"text":${JSON.stringify(selected.bodySecret)}}}\n\n`,
          'event: message_delta\ndata: {"type":"message_delta","usage":{"output_tokens":4}}\n\n',
          'event: message_stop\ndata: {"type":"message_stop"}\n\n',
        ]
    for (const chunk of chunks) response.write(chunk)
    response.end()
  })
  await new Promise<void>((resolveListen, reject) => {
    server.once("error", reject)
    server.listen(0, "127.0.0.1", resolveListen)
  })
  const address = server.address()
  if (!address || typeof address === "string") throw new Error("request-record-upstream-address-missing")
  return {
    target,
    credential,
    calls,
    baseUrl: `http://127.0.0.1:${address.port}/v1`,
    setBehavior(next: RequestRecordFixtureBehavior) {
      behavior = next
      if (next.kind === "cancelled-stream") {
        cancellationStartedPromise = new Promise<void>((resolveStarted) => {
          cancellationStarted = resolveStarted
        })
      }
    },
    waitForCancellationStart: () => cancellationStartedPromise,
    async waitForCallCount(count: number) {
      if (calls.length >= count) return
      await new Promise<void>((resolveCount, reject) => {
        const timeout = setTimeout(() => {
          waiters.delete(notify)
          reject(new Error(`request-record-upstream-call-timeout:${target}:${count}`))
        }, deadlineMs)
        const notify = () => {
          if (calls.length < count) return
          clearTimeout(timeout)
          waiters.delete(notify)
          resolveCount()
        }
        waiters.add(notify)
      })
    },
    async stop() {
      server.closeAllConnections()
      await new Promise<void>((resolveClose) => server.close(() => resolveClose()))
    },
  }
}

async function startHeldReconciliationUpstream(credentials: Readonly<Record<HeldReconciliationTarget, string>>) {
  const calls: CapturedRequest[] = []
  let hold: {
    target: HeldReconciliationTarget
    started: () => void
    release: Promise<void>
    releaseNow: () => void
  } | undefined
  const server = createHttpServer(async (request, response) => {
    const body: Buffer[] = []
    for await (const chunk of request) body.push(Buffer.from(chunk))
    const target: HeldReconciliationTarget | undefined = request.url?.startsWith("/v1/responses")
      ? "codex"
      : request.url?.startsWith("/v1/messages")
        ? "claude"
        : undefined
    const captured: CapturedRequest = {
      authorization: typeof request.headers.authorization === "string" ? request.headers.authorization : null,
      headers: Object.fromEntries(Object.entries(request.headers).map(([name, value]) => [name, value ?? null])),
      contentType: typeof request.headers["content-type"] === "string" ? request.headers["content-type"] : null,
      method: request.method ?? "",
      testHeader: typeof request.headers["x-test-preserved"] === "string" ? request.headers["x-test-preserved"] : null,
      body: Buffer.concat(body).toString("utf8"),
      path: request.url ?? "",
    }
    calls.push(captured)
    if (!target || captured.authorization !== `Bearer ${credentials[target]}`) {
      response.writeHead(target ? 403 : 404).end()
      return
    }
    if (hold?.target === target) {
      const current = hold
      current.started()
      await current.release
      if (hold === current) hold = undefined
    }
    if (target === "codex") {
      response.writeHead(201, { "content-type": "text/event-stream" })
      for (const part of SSE_BYTES) response.write(part)
    } else {
      response.writeHead(200, { "content-type": "text/event-stream" })
      for (const part of CLAUDE_SSE_BYTES) response.write(part)
    }
    response.end()
  })
  await new Promise<void>((resolveListen, reject) => {
    server.once("error", reject)
    server.listen(0, "127.0.0.1", resolveListen)
  })
  const address = server.address()
  if (!address || typeof address === "string") throw new Error("reconciliation-upstream-address-missing")
  return {
    baseUrl: `http://127.0.0.1:${address.port}/v1`,
    calls,
    holdNext(target: HeldReconciliationTarget) {
      if (hold) throw new Error("reconciliation-upstream-hold-already-armed")
      let started!: () => void
      let release!: () => void
      const startedPromise = new Promise<void>((resolveStarted) => { started = resolveStarted })
      const releasePromise = new Promise<void>((resolveRelease) => { release = resolveRelease })
      hold = { target, started, release: releasePromise, releaseNow: release }
      return { started: startedPromise, release }
    },
    releaseHeld() {
      const current = hold
      if (!current) return
      hold = undefined
      current.releaseNow()
    },
    async stop() {
      await new Promise<void>((resolveClose) => server.close(() => resolveClose()))
    },
  }
}

function auditReconciliationUpstreamRequests(
  calls: readonly CapturedRequest[],
  providerSecrets: ReadonlyArray<{ target: HeldReconciliationTarget; secret: string }>,
  routingSecrets: readonly string[],
): void {
  const providerSecretValues = providerSecrets.map(({ secret }) => secret)
  for (const request of calls) {
    scanNoSecrets([request], routingSecrets, "reconciliation-upstream-routing-secret")
    const pathname = request.path.split("?", 1)[0]
    const target = pathname === "/v1/responses"
      ? "codex"
      : pathname === "/v1/messages" || pathname === "/v1/messages/count_tokens"
        ? "claude"
        : undefined
    const matched = providerSecrets.filter((entry) => {
      return entry.target === target && request.authorization === `Bearer ${entry.secret}`
    })
    if (
      !target
      || matched.length !== 1
      || request.headers.authorization !== request.authorization
    ) throw new Error("reconciliation-upstream-provider-auth-invalid")
    const redacted = {
      ...request,
      authorization: null,
      headers: { ...request.headers, authorization: null },
    }
    scanNoSecrets([redacted], providerSecretValues, "reconciliation-upstream-provider-secret")
  }
}

type SecretOccurrence = { count: number; exact: boolean; path: string; value: string }

function jsonSecretOccurrences(value: unknown, secret: string, path: string[] = []): SecretOccurrence[] {
  if (typeof value === "string") {
    const count = value.split(secret).length - 1
    return count > 0 ? [{ count, exact: value === secret, path: path.join("."), value }] : []
  }
  if (Array.isArray(value)) {
    return value.flatMap((entry, index) => jsonSecretOccurrences(entry, secret, [...path, String(index)]))
  }
  if (!value || typeof value !== "object") return []
  return Object.entries(value as Record<string, unknown>)
    .flatMap(([key, entry]) => jsonSecretOccurrences(entry, secret, [...path, key]))
}

function auditAndExtractClaudeRoutingCredential(
  text: string,
  forbiddenSecrets: readonly string[],
): { credential: string; parsed: Record<string, unknown> & { env?: Record<string, unknown> } } {
  scanNoSecrets([text], forbiddenSecrets, "claude-settings-raw")
  let parsed: Record<string, unknown> & { env?: Record<string, unknown> }
  try {
    parsed = JSON.parse(text) as typeof parsed
  } catch {
    throw new Error("claude-managed-settings-parse-invalid")
  }
  const credential = parsed.env?.ANTHROPIC_AUTH_TOKEN
  if (typeof credential !== "string" || !/^[a-f0-9]{64}$/.test(credential)) {
    throw new Error("claude-managed-credential-invalid")
  }
  const occurrences = jsonSecretOccurrences(parsed, credential)
  if (
    occurrences.length !== 1
    || occurrences[0]!.path !== "env.ANTHROPIC_AUTH_TOKEN"
    || !occurrences[0]!.exact
  ) throw new Error("secret-scan-failed:claude-settings-routing-secret")
  return { credential, parsed }
}

function extractClaudeManagedSettings(
  text: string,
  expectedModel: string,
  forbiddenSecrets: readonly string[] = [],
): { endpoint: string; credential: string; semantic: unknown } {
  const { credential, parsed } = auditAndExtractClaudeRoutingCredential(text, forbiddenSecrets)
  const env = parsed.env
  if (!env || typeof env !== "object") throw new Error("claude-managed-settings-invalid")
  const endpoint = env.ANTHROPIC_BASE_URL
  if (typeof endpoint !== "string" || !/^http:\/\/127\.0\.0\.1:\d+$/.test(endpoint)) {
    throw new Error("claude-managed-endpoint-invalid")
  }
  const rootKeys = Object.keys(parsed).sort()
  const envKeys = Object.keys(env).sort()
  const expectedEnvKeys = [
      "ANTHROPIC_AUTH_TOKEN",
      "ANTHROPIC_BASE_URL",
      "ANTHROPIC_MODEL",
      "OPERATOR_UNRELATED",
    ]
  const operator = parsed.operator as Record<string, unknown> | undefined
  const mismatch = [
    JSON.stringify(rootKeys) !== JSON.stringify(["env", "operator"]),
    JSON.stringify(envKeys) !== JSON.stringify(expectedEnvKeys),
    !operator
      || JSON.stringify(Object.keys(operator).sort()) !== JSON.stringify(["hooks", "theme"])
      || operator.theme !== "dark"
      || JSON.stringify(operator.hooks) !== JSON.stringify(["keep"]),
    env.OPERATOR_UNRELATED !== "keep-me",
    env.ANTHROPIC_MODEL !== expectedModel,
  ]
  if (mismatch.some(Boolean)) {
    throw new Error("claude-managed-settings-mismatch")
  }
  return { endpoint, credential, semantic: parsed }
}

function auditClaudeSettingsSecrets(
  text: string,
  routingCredential: string,
  forbiddenSecrets: readonly string[],
): void {
  const audited = auditAndExtractClaudeRoutingCredential(text, forbiddenSecrets)
  if (audited.credential !== routingCredential) {
    throw new Error("claude-settings-routing-location-invalid")
  }
}

type ClaudeDirectSettingsExpectation = {
  authentication: "anthropic-api-key" | "anthropic-bearer"
  baseUrl: string
  credential: string
  model: string
}

type ClaudeDirectSettingsAuditPhase =
  | { kind: "unmanaged"; priorAuthToken: string; priorApiKey: string }
  | { kind: "direct"; expected: ClaudeDirectSettingsExpectation }

function assertExactClaudeDirectSettings(
  text: string,
  expected: ClaudeDirectSettingsExpectation,
  forbiddenSecrets: readonly string[],
): void {
  scanNoSecrets([text], forbiddenSecrets, "claude-direct-settings-raw")
  let parsed: Record<string, unknown>
  try {
    parsed = JSON.parse(text) as Record<string, unknown>
  } catch {
    throw new Error("claude-direct-settings-json-invalid")
  }
  const credentialKey = expected.authentication === "anthropic-bearer"
    ? "ANTHROPIC_AUTH_TOKEN"
    : "ANTHROPIC_API_KEY"
  const desired = {
    env: {
      ANTHROPIC_BASE_URL: expected.baseUrl,
      ANTHROPIC_MODEL: expected.model,
      OPERATOR_UNRELATED: "keep-me",
      [credentialKey]: expected.credential,
    },
    operator: { hooks: ["keep"], theme: "dark" },
  }
  const occurrences = jsonSecretOccurrences(parsed, expected.credential)
  if (
    occurrences.length !== 1
    || occurrences[0]!.path !== `env.${credentialKey}`
    || !occurrences[0]!.exact
    || canonicalJson(parsed) !== canonicalJson(desired)
  ) throw new Error("claude-direct-settings-mismatch")
}

async function auditClaudeDirectSettingsFile(
  path: string,
  phase: ClaudeDirectSettingsAuditPhase,
  secrets: readonly string[],
  expectedMode: number,
): Promise<void> {
  const bytes = await readFile(path)
  const allowedSecrets = phase.kind === "unmanaged"
    ? [phase.priorAuthToken, phase.priorApiKey]
    : [phase.expected.credential]
  const forbiddenSecrets = secrets.filter((secret) => !allowedSecrets.includes(secret))
  assertBytesContainNoSecrets(bytes, forbiddenSecrets, "claude-direct-final-settings-bytes")
  const text = bytes.toString("utf8")
  scanNoSecrets([text], forbiddenSecrets, "claude-direct-final-settings-text")

  if (phase.kind === "direct") {
    assertExactClaudeDirectSettings(text, phase.expected, [])
  } else {
    let parsed: Record<string, unknown>
    try { parsed = JSON.parse(text) as Record<string, unknown> } catch {
      throw new Error("claude-direct-final-settings-json-invalid")
    }
    const expected = {
      env: {
        ANTHROPIC_API_KEY: phase.priorApiKey,
        ANTHROPIC_AUTH_TOKEN: phase.priorAuthToken,
        OPERATOR_UNRELATED: "keep-me",
      },
      operator: { hooks: ["keep"], theme: "dark" },
    }
    const locationsAreExact = [
      [phase.priorAuthToken, "env.ANTHROPIC_AUTH_TOKEN"],
      [phase.priorApiKey, "env.ANTHROPIC_API_KEY"],
    ].every(([secret, expectedPath]) => {
      const occurrences = jsonSecretOccurrences(parsed, secret!)
      return occurrences.length === 1
        && occurrences[0]!.path === expectedPath
        && occurrences[0]!.exact
    })
    if (!locationsAreExact || canonicalJson(parsed) !== canonicalJson(expected)) {
      throw new Error("claude-direct-final-settings-mismatch")
    }
  }
  assertExactFileMode((await stat(path)).mode & 0o777, expectedMode)
}

type ClaudeRequestProjection = {
  method: string
  pathClass: "models" | "messages" | "count-tokens" | "reachability" | "other"
  authentication: "api-key" | "bearer" | "absent"
  model: string | null
  stream: boolean
  delayed: boolean
  error: boolean
  version: string | null
  betaValues: string[]
  correlation: string | null
  query: string
  toolsDigest: string | null
  unknownDigest: string | null
}

function capturedHeaderValues(value: string | string[] | null | undefined): string[] {
  if (value === null || value === undefined) return []
  return (Array.isArray(value) ? value : [value]).flatMap((entry) => entry.split(","))
    .map((entry) => entry.trim()).filter(Boolean)
}

function singleCapturedHeader(value: string | string[] | null | undefined): string | null {
  const values = capturedHeaderValues(value)
  return values.length === 1 ? values[0]! : null
}

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`
  if (value && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>).sort(([left], [right]) => left.localeCompare(right))
    return `{${entries.map(([key, entry]) => `${JSON.stringify(key)}:${canonicalJson(entry)}`).join(",")}}`
  }
  return JSON.stringify(value)
}

function auditClaudeRequestAndProject(
  request: CapturedRequest,
  policy: {
    apiKeyCredential: string
    bearerCredential: string
    forbiddenRoutingCredentials: readonly string[]
  },
): ClaudeRequestProjection {
  scanNoSecrets([request], policy.forbiddenRoutingCredentials, "claude-captured-routing-credential")
  const apiKeyMatches = request.apiKey === policy.apiKeyCredential
  const bearerMatches = request.authorization === `Bearer ${policy.bearerCredential}`
  const noApiKey = request.apiKey === null || request.apiKey === undefined
  const noBearer = request.authorization === null
  const authentication = apiKeyMatches && noBearer
    ? "api-key"
    : bearerMatches && noApiKey
      ? "bearer"
      : noApiKey && noBearer
        ? "absent"
        : undefined
  const redacted = {
    ...request,
    apiKey: apiKeyMatches ? null : request.apiKey,
    authorization: bearerMatches ? null : request.authorization,
    headers: {
      ...request.headers,
      "x-api-key": apiKeyMatches ? null : request.headers["x-api-key"],
      authorization: bearerMatches ? null : request.headers.authorization,
    },
  }
  scanNoSecrets(
    [redacted],
    [policy.apiKeyCredential, policy.bearerCredential],
    "claude-captured-provider-credential",
  )
  if (!authentication) throw new Error("claude-captured-authentication-invalid")
  let parsed: Record<string, unknown> = {}
  if (request.body !== "") {
    try {
      parsed = JSON.parse(request.body) as Record<string, unknown>
    } catch {
      throw new Error("claude-captured-body-invalid")
    }
  }
  const path = request.path.split("?", 1)[0]
  return {
    method: request.method,
    pathClass: path === "/v1/models"
      ? "models"
      : path === "/v1/messages"
        ? "messages"
        : path === "/v1/messages/count_tokens"
          ? "count-tokens"
          : request.method === "GET"
            ? "reachability"
            : "other",
    authentication,
    model: typeof parsed.model === "string" ? parsed.model : null,
    stream: parsed.stream === true,
    delayed: parsed.fixture_delay === true,
    error: parsed.fixture_error === true,
    version: capturedHeaderValues(request.headers["anthropic-version"])[0] ?? null,
    betaValues: capturedHeaderValues(request.headers["anthropic-beta"]),
    correlation: capturedHeaderValues(request.headers["x-claude-code-fixture"])[0] ?? null,
    query: request.path.split("?", 2)[1] ?? "",
    toolsDigest: parsed.tools === undefined ? null : sensitiveDigest(canonicalJson(parsed.tools)),
    unknownDigest: parsed.future_extension === undefined
      ? null
      : sensitiveDigest(canonicalJson(parsed.future_extension)),
  }
}

function assertExactClaudeMessagesProjection(projection: ClaudeRequestProjection): void {
  if (projection.version !== "2023-06-01") throw new Error("claude-messages-version-mismatch")
  if (projection.toolsDigest !== "sha256:bfcc7fa1821a5c8fba2595839a9a267926840b77999fa6f9cb70894ed48716ec") {
    throw new Error("claude-messages-tools-mismatch")
  }
  if (projection.unknownDigest !== "sha256:6b6e86a08acc82a7339ef0e0c0566dc4461d882e4b5ed97b8497e0c2e3d817d2") {
    throw new Error("claude-messages-unknown-mismatch")
  }
  if (
    projection.method !== "POST"
    || projection.pathClass !== "messages"
    || projection.model !== "claude-api-model"
    || projection.query !== "beta=true&fixture=exact"
    || projection.correlation !== "claude-contract-request"
    || JSON.stringify(projection.betaValues) !== JSON.stringify([
      "tools-2025-01-01",
      "context-1m-2025-08-07",
    ])
  ) throw new Error("claude-messages-envelope-mismatch")
}

function assertExactClaudeDiscoveryProjection(projection: ClaudeRequestProjection): void {
  if (
    projection.method !== "GET"
    || projection.pathClass !== "models"
    || projection.version !== "2023-06-01"
  ) throw new Error("claude-discovery-version-mismatch")
}

function assertClaudeUpstreamResponseHeaders(headers: Record<string, string | string[] | undefined>): void {
  if (headers["x-upstream"] !== "claude-fixture") throw new Error("claude-response-header-mismatch")
}

function fixedClaudeRequestAuditDiagnostic(error: unknown): string {
  const message = error instanceof Error ? error.message : ""
  return [
    "secret-scan-failed:claude-captured-routing-credential",
    "secret-scan-failed:claude-captured-provider-credential",
    "claude-captured-authentication-invalid",
    "claude-captured-body-invalid",
  ].includes(message) ? message : "claude-captured-request-audit-failed"
}

function assertChildVisibleEnvironment(
  requested: Readonly<Record<"HOME" | "CODEX_HOME" | "CLAUDE_CONFIG_DIR", string>>,
  actual: Readonly<NodeJS.ProcessEnv>,
  requestedCwd?: string,
  actualCwd?: string,
): void {
  if (
    actual.HOME !== requested.HOME
    || actual.CODEX_HOME !== requested.CODEX_HOME
    || actual.CLAUDE_CONFIG_DIR !== requested.CLAUDE_CONFIG_DIR
    || requestedCwd !== actualCwd
  ) throw new Error("claude-environment-traps-not-child-visible")
}

type ClaudeSecurityFinalizerStep = {
  name: string
  run: () => void | Promise<void>
}

async function runClaudeSecurityFinalizer(steps: readonly ClaudeSecurityFinalizerStep[]): Promise<void> {
  const failures: string[] = []
  for (const step of steps) {
    try { await step.run() } catch { failures.push(step.name) }
  }
  if (failures.length > 0) {
    throw new Error(`claude-security-finalizer-failed:${failures.join(",")}`)
  }
}

type ReconciliationSecurityFinalizer = {
  heldRelease: () => void | Promise<void>
  nativeFrameDrain: () => void | Promise<void>
  rendererDestroy: () => void | Promise<void>
  sessionClose: () => void | Promise<void>
  shutdownSignal: () => void | Promise<void>
  processKill: () => void | Promise<void>
  processOutputDrain: () => void | Promise<void>
  upstreamDrain: () => void | Promise<void>
  previewAudit: () => void | Promise<void>
  targetViewAudit: () => void | Promise<void>
  outcomeAudit: () => void | Promise<void>
  receiptAudit: () => void | Promise<void>
  activityAudit: () => void | Promise<void>
  nativeFrameAudit: () => void | Promise<void>
  sqliteAudit: () => void | Promise<void>
  reconciliationRecoveryAudit: () => void | Promise<void>
  targetSettingsAudit: () => void | Promise<void>
  rawRpcAudit: () => void | Promise<void>
  processOutputAudit: () => void | Promise<void>
  upstreamRequestAudit: () => void | Promise<void>
}

async function runReconciliationSecurityFinalizer(steps: ReconciliationSecurityFinalizer): Promise<void> {
  await runClaudeSecurityFinalizer([
    { name: "held-release", run: steps.heldRelease },
    { name: "native-frame-drain", run: steps.nativeFrameDrain },
    { name: "renderer-destroy", run: steps.rendererDestroy },
    { name: "session-close", run: steps.sessionClose },
    { name: "shutdown-signal", run: steps.shutdownSignal },
    { name: "process-kill", run: steps.processKill },
    { name: "process-output-drain", run: steps.processOutputDrain },
    { name: "upstream-drain", run: steps.upstreamDrain },
    { name: "preview", run: steps.previewAudit },
    { name: "target-view", run: steps.targetViewAudit },
    { name: "outcome", run: steps.outcomeAudit },
    { name: "receipt", run: steps.receiptAudit },
    { name: "activity", run: steps.activityAudit },
    { name: "native-frame", run: steps.nativeFrameAudit },
    { name: "sqlite", run: steps.sqliteAudit },
    { name: "reconciliation-recovery", run: steps.reconciliationRecoveryAudit },
    { name: "target-settings", run: steps.targetSettingsAudit },
    { name: "raw-rpc", run: steps.rawRpcAudit },
    { name: "process-output", run: steps.processOutputAudit },
    { name: "upstream-request", run: steps.upstreamRequestAudit },
  ])
}

function extractManagedConfig(text: string, expectedModel: string): { endpoint: string; credential: string } {
  expect(text.includes("# operator comment survives")).toBeTrue()
  expect(text.includes('unrelated = "keep-me"')).toBeTrue()
  expect(text.includes(`model = "${expectedModel}"`)).toBeTrue()
  expect(text.includes('model_provider = "muxvia_codex"')).toBeTrue()
  expect(text.includes('[model_providers.muxvia_codex]')).toBeTrue()
  expect(text.includes('name = "Muxvia"')).toBeTrue()
  expect(text.includes('wire_api = "responses"')).toBeTrue()
  expect(text.includes("supports_websockets = false")).toBeTrue()
  const endpoint = text.match(/base_url\s*=\s*"(http:\/\/127\.0\.0\.1:\d+\/v1)"/)?.[1]
  const credential = text.match(/X-Muxvia-Routing-Credential"\s*=\s*"([a-f0-9]{64})"/)?.[1]
  if (!endpoint || !credential) throw new Error("managed endpoint or credential missing")
  return { endpoint, credential }
}

function extractReconciledCodexRoute(
  text: string,
  expectedModel: string,
  forbiddenSecrets: readonly string[],
): { endpoint: string; credential: string } {
  scanNoSecrets([text], forbiddenSecrets, "reconciliation-codex-settings")
  if (!text.includes(`model = "${expectedModel}"`)) throw new Error("reconciliation-codex-model-mismatch")
  const endpoint = text.match(/base_url\s*=\s*"(http:\/\/127\.0\.0\.1:\d+\/v1)"/)?.[1]
  const credential = text.match(/X-Muxvia-Routing-Credential"\s*=\s*"([a-f0-9]{64})"/)?.[1]
  if (!endpoint || !credential) throw new Error("reconciliation-codex-route-missing")
  return { endpoint, credential }
}

function extractReconciledClaudeRoute(
  text: string,
  expectedModel: string,
  forbiddenSecrets: readonly string[],
): { endpoint: string; credential: string } {
  scanNoSecrets([text], forbiddenSecrets, "reconciliation-claude-settings")
  let parsed: { env?: Record<string, unknown> }
  try { parsed = JSON.parse(text) as typeof parsed } catch {
    throw new Error("reconciliation-claude-settings-invalid")
  }
  const endpoint = parsed.env?.ANTHROPIC_BASE_URL
  const credential = parsed.env?.ANTHROPIC_AUTH_TOKEN
  if (
    parsed.env?.ANTHROPIC_MODEL !== expectedModel
    || typeof endpoint !== "string"
    || !/^http:\/\/127\.0\.0\.1:\d+$/.test(endpoint)
    || typeof credential !== "string"
    || !/^[a-f0-9]{64}$/.test(credential)
  ) throw new Error("reconciliation-claude-route-mismatch")
  return { endpoint, credential }
}

function assertExactDirectManagedConfig(text: string, expectedText: string): void {
  if (text !== expectedText) {
    throw new Error("direct-managed-configuration-mismatch")
  }
}

function assertExactFileMode(actual: number, expected: number): void {
  if (actual !== expected) throw new Error("direct-managed-configuration-mode-mismatch")
}

function assertBytesContainNoSecrets(bytes: Buffer, secrets: readonly string[], label: string): void {
  const contaminated = secrets.some((secret) => secret.length > 0 && bytes.includes(Buffer.from(secret)))
  if (contaminated) throw new Error(`secret-scan-failed:${label}`)
}

function scanNoSecrets(
  values: readonly unknown[],
  secrets: readonly string[] = [providerSecret, wrongRoutingSecret],
  label = "structured-surfaces",
): void {
  const serialized = values.map((value) => {
    if (typeof value === "string") return value
    const json = JSON.stringify(value)
    return json === undefined ? String(value) : json
  }).join("\n")
  assertBytesContainNoSecrets(Buffer.from(serialized), secrets, label)
}

function fixedSurfaceError(error: unknown, scanFailure: string, fallback: string): Error {
  const message = error instanceof Error ? error.message : ""
  return new Error(message === scanFailure ? scanFailure : fallback)
}

async function waitForSecretSafeText(
  read: () => string,
  predicate: (text: string) => boolean,
  secrets: readonly string[],
  label: string,
): Promise<string> {
  const scanLabel = `${label}-text`
  const scanFailure = `secret-scan-failed:${scanLabel}`
  try {
    await waitFor(() => {
      const current = read()
      scanNoSecrets([current], secrets, scanLabel)
      return predicate(current)
    }, label)
    const current = read()
    scanNoSecrets([current], secrets, scanLabel)
    return current
  } catch (error) {
    throw fixedSurfaceError(error, scanFailure, `text-wait-failed:${label}`)
  }
}

async function waitForSecretSafeFrame(
  setup: Awaited<ReturnType<typeof testRender>>,
  predicate: (frame: string) => boolean,
  secrets: readonly string[],
  label: string,
): Promise<string> {
  const scanLabel = `${label}-frame`
  const scanFailure = `secret-scan-failed:${scanLabel}`
  try {
    const frame = await setup.waitForFrame((current) => {
      scanNoSecrets([current], secrets, scanLabel)
      return predicate(current)
    })
    scanNoSecrets([frame], secrets, scanLabel)
    return frame
  } catch (error) {
    throw fixedSurfaceError(error, scanFailure, `renderer-wait-failed:${label}`)
  }
}

function captureSecretSafeFrame(
  setup: Awaited<ReturnType<typeof testRender>>,
  renderedFrames: string[],
  secrets: readonly string[],
  label: string,
): string {
  const scanLabel = `${label}-frame`
  const scanFailure = `secret-scan-failed:${scanLabel}`
  try {
    const frame = setup.captureCharFrame()
    scanNoSecrets([frame], secrets, scanLabel)
    renderedFrames.push(frame)
    return frame
  } catch (error) {
    throw fixedSurfaceError(error, scanFailure, `renderer-capture-failed:${label}`)
  }
}

function assertSecretSafeStructured<T>(
  value: T,
  secrets: readonly string[],
  label: string,
  assertion: (safeValue: T) => void = () => {},
): void {
  const scanLabel = `${label}-structured`
  const scanFailure = `secret-scan-failed:${scanLabel}`
  try {
    scanNoSecrets([value], secrets, scanLabel)
    assertion(value)
  } catch (error) {
    throw fixedSurfaceError(error, scanFailure, `structured-assertion-failed:${label}`)
  }
}

function asyncErrorSurface(error: unknown): unknown {
  if (!(error instanceof Error)) return error
  const control = error as Error & { code?: unknown; authoritativeView?: unknown }
  return {
    name: control.name,
    message: control.message,
    code: control.code,
    authoritativeView: control.authoritativeView,
  }
}

async function actSecretSafe(
  session: TargetSession,
  action: OrdinaryTargetAction,
  secrets: readonly string[],
  label: string,
): Promise<Awaited<ReturnType<TargetSession["act"]>>> {
  try {
    const outcome = await session.act(action)
    assertSecretSafeStructured(outcome, secrets, `${label}-outcome`)
    return outcome
  } catch (error) {
    const scanLabel = `${label}-error`
    const scanFailure = `secret-scan-failed:${scanLabel}`
    try {
      scanNoSecrets([asyncErrorSurface(error)], secrets, scanLabel)
    } catch (scanError) {
      throw fixedSurfaceError(scanError, scanFailure, `target-action-failed:${label}`)
    }
    throw new Error(`target-action-failed:${label}`)
  }
}

async function waitForSecretSafeSession(
  session: TargetSession,
  predicate: (view: Readonly<TargetView>) => boolean,
  secrets: readonly string[],
  label: string,
): Promise<Readonly<TargetView>> {
  const structuredLabel = `${label}-target-view`
  const scanFailure = `secret-scan-failed:${structuredLabel}-structured`
  const inspect = (view: Readonly<TargetView>) => {
    assertSecretSafeStructured(view, secrets, structuredLabel)
    return predicate(view)
  }
  try {
    const current = session.get()
    if (inspect(current)) return current
    return await new Promise<Readonly<TargetView>>((resolveWait, reject) => {
      let unsubscribe = () => {}
      const timeout = setTimeout(() => {
        unsubscribe()
        reject(new Error(`target-view-wait-failed:${label}`))
      }, deadlineMs)
      const finish = (view: Readonly<TargetView>) => {
        clearTimeout(timeout)
        unsubscribe()
        resolveWait(view)
      }
      unsubscribe = session.subscribe((view) => {
        try {
          if (inspect(view)) finish(view)
        } catch (error) {
          clearTimeout(timeout)
          unsubscribe()
          reject(fixedSurfaceError(error, scanFailure, `target-view-wait-failed:${label}`))
        }
      })
      try {
        const latest = session.get()
        if (inspect(latest)) finish(latest)
      } catch (error) {
        clearTimeout(timeout)
        unsubscribe()
        reject(fixedSurfaceError(error, scanFailure, `target-view-wait-failed:${label}`))
      }
    })
  } catch (error) {
    throw fixedSurfaceError(error, scanFailure, `target-view-wait-failed:${label}`)
  }
}

type ClaudeDirectFrameEvidence = {
  type?: unknown
  result?: { kind?: unknown; outcome?: { status?: unknown; view?: unknown } }
  view?: unknown
}

function matchesClaudeDirectView(
  value: unknown,
  expected: { providerId: string; model: string; settingsPath: string },
): boolean {
  if (!value || typeof value !== "object") return false
  const view = value as TargetView
  return view.target === "claude"
    && view.mode === "direct"
    && view.takeover.state === "inactive"
    && view.takeover.endpoint === null
    && view.currentProviderId === expected.providerId
    && view.servingProviderId === null
    && view.managedConfiguration.state === "applied"
    && view.managedConfiguration.path === expected.settingsPath
    && view.managedConfiguration.restartRequired === true
    && view.recovery.state === "committed"
    && view.activatedSnapshot?.providerId === expected.providerId
    && view.activatedSnapshot.model === expected.model
    && view.activatedSnapshot.protocol === "anthropic-messages"
}

async function assertClaudeDirectResponseAndPush(
  frames: readonly unknown[],
  start: number,
  expected: { providerId: string; model: string; settingsPath: string },
  secrets: readonly string[],
  label: string,
): Promise<void> {
  const candidates = frames.slice(start)
  scanNoSecrets(candidates, secrets, `${label}-raw-frame`)
  const responses = candidates.filter((candidate) => {
    const frame = candidate as ClaudeDirectFrameEvidence
    return frame.type === "response" && frame.result?.kind === "action-outcome"
  }) as ClaudeDirectFrameEvidence[]
  const pushes = candidates.filter((candidate) => (
    candidate as ClaudeDirectFrameEvidence
  ).type === "target-view") as ClaudeDirectFrameEvidence[]
  const responseIndex = candidates.indexOf(responses[0])
  const pushIndex = candidates.indexOf(pushes[0])
  const responseView = responses[0]?.result?.outcome?.view
  const pushView = pushes[0]?.view
  if (
    responses.length !== 1
    || pushes.length !== 1
    || responseIndex < 0
    || pushIndex !== responseIndex + 1
    || responses[0]!.result?.outcome?.status !== "applied"
    || !matchesClaudeDirectView(responseView, expected)
    || !matchesClaudeDirectView(pushView, expected)
    || canonicalJson(responseView) !== canonicalJson(pushView)
  ) throw new Error(`claude-direct-response-push-mismatch:${label}`)
}

function assertReconciliationResponseBeforeSinglePush(
  frames: readonly unknown[],
  start: number,
  target: "codex" | "claude",
  outcome: unknown,
  label: string,
): void {
  const candidates = frames.slice(start) as ClaudeDirectFrameEvidence[]
  const responses = candidates.filter((frame) => frame.type === "response" && frame.result?.kind === "action-outcome")
  const pushes = candidates.filter((frame) => frame.type === "target-view")
  const responseIndex = candidates.indexOf(responses[0]!)
  const pushIndex = candidates.indexOf(pushes[0]!)
  const responseOutcome = responses[0]?.result?.outcome
  const responseView = responseOutcome?.view as TargetView | undefined
  const pushView = pushes[0]?.view as TargetView | undefined
  if (
    candidates.length !== 2
    || responses.length !== 1
    || pushes.length !== 1
    || responseIndex !== 0
    || pushIndex !== 1
    || responseView?.target !== target
    || pushView?.target !== target
    || canonicalJson(responseOutcome) !== canonicalJson(outcome)
    || canonicalJson(responseView) !== canonicalJson(pushView)
  ) throw new Error(`reconciliation-response-push-mismatch:${label}`)
}

function scanRawRpcFramesNoSecrets(
  streams: readonly (readonly Buffer[])[],
  secrets: readonly string[] = [providerSecret, wrongRoutingSecret],
): void {
  if (streams.reduce((count, frames) => count + frames.length, 0) === 0) {
    throw new Error("rpc-frame-audit-empty")
  }
  const decodedFrames = streams.flatMap((frames) => {
    const decoder = new FrameDecoder()
    try {
      const decoded = frames.flatMap((frame) => decoder.push(frame))
      decoder.finish()
      return decoded
    } catch {
      throw new Error("rpc-frame-audit-invalid")
    }
  })
  scanNoSecrets(decodedFrames, secrets, "decoded-rpc-frame")
}

function createOutboundOperationAudit(): {
  operationKinds: string[]
  observe: (chunk: Uint8Array) => void
  finish: () => void
} {
  const decoder = new FrameDecoder()
  const operationKinds: string[] = []
  return {
    operationKinds,
    observe: (chunk) => {
      try {
        for (const value of decoder.push(chunk)) {
          const frame = parseClientFrame(value)
          if (frame.type === "request") operationKinds.push(frame.operation.kind)
        }
      } catch {
        throw new Error("outbound-operation-audit-invalid")
      }
    },
    finish: () => {
      try {
        decoder.finish()
      } catch {
        throw new Error("outbound-operation-audit-incomplete")
      }
    },
  }
}

function createRendererAudit(_setup: Awaited<ReturnType<typeof testRender>>): {
  frames: () => string[]
  start: () => void
  stop: () => void
} {
  const recorder = new TestRecorder(_setup.renderer)
  return {
    frames: () => recorder.recordedFrames.map(({ frame }) => frame),
    start: () => recorder.rec(),
    stop: () => recorder.stop(),
  }
}

function scanProcessOutputNoSecrets(
  streams: readonly (readonly Buffer[])[],
  secrets: readonly string[],
): void {
  streams.forEach((chunks, index) => {
    assertBytesContainNoSecrets(Buffer.concat(chunks), secrets, `process-output-stream-${index}`)
  })
}

function captureProcessOutput(child: ReturnType<typeof spawn>): {
  streams: readonly [Buffer[], Buffer[]]
  completed: Promise<{ code: number | null; signal: NodeJS.Signals | null }>
} {
  const stdoutChunks: Buffer[] = []
  const stderrChunks: Buffer[] = []
  child.stdout?.on("data", (chunk) => stdoutChunks.push(Buffer.from(chunk)))
  child.stderr?.on("data", (chunk) => stderrChunks.push(Buffer.from(chunk)))
  const stdoutFinished = child.stdout ? finished(child.stdout) : Promise.resolve()
  const stderrFinished = child.stderr ? finished(child.stderr) : Promise.resolve()
  const closed = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolveClose, reject) => {
    child.once("error", () => reject(new Error("routing-service-spawn-failed")))
    child.once("close", (code, signal) => resolveClose({ code, signal }))
  })
  return {
    streams: [stdoutChunks, stderrChunks],
    completed: closed.then(async (result) => {
      try {
        await Promise.all([stdoutFinished, stderrFinished])
      } catch {
        throw new Error("routing-service-output-drain-failed")
      }
      return result
    }),
  }
}

async function observeDarwinTcpListenerPorts(pid: number): Promise<number[]> {
  const child = spawn("/usr/sbin/lsof", [
    "-nP",
    "-a",
    "-p", String(pid),
    "-iTCP",
    "-sTCP:LISTEN",
    "-Fn",
  ], { stdio: ["ignore", "pipe", "pipe"] })
  const output = captureProcessOutput(child)
  const completed = await Promise.race([
    output.completed,
    Bun.sleep(5_000).then(() => undefined),
  ])
  if (!completed) {
    child.kill("SIGKILL")
    await output.completed.catch(() => {})
    throw new Error("tcp-listener-observation-timeout")
  }
  if (completed.signal !== null || (completed.code !== 0 && completed.code !== 1)) {
    throw new Error("tcp-listener-observation-failed")
  }
  const ports = Buffer.concat(output.streams[0]).toString("utf8").split("\n").flatMap((line) => {
    const match = line.match(/^n.*:(\d+)$/)
    return match ? [Number(match[1])] : []
  })
  return [...new Set(ports)].sort((left, right) => left - right)
}

async function readLinuxTcpTable(path: string, socketInodes: ReadonlySet<string>): Promise<number[]> {
  let table: string
  try {
    table = await readFile(path, "utf8")
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return []
    throw new Error("tcp-listener-observation-failed")
  }
  return table.split("\n").slice(1).flatMap((line) => {
    const fields = line.trim().split(/\s+/)
    if (fields.length < 10 || fields[3] !== "0A" || !socketInodes.has(fields[9]!)) return []
    const portHex = fields[1]?.split(":").at(-1)
    const port = portHex ? Number.parseInt(portHex, 16) : Number.NaN
    return Number.isInteger(port) ? [port] : []
  })
}

async function observeLinuxTcpListenerPorts(pid: number): Promise<number[]> {
  let descriptors: string[]
  try {
    descriptors = await readdir(`/proc/${pid}/fd`)
  } catch {
    throw new Error("tcp-listener-observation-failed")
  }
  const socketInodes = new Set<string>()
  await Promise.all(descriptors.map(async (descriptor) => {
    try {
      const target = await readlink(`/proc/${pid}/fd/${descriptor}`)
      const match = target.match(/^socket:\[(\d+)]$/)
      if (match) socketInodes.add(match[1]!)
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
        throw new Error("tcp-listener-observation-failed")
      }
    }
  }))
  const ports = [
    ...await readLinuxTcpTable(`/proc/${pid}/net/tcp`, socketInodes),
    ...await readLinuxTcpTable(`/proc/${pid}/net/tcp6`, socketInodes),
  ]
  return [...new Set(ports)].sort((left, right) => left - right)
}

async function observeTcpListenerPorts(pid: number): Promise<number[]> {
  if (!Number.isSafeInteger(pid) || pid <= 0) throw new Error("tcp-listener-observation-invalid-pid")
  if (process.platform === "darwin") return await observeDarwinTcpListenerPorts(pid)
  if (process.platform === "linux") return await observeLinuxTcpListenerPorts(pid)
  throw new Error("tcp-listener-observation-unsupported")
}

async function waitForTcpListenerObservation(
  observe: () => Promise<number[]>,
  predicate: (ports: readonly number[]) => boolean,
  label: string,
  timeoutMs = deadlineMs,
  schedule: (next: () => void) => void = (next) => { setImmediate(next) },
): Promise<number[]> {
  return await new Promise<number[]>((resolveObservation, reject) => {
    let settled = false
    const timeout = setTimeout(() => {
      settled = true
      reject(new Error(`event-barrier-failed:${label}`))
    }, timeoutMs)
    const inspect = async () => {
      if (settled) return
      try {
        const ports = await observe()
        if (predicate(ports)) {
          settled = true
          clearTimeout(timeout)
          resolveObservation(ports)
          return
        }
        schedule(() => { void inspect() })
      } catch {
        settled = true
        clearTimeout(timeout)
        reject(new Error(`event-barrier-failed:${label}`))
      }
    }
    void inspect()
  })
}

async function waitForEventTurnReadiness(
  attempt: () => Promise<boolean>,
  processExit: Promise<never>,
  label: string,
  timeoutMs = deadlineMs,
  schedule: (next: () => void) => void = (next) => { setImmediate(next) },
): Promise<void> {
  return await new Promise<void>((resolveReady, reject) => {
    let settled = false
    const finish = (failure?: unknown) => {
      if (settled) return
      settled = true
      clearTimeout(timeout)
      if (failure === undefined) resolveReady()
      else reject(failure)
    }
    const timeout = setTimeout(() => finish(new Error(`event-barrier-failed:${label}`)), timeoutMs)
    processExit.then(
      () => finish(new Error(`event-barrier-failed:${label}`)),
      (error) => finish(error),
    )
    const inspect = async () => {
      if (settled) return
      try {
        if (await attempt()) {
          finish()
          return
        }
        schedule(() => { void inspect() })
      } catch {
        finish(new Error(`event-barrier-failed:${label}`))
      }
    }
    void inspect()
  })
}

async function probeUnixSocket(
  path: string,
  retained: Array<ReturnType<typeof createConnection>>,
): Promise<boolean> {
  return await new Promise<boolean>((resolveProbe) => {
    const socket = createConnection({ path })
    const finish = (ready: boolean) => {
      socket.removeAllListeners()
      if (ready) retained.push(socket)
      else socket.destroy()
      resolveProbe(ready)
    }
    socket.once("connect", () => finish(true))
    socket.once("error", () => finish(false))
  })
}

async function assertNoTcpListeners(pid: number): Promise<void> {
  if ((await observeTcpListenerPorts(pid)).length !== 0) {
    throw new Error("direct-service-unexpected-tcp-listener")
  }
}

function sensitiveDigest(value: unknown): string | null {
  if (value === null || value === undefined) return null
  const bytes = Buffer.isBuffer(value)
    ? value
    : value instanceof Uint8Array
      ? Buffer.from(value)
      : Buffer.from(String(value))
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`
}

function authorizationClassification(
  value: string | null,
  expectedProviderCredential: string,
): "absent" | "expected-provider" | "unexpected" {
  if (value === null) return "absent"
  return value === `Bearer ${expectedProviderCredential}` ? "expected-provider" : "unexpected"
}

function projectCapturedRequest(
  request: CapturedRequest,
  expectedProviderCredential: string,
): {
  method: string
  pathClass: "models" | "responses" | "edited-reachability" | "other"
  hasQuery: boolean
  authorizationClass: "absent" | "expected-provider" | "unexpected"
  headerAuthorizationClass: "absent" | "expected-provider" | "unexpected"
  hasAuthorizationHeader: boolean
  hasContentTypeHeader: boolean
  hasTestHeader: boolean
  contentTypeClass: "absent" | "application-json" | "other"
  testHeaderClass: "absent" | "preserved" | "other"
  bodyClass: "empty" | "expected-response-request" | "other"
  bodyBytes: number
  bodyDigest: string
} {
  const path = request.path.split("?", 1)[0]
  const pathClass = path === "/v1/models"
    ? "models"
    : path === "/v1/responses"
      ? "responses"
      : path === "/v1/edited"
        ? "edited-reachability"
        : "other"
  return {
    method: request.method,
    pathClass,
    hasQuery: request.path.includes("?"),
    authorizationClass: authorizationClassification(request.authorization, expectedProviderCredential),
    headerAuthorizationClass: authorizationClassification(
      singleCapturedHeader(request.headers.authorization),
      expectedProviderCredential,
    ),
    hasAuthorizationHeader: request.headers.authorization !== undefined,
    hasContentTypeHeader: request.headers["content-type"] !== undefined,
    hasTestHeader: request.headers["x-test-preserved"] !== undefined,
    contentTypeClass: request.contentType === null
      ? "absent"
      : request.contentType === "application/json"
        ? "application-json"
        : "other",
    testHeaderClass: request.testHeader === null
      ? "absent"
      : request.testHeader === "preserved"
        ? "preserved"
        : "other",
    bodyClass: request.body === ""
      ? "empty"
      : request.body === '{"model":"gpt-test","input":"hello"}'
        ? "expected-response-request"
        : "other",
    bodyBytes: Buffer.byteLength(request.body),
    bodyDigest: sensitiveDigest(request.body)!,
  }
}

function auditCapturedRequestAndProject(
  request: CapturedRequest,
  policy: {
    providerCredential: string
    forbiddenRoutingCredentials: readonly string[]
    authorization: "expected-provider" | "absent"
  },
): ReturnType<typeof projectCapturedRequest> {
  scanNoSecrets(
    [request],
    policy.forbiddenRoutingCredentials,
    "captured-request-routing-credential",
  )

  const expectedAuthorization = `Bearer ${policy.providerCredential}`
  const hasExpectedPropertyAuthorization = request.authorization === expectedAuthorization
  const hasExpectedHeaderAuthorization = request.headers.authorization === expectedAuthorization
  const hasExactExpectedAuthorization = hasExpectedPropertyAuthorization && hasExpectedHeaderAuthorization
  const providerAuditRequest = policy.authorization === "expected-provider" && hasExactExpectedAuthorization
    ? {
        ...request,
        authorization: null,
        headers: { ...request.headers, authorization: null },
      }
    : request
  scanNoSecrets(
    [providerAuditRequest],
    [policy.providerCredential],
    "captured-request-provider-credential",
  )

  const authorizationPolicySatisfied = policy.authorization === "expected-provider"
    ? hasExactExpectedAuthorization
    : request.authorization === null && (request.headers.authorization ?? null) === null
  if (!authorizationPolicySatisfied) throw new Error("captured-request-authorization-policy-failed")

  return projectCapturedRequest(request, policy.providerCredential)
}

type CapturedRequestProjection = ReturnType<typeof projectCapturedRequest>

const expectedModelRequestProjection: CapturedRequestProjection = {
  method: "GET",
  pathClass: "models",
  hasQuery: false,
  authorizationClass: "expected-provider",
  headerAuthorizationClass: "expected-provider",
  hasAuthorizationHeader: true,
  hasContentTypeHeader: false,
  hasTestHeader: false,
  contentTypeClass: "absent",
  testHeaderClass: "absent",
  bodyClass: "empty",
  bodyBytes: 0,
  bodyDigest: "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
}

const expectedResponseRequestProjection: CapturedRequestProjection = {
  method: "POST",
  pathClass: "responses",
  hasQuery: false,
  authorizationClass: "expected-provider",
  headerAuthorizationClass: "expected-provider",
  hasAuthorizationHeader: true,
  hasContentTypeHeader: true,
  hasTestHeader: true,
  contentTypeClass: "application-json",
  testHeaderClass: "preserved",
  bodyClass: "expected-response-request",
  bodyBytes: 36,
  bodyDigest: "sha256:a17bd595d1c0f60f0ac4d95a500643fa08ab4f8519760f80f30c5a9fa74a6da5",
}

const expectedReachabilityRequestProjection: CapturedRequestProjection = {
  method: "GET",
  pathClass: "edited-reachability",
  hasQuery: false,
  authorizationClass: "absent",
  headerAuthorizationClass: "absent",
  hasAuthorizationHeader: false,
  hasContentTypeHeader: false,
  hasTestHeader: false,
  contentTypeClass: "absent",
  testHeaderClass: "absent",
  bodyClass: "empty",
  bodyBytes: 0,
  bodyDigest: "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
}

function fixedCapturedRequestAuditDiagnostic(error: unknown): string {
  const message = error instanceof Error ? error.message : ""
  if (
    message === "secret-scan-failed:captured-request-routing-credential"
    || message === "secret-scan-failed:captured-request-provider-credential"
    || message === "captured-request-authorization-policy-failed"
    || message === "captured-request-unexpected"
  ) return message
  return "captured-request-audit-failed"
}

function createCapturedRequestAudit(config: {
  providerCredential: string
  forbiddenRoutingCredentials: () => readonly string[]
  expectedRequestCount: number
}): {
  expectNext: (authorization: "expected-provider" | "absent") => void
  observe: (request: CapturedRequest) => void
  projection: (index: number) => CapturedRequestProjection
  assertComplete: (requestCount: number) => void
} {
  let pendingAuthorization: "expected-provider" | "absent" | undefined
  let expectedCount = 0
  let auditedCount = 0
  let failure: string | undefined
  const projections: CapturedRequestProjection[] = []
  const inspectedIndexes = new Set<number>()

  return {
    expectNext: (authorization) => {
      if (pendingAuthorization || expectedCount >= config.expectedRequestCount) {
        failure ??= "captured-request-audit-plan-invalid"
        return
      }
      pendingAuthorization = authorization
      expectedCount++
    },
    observe: (request) => {
      const authorization = pendingAuthorization
      pendingAuthorization = undefined
      try {
        const projection = auditCapturedRequestAndProject(request, {
          providerCredential: config.providerCredential,
          forbiddenRoutingCredentials: config.forbiddenRoutingCredentials(),
          authorization: authorization ?? "absent",
        })
        if (!authorization) throw new Error("captured-request-unexpected")
        projections.push(projection)
      } catch (error) {
        failure ??= fixedCapturedRequestAuditDiagnostic(error)
      } finally {
        auditedCount++
      }
    },
    projection: (index) => {
      if (failure) throw new Error(failure)
      const projection = projections[index]
      if (!projection) throw new Error("captured-request-audit-projection-missing")
      inspectedIndexes.add(index)
      return projection
    },
    assertComplete: (requestCount) => {
      if (failure) throw new Error(failure)
      if (requestCount !== auditedCount) throw new Error("captured-request-audit-count-mismatch")
      if (pendingAuthorization || expectedCount !== config.expectedRequestCount) {
        throw new Error("captured-request-audit-incomplete")
      }
      if (
        auditedCount !== config.expectedRequestCount
        || projections.length !== config.expectedRequestCount
        || inspectedIndexes.size !== config.expectedRequestCount
      ) {
        throw new Error("captured-request-audit-request-set-mismatch")
      }
    },
  }
}

function readDatabaseProjection(path: string): unknown {
  const database = new Database(path, { readonly: true })
  try {
    const credentials = database.query(`SELECT id, target, bearer_token AS bearerToken
      FROM credentials ORDER BY id`).all() as Array<Record<string, unknown> & { bearerToken: string }>
    const targetRoute = database.query(`SELECT target, management_revision, view_sequence, current_provider_id,
      serving_provider_id, takeover_state, route_port, routing_credential AS routingCredential,
      activated_snapshot_id, managed_config_path, recovery_state
      FROM target_route_state ORDER BY target`).all() as Array<Record<string, unknown> & { routingCredential: string | null }>
    const targetProblems = database.query(`SELECT target, code, message
      FROM target_problems ORDER BY target, code`).all() as Array<Record<string, unknown> & { message: string }>
    const snapshots = database.query(`SELECT id, target, provider_id, base_url, model,
      provider_bearer_token AS providerBearerToken, epoch
      FROM activated_snapshots ORDER BY id`).all() as Array<Record<string, unknown> & { providerBearerToken: string }>
    const receipts = database.query(`SELECT target, action_id, action_kind, committed_revision,
      outcome_json AS outcomeJson FROM action_receipts ORDER BY committed_revision, action_id`
    ).all() as Array<Record<string, unknown> & { outcomeJson: string }>
    const recovery = database.query(`SELECT id, target, action_id, config_path,
      file_identity_json AS fileIdentityJson, payload_json AS payloadJson, state, created_revision
      FROM activation_recovery ORDER BY created_revision, id`).all() as Array<Record<string, unknown> & {
        fileIdentityJson: string
        payloadJson: string
      }>
    return [
      ["metadata", database.query("SELECT key, value FROM metadata ORDER BY key").all()],
      ["credentials", credentials.map(({ bearerToken, ...row }) => ({
        ...row,
        bearerTokenDigest: sensitiveDigest(bearerToken),
      }))],
      ["providers", database.query(`SELECT id, target, position, provider_revision, name, base_url, model, protocol, authentication,
        credential_id, provenance_kind, provenance_key, generated_owner_id FROM providers ORDER BY position`).all()],
      ["target-route", targetRoute.map(({ routingCredential, ...row }) => ({
        ...row,
        routingCredentialDigest: sensitiveDigest(routingCredential),
      }))],
      ["target-problems", targetProblems.map(({ message, ...row }) => ({
        ...row,
        messageDigest: sensitiveDigest(message),
      }))],
      ["snapshots", snapshots.map(({ providerBearerToken, ...row }) => ({
        ...row,
        providerBearerTokenDigest: sensitiveDigest(providerBearerToken),
      }))],
      ["receipts", receipts.map(({ outcomeJson, ...row }) => ({
        ...row,
        outcomeJsonDigest: sensitiveDigest(outcomeJson),
      }))],
      ["recovery", recovery.map(({
        fileIdentityJson,
        payloadJson,
        ...row
      }) => ({
        ...row,
        fileIdentityJsonDigest: sensitiveDigest(fileIdentityJson),
        payloadJsonDigest: sensitiveDigest(payloadJson),
      }))],
    ]
  } finally {
    database.close()
  }
}

type SqliteSecretPolicy = {
  providerSecrets: ReadonlyArray<{ target: "codex" | "claude"; name: string; secret: string }>
  routingSecrets: ReadonlyArray<{ target: "codex" | "claude"; secret: string }>
}

function claudeRecoveryOwnershipVersion(payload: unknown): number | undefined {
  if (!payload || typeof payload !== "object") return undefined
  const root = payload as Record<string, unknown>
  if (root.target !== "claude" || !root.before || typeof root.before !== "object"
    || !root.desired || typeof root.desired !== "object") return undefined
  const before = root.before as Record<string, unknown>
  const desired = root.desired as Record<string, unknown>
  const versions = [
    root.ownership_version ?? 1,
    before.ownership_version ?? 1,
    desired.ownership_version ?? 1,
  ]
  if (!versions.every((version) => version === 1 || version === 2)) return undefined
  return versions.every((version) => version === versions[0]) ? versions[0] as number : undefined
}

function approvedRecoveryOccurrence(
  target: "codex" | "claude",
  occurrence: SecretOccurrence,
  ownershipVersion?: number,
): boolean {
  if (target === "claude") {
    const approvedPaths = [
      "before.owned.auth_token",
      "desired.owned.auth_token",
    ]
    if (ownershipVersion === 2) {
      approvedPaths.push("before.owned.api_key", "desired.owned.api_key")
    }
    return occurrence.exact && approvedPaths.includes(occurrence.path)
  }
  if ([
    "before.owned.provider_http_headers.rendered",
    "before.owned.provider_http_headers.semantic.Authorization",
    "desired.provider_http_headers.rendered",
    "desired.provider_http_headers.semantic.Authorization",
  ].includes(occurrence.path)) return occurrence.count === 1
  return occurrence.count === 1 && occurrence.exact && [
    "before.owned.provider_http_headers.semantic.X-Muxvia-Routing-Credential",
    "desired.provider_http_headers.semantic.X-Muxvia-Routing-Credential",
  ].includes(occurrence.path)
}

function approvedReconciliationOccurrence(
  target: "codex" | "claude",
  occurrence: SecretOccurrence,
  secret: string,
): boolean {
  if (occurrence.count !== 1) return false
  if (target === "claude") {
    return occurrence.exact && /^(before|desired)\.owned\.(auth_token|api_key)$/.test(occurrence.path)
  }
  const authorizationPath = (
    /^before\.unrelated\.model_providers\.[^.]+\.http_headers\.Authorization$/.test(occurrence.path)
    || /^(before\.owned|before\.provider_restore|desired|desired\.desired|desired\.provider_restore)\.provider_http_headers\.semantic\.Authorization$/.test(occurrence.path)
    || /^(before\.owned|before\.provider_restore(?:\.Present)?|desired|desired\.desired|desired\.provider_restore(?:\.Present)?)\.http_headers\.semantic\.Authorization$/.test(occurrence.path)
  )
  if (authorizationPath) return occurrence.value === `Bearer ${secret}`
  const renderedAuthorizationPath = (
    /^(before\.owned|before\.provider_restore|desired|desired\.desired|desired\.provider_restore)\.provider_http_headers\.rendered$/.test(occurrence.path)
    || /^(before\.owned|before\.provider_restore(?:\.Present)?|desired|desired\.desired|desired\.provider_restore(?:\.Present)?)\.http_headers\.rendered$/.test(occurrence.path)
  )
  if (renderedAuthorizationPath) {
    const escapedSecret = secret.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
    return new RegExp(`(?:^|[\\s{,])(?:"Authorization"|Authorization)\\s*=\\s*"Bearer ${escapedSecret}"(?:$|[\\s},])`)
      .test(occurrence.value)
      || new RegExp(`(?:^|[\\s{,])"X-Muxvia-Routing-Credential"\\s*=\\s*"${escapedSecret}"(?:$|[\\s},])`)
        .test(occurrence.value)
  }
  return occurrence.exact && (
    /^before\.unrelated\.model_providers\.[^.]+\.http_headers\.X-Muxvia-Routing-Credential$/.test(occurrence.path)
    || /^(before\.owned|before\.provider_restore|desired|desired\.desired|desired\.provider_restore)\.provider_http_headers\.semantic\.X-Muxvia-Routing-Credential$/.test(occurrence.path)
    || /^(before\.owned|before\.provider_restore(?:\.Present)?|desired|desired\.desired|desired\.provider_restore(?:\.Present)?)\.http_headers\.semantic\.X-Muxvia-Routing-Credential$/.test(occurrence.path)
  )
}

function auditReconciliationRecoveryRows(
  rows: readonly { target: string; beforeJson: string; desiredJson: string }[],
  secrets: ReadonlyArray<{ target: HeldReconciliationTarget; secret: string }>,
): void {
  for (const row of rows) {
    if (row.target !== "codex" && row.target !== "claude") {
      throw new Error("reconciliation-recovery-target-invalid")
    }
    const target = row.target
    let material: unknown
    try {
      material = { before: JSON.parse(row.beforeJson), desired: JSON.parse(row.desiredJson) }
    } catch {
      throw new Error("reconciliation-recovery-json-invalid")
    }
    for (const entry of secrets) {
      const secret = entry.secret
      const occurrences = jsonSecretOccurrences(material, secret)
      if (entry.target !== target && occurrences.length > 0) {
        throw new Error("secret-scan-failed:reconciliation-recovery-secret-location")
      }
      if (!occurrences.every((occurrence) => approvedReconciliationOccurrence(target, occurrence, secret))) {
        throw new Error("secret-scan-failed:reconciliation-recovery-secret-location")
      }
    }
  }
}

function auditReconciliationTargetSettings(
  codexBytes: Buffer | undefined,
  claudeBytes: Buffer | undefined,
  providerSettingsSecrets: ReadonlyArray<{ target: HeldReconciliationTarget; secret: string }>,
  routingSettingsSecrets: ReadonlyArray<{ target: HeldReconciliationTarget; secret: string }>,
): void {
  const codexText = codexBytes?.toString("utf8") ?? ""
  const boundSecrets = [...providerSettingsSecrets, ...routingSettingsSecrets]
  for (const { target, secret } of boundSecrets) {
    if (!codexText.includes(secret)) continue
    if (target !== "codex") {
      throw new Error("secret-scan-failed:reconciliation-target-settings-location")
    }
    const escaped = secret.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")
    const provider = providerSettingsSecrets.some((entry) => entry.target === target && entry.secret === secret)
    const exact = provider
      ? new RegExp(`(?:"Authorization"|Authorization)\\s*=\\s*"Bearer ${escaped}"`).test(codexText)
      : new RegExp(`"X-Muxvia-Routing-Credential"\\s*=\\s*"${escaped}"`).test(codexText)
    if (!exact || codexText.split(secret).length !== 2) {
      throw new Error("secret-scan-failed:reconciliation-target-settings-location")
    }
  }
  if (!claudeBytes) return
  let claudeSettings: unknown
  try { claudeSettings = JSON.parse(claudeBytes.toString("utf8")) } catch {
    throw new Error("reconciliation-target-settings-json-invalid")
  }
  for (const { target, secret } of boundSecrets) {
    const occurrences = jsonSecretOccurrences(claudeSettings, secret)
    if (occurrences.length === 0) continue
    if (target !== "claude") {
      throw new Error("secret-scan-failed:reconciliation-target-settings-location")
    }
    const provider = providerSettingsSecrets.some((entry) => entry.target === target && entry.secret === secret)
    const approved = provider
      ? /^env\.ANTHROPIC_(AUTH_TOKEN|API_KEY)$/
      : /^env\.ANTHROPIC_AUTH_TOKEN$/
    if (!occurrences.every((occurrence) => occurrence.count === 1 && occurrence.exact && approved.test(occurrence.path))) {
      throw new Error("secret-scan-failed:reconciliation-target-settings-location")
    }
  }
}

function auditSqliteSecretLocations(path: string, policy: SqliteSecretPolicy): void {
  const secrets = [...policy.providerSecrets, ...policy.routingSecrets]
  const database = new Database(path, { readonly: true })
  try {
    const definitions = database.query(`SELECT type, name, sql FROM sqlite_master
      WHERE type IN ('table', 'index', 'trigger', 'view') ORDER BY type, name`).all() as Array<{
        type: string
        name: string
        sql: string | null
      }>
    scanNoSecrets(definitions, secrets.map(({ secret }) => secret), "claude-sqlite-schema")
    const tableNames = definitions.filter(({ type, sql, name }) =>
      type === "table" && sql && !name.startsWith("sqlite_"))
      .map(({ name }) => name)
    const providers = tableNames.includes("providers")
      ? database.query("SELECT id, target, name, credential_id FROM providers").all() as Array<{
          id: string
          target: string
          name: string
          credential_id: string | null
        }>
      : []
    for (const table of tableNames) {
      const quoted = `"${table.replaceAll('"', '""')}"`
      const rows = database.query(`SELECT * FROM ${quoted}`).all() as Array<Record<string, unknown>>
      for (const row of rows) {
        let recoveryPayload: unknown
        let recoveryOwnershipVersion: number | undefined
        let invalidClaudeRecoveryOwnership = false
        if (table === "activation_recovery") {
          try { recoveryPayload = JSON.parse(String(row.payload_json)) } catch {
            throw new Error("claude-sqlite-recovery-json-invalid")
          }
          if (row.target === "claude") {
            recoveryOwnershipVersion = claudeRecoveryOwnershipVersion(recoveryPayload)
            invalidClaudeRecoveryOwnership = recoveryOwnershipVersion === undefined
          }
        }
        for (const { target, secret } of secrets) {
          for (const [column, value] of Object.entries(row)) {
            if (typeof value !== "string" || !value.includes(secret)) continue
            const exactApprovedValue = (
              table === "credentials" && column === "bearer_token"
              && policy.providerSecrets.some((entry) => entry.target === row.target && entry.secret === value
                && providers.some((provider) => provider.credential_id === row.id && provider.name === entry.name))
              || table === "activated_snapshots" && column === "provider_bearer_token"
                && policy.providerSecrets.some((entry) => entry.target === row.target && entry.secret === value
                  && providers.some((provider) => provider.id === row.provider_id && provider.name === entry.name))
            )
              || table === "target_route_state" && column === "routing_credential"
                && policy.routingSecrets.some((entry) => entry.target === row.target && entry.secret === value)
            if (exactApprovedValue) continue
            if (
              table === "activation_recovery"
              && column === "payload_json"
              && row.target === target
            ) {
              const occurrences = jsonSecretOccurrences(recoveryPayload, secret)
              if (
                occurrences.length > 0
                && occurrences.every((entry) => approvedRecoveryOccurrence(
                  target,
                  entry,
                  recoveryOwnershipVersion,
                ))
              ) continue
            }
            if (
              table === "reconciliation_intents"
              && (column === "before_json" || column === "desired_json")
            ) {
              auditReconciliationRecoveryRows([{
                target: String(row.target),
                beforeJson: String(row.before_json),
                desiredJson: String(row.desired_json),
              }], [{ target, secret }])
              continue
            }
            throw new Error("secret-scan-failed:claude-sqlite-secret-location")
          }
        }
        if (invalidClaudeRecoveryOwnership) {
          throw new Error("claude-sqlite-recovery-ownership-version-invalid")
        }
      }
    }
  } finally {
    database.close()
  }
}

async function readManagedConfigurationFingerprint(path: string): Promise<string> {
  try {
    return sensitiveDigest(await readFile(path))!
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return "missing"
    throw error
  }
}

async function readOnlyStateFingerprint(databasePath: string, configPath: string): Promise<unknown> {
  return {
    database: readDatabaseProjection(databasePath),
    managedConfiguration: await readManagedConfigurationFingerprint(configPath),
  }
}

function readTargetDatabaseFingerprint(databasePath: string, target: "codex" | "claude"): string {
  const database = new Database(databasePath, { readonly: true })
  try {
    const state = [
      database.query("SELECT * FROM target_route_state WHERE target = ?").get(target),
      database.query("SELECT * FROM target_problems WHERE target = ? ORDER BY code").all(target),
      database.query("SELECT * FROM providers WHERE target = ? ORDER BY position").all(target),
      database.query("SELECT * FROM credentials WHERE target = ? ORDER BY id").all(target),
      database.query("SELECT * FROM activated_snapshots WHERE target = ? ORDER BY id").all(target),
      database.query("SELECT * FROM action_receipts WHERE target = ? ORDER BY committed_revision, action_id").all(target),
      database.query("SELECT * FROM activation_recovery WHERE target = ? ORDER BY created_revision, id").all(target),
    ]
    return sensitiveDigest(JSON.stringify(state))!
  } finally {
    database.close()
  }
}

function readStableTargetDatabaseFingerprint(
  databasePath: string,
  target: "codex" | "claude",
  includeViewSequence = true,
): string {
  const database = new Database(databasePath, { readonly: true })
  try {
    database.exec("PRAGMA busy_timeout = 5000")
    const route = database.query(`SELECT target, management_revision, current_provider_id, serving_provider_id,
      view_sequence, takeover_state, route_port, routing_credential, activated_snapshot_id, managed_config_path,
      recovery_state FROM target_route_state WHERE target = ?`).get(target) as Record<string, unknown> | null
    const stableRoute = includeViewSequence || !route
      ? route
      : (({ view_sequence: _viewSequence, ...rest }) => rest)(route)
    const state = [
      stableRoute,
      database.query("SELECT * FROM target_problems WHERE target = ? ORDER BY code").all(target),
      database.query("SELECT * FROM providers WHERE target = ? ORDER BY position").all(target),
      database.query("SELECT * FROM credentials WHERE target = ? ORDER BY id").all(target),
      database.query("SELECT * FROM activated_snapshots WHERE target = ? ORDER BY id").all(target),
      database.query("SELECT * FROM action_receipts WHERE target = ? ORDER BY committed_revision, action_id").all(target),
      database.query("SELECT * FROM activation_recovery WHERE target = ? ORDER BY created_revision, id").all(target),
      database.query("SELECT * FROM reconciliation_intents WHERE target = ? ORDER BY created_revision, action_id").all(target),
      database.query("SELECT * FROM target_compatibility WHERE target = ?").all(target),
    ]
    return sensitiveDigest(JSON.stringify(state))!
  } finally {
    database.close()
  }
}

function stableTargetViewFingerprint(view: Readonly<TargetView>, includeViewSequence = true): string {
  const { service, ...stable } = view
  const { epoch: _epoch, ...stableService } = service
  const projected = includeViewSequence
    ? stable
    : (({ viewSequence: _viewSequence, ...rest }) => rest)(stable)
  return sensitiveDigest(canonicalJson({ ...projected, service: stableService }))!
}

function credentialReferences(databasePath: string): Array<{ name: string; credentialId: string | null }> {
  const database = new Database(databasePath, { readonly: true })
  try {
    return database.query(`SELECT name, credential_id AS credentialId
      FROM providers ORDER BY position`).all() as Array<{ name: string; credentialId: string | null }>
  } finally {
    database.close()
  }
}

function targetHistory(databasePath: string, target: HeldReconciliationTarget): {
  providers: Array<Record<string, unknown>>
  snapshots: Array<Record<string, unknown>>
  credentials: Array<Record<string, unknown>>
} {
  const database = new Database(databasePath, { readonly: true })
  try {
    const snapshots = database.query("SELECT * FROM activated_snapshots WHERE target = ? ORDER BY id")
      .all(target) as Array<Record<string, unknown>>
    const credentials = database.query("SELECT * FROM credentials WHERE target = ? ORDER BY id")
      .all(target) as Array<Record<string, unknown>>
    return {
      providers: database.query("SELECT * FROM providers WHERE target = ? ORDER BY position")
        .all(target) as Array<Record<string, unknown>>,
      snapshots: snapshots.map(({ provider_bearer_token, ...snapshot }) => ({
        ...snapshot,
        provider_bearer_token_digest: sensitiveDigest(provider_bearer_token),
      })),
      credentials: credentials.map(({ bearer_token, ...credential }) => ({
        ...credential,
        bearer_token_digest: sensitiveDigest(bearer_token),
      })),
    }
  } finally {
    database.close()
  }
}

function assertAdoptedImmutableHistory(
  before: ReturnType<typeof targetHistory>,
  after: ReturnType<typeof targetHistory>,
  currentProviderId: string,
  activatedSnapshotId: string,
): void {
  expect(after.providers).toHaveLength(before.providers.length + 1)
  expect(after.credentials).toHaveLength(before.credentials.length + 1)
  expect(after.snapshots).toHaveLength(before.snapshots.length + 1)
  for (const [rowsBefore, rowsAfter] of [
    [before.providers, after.providers],
    [before.credentials, after.credentials],
    [before.snapshots, after.snapshots],
  ] as const) {
    for (const row of rowsBefore) {
      expect(rowsAfter.find(({ id }) => id === row.id)).toEqual(row)
    }
  }
  const adoptedProvider = after.providers.find(({ id }) => id === currentProviderId)!
  const credentialId = adoptedProvider.credential_id
  expect(typeof credentialId).toBe("string")
  expect(before.providers.some(({ id }) => id === currentProviderId)).toBeFalse()
  expect(before.credentials.some(({ id }) => id === credentialId)).toBeFalse()
  expect(after.credentials.some(({ id }) => id === credentialId)).toBeTrue()
  expect(before.snapshots.some(({ id }) => id === activatedSnapshotId)).toBeFalse()
  expect(after.snapshots.find(({ id }) => id === activatedSnapshotId)?.provider_id).toBe(currentProviderId)
}

async function expectSecretSafeControlCode(
  operation: Promise<unknown>,
  expectedCode: string,
  secrets: readonly string[],
  label: string,
): Promise<void> {
  try {
    await operation
  } catch (error) {
    const scanLabel = `${label}-error`
    const scanFailure = `secret-scan-failed:${scanLabel}`
    try {
      scanNoSecrets([asyncErrorSurface(error)], secrets, scanLabel)
      const code = typeof error === "object" && error !== null && "code" in error
        ? String(error.code)
        : ""
      if (code !== expectedCode) throw new Error("wrong-control-code")
      return
    } catch (auditError) {
      throw fixedSurfaceError(auditError, scanFailure, `control-error-mismatch:${label}`)
    }
  }
  throw new Error(`control-error-missing:${label}`)
}

async function writeMutableProbe(
  executable: string,
  versionPath: string,
  target: HeldReconciliationTarget,
): Promise<void> {
  const versionPrefix = target === "codex" ? "codex-cli " : ""
  const versionSuffix = target === "codex" ? "" : " (Claude Code)"
  const help = target === "codex"
    ? "Usage: codex [OPTIONS]\\n--config <key=value>"
    : "Usage: claude [OPTIONS]\\n--settings <file>\\n--model <model>"
  await writeFile(executable, `#!/bin/sh\nset -eu\ncase "\${1-}" in\n  --version) printf '${versionPrefix}%s${versionSuffix}\\n' "$(sed -n '1p' '${versionPath}')" ;;\n  --help) printf '%b\\n' '${help}' ;;\n  *) exit 64 ;;\nesac\n`, { mode: 0o700 })
  await chmod(executable, 0o700)
}

function captureFrame(
  setup: Awaited<ReturnType<typeof testRender>>,
  renderedFrames: string[],
): string {
  const frame = setup.captureCharFrame()
  renderedFrames.push(frame)
  return frame
}

function splitEncodedFrameWithin(frame: Uint8Array, sentinel: string): Buffer[] {
  const bytes = Buffer.from(frame)
  const sentinelStart = bytes.indexOf(Buffer.from(sentinel))
  expect(sentinelStart).toBeGreaterThanOrEqual(4)
  const withinSentinel = sentinelStart + Math.floor(Buffer.byteLength(sentinel) / 2)
  return [
    bytes.subarray(0, 2),
    bytes.subarray(2, 4),
    bytes.subarray(4, withinSentinel),
    bytes.subarray(withinSentinel),
  ]
}

test("secret scanning catches additive RPC sentinels split across valid frame chunks", () => {
  const cleanFrame = encodeFrame({ type: "target-view", additiveDiagnostic: "safe" })
  expect(() => scanRawRpcFramesNoSecrets([splitEncodedFrameWithin(cleanFrame, "safe")])).not.toThrow()

  const providerFrame = encodeFrame({ type: "target-view", additiveDiagnostic: providerSecret })
  expect(() => scanRawRpcFramesNoSecrets(
    [splitEncodedFrameWithin(providerFrame, providerSecret)],
  )).toThrow()

  const routingFrame = encodeFrame({ type: "response", additiveDiagnostic: wrongRoutingSecret })
  expect(() => scanRawRpcFramesNoSecrets(
    [splitEncodedFrameWithin(routingFrame, wrongRoutingSecret)],
  )).toThrow()
})

test("Direct configuration audit rejects every unrelated TOML mutation with fixed diagnostics", () => {
  const credential = "controlled-direct-provider-credential"
  const expected = [
    "# operator comment survives",
    'unrelated = "keep-me"',
    'model = "controlled-direct-model"',
    'model_provider = "muxvia_codex"',
    "",
    "[operator_settings]",
    'theme = "dark"',
    "nested = { enabled = true, count = 2 }",
    "",
    "[model_providers]",
    "",
    "[model_providers.muxvia_codex]",
    'name = "Muxvia Direct"',
    'base_url = "https://controlled.invalid/v1"',
    'wire_api = "responses"',
    `http_headers = { Authorization = "Bearer ${credential}" }`,
    "supports_websockets = false",
    "",
  ].join("\n")
  const mutations = [
    expected.replace("# operator comment survives", "# operator comment changed"),
    expected.replace('unrelated = "keep-me"', 'unrelated = "changed"'),
    expected.replace('unrelated = "keep-me"\n', ""),
    expected.replace('unrelated = "keep-me"\n', 'unrelated = "keep-me"\nadded = true\n'),
    expected.replace('theme = "dark"', 'theme = "light"'),
    expected.replace("nested = { enabled = true, count = 2 }\n", ""),
    expected.replace("[operator_settings]\n", "[operator_settings]\nadded = true\n"),
    `${expected}[unexpected]\nvalue = true\n`,
  ]

  for (const mutation of mutations) {
    let diagnostic = ""
    try {
      assertExactDirectManagedConfig(mutation, expected)
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic === "direct-managed-configuration-mismatch").toBeTrue()
    expect(diagnostic.includes(credential)).toBeFalse()
    expect(diagnostic.includes(mutation)).toBeFalse()
  }
})

test("Direct configuration audit rejects a changed file mode with fixed diagnostics", () => {
  let diagnostic = ""
  try {
    assertExactFileMode(0o600, 0o640)
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic === "direct-managed-configuration-mode-mismatch").toBeTrue()
})

test("TCP listener observation detects a known current-process listener", async () => {
  const server = createTcpServer()
  await new Promise<void>((resolveListen, reject) => {
    server.once("error", reject)
    server.listen(0, "127.0.0.1", resolveListen)
  })
  try {
    const address = server.address()
    if (!address || typeof address === "string") throw new Error("tcp-listener-fixture-address-missing")
    const ports = await observeTcpListenerPorts(process.pid)
    expect(ports.includes(address.port)).toBeTrue()
  } finally {
    await new Promise<void>((resolveClose) => server.close(() => resolveClose()))
  }
})

test("renderer wait diagnostics never retain a secret current frame", async () => {
  const setup = await testRender(() => <text>{providerSecret}</text>, { width: 60, height: 4, useThread: false })
  try {
    await setup.renderOnce()
    let diagnostic = ""
    try {
      await waitForSecretSafeFrame(setup, () => false, [providerSecret], "controlled-renderer-wait")
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic === "secret-scan-failed:controlled-renderer-wait-frame").toBeTrue()
    expect(diagnostic.includes(providerSecret)).toBeFalse()
  } finally {
    setup.renderer.destroy()
  }
})

test("text polling scans every observed frame before semantic predicates", async () => {
  const secret = "controlled-transient-terminal-secret"
  const screens = [secret, "Request History Estimated Unpriced"]
  let index = 0
  let diagnostic = ""
  try {
    await waitForSecretSafeText(
      () => screens[Math.min(index++, screens.length - 1)]!,
      (screen) => screen.includes("Request History"),
      [secret],
      "controlled-terminal-wait",
    )
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("secret-scan-failed:controlled-terminal-wait-text")
  expect(diagnostic.includes(secret)).toBeFalse()
})

test("structured assertion diagnostics never retain additive view secrets", () => {
  const controlledViews = [
    { mode: "direct", currentProviderId: "actual-provider", additiveDiagnostic: providerSecret },
    { mode: "direct", currentProviderId: providerSecret, additiveDiagnostic: "safe" },
  ]
  for (const controlledView of controlledViews) {
    let diagnostic = ""
    try {
      assertSecretSafeStructured(controlledView, [providerSecret], "controlled-target-view", (safeView) => {
        expect(safeView).toMatchObject({ currentProviderId: "expected-provider" })
      })
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic === "secret-scan-failed:controlled-target-view-structured").toBeTrue()
    expect(diagnostic.includes(providerSecret)).toBeFalse()
    expect(diagnostic.includes(JSON.stringify(controlledView))).toBeFalse()
  }
})

test("action error diagnostics never retain a secret error or authoritative view", async () => {
  const controlledError = new Error(`controlled action failed: ${providerSecret}`) as Error & {
    authoritativeView?: unknown
  }
  controlledError.authoritativeView = { currentProviderId: providerSecret }
  const controlledSession = {
    act: async () => { throw controlledError },
  } as unknown as TargetSession
  let diagnostic = ""
  try {
    await actSecretSafe(controlledSession, {
      kind: "activate-provider",
      providerId: "controlled-provider",
      mode: "direct",
    }, [providerSecret], "controlled-act")
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic === "secret-scan-failed:controlled-act-error").toBeTrue()
  expect(diagnostic.includes(providerSecret)).toBeFalse()
  expect(diagnostic.includes(controlledError.message)).toBeFalse()
})

test("Claude Direct audits reject secrets across every public diagnostic surface", async () => {
  const secret = "controlled-claude-direct-public-surface-secret"
  const surfaces = [
    { label: "claude-direct-view", value: { target: "claude", additiveDiagnostic: secret } },
    { label: "claude-direct-receipt", value: { actionKind: "activate-provider", outcomeJson: secret } },
    { label: "claude-direct-activity", value: { kind: "success", message: secret } },
    { label: "claude-direct-renderer-frame", value: `frame:${secret}` },
  ]
  for (const { label, value } of surfaces) {
    let diagnostic = ""
    try { scanNoSecrets([value], [secret], label) } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe(`secret-scan-failed:${label}`)
    expect(diagnostic.includes(secret)).toBeFalse()
  }

  let processDiagnostic = ""
  try {
    scanProcessOutputNoSecrets([[Buffer.from(`stdout:${secret}`)], []], [secret])
  } catch (error) {
    processDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(processDiagnostic).toBe("secret-scan-failed:process-output-stream-0")
  expect(processDiagnostic.includes(secret)).toBeFalse()

  const controlledError = new Error(`failed:${secret}`) as Error & { authoritativeView?: unknown }
  controlledError.authoritativeView = { target: "claude", additiveDiagnostic: secret }
  const controlledSession = {
    act: async () => { throw controlledError },
  } as unknown as TargetSession
  let actionDiagnostic = ""
  try {
    await actSecretSafe(controlledSession, {
      kind: "activate-provider",
      providerId: "controlled-provider",
      mode: "direct",
    }, [secret], "claude-direct-controlled-action")
  } catch (error) {
    actionDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(actionDiagnostic).toBe("secret-scan-failed:claude-direct-controlled-action-error")
  expect(actionDiagnostic.includes(secret)).toBeFalse()
})

test("Claude Direct settings audit detects dual credentials and unrelated semantic or mode mutation", () => {
  const bearer = "controlled-claude-direct-bearer"
  const apiKey = "controlled-claude-direct-api-key"
  const expected: ClaudeDirectSettingsExpectation = {
    authentication: "anthropic-bearer",
    baseUrl: "https://direct-claude.invalid/v1",
    credential: bearer,
    model: "claude-direct",
  }
  const clean = JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] },
    env: {
      OPERATOR_UNRELATED: "keep-me",
      ANTHROPIC_BASE_URL: expected.baseUrl,
      ANTHROPIC_MODEL: expected.model,
      ANTHROPIC_AUTH_TOKEN: bearer,
    },
  })
  expect(() => assertExactClaudeDirectSettings(clean, expected, [apiKey])).not.toThrow()

  const mutations = [
    JSON.stringify({
      operator: { theme: "dark", hooks: ["keep"] },
      env: {
        OPERATOR_UNRELATED: "keep-me",
        ANTHROPIC_BASE_URL: expected.baseUrl,
        ANTHROPIC_MODEL: expected.model,
        ANTHROPIC_AUTH_TOKEN: bearer,
        ANTHROPIC_API_KEY: apiKey,
      },
    }),
    clean.replace('"theme":"dark"', '"theme":"light"'),
    clean.replace('"OPERATOR_UNRELATED":"keep-me"', '"OPERATOR_UNRELATED":"changed"'),
  ]
  const expectedDiagnostics = [
    "secret-scan-failed:claude-direct-settings-raw",
    "claude-direct-settings-mismatch",
    "claude-direct-settings-mismatch",
  ]
  mutations.forEach((mutation, index) => {
    let diagnostic = ""
    try { assertExactClaudeDirectSettings(mutation, expected, [apiKey]) } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe(expectedDiagnostics[index]!)
    expect(diagnostic.includes(bearer)).toBeFalse()
    expect(diagnostic.includes(apiKey)).toBeFalse()
    expect(diagnostic.includes(mutation)).toBeFalse()
  })

  let modeDiagnostic = ""
  try { assertExactFileMode(0o600, 0o640) } catch (error) {
    modeDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(modeDiagnostic).toBe("direct-managed-configuration-mode-mismatch")
})

test("Claude Direct final settings audit survives an earlier failure at partial progress", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-direct-final-settings-"))
  roots.push(root)
  const settingsPath = join(root, "settings.json")
  const bearer = "controlled-final-settings-bearer"
  const apiKey = "controlled-final-settings-api-key"
  const expected: ClaudeDirectSettingsExpectation = {
    authentication: "anthropic-bearer",
    baseUrl: "https://controlled-final-settings.invalid/v1",
    credential: bearer,
    model: "controlled-final-settings-model",
  }
  const clean = JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] },
    env: {
      OPERATOR_UNRELATED: "keep-me",
      ANTHROPIC_BASE_URL: expected.baseUrl,
      ANTHROPIC_MODEL: expected.model,
      ANTHROPIC_AUTH_TOKEN: bearer,
    },
  })
  const mutations = [
    {
      text: clean.replace('"theme":"dark"', `"theme":"dark","leak":"${apiKey}"`),
      mode: 0o640,
    },
    { text: clean, mode: 0o600 },
    {
      text: clean.replace('"OPERATOR_UNRELATED":"keep-me"', '"OPERATOR_UNRELATED":"changed"'),
      mode: 0o640,
    },
  ]

  await writeFile(settingsPath, clean, { mode: 0o640 })
  await chmod(settingsPath, 0o640)
  await expect(auditClaudeDirectSettingsFile(settingsPath, {
    kind: "direct",
    expected,
  }, [bearer, apiKey], 0o640)).resolves.toBeUndefined()

  for (const mutation of mutations) {
    await writeFile(settingsPath, mutation.text, { mode: mutation.mode })
    await chmod(settingsPath, mutation.mode)
    let diagnostic = ""
    try {
      try {
        throw new Error("controlled-earlier-functional-failure")
      } finally {
        await runClaudeSecurityFinalizer([{
          name: "settings",
          run: () => auditClaudeDirectSettingsFile(settingsPath, {
            kind: "direct",
            expected,
          }, [bearer, apiKey], 0o640),
        }])
      }
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe("claude-security-finalizer-failed:settings")
    expect(diagnostic.includes(bearer)).toBeFalse()
    expect(diagnostic.includes(apiKey)).toBeFalse()
    expect(diagnostic.includes(mutation.text)).toBeFalse()
  }
})

test("inbound action barrier rejects a wrong error with fixed diagnostics", async () => {
  const wrongError = {
    type: "error",
    requestId: "controlled-request",
    problem: { code: "stale-revision", message: providerSecret },
  }
  let diagnostic = ""
  try {
    await waitForInboundResult(
      [wrongError],
      0,
      { errorCode: "provider-referenced" },
      "controlled-active-provider-delete",
      async (predicate) => {
        if (!(await predicate())) throw new Error(JSON.stringify(wrongError))
      },
    )
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic === "inbound-result-wait-failed:controlled-active-provider-delete").toBeTrue()
  expect(diagnostic.includes(providerSecret)).toBeFalse()
  expect(diagnostic.includes(JSON.stringify(wrongError))).toBeFalse()
})

test("Direct tracer fixture enforces file modes under a restrictive umask", async () => {
  const child = spawn(process.execPath, [
    "test",
    "./packages/control-plane/test/walking-skeleton.e2e.tsx",
    "--test-name-pattern",
    "real processes prove Codex direct activation is control-only and survives restart",
  ], {
    cwd: repoRoot,
    env: { ...process.env, MUXVIA_DIRECT_RESTRICTIVE_UMASK_CHILD: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  })
  const output = captureProcessOutput(child)
  const completed = await Promise.race([
    output.completed,
    Bun.sleep(deadlineMs).then(() => undefined),
  ])
  if (!completed) {
    child.kill("SIGKILL")
    await output.completed.catch(() => {})
    throw new Error("restrictive-umask-direct-tracer-timeout")
  }
  scanProcessOutputNoSecrets(output.streams, [providerSecret, wrongRoutingSecret, authSentinel])
  if (completed.code !== 0 || completed.signal !== null) {
    throw new Error("restrictive-umask-direct-tracer-failed")
  }
})

test("outbound operation audit catches a preset-phase discovery without retaining its payload", () => {
  const credential = "controlled-outbound-credential"
  const baseUrl = "https://controlled-outbound.invalid/v1"
  const audit = createOutboundOperationAudit()
  audit.observe(encodeFrame({
    type: "request",
    requestId: "controlled-discovery",
    operation: {
      kind: "discover-models",
      target: "codex",
      source: {
        kind: "draft",
        baseUrl,
        authentication: "anthropic-api-key",
        credentialSource: { kind: "ephemeral", value: credential },
      },
    },
  }))
  audit.finish()

  expect(audit.operationKinds.join("|") === "discover-models").toBeTrue()
  const diagnostic = JSON.stringify(audit.operationKinds)
  expect(diagnostic.includes(credential)).toBeFalse()
  expect(diagnostic.includes(baseUrl)).toBeFalse()
})

test("renderer audit catches a secret on a native frame without a manual capture", async () => {
  const setup = await testRender(() => <text>{providerSecret}</text>, { width: 40, height: 4, useThread: false })
  const audit = createRendererAudit(setup)
  try {
    audit.start()
    await setup.renderOnce()
    let caught = false
    try {
      scanNoSecrets(audit.frames(), [providerSecret])
    } catch {
      caught = true
    }
    expect(caught).toBeTrue()
  } finally {
    audit.stop()
    setup.renderer.destroy()
  }
})

test("read-only fingerprints change for every sensitive database payload without retaining values", async () => {
  const root = await mkdtemp(join(tmpdir(), "muxvia-fingerprint-"))
  roots.push(root)
  const databasePath = join(root, "state.db")
  const configPath = join(root, "config.toml")
  const database = new Database(databasePath)
  try {
    database.exec(await readFile(resolve(repoRoot, "crates/routing-service/src/state/schema.sql"), "utf8"))
    database.exec(`
      INSERT INTO credentials (id, target, bearer_token)
        VALUES ('credential-id', 'codex', 'credential-sensitive-one');
      INSERT INTO activated_snapshots
        (id, target, provider_id, base_url, model, protocol, authentication, provider_bearer_token, epoch)
        VALUES ('snapshot-id', 'codex', 'provider-id', 'https://snapshot.invalid/v1', 'model',
          'openai-responses', 'openai-bearer', 'snapshot-sensitive-one', 'epoch');
      UPDATE target_route_state SET routing_credential = 'routing-sensitive-one' WHERE target = 'codex';
      INSERT INTO activation_recovery
        (id, target, action_id, config_path, file_identity_json, payload_json, state, created_revision)
        VALUES ('recovery-id', 'codex', 'action-id', '/controlled/config',
          'identity-sensitive-one', 'payload-sensitive-one', 'pending', 1);
    `)
    await writeFile(configPath, "managed-sensitive-one")

    const mutations = [
      "UPDATE credentials SET bearer_token = 'credential-sensitive-two'",
      "UPDATE target_route_state SET routing_credential = 'routing-sensitive-two' WHERE target = 'codex'",
      "UPDATE activated_snapshots SET provider_bearer_token = 'snapshot-sensitive-two'",
      "UPDATE activation_recovery SET file_identity_json = 'identity-sensitive-two'",
      "UPDATE activation_recovery SET payload_json = 'payload-sensitive-two'",
    ]
    for (const mutation of mutations) {
      const before = JSON.stringify(readDatabaseProjection(databasePath))
      database.exec(mutation)
      const after = JSON.stringify(readDatabaseProjection(databasePath))
      expect(after === before).toBeFalse()
      expect(after.includes("sensitive-")).toBeFalse()
    }

    const beforeConfiguration = JSON.stringify(await readOnlyStateFingerprint(databasePath, configPath))
    await writeFile(configPath, "managed-sensitive-two")
    const afterConfiguration = JSON.stringify(await readOnlyStateFingerprint(databasePath, configPath))
    expect(afterConfiguration === beforeConfiguration).toBeFalse()
    expect(afterConfiguration.includes("managed-sensitive-")).toBeFalse()
  } finally {
    database.close()
  }
})

test("process-output audit catches cross-chunk and final-tail secrets without inserting separators", () => {
  const secret = "controlled-process-output-secret"
  const cases: readonly (readonly Buffer[])[][] = [
    [
      [Buffer.from("prefix-controlled-process-"), Buffer.from("output-secret")],
      [Buffer.from("clean-stderr")],
    ],
    [
      [Buffer.from("clean-stdout")],
      [Buffer.from("clean-prefix"), Buffer.from(secret)],
    ],
  ]
  for (const streams of cases) {
    let caught = false
    try {
      scanProcessOutputNoSecrets(streams, [secret])
    } catch {
      caught = true
    }
    expect(caught).toBeTrue()
  }
})

test("secret-scan failures expose only a fixed redacted diagnostic", () => {
  const secret = "controlled-redaction-secret"
  const surface = `controlled raw surface ${secret}`
  let diagnostic = ""
  try {
    scanNoSecrets([surface], [secret], "controlled-surface")
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic === "secret-scan-failed:controlled-surface").toBeTrue()
  expect(diagnostic.includes(secret)).toBeFalse()
  expect(diagnostic.includes(surface)).toBeFalse()
})

test("CapturedRequest matcher diagnostics never retain raw request surfaces", () => {
  const expectedProviderCredential = "controlled-expected-provider-credential"
  const surfaceSentinel = "controlled-raw-request-surface"
  const request: CapturedRequest = {
    authorization: `Bearer ${expectedProviderCredential}`,
    headers: {
      authorization: `Bearer ${expectedProviderCredential}`,
      "x-controlled": surfaceSentinel,
    },
    contentType: "application/json",
    method: "POST",
    testHeader: "preserved",
    body: `{"surface":"${surfaceSentinel}"}`,
    path: `/v1/responses?surface=${surfaceSentinel}`,
  }
  const projection = auditCapturedRequestAndProject(request, {
    providerCredential: expectedProviderCredential,
    forbiddenRoutingCredentials: [wrongRoutingSecret],
    authorization: "expected-provider",
  })
  let diagnostic = ""
  try {
    expect(projection).toMatchObject({ method: "GET" })
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }

  expect(diagnostic.length > 0).toBeTrue()
  expect(diagnostic.includes(expectedProviderCredential)).toBeFalse()
  expect(diagnostic.includes(surfaceSentinel)).toBeFalse()
  expect(diagnostic.includes(request.body)).toBeFalse()
  expect(diagnostic.includes(request.path)).toBeFalse()
  expect(diagnostic.includes(JSON.stringify(request.headers))).toBeFalse()
  expect(projection).toMatchObject({
    method: "POST",
    pathClass: "responses",
    hasQuery: true,
    authorizationClass: "expected-provider",
    headerAuthorizationClass: "expected-provider",
    hasAuthorizationHeader: true,
    hasContentTypeHeader: false,
    hasTestHeader: false,
    contentTypeClass: "application-json",
    testHeaderClass: "preserved",
    bodyClass: "other",
  })
  expect(projection.bodyDigest.includes(expectedProviderCredential)).toBeFalse()
  expect(projection.bodyDigest.includes(surfaceSentinel)).toBeFalse()
})

test("CapturedRequest secret scanning precedes safe semantic projection", () => {
  const expectedProviderCredential = "controlled-expected-provider"
  const request: CapturedRequest = {
    authorization: `Bearer ${expectedProviderCredential}`,
    headers: {
      authorization: `Bearer ${expectedProviderCredential}`,
      "x-forbidden-routing": wrongRoutingSecret,
    },
    contentType: "application/json",
    method: "POST",
    testHeader: "preserved",
    body: `{"routing":"${wrongRoutingSecret}"}`,
    path: "/v1/responses",
  }
  let diagnostic = ""
  try {
    const projection = auditCapturedRequestAndProject(request, {
      providerCredential: expectedProviderCredential,
      forbiddenRoutingCredentials: [wrongRoutingSecret],
      authorization: "expected-provider",
    })
    expect(projection).toMatchObject({ method: "GET" })
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }

  expect(diagnostic === "secret-scan-failed:captured-request-routing-credential").toBeTrue()
  expect(diagnostic.includes(wrongRoutingSecret)).toBeFalse()
  expect(diagnostic.includes(request.body)).toBeFalse()
})

test("Claude request audit rejects misplaced credentials with fixed diagnostics", () => {
  const apiKey = "controlled-claude-api-key"
  const bearer = "controlled-claude-bearer"
  const routing = "c".repeat(64)
  const mutations: CapturedRequest[] = [
    {
      authorization: null,
      apiKey,
      headers: { "x-api-key": apiKey, "x-misplaced": bearer },
      contentType: "application/json",
      method: "POST",
      testHeader: null,
      body: '{"messages":[]}',
      path: "/v1/messages",
    },
    {
      authorization: `Bearer ${bearer}`,
      apiKey: null,
      headers: { authorization: `Bearer ${bearer}` },
      contentType: "application/json",
      method: "POST",
      testHeader: null,
      body: `{"messages":[],"misplaced":"${routing}"}`,
      path: "/v1/messages",
    },
  ]
  const expected = [
    "secret-scan-failed:claude-captured-provider-credential",
    "secret-scan-failed:claude-captured-routing-credential",
  ]
  mutations.forEach((request, index) => {
    let diagnostic = ""
    try {
      auditClaudeRequestAndProject(request, {
        apiKeyCredential: apiKey,
        bearerCredential: bearer,
        forbiddenRoutingCredentials: [routing],
      })
    } catch (error) {
      diagnostic = fixedClaudeRequestAuditDiagnostic(error)
    }
    expect(diagnostic === expected[index]).toBeTrue()
    expect(diagnostic.includes(apiKey)).toBeFalse()
    expect(diagnostic.includes(bearer)).toBeFalse()
    expect(diagnostic.includes(routing)).toBeFalse()
    expect(diagnostic.includes(request.body)).toBeFalse()
  })

  const settingsMutation = JSON.stringify({
    operator: { misplaced: routing },
    env: { ANTHROPIC_AUTH_TOKEN: routing },
  })
  let settingsDiagnostic = ""
  try {
    auditClaudeSettingsSecrets(settingsMutation, routing, [])
  } catch (error) {
    settingsDiagnostic = error instanceof Error ? error.message : ""
  }
  expect(settingsDiagnostic === "secret-scan-failed:claude-settings-routing-secret").toBeTrue()
  expect(settingsDiagnostic.includes(routing)).toBeFalse()
  expect(settingsDiagnostic.includes(settingsMutation)).toBeFalse()

  const earlyFailureMutation = JSON.stringify({
    env: { ANTHROPIC_MODEL: "wrong", MISPLACED_PROVIDER: apiKey },
  })
  let earlyDiagnostic = ""
  try {
    extractClaudeManagedSettings(earlyFailureMutation, "expected", [apiKey])
  } catch (error) {
    earlyDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(earlyDiagnostic).toBe("secret-scan-failed:claude-settings-raw")
  expect(earlyDiagnostic.includes(apiKey)).toBeFalse()
  expect(earlyDiagnostic.includes(earlyFailureMutation)).toBeFalse()

  const generated = "a".repeat(64)
  const duplicatedGeneratedMutation = JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"], duplicate: generated },
    env: {
      ANTHROPIC_AUTH_TOKEN: generated,
      ANTHROPIC_BASE_URL: "http://127.0.0.1:43124",
      ANTHROPIC_MODEL: "wrong-model",
      OPERATOR_UNRELATED: "keep-me",
    },
  })
  let duplicatedDiagnostic = ""
  try {
    extractClaudeManagedSettings(duplicatedGeneratedMutation, "expected-model")
  } catch (error) {
    duplicatedDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(duplicatedDiagnostic).toBe("secret-scan-failed:claude-settings-routing-secret")
  expect(duplicatedDiagnostic.includes(generated)).toBeFalse()
  expect(duplicatedDiagnostic.includes(duplicatedGeneratedMutation)).toBeFalse()
})

test("Claude environment trap audit rejects traps stripped before child spawn", () => {
  const requested = {
    HOME: "/controlled/trap-home",
    CODEX_HOME: "/controlled/trap-codex",
    CLAUDE_CONFIG_DIR: "/controlled/trap-claude",
  }
  const stripped = { HOME: "/controlled/target-home" }
  let diagnostic = ""
  try {
    assertChildVisibleEnvironment(requested, stripped)
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("claude-environment-traps-not-child-visible")
  expect(diagnostic.includes(requested.HOME)).toBeFalse()
})

test("Claude Direct environment trap fingerprint rejects mutation", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-direct-trap-"))
  roots.push(root)
  const canary = join(root, "canary")
  const secret = "controlled-claude-direct-trap-secret"
  await writeFile(canary, "unchanged\n", { mode: 0o600 })
  const fingerprint = await controlledTreeFingerprint(root)
  await expect(assertControlledTreeUnchanged(
    root,
    fingerprint,
    "claude-direct-environment-trap-mutated",
  )).resolves.toBeUndefined()

  await writeFile(canary, secret, { mode: 0o600 })
  let diagnostic = ""
  try {
    await assertControlledTreeUnchanged(
      root,
      fingerprint,
      "claude-direct-environment-trap-mutated",
    )
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("claude-direct-environment-trap-mutated")
  expect(diagnostic.includes(secret)).toBeFalse()
})

test("real service keeps child-visible environment traps outside the explicit target home", async () => {
  const root = await mkdtemp(join(tmpdir(), "ce-"))
  roots.push(root)
  const targetHome = join(root, "h")
  const muxviaHome = join(targetHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const shutdown = join(root, "shutdown")
  const requested = {
    HOME: join(root, "th"),
    CODEX_HOME: join(root, "tc"),
    CLAUDE_CONFIG_DIR: join(root, "tl"),
  }
  await mkdir(targetHome, { recursive: true, mode: 0o700 })
  await mkdir(requested.HOME, { recursive: true, mode: 0o700 })
  await mkdir(requested.CODEX_HOME, { recursive: true, mode: 0o700 })
  await mkdir(requested.CLAUDE_CONFIG_DIR, { recursive: true, mode: 0o700 })
  await writeFile(join(requested.HOME, "canary"), "home-canary\n", { mode: 0o600 })
  await writeFile(join(requested.CODEX_HOME, "canary"), "codex-canary\n", { mode: 0o600 })
  await writeFile(join(requested.CLAUDE_CONFIG_DIR, "canary"), "claude-canary\n", { mode: 0o600 })
  const fingerprints = await Promise.all(Object.values(requested).map(controlledTreeFingerprint))
  const environment: NodeJS.ProcessEnv = {
    ...requested,
    PATH: `${dirname(fakeClaude)}:/usr/bin:/bin`,
    MUXVIA_INTEGRATION_TEST: "1",
  }
  assertChildVisibleEnvironment(requested, environment)
  const child = spawn(serviceBinary, [
    "--home", muxviaHome,
    "--test-shutdown-file", shutdown,
    "--test-codex-executable", fakeCodex,
    "--test-claude-executable", fakeClaude,
  ], { cwd: root, env: environment, stdio: ["ignore", "pipe", "pipe"] })
  const output = captureProcessOutput(child)
  let client: RpcClient | undefined
  let session: TargetSession | undefined
  try {
    await waitFor(async () => {
      if (child.exitCode !== null) {
        const drained = Buffer.concat(output.streams.flat()).toString("utf8")
        const category = drained.includes("configuration home")
          ? "configuration-home"
          : drained.includes("control transport failed")
            ? "control"
          : drained.includes("state is unavailable")
            ? "state"
            : drained.includes("I/O failed")
              ? "io"
              : "unknown"
        throw new Error(`child-visible-trap-service-exited:${category}`)
      }
      try { return (await stat(socketPath)).isSocket() } catch { return false }
    }, "child-visible trap service socket")
    client = await RpcClient.connect(socketPath, "claude-child-visible-traps")
    session = await client.openTarget("claude", {
      claudeConfigDir: requested.CLAUDE_CONFIG_DIR,
      selectorState: "unset",
      hostManagedState: "unmanaged",
      cwd: repoRoot,
    })
    const saved = await session.act({
      kind: "create-provider",
      name: "Blocked Claude",
      baseUrl: "http://127.0.0.1:9/v1",
      model: "claude-blocked",
      credential: { kind: "replace", value: "blocked-provider-secret" },
      authentication: "anthropic-api-key",
      presetKey: null,
    })
    let code = ""
    try {
      await session.act({ kind: "activate-provider", providerId: saved.view.providers[0]!.id, mode: "takeover" })
    } catch (error) {
      code = typeof error === "object" && error !== null && "code" in error ? String(error.code) : ""
    }
    expect(code).toBe("unsupported-configuration-home")
    expect((await stat(join(muxviaHome, "state/muxvia.db"))).mode & 0o777).toBe(0o600)
    expect((await stat(muxviaHome)).mode & 0o777).toBe(0o700)
    expect(await Promise.all(Object.values(requested).map(controlledTreeFingerprint))).toEqual(fingerprints)
  } finally {
    await session?.close().catch(() => {})
    await writeFile(shutdown, "shutdown\n", { mode: 0o600 }).catch(() => {})
    const completed = await Promise.race([output.completed, Bun.sleep(deadlineMs).then(() => undefined)])
    if (!completed && child.exitCode === null) child.kill("SIGKILL")
    await output.completed.catch(() => undefined)
    scanProcessOutputNoSecrets(output.streams, ["blocked-provider-secret"])
  }
}, 20_000)

test("Claude tracer finalizer attempts every audit and reports fixed accumulated failures", async () => {
  const leaked = "controlled-final-audit-leak"
  const attempted: string[] = []
  let diagnostic = ""
  try {
    try {
      throw new Error("early-functional-failure")
    } finally {
      await runClaudeSecurityFinalizer([
        { name: "credential-recovery", run: () => {
          attempted.push("credential-recovery")
          throw new Error(`raw-${leaked}`)
        } },
        { name: "raw-rpc", run: () => {
          attempted.push("raw-rpc")
          scanNoSecrets([{ frame: leaked }], [leaked], "controlled-raw-rpc")
        } },
        { name: "process-output-drain", run: async () => {
          attempted.push("process-output-drain")
          throw new Error(`drain-${leaked}`)
        } },
        { name: "native-frames", run: () => { attempted.push("native-frames") } },
        { name: "process-output", run: () => {
          attempted.push("process-output")
          throw new Error(`stdout-${leaked}`)
        } },
        { name: "trap", run: () => { attempted.push("trap") } },
      ])
    }
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(attempted).toEqual([
    "credential-recovery",
    "raw-rpc",
    "process-output-drain",
    "native-frames",
    "process-output",
    "trap",
  ])
  expect(diagnostic).toBe(
    "claude-security-finalizer-failed:credential-recovery,raw-rpc,process-output-drain,process-output",
  )
  expect(diagnostic.includes(leaked)).toBeFalse()
})

test("reconciliation tracer scans every ordered security surface after an earlier failure", async () => {
  const secret = "controlled-reconciliation-finalizer-secret"
  const attempted: string[] = []
  const orderedSteps = [
    "held-release", "native-frame-drain", "renderer-destroy", "session-close",
    "shutdown-signal", "process-kill", "process-output-drain", "upstream-drain",
    "preview", "target-view", "outcome", "receipt", "activity", "native-frame",
    "sqlite", "reconciliation-recovery", "target-settings", "raw-rpc",
    "process-output", "upstream-request",
  ] as const
  const mutation = (name: typeof orderedSteps[number]) => () => {
    attempted.push(name)
    scanNoSecrets([{ [name]: secret }], [secret], `controlled-reconciliation-${name}`)
  }
  let diagnostic = ""
  try {
    try {
      throw new Error("earlier-reconciliation-functional-failure")
    } finally {
      await runReconciliationSecurityFinalizer({
        heldRelease: mutation("held-release"),
        nativeFrameDrain: mutation("native-frame-drain"),
        rendererDestroy: mutation("renderer-destroy"),
        sessionClose: mutation("session-close"),
        shutdownSignal: mutation("shutdown-signal"),
        processKill: mutation("process-kill"),
        processOutputDrain: mutation("process-output-drain"),
        upstreamDrain: mutation("upstream-drain"),
        previewAudit: mutation("preview"),
        targetViewAudit: mutation("target-view"),
        outcomeAudit: mutation("outcome"),
        receiptAudit: mutation("receipt"),
        activityAudit: mutation("activity"),
        nativeFrameAudit: mutation("native-frame"),
        sqliteAudit: mutation("sqlite"),
        reconciliationRecoveryAudit: mutation("reconciliation-recovery"),
        targetSettingsAudit: mutation("target-settings"),
        rawRpcAudit: mutation("raw-rpc"),
        processOutputAudit: mutation("process-output"),
        upstreamRequestAudit: mutation("upstream-request"),
      })
    }
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(attempted).toEqual([...orderedSteps])
  expect(diagnostic).toBe(`claude-security-finalizer-failed:${orderedSteps.join(",")}`)
  expect(diagnostic.includes(secret)).toBeFalse()
  expect(diagnostic.includes("earlier-reconciliation-functional-failure")).toBeFalse()
})

test("reconciliation recovery audit allows Authorization envelopes but rejects prefixed routing semantics", () => {
  const secret = "controlled-reconciliation-recovery-secret"
  expect(() => auditReconciliationRecoveryRows([{
    target: "codex",
    beforeJson: JSON.stringify({
      unrelated: { model_providers: { operator: { http_headers: { Authorization: `Bearer ${secret}` } } } },
    }),
    desiredJson: JSON.stringify({}),
  }], [{ target: "codex", secret }])).not.toThrow()

  let diagnostic = ""
  try {
    auditReconciliationRecoveryRows([{
      target: "codex",
      beforeJson: JSON.stringify({}),
      desiredJson: JSON.stringify({
        provider_http_headers: {
          semantic: { "X-Muxvia-Routing-Credential": `prefix-${secret}-suffix` },
        },
      }),
    }], [{ target: "codex", secret }])
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("secret-scan-failed:reconciliation-recovery-secret-location")
  expect(diagnostic.includes(secret)).toBeFalse()
})

test("reconciliation recovery audit rejects inexact Authorization envelopes in semantic and rendered fields", () => {
  const secret = "controlled-reconciliation-authorization-secret"
  const mutations = [
    { semantic: `Bearer prefix-${secret}`, rendered: `{ Authorization = "Bearer prefix-${secret}" }` },
    { semantic: `Bearer ${secret}-suffix`, rendered: `{ Authorization = "Bearer ${secret}-suffix" }` },
    { semantic: `Basic ${secret}`, rendered: `{ Authorization = "Basic ${secret}" }` },
  ]
  for (const mutation of mutations) {
    let diagnostic = ""
    try {
      auditReconciliationRecoveryRows([{
        target: "codex",
        beforeJson: JSON.stringify({}),
        desiredJson: JSON.stringify({
          provider_http_headers: {
            rendered: mutation.rendered,
            semantic: { Authorization: mutation.semantic },
          },
        }),
      }], [{ target: "codex", secret }])
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe("secret-scan-failed:reconciliation-recovery-secret-location")
    expect(diagnostic.includes(secret)).toBeFalse()
  }
})

test("reconciliation intent audit rejects credentials bound to the other Target", () => {
  const codexSecret = "controlled-codex-intent-bound-secret"
  const claudeSecret = "controlled-claude-intent-bound-secret"
  const mutations = [
    {
      row: {
        target: "codex",
        beforeJson: JSON.stringify({}),
        desiredJson: JSON.stringify({
          provider_http_headers: {
            rendered: `{ Authorization = "Bearer ${claudeSecret}" }`,
            semantic: { Authorization: `Bearer ${claudeSecret}` },
          },
        }),
      },
      bound: { target: "claude", secret: claudeSecret },
    },
    {
      row: {
        target: "claude",
        beforeJson: JSON.stringify({}),
        desiredJson: JSON.stringify({ owned: { auth_token: codexSecret } }),
      },
      bound: { target: "codex", secret: codexSecret },
    },
  ] as const
  for (const mutation of mutations) {
    let diagnostic = ""
    try {
      auditReconciliationRecoveryRows([mutation.row], [mutation.bound])
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe("secret-scan-failed:reconciliation-recovery-secret-location")
    expect(diagnostic.includes(codexSecret) || diagnostic.includes(claudeSecret)).toBeFalse()
  }
})

test("reconciliation target-settings audit requires exact owned credential envelopes", () => {
  const codexSecret = "controlled-codex-settings-secret"
  const claudeSecret = "controlled-claude-settings-secret"
  expect(() => auditReconciliationTargetSettings(
    Buffer.from(`http_headers = { Authorization = "Bearer ${codexSecret}" }\n`),
    Buffer.from(JSON.stringify({ env: { ANTHROPIC_AUTH_TOKEN: claudeSecret } })),
    [
      { target: "codex", secret: codexSecret },
      { target: "claude", secret: claudeSecret },
    ],
    [],
  )).not.toThrow()
  for (const [codex, claude] of [
    [`Authorization = "Bearer prefix-${codexSecret}"`, claudeSecret],
    [`Authorization = "Bearer ${codexSecret}-suffix"`, claudeSecret],
    [`Authorization = "Basic ${codexSecret}"`, `prefix-${claudeSecret}`],
  ] as const) {
    let diagnostic = ""
    try {
      auditReconciliationTargetSettings(
        Buffer.from(codex),
        Buffer.from(JSON.stringify({ env: { ANTHROPIC_AUTH_TOKEN: claude } })),
        [
          { target: "codex", secret: codexSecret },
          { target: "claude", secret: claudeSecret },
        ],
        [],
      )
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe("secret-scan-failed:reconciliation-target-settings-location")
    expect(diagnostic.includes(codexSecret) || diagnostic.includes(claudeSecret)).toBeFalse()
  }
})

test("reconciliation target-settings audit rejects credentials bound to the other Target", () => {
  const codexSecret = "controlled-codex-target-bound-secret"
  const claudeSecret = "controlled-claude-target-bound-secret"
  const mutations = [
    {
      codex: Buffer.from(`http_headers = { Authorization = "Bearer ${claudeSecret}" }\n`),
      claude: Buffer.from(JSON.stringify({ env: {} })),
    },
    {
      codex: Buffer.from(""),
      claude: Buffer.from(JSON.stringify({ env: { ANTHROPIC_AUTH_TOKEN: codexSecret } })),
    },
  ]
  for (const mutation of mutations) {
    let diagnostic = ""
    try {
      auditReconciliationTargetSettings(
        mutation.codex,
        mutation.claude,
        [
          { target: "codex", secret: codexSecret },
          { target: "claude", secret: claudeSecret },
        ],
        [],
      )
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe("secret-scan-failed:reconciliation-target-settings-location")
    expect(diagnostic.includes(codexSecret) || diagnostic.includes(claudeSecret)).toBeFalse()
  }
})

test("reconciliation upstream audit rejects provider credentials bound to the other Target", () => {
  const codexSecret = "controlled-codex-upstream-bound-secret"
  const claudeSecret = "controlled-claude-upstream-bound-secret"
  for (const request of [
    { path: "/v1/responses", secret: claudeSecret },
    { path: "/v1/messages", secret: codexSecret },
  ]) {
    let diagnostic = ""
    try {
      auditReconciliationUpstreamRequests([{
        authorization: `Bearer ${request.secret}`,
        headers: { authorization: `Bearer ${request.secret}` },
        contentType: "application/json",
        method: "POST",
        testHeader: null,
        body: "{}",
        path: request.path,
      }], [
        { target: "codex", secret: codexSecret },
        { target: "claude", secret: claudeSecret },
      ], [])
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe("reconciliation-upstream-provider-auth-invalid")
    expect(diagnostic.includes(codexSecret) || diagnostic.includes(claudeSecret)).toBeFalse()
  }
})

test("cross-Target scanners override an earlier failure through the real ordered finalizer", async () => {
  const codexSecret = "controlled-cross-target-codex-secret"
  const claudeSecret = "controlled-cross-target-claude-secret"
  const noOp = () => {}
  let diagnostic = ""
  try {
    try {
      throw new Error("controlled-earlier-cross-target-failure")
    } finally {
      await runReconciliationSecurityFinalizer({
        heldRelease: noOp,
        nativeFrameDrain: noOp,
        rendererDestroy: noOp,
        sessionClose: noOp,
        shutdownSignal: noOp,
        processKill: noOp,
        processOutputDrain: noOp,
        upstreamDrain: noOp,
        previewAudit: noOp,
        targetViewAudit: noOp,
        outcomeAudit: noOp,
        receiptAudit: noOp,
        activityAudit: noOp,
        nativeFrameAudit: noOp,
        sqliteAudit: noOp,
        reconciliationRecoveryAudit: () => auditReconciliationRecoveryRows([{
          target: "codex",
          beforeJson: JSON.stringify({}),
          desiredJson: JSON.stringify({
            provider_http_headers: {
              rendered: `{ Authorization = "Bearer ${claudeSecret}" }`,
              semantic: { Authorization: `Bearer ${claudeSecret}` },
            },
          }),
        }], [{ target: "claude", secret: claudeSecret }]),
        targetSettingsAudit: () => auditReconciliationTargetSettings(
          Buffer.from(`Authorization = "Bearer ${claudeSecret}"`),
          Buffer.from(JSON.stringify({ env: {} })),
          [
            { target: "codex", secret: codexSecret },
            { target: "claude", secret: claudeSecret },
          ],
          [],
        ),
        rawRpcAudit: noOp,
        processOutputAudit: noOp,
        upstreamRequestAudit: () => auditReconciliationUpstreamRequests([{
          authorization: `Bearer ${claudeSecret}`,
          headers: { authorization: `Bearer ${claudeSecret}` },
          contentType: "application/json",
          method: "POST",
          testHeader: null,
          body: "{}",
          path: "/v1/responses",
        }], [
          { target: "codex", secret: codexSecret },
          { target: "claude", secret: claudeSecret },
        ], []),
      })
    }
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe(
    "claude-security-finalizer-failed:reconciliation-recovery,target-settings,upstream-request",
  )
  expect(diagnostic.includes(codexSecret) || diagnostic.includes(claudeSecret)).toBeFalse()
  expect(diagnostic.includes("controlled-earlier-cross-target-failure")).toBeFalse()
})

async function createStableFingerprintMutationDatabase(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "reconciliation-stable-fingerprint-"))
  roots.push(root)
  const path = join(root, "mutation.db")
  const database = new Database(path, { create: true })
  database.exec(`
    CREATE TABLE target_route_state (
      target TEXT, management_revision INTEGER, view_sequence INTEGER,
      current_provider_id TEXT, serving_provider_id TEXT, takeover_state TEXT,
      route_port INTEGER, routing_credential TEXT, activated_snapshot_id TEXT,
      managed_config_path TEXT, recovery_state TEXT
    );
    CREATE TABLE target_problems (target TEXT, code TEXT);
    CREATE TABLE providers (target TEXT, id TEXT, position INTEGER);
    CREATE TABLE credentials (target TEXT, id TEXT);
    CREATE TABLE activated_snapshots (target TEXT, id TEXT);
    CREATE TABLE action_receipts (target TEXT, committed_revision INTEGER, action_id TEXT);
    CREATE TABLE activation_recovery (target TEXT, created_revision INTEGER, id TEXT);
    CREATE TABLE reconciliation_intents (
      target TEXT, created_revision INTEGER, action_id TEXT, desired_json TEXT
    );
    CREATE TABLE target_compatibility (target TEXT, observed_version TEXT);
    INSERT INTO target_route_state VALUES (
      'codex', 3, 4, NULL, NULL, 'inactive', NULL, NULL, NULL, '/tmp/config.toml', 'clean'
    );
  `)
  database.close()
  return path
}

test("stable target database fingerprint includes reconciliation intents", async () => {
  const path = await createStableFingerprintMutationDatabase()
  const before = readStableTargetDatabaseFingerprint(path, "codex")
  const database = new Database(path)
  database.query("INSERT INTO reconciliation_intents VALUES ('codex', 5, 'action-1', '{}')").run()
  database.close()
  expect(readStableTargetDatabaseFingerprint(path, "codex")).not.toBe(before)
})

test("stable target database fingerprint includes compatibility", async () => {
  const path = await createStableFingerprintMutationDatabase()
  const before = readStableTargetDatabaseFingerprint(path, "codex")
  const database = new Database(path)
  database.query("INSERT INTO target_compatibility VALUES ('codex', '77.1.0')").run()
  database.close()
  expect(readStableTargetDatabaseFingerprint(path, "codex")).not.toBe(before)
})

test("stable target database fingerprint includes durable view sequence", async () => {
  const path = await createStableFingerprintMutationDatabase()
  const before = readStableTargetDatabaseFingerprint(path, "codex")
  const database = new Database(path)
  database.query("UPDATE target_route_state SET view_sequence = 5 WHERE target = 'codex'").run()
  database.close()
  expect(readStableTargetDatabaseFingerprint(path, "codex")).not.toBe(before)
})

test("stable target view fingerprint preserves view sequence", () => {
  const first = {
    target: "codex",
    viewSequence: 10,
    service: { epoch: "ephemeral", state: "ready" },
  } as unknown as TargetView
  const second = { ...first, viewSequence: 11 }
  expect(stableTargetViewFingerprint(second)).not.toBe(stableTargetViewFingerprint(first))
})

const controlledReconciliationSecret = "controlled-reconciliation-response-secret"

function controlledReconciliationView(overrides: Record<string, unknown> = {}): TargetView {
  return {
    target: "codex",
    viewSequence: 17,
    controlled: controlledReconciliationSecret,
    ...overrides,
  } as unknown as TargetView
}

function controlledReconciliationOutcome(view: TargetView): Record<string, unknown> {
  return { status: "applied", view }
}

function controlledReconciliationResponsePushDiagnostic(
  frames: readonly unknown[],
  target: "codex" | "claude",
  outcome: unknown,
  label: string,
): string {
  let diagnostic = ""
  try {
    assertReconciliationResponseBeforeSinglePush(frames, 0, target, outcome, label)
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  return diagnostic
}

function expectControlledReconciliationResponsePushFailure(
  frames: readonly unknown[],
  target: "codex" | "claude",
  outcome: unknown,
  label: string,
): void {
  const diagnostic = controlledReconciliationResponsePushDiagnostic(frames, target, outcome, label)
  expect(diagnostic).toBe(`reconciliation-response-push-mismatch:${label}`)
  expect(diagnostic.includes(controlledReconciliationSecret)).toBeFalse()
}

test("Reconciliation response/push audit rejects a push that precedes its response", () => {
  const view = controlledReconciliationView()
  const outcome = controlledReconciliationOutcome(view)
  expectControlledReconciliationResponsePushFailure([
    { type: "target-view", view },
    { type: "response", result: { kind: "action-outcome", outcome } },
  ], "codex", outcome, "controlled-order")
})

test("Reconciliation response/push audit rejects a missing push", () => {
  const view = controlledReconciliationView()
  const outcome = controlledReconciliationOutcome(view)
  expectControlledReconciliationResponsePushFailure([
    { type: "response", result: { kind: "action-outcome", outcome } },
  ], "codex", outcome, "controlled-missing-push")
})

test("Reconciliation response/push audit rejects a duplicate push", () => {
  const view = controlledReconciliationView()
  const outcome = controlledReconciliationOutcome(view)
  expectControlledReconciliationResponsePushFailure([
    { type: "response", result: { kind: "action-outcome", outcome } },
    { type: "target-view", view },
    { type: "target-view", view },
  ], "codex", outcome, "controlled-duplicate-push")
})

test("Reconciliation response/push audit rejects the wrong Target", () => {
  const view = controlledReconciliationView({ target: "claude" })
  const outcome = controlledReconciliationOutcome(view)
  expectControlledReconciliationResponsePushFailure([
    { type: "response", result: { kind: "action-outcome", outcome } },
    { type: "target-view", view },
  ], "codex", outcome, "controlled-wrong-target")
})

test("Reconciliation response/push audit rejects the wrong action view", () => {
  const expectedView = controlledReconciliationView()
  const expectedOutcome = controlledReconciliationOutcome(expectedView)
  const wrongView = controlledReconciliationView({ viewSequence: 18 })
  const wrongOutcome = controlledReconciliationOutcome(wrongView)
  expectControlledReconciliationResponsePushFailure([
    { type: "response", result: { kind: "action-outcome", outcome: wrongOutcome } },
    { type: "target-view", view: wrongView },
  ], "codex", expectedOutcome, "controlled-wrong-view")
})

test("listener observer uses event turns and a bounded negative barrier", async () => {
  const observations = [[4101, 4102], [4102]]
  let observed = 0
  await waitForTcpListenerObservation(
    async () => observations[Math.min(observed++, observations.length - 1)]!,
    (ports) => !ports.includes(4101) && ports.includes(4102),
    "controlled-listener-transition",
    deadlineMs,
    (next) => queueMicrotask(next),
  )
  expect(observed).toBe(2)

  let diagnostic = ""
  try {
    await waitForTcpListenerObservation(
      async () => [4101],
      (ports) => !ports.includes(4101),
      "controlled-listener-negative",
      1,
      () => {},
    )
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("event-barrier-failed:controlled-listener-negative")
})

test("UDS readiness uses event turns and preserves process-exit failure", async () => {
  let attempts = 0
  await waitForEventTurnReadiness(
    async () => ++attempts === 2,
    new Promise<never>(() => {}),
    "controlled-uds-ready",
    deadlineMs,
    (next) => queueMicrotask(next),
  )
  expect(attempts).toBe(2)

  let diagnostic = ""
  try {
    await waitForEventTurnReadiness(
      async () => false,
      Promise.reject(new Error("controlled-process-exited-before-uds")),
      "controlled-uds-exit",
      deadlineMs,
      (next) => queueMicrotask(next),
    )
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("controlled-process-exited-before-uds")
})

test("Claude SQLite audit rejects a secret in an unrelated state column", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-sqlite-audit-"))
  roots.push(root)
  const path = join(root, "mutation.db")
  const database = new Database(path, { create: true })
  database.exec("CREATE TABLE target_problems (target TEXT, code TEXT, message TEXT)")
  const secret = "controlled-sqlite-misplaced-secret"
  database.query("INSERT INTO target_problems VALUES ('claude', 'controlled', ?)").run(secret)
  database.close()
  let diagnostic = ""
  try {
    auditSqliteSecretLocations(path, { providerSecrets: [], routingSecrets: [{ target: "claude", secret }] })
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("secret-scan-failed:claude-sqlite-secret-location")
  expect(diagnostic.includes(secret)).toBeFalse()
})

test("Claude SQLite audit scans index definitions without querying indexes as tables", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-sqlite-index-audit-"))
  roots.push(root)
  const path = join(root, "indexed.db")
  const database = new Database(path, { create: true })
  database.exec("CREATE TABLE providers (id TEXT PRIMARY KEY, target TEXT, name TEXT, credential_id TEXT)")
  database.exec("CREATE UNIQUE INDEX providers_generated_owner_target ON providers(target, id)")
  database.close()
  expect(() => auditSqliteSecretLocations(path, {
    providerSecrets: [],
    routingSecrets: [],
  })).not.toThrow()
})

test("Claude SQLite recovery audit rejects a prefixed semantic token", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-recovery-audit-"))
  roots.push(root)
  const path = join(root, "mutation.db")
  const secret = "controlled-recovery-routing-secret"
  const database = new Database(path, { create: true })
  database.exec("CREATE TABLE activation_recovery (target TEXT, payload_json TEXT)")
  database.query("INSERT INTO activation_recovery VALUES ('claude', ?)").run(JSON.stringify({
    target: "claude",
    before: { owned: { auth_token: null } },
    desired: { owned: { auth_token: `prefix-${secret}-suffix` } },
  }))
  database.close()
  let diagnostic = ""
  try {
    auditSqliteSecretLocations(path, { providerSecrets: [], routingSecrets: [{ target: "claude", secret }] })
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("secret-scan-failed:claude-sqlite-secret-location")
  expect(diagnostic.includes(secret)).toBeFalse()
})

test("Claude SQLite recovery audit allows v2 API-key ownership but rejects the same legacy claim", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-recovery-version-audit-"))
  roots.push(root)
  const path = join(root, "mutation.db")
  const secret = "controlled-versioned-recovery-api-key"
  const database = new Database(path, { create: true })
  database.exec("CREATE TABLE activation_recovery (target TEXT, payload_json TEXT)")
  const payload = (ownershipVersion: number) => JSON.stringify({
    target: "claude",
    ownership_version: ownershipVersion,
    before: {
      ownership_version: ownershipVersion,
      owned: { base_url: null, auth_token: null, model: null, api_key: secret },
    },
    desired: {
      ownership_version: ownershipVersion,
      mode: "direct",
      owned: { base_url: "https://controlled.invalid", auth_token: null, model: "controlled", api_key: null },
    },
  })
  database.query("INSERT INTO activation_recovery VALUES ('claude', ?)").run(payload(2))
  database.close()
  const policy = {
    providerSecrets: [{ target: "claude" as const, name: "Prior API Key", secret }],
    routingSecrets: [],
  }
  expect(() => auditSqliteSecretLocations(path, policy)).not.toThrow()

  const mutation = new Database(path)
  mutation.query("UPDATE activation_recovery SET payload_json = ?").run(payload(1))
  mutation.close()
  let diagnostic = ""
  try { auditSqliteSecretLocations(path, policy) } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(diagnostic).toBe("secret-scan-failed:claude-sqlite-secret-location")
  expect(diagnostic.includes(secret)).toBeFalse()
})

test("Claude SQLite recovery audit rejects inconsistent ownership versions scan-first", async () => {
  const root = await mkdtemp(join(tmpdir(), "claude-recovery-consistency-audit-"))
  roots.push(root)
  const path = join(root, "mutation.db")
  const secret = "controlled-inconsistent-recovery-api-key"
  const database = new Database(path, { create: true })
  database.exec("CREATE TABLE activation_recovery (target TEXT, payload_json TEXT)")
  const writePayload = (payload: unknown) => {
    database.query("DELETE FROM activation_recovery").run()
    database.query("INSERT INTO activation_recovery VALUES ('claude', ?)").run(JSON.stringify(payload))
  }
  const policy = {
    providerSecrets: [{ target: "claude" as const, name: "Prior API Key", secret }],
    routingSecrets: [],
  }
  writePayload({
    target: "claude",
    ownership_version: 2,
    before: {
      ownership_version: 1,
      owned: { base_url: null, auth_token: null, model: null, api_key: secret },
    },
    desired: {
      ownership_version: 2,
      mode: "direct",
      owned: { base_url: "https://controlled.invalid", auth_token: null, model: "controlled", api_key: null },
    },
  })
  let scanFirstDiagnostic = ""
  try { auditSqliteSecretLocations(path, policy) } catch (error) {
    scanFirstDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(scanFirstDiagnostic).toBe("secret-scan-failed:claude-sqlite-secret-location")
  expect(scanFirstDiagnostic.includes(secret)).toBeFalse()

  writePayload({
    target: "claude",
    ownership_version: 2,
    before: { ownership_version: 2, owned: {} },
    desired: { ownership_version: 1, owned: {} },
  })
  database.close()
  let semanticDiagnostic = ""
  try { auditSqliteSecretLocations(path, policy) } catch (error) {
    semanticDiagnostic = error instanceof Error ? error.message : String(error)
  }
  expect(semanticDiagnostic).toBe("claude-sqlite-recovery-ownership-version-invalid")
  expect(semanticDiagnostic.includes(secret)).toBeFalse()
})

test("Claude Messages projection rejects contract mutations with fixed diagnostics", () => {
  const exact: ClaudeRequestProjection = {
    method: "POST",
    pathClass: "messages",
    authentication: "api-key",
    model: "claude-api-model",
    stream: false,
    delayed: false,
    error: false,
    version: "2023-06-01",
    betaValues: ["tools-2025-01-01", "context-1m-2025-08-07"],
    correlation: "claude-contract-request",
    query: "beta=true&fixture=exact",
    toolsDigest: "sha256:bfcc7fa1821a5c8fba2595839a9a267926840b77999fa6f9cb70894ed48716ec",
    unknownDigest: "sha256:6b6e86a08acc82a7339ef0e0c0566dc4461d882e4b5ed97b8497e0c2e3d817d2",
  }
  const mutations: Array<[ClaudeRequestProjection, string]> = [
    [{ ...exact, toolsDigest: sensitiveDigest("altered")! }, "claude-messages-tools-mismatch"],
    [{ ...exact, unknownDigest: sensitiveDigest("altered")! }, "claude-messages-unknown-mismatch"],
    [{ ...exact, version: null }, "claude-messages-version-mismatch"],
  ]
  for (const [projection, expected] of mutations) {
    let diagnostic = ""
    try { assertExactClaudeMessagesProjection(projection) } catch (error) {
      diagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(diagnostic).toBe(expected)
    expect(diagnostic.includes(JSON.stringify(projection))).toBeFalse()
  }
  expect(() => assertClaudeUpstreamResponseHeaders({})).toThrow("claude-response-header-mismatch")
})

test("fake Claude upstream rejects missing version and altered schema with fixed safe responses", async () => {
  const apiKey = "controlled-fake-upstream-api-key"
  const upstream = await startFakeUpstream("unused", undefined, { apiKeyCredentials: [apiKey] })
  try {
    const missingDiscoveryVersion = await fetch(`${upstream.baseUrl}/models`, {
      headers: { "x-api-key": apiKey },
    })
    expect(missingDiscoveryVersion.status).toBe(422)
    expect(await missingDiscoveryVersion.text()).toBe('{"error":"fixture-contract-rejected"}')
    const missingVersion = await fetch(`${upstream.baseUrl}/messages`, {
      method: "POST",
      headers: { "content-type": "application/json", "x-api-key": apiKey },
      body: '{"messages":[]}',
    })
    expect(missingVersion.status).toBe(422)
    expect(await missingVersion.text()).toBe('{"error":"fixture-contract-rejected"}')
    const alteredSchema = await fetch(`${upstream.baseUrl}/messages?beta=true&fixture=exact`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "x-api-key": apiKey,
        "anthropic-version": "2023-06-01",
        "anthropic-beta": "tools-2025-01-01, context-1m-2025-08-07",
        "x-claude-code-fixture": "claude-contract-request",
      },
      body: JSON.stringify({
        messages: [],
        tools: [{ name: "mutated", input_schema: { type: "object" } }],
        future_extension: { retained: true, nested: { version: 7 } },
      }),
    })
    expect(alteredSchema.status).toBe(422)
    expect(await alteredSchema.text()).toBe('{"error":"fixture-contract-rejected"}')
  } finally {
    await upstream.stop()
  }
})

test("real processes reconcile Codex and Claude configuration drift", async () => {
  const root = await mkdtemp(join(tmpdir(), "mr-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const codexConfig = join(userHome, ".codex/config.toml")
  const claudeSettings = join(userHome, ".claude/settings.json")
  const codexVersion = join(root, "codex-version")
  const claudeVersion = join(root, "claude-version")
  const codexExecutable = join(root, "codex")
  const claudeExecutable = join(root, "claude")
  const firstShutdown = join(root, "shutdown-first")
  const finalShutdown = join(root, "shutdown-final")
  const initialClaudeSecret = "reconcile-claude-initial-secret-must-not-escape"
  const adoptedCodexSecret = "reconcile-codex-adopted-secret-must-not-escape"
  const adoptedClaudeSecret = "reconcile-claude-adopted-secret-must-not-escape"
  const providerSecrets = [providerSecret, initialClaudeSecret, adoptedCodexSecret, adoptedClaudeSecret]
  const providerSecretBindings = [
    { target: "codex", secret: providerSecret },
    { target: "claude", secret: initialClaudeSecret },
    { target: "codex", secret: adoptedCodexSecret },
    { target: "claude", secret: adoptedClaudeSecret },
  ] as const

  await mkdir(dirname(codexConfig), { recursive: true, mode: 0o700 })
  await mkdir(dirname(claudeSettings), { recursive: true, mode: 0o700 })
  await writeFile(codexConfig, '# operator comment survives\nunrelated = "keep-me"\n', { mode: 0o600 })
  await writeFile(claudeSettings, JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] }, env: { OPERATOR_UNRELATED: "keep-me" },
  }, null, 2), { mode: 0o640 })
  await chmod(claudeSettings, 0o640)
  await writeFile(codexVersion, "0.147.0\n", { mode: 0o600 })
  await writeFile(claudeVersion, "2.1.228\n", { mode: 0o600 })
  await writeMutableProbe(codexExecutable, codexVersion, "codex")
  await writeMutableProbe(claudeExecutable, claudeVersion, "claude")

  const initialUpstream = await startFakeUpstream(providerSecret, undefined, {
    bearerCredentials: [initialClaudeSecret],
  })
  const adoptedUpstream = await startHeldReconciliationUpstream({
    codex: adoptedCodexSecret,
    claude: adoptedClaudeSecret,
  })
  const services: Array<{ child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> }> = []
  const rpcStreams: Buffer[][] = []
  const readinessSockets: Array<ReturnType<typeof createConnection>> = []
  const reconciliationFrames: Record<HeldReconciliationTarget, unknown[]> = { codex: [], claude: [] }
  const observed: unknown[] = []
  const observedPreviews: unknown[] = []
  const observedTargetViews: unknown[] = []
  const observedOutcomes: unknown[] = []
  const activitiesAndNativeFrames: string[] = []
  let codexRouting = ""
  let claudeRouting = ""
  const routingSecrets: Record<HeldReconciliationTarget, Set<string>> = {
    codex: new Set<string>(),
    claude: new Set<string>(),
  }
  let codexSession: TargetSession | undefined
  let claudeSession: TargetSession | undefined
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  let subscriptions: Array<() => void> = []
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let recorder: ReturnType<typeof createRendererAudit> | undefined
  const secrets = () => [
    ...providerSecrets,
    wrongRoutingSecret,
    ...routingSecrets.codex,
    ...routingSecrets.claude,
  ]
  const recoverySecretBindings = () => [
    ...providerSecretBindings,
    ...(["codex", "claude"] as const).flatMap((target) =>
      [...routingSecrets[target]].map((secret) => ({ target, secret }))),
  ]
  const rememberRouting = (target: HeldReconciliationTarget, credential: string) => {
    routingSecrets[target].add(credential)
    if (target === "codex") codexRouting = credential
    else claudeRouting = credential
  }
  const startService = async (shutdown: string, label: string) => {
    const child = spawn(serviceBinary, [
      "--home", muxviaHome,
      "--test-shutdown-file", shutdown,
      "--test-codex-executable", codexExecutable,
      "--test-claude-executable", claudeExecutable,
    ], {
      cwd: root,
      env: { HOME: userHome, PATH: "/usr/bin:/bin", MUXVIA_INTEGRATION_TEST: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    })
    const output = captureProcessOutput(child)
    services.push({ child, output })
    const exitedBeforeReady = output.completed.then(({ code }) => {
      scanProcessOutputNoSecrets(output.streams, secrets())
      const text = Buffer.concat(output.streams.flat()).toString("utf8")
      const category = text.includes("state is unavailable")
        ? "state"
        : text.includes("control transport failed")
          ? "control"
          : text.includes("I/O failed")
            ? "io"
            : text.includes("test-only Routing Service options")
              ? "integration-guard"
              : text.includes("probe") || text.includes("Target CLI")
                ? "probe"
                : "unknown"
      throw new Error(`reconciliation-service-exited:${label}:${code}:${category}`)
    }) as Promise<never>
    await waitForEventTurnReadiness(
      () => probeUnixSocket(socketPath, readinessSockets),
      exitedBeforeReady,
      `${label}-reconciliation-service`,
    )
    return { child, output }
  }
  const connect = async (target: HeldReconciliationTarget, release: string) => {
    const chunks: Buffer[] = []
    const decoder = new FrameDecoder()
    rpcStreams.push(chunks)
    const client = await eventBarrier(RpcClient.connect(socketPath, release, undefined, (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => {
        const bytes = Buffer.from(chunk)
        chunks.push(bytes)
        for (const frame of decoder.push(bytes)) {
          assertSecretSafeStructured(frame, secrets(), "reconciliation-rpc-frame")
          observed.push(frame)
          reconciliationFrames[target].push(frame)
        }
      })
      return socket
    }), `${target}-${release}-connect`)
    readinessSockets.splice(0).forEach((socket) => socket.destroy())
    return client
  }
  const openSessions = async (release: string) => {
    codexClient = await connect("codex", `${release}-codex`)
    claudeClient = await connect("claude", `${release}-claude`)
    codexSession = await eventBarrier(codexClient.openTarget("codex"), `${release}-codex-open`)
    claudeSession = await eventBarrier(claudeClient.openTarget("claude", {
      claudeConfigDir: null,
      selectorState: "unset",
      hostManagedState: "unmanaged",
      cwd: root,
    }), `${release}-claude-open`)
    const collect = (view: TargetView) => {
      assertSecretSafeStructured(view, secrets(), "reconciliation-target-view")
      observed.push(view)
      observedTargetViews.push(view)
    }
    collect(codexSession.get() as TargetView)
    collect(claudeSession.get() as TargetView)
    subscriptions = [
      codexSession.subscribe((view) => collect(view as TargetView)),
      claudeSession.subscribe((view) => collect(view as TargetView)),
    ]
  }
  const closeSessions = async () => {
    subscriptions.splice(0).forEach((unsubscribe) => unsubscribe())
    await Promise.all([codexSession?.close(), claudeSession?.close()])
    codexSession = undefined
    claudeSession = undefined
    codexClient = undefined
    claudeClient = undefined
  }
  const safeAct = async (
    session: TargetSession,
    action: OrdinaryTargetAction,
    label: string,
  ) => {
    try {
      const outcome = await eventBarrier(session.act(action), `${label}-rpc-response`)
      assertSecretSafeStructured(outcome, secrets(), `${label}-outcome`)
      observed.push(outcome)
      observedOutcomes.push(outcome)
      return outcome
    } catch (error) {
      scanNoSecrets([asyncErrorSurface(error)], secrets(), `${label}-error`)
      const code = typeof error === "object" && error !== null && "code" in error
        ? String(error.code)
        : "internal-failure"
      const view = session.get()
      throw new Error(`reconciliation-action-failed:${label}:${code}:${view.recovery.state}:${view.problems.map(({ code }) => code).join("+") || "none"}`)
    }
  }
  const expectDriftBlock = async (session: TargetSession, target: HeldReconciliationTarget) => {
    const provider = session.get().providers.find(({ id }) => id === session.get().currentProviderId)!
    await expectSecretSafeControlCode(eventBarrier(session.act({
      kind: "update-provider",
      providerId: provider.id,
      providerRevision: provider.providerRevision,
      name: provider.name,
      baseUrl: provider.baseUrl,
      model: provider.model,
      credential: { kind: "keep" },
      authentication: provider.authentication,
    }), `${target}-drift-block-rpc-response`), "configuration-drift", secrets(), `${target}-drift-block`)
    expect(session.get().managedConfiguration.state).toBe("configuration-drift")
  }
  const preview = async (session: TargetSession, strategy: "adopt" | "reapply" | "restore", label: string) => {
    const result = await eventBarrier(
      session.previewReconciliation(strategy),
      `${label}-rpc-response`,
    )
    assertSecretSafeStructured(result, secrets(), `${label}-preview`)
    observed.push(result)
    observedPreviews.push(result)
    return result
  }
  const apply = async (
    session: TargetSession,
    strategy: "adopt" | "reapply" | "restore",
    token: string,
    acknowledgeVersion?: string,
  ) => {
    const target = session.get().target
    const frameStart = reconciliationFrames[target].length
    const priorSequence = session.get().viewSequence
    let unsubscribe = () => {}
    const pushed = new Promise<Readonly<TargetView>>((resolvePush) => {
      unsubscribe = session.subscribe((view) => {
        if (view.viewSequence > priorSequence) resolvePush(view)
      })
    })
    let outcome
    try {
      outcome = await eventBarrier(session.applyReconciliation({
        strategy,
        observationToken: token,
        ...(acknowledgeVersion ? { acknowledgeVersion } : {}),
      }), `${target}-${strategy}-rpc-response`)
      await eventBarrier(pushed, `${target}-${strategy}-single-push`)
    } finally {
      unsubscribe()
    }
    assertSecretSafeStructured(outcome, secrets(), `${strategy}-outcome`)
    observed.push(outcome)
    observedOutcomes.push(outcome)
    assertReconciliationResponseBeforeSinglePush(
      reconciliationFrames[target], frameStart, target, outcome, `${target}-${strategy}`,
    )
    return outcome
  }
  const withSecurityDatabase = async (run: (database: Database) => void): Promise<void> => {
    try { await stat(databasePath) } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return
      throw error
    }
    const database = new Database(databasePath, { readonly: true })
    try { run(database) } finally { database.close() }
  }

  try {
    const first = await startService(firstShutdown, "first")
    await openSessions("reconciliation-first")
    const codexSaved = await safeAct(codexSession!, {
      kind: "create-provider", name: "Codex Baseline", baseUrl: initialUpstream.baseUrl,
      model: "codex-baseline", credential: { kind: "replace", value: providerSecret },
      authentication: "openai-bearer", presetKey: null,
    }, "codex-baseline-save")
    await safeAct(codexSession!, {
      kind: "activate-provider", providerId: codexSaved.view.providers[0]!.id, mode: "takeover",
    }, "codex-baseline-activate")
    const initialCodexRoute = extractManagedConfig(await readFile(codexConfig, "utf8"), "codex-baseline")
    rememberRouting("codex", initialCodexRoute.credential)
    const claudeSaved = await safeAct(claudeSession!, {
      kind: "create-provider", name: "Claude Baseline", baseUrl: initialUpstream.baseUrl,
      model: "claude-baseline", credential: { kind: "replace", value: initialClaudeSecret },
      authentication: "anthropic-bearer", presetKey: null,
    }, "claude-baseline-save")
    await safeAct(claudeSession!, {
      kind: "activate-provider", providerId: claudeSaved.view.providers[0]!.id, mode: "takeover",
    }, "claude-baseline-activate")
    const initialClaudeRoute = extractClaudeManagedSettings(
      await readFile(claudeSettings, "utf8"), "claude-baseline", providerSecrets,
    )
    rememberRouting("claude", initialClaudeRoute.credential)
    await waitForTcpListenerObservation(
      () => observeTcpListenerPorts(first.child.pid!),
      (ports) => [initialCodexRoute.endpoint, initialClaudeRoute.endpoint]
        .every((endpoint) => ports.includes(Number(new URL(endpoint).port))),
      "initial-target-listeners",
    )

    const continuing = claudePost(initialClaudeRoute.endpoint, claudeRouting, "/v1/messages", {
      messages: [], stream: true, fixture_delay: true,
    })
    await eventBarrier(initialUpstream.waitForDelayedStart(), "initial-peer-request-start")
    await writeFile(codexConfig, (await readFile(codexConfig, "utf8"))
      .replace('model = "codex-baseline"', 'model = "codex-drift"')
      .concat('\noperator_reapply = "codex-preserved"\n'), { mode: 0o600 })
    await expectDriftBlock(codexSession!, "codex")
    const peer = claudeSession!.get().providers[0]!
    await safeAct(claudeSession!, {
      kind: "update-provider", providerId: peer.id, providerRevision: peer.providerRevision,
      name: "Claude Baseline peer", baseUrl: peer.baseUrl, model: peer.model,
      credential: { kind: "keep" }, authentication: peer.authentication,
    }, "healthy-peer-write")
    initialUpstream.releaseDelayed()
    expect((await eventBarrier(continuing, "initial-peer-request-complete")).status).toBe(200)
    const codexReapply = await preview(codexSession!, "reapply", "codex-reapply")
    await apply(codexSession!, "reapply", codexReapply.observationToken)
    const codexReapplied = await readFile(codexConfig, "utf8")
    expect(codexReapplied.includes('model = "codex-baseline"')).toBeTrue()
    expect(codexReapplied.includes('operator_reapply = "codex-preserved"')).toBeTrue()

    const claudeDrift = JSON.parse(await readFile(claudeSettings, "utf8")) as {
      env: Record<string, unknown>; operator: Record<string, unknown>
    }
    claudeDrift.env.ANTHROPIC_MODEL = "claude-drift"
    claudeDrift.operator.reapply = "claude-preserved"
    await writeFile(claudeSettings, JSON.stringify(claudeDrift, null, 2), { mode: 0o640 })
    await chmod(claudeSettings, 0o640)
    await expectDriftBlock(claudeSession!, "claude")
    const codexPeer = codexSession!.get().providers[0]!
    await safeAct(codexSession!, {
      kind: "update-provider", providerId: codexPeer.id, providerRevision: codexPeer.providerRevision,
      name: "Codex Baseline peer", baseUrl: codexPeer.baseUrl, model: codexPeer.model,
      credential: { kind: "keep" }, authentication: codexPeer.authentication,
    }, "codex-healthy-peer-write")
    const claudeReapply = await preview(claudeSession!, "reapply", "claude-reapply")
    await apply(claudeSession!, "reapply", claudeReapply.observationToken)
    const claudeReapplied = JSON.parse(await readFile(claudeSettings, "utf8")) as {
      env: Record<string, unknown>; operator: Record<string, unknown>
    }
    expect(claudeReapplied.env.ANTHROPIC_MODEL).toBe("claude-baseline")
    expect(claudeReapplied.operator.reapply).toBe("claude-preserved")

    await writeFile(codexVersion, "77.1.0\n", { mode: 0o600 })
    await writeFile(claudeVersion, "77.1.0\n", { mode: 0o600 })
    const codexHistory = targetHistory(databasePath, "codex")
    await writeFile(codexConfig, `model = "codex-adopted"\nmodel_provider = "operator_openai"\noperator_reapply = "codex-preserved"\n\n[model_providers.operator_openai]\nname = "Codex Adopted"\nbase_url = "${adoptedUpstream.baseUrl}"\nwire_api = "responses"\nhttp_headers = { Authorization = "Bearer ${adoptedCodexSecret}" }\nsupports_websockets = false\noperator_note = "keep"\n`, { mode: 0o600 })
    const codexAdoptPreview = await preview(codexSession!, "adopt", "codex-adopt")
    expect(codexAdoptPreview.compatibility).toMatchObject({
      version: "codex-cli 77.1.0", classification: "unknown-compatible", acknowledgementRequired: true,
    })
    const codexAdopt = await apply(
      codexSession!, "adopt", codexAdoptPreview.observationToken, codexAdoptPreview.compatibility.version,
    )
    expect(codexAdopt.view.providers).toHaveLength(2)
    expect(codexAdopt.view.currentProviderId).not.toBe(codexSaved.view.providers[0]!.id)
    expect(codexAdopt.view).toMatchObject({
      target: "codex", mode: "direct", takeover: { state: "inactive", endpoint: null },
      servingProviderId: null,
    })
    expect(codexAdopt.view.activatedSnapshot).toMatchObject({
      providerId: codexAdopt.view.currentProviderId,
      model: "codex-adopted",
      protocol: "openai-responses",
      authentication: "openai-bearer",
    })
    assertAdoptedImmutableHistory(
      codexHistory,
      targetHistory(databasePath, "codex"),
      codexAdopt.view.currentProviderId!,
      codexAdopt.view.activatedSnapshot!.id,
    )
    await safeAct(codexSession!, {
      kind: "activate-provider", providerId: codexAdopt.view.currentProviderId!, mode: "takeover",
    }, "codex-adopt-takeover")
    rememberRouting("codex", extractReconciledCodexRoute(
      await readFile(codexConfig, "utf8"), "codex-adopted", providerSecrets,
    ).credential)

    const claudeHistory = targetHistory(databasePath, "claude")
    await writeFile(claudeSettings, JSON.stringify({
      operator: { reapply: "claude-preserved" },
      env: {
        ANTHROPIC_BASE_URL: adoptedUpstream.baseUrl,
        ANTHROPIC_MODEL: "claude-adopted",
        ANTHROPIC_AUTH_TOKEN: adoptedClaudeSecret,
        OPERATOR_UNRELATED: "keep",
      },
    }, null, 2), { mode: 0o640 })
    await chmod(claudeSettings, 0o640)
    const claudeAdoptPreview = await preview(claudeSession!, "adopt", "claude-adopt")
    expect(claudeAdoptPreview.compatibility).toMatchObject({
      version: "77.1.0 (Claude Code)", classification: "unknown-compatible", acknowledgementRequired: true,
    })
    const claudeAdopt = await apply(
      claudeSession!, "adopt", claudeAdoptPreview.observationToken, claudeAdoptPreview.compatibility.version,
    )
    expect(claudeAdopt.view.providers).toHaveLength(2)
    expect(claudeAdopt.view.currentProviderId).not.toBe(claudeSaved.view.providers[0]!.id)
    expect(claudeAdopt.view).toMatchObject({
      target: "claude", mode: "direct", takeover: { state: "inactive", endpoint: null },
      servingProviderId: null,
    })
    expect(claudeAdopt.view.activatedSnapshot).toMatchObject({
      providerId: claudeAdopt.view.currentProviderId,
      model: "claude-adopted",
      protocol: "anthropic-messages",
      authentication: "anthropic-bearer",
    })
    assertAdoptedImmutableHistory(
      claudeHistory,
      targetHistory(databasePath, "claude"),
      claudeAdopt.view.currentProviderId!,
      claudeAdopt.view.activatedSnapshot!.id,
    )
    await safeAct(claudeSession!, {
      kind: "activate-provider", providerId: claudeAdopt.view.currentProviderId!, mode: "takeover",
    }, "claude-adopt-takeover")
    rememberRouting("claude", extractReconciledClaudeRoute(
      await readFile(claudeSettings, "utf8"), "claude-adopted", providerSecrets,
    ).credential)

    await closeSessions()
    await writeFile(firstShutdown, "shutdown\n", { mode: 0o600 })
    expect(await eventBarrier(first.output.completed, "first-service-restart-exit")).toEqual({ code: 0, signal: null })
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    const second = await startService(finalShutdown, "second")
    await openSessions("reconciliation-second")
    const sameVersionCodex = await preview(codexSession!, "reapply", "codex-same-version")
    const sameVersionClaude = await preview(claudeSession!, "reapply", "claude-same-version")
    expect(sameVersionCodex.compatibility.acknowledgementRequired).toBeFalse()
    expect(sameVersionClaude.compatibility.acknowledgementRequired).toBeFalse()

    await writeFile(codexVersion, "77.2.0\n", { mode: 0o600 })
    await writeFile(claudeVersion, "77.2.0\n", { mode: 0o600 })
    for (const [target, session] of [["codex", codexSession!], ["claude", claudeSession!]] as const) {
      const changed = await preview(session, "reapply", `${target}-changed-version`)
      expect(changed.compatibility.acknowledgementRequired).toBeTrue()
      await expectSecretSafeControlCode(eventBarrier(session.applyReconciliation({
        strategy: "reapply", observationToken: changed.observationToken,
      }), `${target}-reack-required-rpc-response`), "compatibility-acknowledgement-required", secrets(), `${target}-reack-required`)
      await apply(session, "reapply", changed.observationToken, changed.compatibility.version)
      expect((await preview(session, "reapply", `${target}-reack-persisted`))
        .compatibility.acknowledgementRequired).toBeFalse()
    }

    setup = await testRender(() => <App sessions={{ codex: codexSession!, claude: claudeSession! }} />, {
      width: 100, height: 30, useThread: false, kittyKeyboard: true,
    })
    recorder = createRendererAudit(setup)
    recorder.start()
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    const codexPage = await waitForSecretSafeFrame(
      setup, (frame) => frame.includes("Codex CLI") && frame.includes("Run a target action"),
      secrets(), "reconciliation-opentui-codex",
    )
    activitiesAndNativeFrames.push(codexPage)

    const restoreBusyThenApply = async (target: HeldReconciliationTarget) => {
      const session = target === "codex" ? codexSession! : claudeSession!
      const config = target === "codex" ? codexConfig : claudeSettings
      if (target === "codex") {
        await writeFile(config, (await readFile(config, "utf8"))
          .replace('model = "codex-adopted"', 'model = "codex-restore-drift"')
          .concat('\nrestore_unrelated = "codex-kept"\n'), { mode: 0o600 })
      } else {
        const value = JSON.parse(await readFile(config, "utf8")) as {
          env: Record<string, unknown>; operator: Record<string, unknown>
        }
        value.env.ANTHROPIC_MODEL = "claude-restore-drift"
        value.operator.restore_unrelated = "claude-kept"
        await writeFile(config, JSON.stringify(value, null, 2), { mode: 0o640 })
        await chmod(config, 0o640)
      }
      const endpoint = session.get().takeover.endpoint!
      const targetPort = Number(new URL(endpoint).port)
      const routing = target === "codex" ? codexRouting : claudeRouting
      const held = adoptedUpstream.holdNext(target)
      const request = target === "codex"
        ? chunkedPost(`${endpoint}/v1`, routing)
        : claudePost(endpoint, routing, "/v1/messages", { messages: [], stream: true })
      await eventBarrier(Promise.race([
        held.started,
        request.then(({ status }) => {
          const path = adoptedUpstream.calls.at(-1)?.path ?? "not-captured"
          const pathClass = path === "/v1/responses"
            ? "codex"
            : path.startsWith("/v1/messages")
              ? "claude"
              : path === "not-captured"
                ? "not-captured"
                : "unexpected"
          throw new Error(`held-request-completed-before-upstream:${target}:${status}:${pathClass}`)
        }),
      ]), `${target}-held-request-start`)
      const restore = await preview(session, "restore", `${target}-restore-busy`)
      const before = {
        database: readStableTargetDatabaseFingerprint(databasePath, target),
        file: await safeFileFingerprint(config),
        view: stableTargetViewFingerprint(session.get()),
        ports: await observeTcpListenerPorts(second.child.pid!),
      }
      await expectSecretSafeControlCode(eventBarrier(session.applyReconciliation({
        strategy: "restore", observationToken: restore.observationToken,
      }), `${target}-restore-busy-rpc-response`), "target-busy", secrets(), `${target}-restore-busy`)
      expect(readStableTargetDatabaseFingerprint(databasePath, target)).toBe(before.database)
      expect(await safeFileFingerprint(config)).toEqual(before.file)
      expect(stableTargetViewFingerprint(session.get())).toBe(before.view)
      expect(await observeTcpListenerPorts(second.child.pid!)).toEqual(before.ports)
      held.release()
      expect((await eventBarrier(request, `${target}-held-request-complete`)).status)
        .toBe(target === "codex" ? 201 : 200)
      const fresh = await preview(session, "restore", `${target}-restore-fresh`)
      const peer = target === "codex" ? claudeSession! : codexSession!
      const peerPort = Number(new URL(peer.get().takeover.endpoint!).port)
      const restored = await apply(session, "restore", fresh.observationToken)
      expect(restored.view.mode).toBe("unmanaged")
      const restoredBytes = await readFile(config)
      const restoredMode = (await stat(config)).mode & 0o777
      if (target === "codex") {
        const restoredText = restoredBytes.toString("utf8")
        if (restoredMode !== 0o600) throw new Error("codex-restore-mode-mismatch")
        if (!restoredText.includes('operator_reapply = "codex-preserved"')) {
          throw new Error("codex-restore-reapply-unrelated-missing")
        }
        if (!restoredText.includes('restore_unrelated = "codex-kept"')) {
          throw new Error("codex-restore-latest-unrelated-missing")
        }
        if (/^model\s*=/m.test(restoredText) || /^model_provider\s*=/m.test(restoredText)
          || restoredText.includes("[model_providers.muxvia_codex]")) {
          throw new Error("codex-restore-muxvia-owned-fields-present")
        }
        if (
          !restoredText.includes("[model_providers.operator_openai]")
          || !restoredText.includes('operator_note = "keep"')
          || /^(name|base_url|wire_api|http_headers|supports_websockets)\s*=/m.test(restoredText)
          || /Authorization|X-Muxvia-Routing-Credential/.test(restoredText)
        ) {
          throw new Error("codex-restore-owned-provider-fields-mismatch")
        }
      } else {
        const restoredSettings = JSON.parse(restoredBytes.toString("utf8")) as {
          env: Record<string, unknown>
          operator: Record<string, unknown>
        }
        if (restoredMode !== 0o640) throw new Error("claude-restore-mode-mismatch")
        if (
          restoredSettings.operator.reapply !== "claude-preserved"
          || restoredSettings.operator.restore_unrelated !== "claude-kept"
          || restoredSettings.env.OPERATOR_UNRELATED !== "keep"
        ) throw new Error("claude-restore-unrelated-fields-mismatch")
        if (
          restoredSettings.env.ANTHROPIC_BASE_URL !== adoptedUpstream.baseUrl
          || restoredSettings.env.ANTHROPIC_MODEL !== "claude-adopted"
          || restoredSettings.env.ANTHROPIC_AUTH_TOKEN !== adoptedClaudeSecret
          || "ANTHROPIC_API_KEY" in restoredSettings.env
        ) {
          throw new Error("claude-restore-pre-muxvia-owned-fields-mismatch")
        }
      }
      await waitForTcpListenerObservation(
        () => observeTcpListenerPorts(second.child.pid!),
        (ports) => !ports.includes(targetPort) && ports.includes(peerPort),
        `${target}-listener-stop-peer-continuity`,
      )
    }
    await restoreBusyThenApply("codex")
    const codexRetained = codexSession!.get().providers.find(({ name }) => name === "Codex Adopted")!
    await safeAct(codexSession!, {
      kind: "activate-provider", providerId: codexRetained.id, mode: "takeover",
    }, "codex-peer-reactivation")
    rememberRouting("codex", extractReconciledCodexRoute(
      await readFile(codexConfig, "utf8"), "codex-adopted", providerSecrets,
    ).credential)
    await restoreBusyThenApply("claude")
    const finalCodexRestore = await preview(codexSession!, "restore", "codex-final-restore")
    await apply(codexSession!, "restore", finalCodexRestore.observationToken)
    await waitForTcpListenerObservation(
      () => observeTcpListenerPorts(second.child.pid!),
      (ports) => ports.length === 0,
      "all-reconciled-listeners-stopped",
    )

    recorder.stop()
    activitiesAndNativeFrames.push(...recorder.frames())
    setup.renderer.destroy()
    setup = undefined
    await closeSessions()
    expect(await eventBarrier(second.output.completed, "final-natural-service-exit")).toEqual({ code: 0, signal: null })
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })

    const database = new Database(databasePath, { readonly: true })
    const receipts = database.query("SELECT outcome_json FROM action_receipts ORDER BY target, committed_revision").all()
    const recovery = database.query(`SELECT target, before_json AS beforeJson, desired_json AS desiredJson
      FROM reconciliation_intents ORDER BY target, created_revision`).all() as Array<{
        target: string; beforeJson: string; desiredJson: string
      }>
    database.close()
    scanNoSecrets(receipts, secrets(), "reconciliation-receipts")
    scanNoSecrets([observed, activitiesAndNativeFrames], secrets(), "reconciliation-observed-surfaces")
    auditReconciliationRecoveryRows(recovery, recoverySecretBindings())
    auditReconciliationTargetSettings(
      await readFile(codexConfig),
      await readFile(claudeSettings),
      [
        { target: "codex", secret: providerSecret },
        { target: "claude", secret: initialClaudeSecret },
        { target: "codex", secret: adoptedCodexSecret },
        { target: "claude", secret: adoptedClaudeSecret },
      ],
      (["codex", "claude"] as const).flatMap((target) =>
        [...routingSecrets[target]].map((secret) => ({ target, secret }))),
    )
    scanRawRpcFramesNoSecrets(rpcStreams, secrets())
    scanProcessOutputNoSecrets(services.flatMap(({ output }) => output.streams), secrets())
    auditReconciliationUpstreamRequests(
      [...initialUpstream.calls, ...adoptedUpstream.calls], providerSecretBindings,
      [wrongRoutingSecret, ...routingSecrets.codex, ...routingSecrets.claude],
    )
    auditSqliteSecretLocations(databasePath, {
      providerSecrets: [
        { target: "codex", name: "Codex Baseline peer", secret: providerSecret },
        { target: "claude", name: "Claude Baseline peer", secret: initialClaudeSecret },
        { target: "codex", name: "Codex Adopted", secret: adoptedCodexSecret },
        { target: "claude", name: "Adopted Claude configuration", secret: adoptedClaudeSecret },
      ],
      routingSecrets: (["codex", "claude"] as const).flatMap((target) =>
        [...routingSecrets[target]].map((secret) => ({ target, secret }))),
    })
  } finally {
    await runReconciliationSecurityFinalizer({
      heldRelease: () => {
        adoptedUpstream.releaseHeld()
        readinessSockets.splice(0).forEach((socket) => socket.destroy())
      },
      nativeFrameDrain: () => {
        if (!recorder) return
        recorder.stop()
        activitiesAndNativeFrames.push(...recorder.frames())
      },
      rendererDestroy: () => {
        if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
      },
      sessionClose: closeSessions,
      shutdownSignal: async () => {
        await writeFile(firstShutdown, "shutdown\n", { mode: 0o600 })
        await writeFile(finalShutdown, "shutdown\n", { mode: 0o600 })
      },
      processKill: () => {
        for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
      },
      processOutputDrain: async () => {
        const results = await Promise.allSettled(services.map(({ output }) => output.completed))
        if (results.some(({ status }) => status === "rejected")) throw new Error("reconciliation-output-drain-failed")
      },
      upstreamDrain: async () => {
        await Promise.all([initialUpstream.stop(), adoptedUpstream.stop()])
      },
      previewAudit: () => scanNoSecrets(observedPreviews, secrets(), "reconciliation-final-preview"),
      targetViewAudit: () => scanNoSecrets(observedTargetViews, secrets(), "reconciliation-final-target-view"),
      outcomeAudit: () => scanNoSecrets(observedOutcomes, secrets(), "reconciliation-final-outcome"),
      receiptAudit: async () => withSecurityDatabase((database) => {
        const present = database.query("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'action_receipts'").get()
        if (!present) return
        const receipts = database.query("SELECT outcome_json FROM action_receipts ORDER BY target, committed_revision").all()
        scanNoSecrets(receipts, secrets(), "reconciliation-final-receipts")
      }),
      activityAudit: () => scanNoSecrets(activitiesAndNativeFrames, secrets(), "reconciliation-final-activity"),
      nativeFrameAudit: () => scanNoSecrets(activitiesAndNativeFrames, secrets(), "reconciliation-final-native-frame"),
      sqliteAudit: async () => {
        try { await stat(databasePath) } catch (error) {
          if ((error as NodeJS.ErrnoException).code === "ENOENT") return
          throw error
        }
        auditSqliteSecretLocations(databasePath, {
          providerSecrets: [
            { target: "codex", name: "Codex Baseline peer", secret: providerSecret },
            { target: "claude", name: "Claude Baseline peer", secret: initialClaudeSecret },
            { target: "codex", name: "Codex Adopted", secret: adoptedCodexSecret },
            { target: "claude", name: "Adopted Claude configuration", secret: adoptedClaudeSecret },
          ],
          routingSecrets: (["codex", "claude"] as const).flatMap((target) =>
            [...routingSecrets[target]].map((secret) => ({ target, secret }))),
        })
      },
      reconciliationRecoveryAudit: async () => withSecurityDatabase((database) => {
        const present = database.query("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'reconciliation_intents'").get()
        if (!present) return
        const recovery = database.query(`SELECT target, before_json AS beforeJson, desired_json AS desiredJson
          FROM reconciliation_intents ORDER BY target, created_revision`).all() as Array<{
            target: string; beforeJson: string; desiredJson: string
          }>
        auditReconciliationRecoveryRows(recovery, recoverySecretBindings())
      }),
      targetSettingsAudit: async () => {
        const readIfPresent = async (path: string) => {
          try { return await readFile(path) } catch (error) {
            if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined
            throw error
          }
        }
        auditReconciliationTargetSettings(
          await readIfPresent(codexConfig),
          await readIfPresent(claudeSettings),
          [
            { target: "codex", secret: providerSecret },
            { target: "claude", secret: initialClaudeSecret },
            { target: "codex", secret: adoptedCodexSecret },
            { target: "claude", secret: adoptedClaudeSecret },
          ],
          (["codex", "claude"] as const).flatMap((target) =>
            [...routingSecrets[target]].map((secret) => ({ target, secret }))),
        )
      },
      rawRpcAudit: () => {
        if (rpcStreams.some((stream) => stream.length > 0)) scanRawRpcFramesNoSecrets(rpcStreams, secrets())
      },
      processOutputAudit: () => {
        scanProcessOutputNoSecrets(services.flatMap(({ output }) => output.streams), secrets())
      },
      upstreamRequestAudit: () => {
        auditReconciliationUpstreamRequests(
          [...initialUpstream.calls, ...adoptedUpstream.calls], providerSecretBindings,
          [wrongRoutingSecret, ...routingSecrets.codex, ...routingSecrets.claude],
        )
      },
    })
  }
}, 120_000)

async function runClaudeRestrictiveUmaskTracer(input: {
  label: string
  pattern: string
  secrets: readonly string[]
}): Promise<void> {
  const child = spawn(process.execPath, [
    "test",
    "./packages/control-plane/test/walking-skeleton.e2e.tsx",
    "--test-name-pattern",
    input.pattern,
  ], {
    cwd: repoRoot,
    env: { ...process.env, MUXVIA_CLAUDE_RESTRICTIVE_UMASK_CHILD: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  })
  const output = captureProcessOutput(child)
  const completed = await Promise.race([
    output.completed,
    Bun.sleep(60_000).then(() => undefined),
  ])
  if (!completed) {
    child.kill("SIGKILL")
    await output.completed.catch(() => {})
    throw new Error(`restrictive-umask-${input.label}-tracer-timeout`)
  }
  scanProcessOutputNoSecrets(output.streams, input.secrets)
  if (completed.code !== 0 || completed.signal !== null) {
    throw new Error(`restrictive-umask-${input.label}-tracer-failed`)
  }
}

test("Claude tracer fixture enforces restrictive umask", async () => {
  await runClaudeRestrictiveUmaskTracer({
    label: "claude",
    pattern: "real processes prove independent Claude takeover, Messages, hot switch, and restart",
    secrets: [
      "claude-api-provider-secret-must-not-escape",
      "claude-bearer-provider-secret-must-not-escape",
      providerSecret,
    ],
  })
}, 70_000)

test("Claude Direct tracer fixture enforces restrictive umask", async () => {
  await runClaudeRestrictiveUmaskTracer({
    label: "claude-direct",
    pattern: "real processes prove Claude Direct authentication replacement and natural idle restart",
    secrets: [
      "claude-direct-bearer-secret-must-not-escape",
      "claude-direct-api-key-secret-must-not-escape",
      "claude-direct-prior-bearer-must-not-escape",
      "claude-direct-prior-api-key-must-not-escape",
    ],
  })
}, 70_000)

const controlledProviderCredential = "controlled-expected-provider"

function createControlledSixRequestAudit(): ReturnType<typeof createCapturedRequestAudit> {
  const audit = createCapturedRequestAudit({
    providerCredential: controlledProviderCredential,
    forbiddenRoutingCredentials: () => [wrongRoutingSecret],
    expectedRequestCount: 6,
  })
  for (let index = 0; index < 6; index++) {
    audit.expectNext("expected-provider")
    audit.observe({
      authorization: `Bearer ${controlledProviderCredential}`,
      headers: { authorization: `Bearer ${controlledProviderCredential}` },
      contentType: null,
      method: "GET",
      testHeader: null,
      body: "",
      path: "/v1/models",
    })
  }
  return audit
}

function closeCapturedRequestAudit(
  audit: ReturnType<typeof createCapturedRequestAudit>,
  requestCount: number,
): string {
  try {
    audit.assertComplete(requestCount)
    return ""
  } catch (error) {
    return error instanceof Error ? error.message : String(error)
  }
}

test("CapturedRequest audit scans a forbidden unexpected seventh request before closing", () => {
  const unexpectedBody = `{"routing":"${wrongRoutingSecret}"}`
  const audit = createControlledSixRequestAudit()
  audit.observe({
    authorization: null,
    headers: { "x-forbidden-routing": wrongRoutingSecret },
    contentType: "application/json",
    method: "DELETE",
    testHeader: null,
    body: unexpectedBody,
    path: "/v1/unexpected",
  })
  const diagnostic = closeCapturedRequestAudit(audit, 7)

  expect(diagnostic === "secret-scan-failed:captured-request-routing-credential").toBeTrue()
  expect(diagnostic.includes(wrongRoutingSecret)).toBeFalse()
  expect(diagnostic.includes(unexpectedBody)).toBeFalse()
})

test("CapturedRequest audit rejects a clean unexpected seventh request with a fixed diagnostic", () => {
  const unexpectedPath = "/v1/unexpected?surface=controlled-raw-seventh-request"
  const audit = createControlledSixRequestAudit()
  audit.observe({
    authorization: null,
    headers: {},
    contentType: null,
    method: "DELETE",
    testHeader: null,
    body: "",
    path: unexpectedPath,
  })
  const diagnostic = closeCapturedRequestAudit(audit, 7)

  expect(diagnostic === "captured-request-unexpected").toBeTrue()
  expect(diagnostic.includes(unexpectedPath)).toBeFalse()
})

test("CapturedRequest audit closes over the exact observed request count", () => {
  const diagnostic = closeCapturedRequestAudit(createControlledSixRequestAudit(), 7)
  expect(diagnostic === "captured-request-audit-count-mismatch").toBeTrue()
})

test("fake upstream quiesce audits a delayed forbidden seventh request before closing", async () => {
  const audit = createControlledSixRequestAudit()
  for (let index = 0; index < 6; index++) audit.projection(index)
  const upstream = await startFakeUpstream(controlledProviderCredential, audit.observe)
  const requestBody = '{"late":true}'
  let request: ReturnType<typeof httpRequest> | undefined
  let legacyDiagnostic = ""
  let finalDiagnostic = ""
  let hasQuiesce = false
  try {
    const url = new URL(`${upstream.baseUrl}/responses`)
    let resolveContinue = () => {}
    let rejectContinue = (_error: unknown) => {}
    const continued = new Promise<void>((resolve, reject) => {
      resolveContinue = resolve
      rejectContinue = reject
    })
    let resolveResponse = () => {}
    let rejectResponse = (_error: unknown) => {}
    const responseCompleted = new Promise<void>((resolve, reject) => {
      resolveResponse = resolve
      rejectResponse = reject
    })
    request = httpRequest({
      hostname: url.hostname,
      port: url.port,
      path: url.pathname,
      method: "POST",
      headers: {
        connection: "close",
        expect: "100-continue",
        "content-length": Buffer.byteLength(requestBody),
        "x-forbidden-routing": wrongRoutingSecret,
      },
    }, (response) => {
      response.resume()
      response.once("end", resolveResponse)
      response.once("error", rejectResponse)
    })
    request.once("continue", resolveContinue)
    request.once("error", (error) => {
      rejectContinue(error)
      rejectResponse(error)
    })
    request.flushHeaders()
    await continued
    await new Promise<void>((resolveWrite, rejectWrite) => {
      request!.write(requestBody.slice(0, 1), (error) => error ? rejectWrite(error) : resolveWrite())
    })
    await upstream.waitForActiveHandlerCount(1)

    legacyDiagnostic = closeCapturedRequestAudit(audit, 6 + upstream.calls.length)
    const quiesce = (upstream as typeof upstream & { quiesce?: () => Promise<void> }).quiesce
    hasQuiesce = typeof quiesce === "function"
    const quiesced = quiesce?.()
    request.end(requestBody.slice(1))
    await responseCompleted
    await quiesced
    finalDiagnostic = closeCapturedRequestAudit(audit, 6 + upstream.calls.length)
  } finally {
    request?.destroy()
    await upstream.stop()
  }

  expect(legacyDiagnostic === "").toBeTrue()
  expect(hasQuiesce).toBeTrue()
  expect(finalDiagnostic === "secret-scan-failed:captured-request-routing-credential").toBeTrue()
  expect(finalDiagnostic.includes(wrongRoutingSecret)).toBeFalse()
  expect(finalDiagnostic.includes(requestBody)).toBeFalse()
})

test("real processes prove independent Claude takeover, Messages, hot switch, and restart", async () => {
  const root = await mkdtemp(join(tmpdir(), "m5-"))
  roots.push(root)
  const userHome = join(root, "h")
  const muxviaHome = join(userHome, ".muxvia")
  const codexHome = join(userHome, ".codex")
  const claudeHome = join(userHome, ".claude")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const codexConfigPath = join(codexHome, "config.toml")
  const claudeSettingsPath = join(claudeHome, "settings.json")
  const firstShutdown = join(root, "shutdown-first")
  const secondShutdown = join(root, "shutdown-second")
  const trapHome = join(root, "operator-home-trap")
  const trapCodex = join(trapHome, ".codex")
  const trapClaude = join(trapHome, ".claude")
  const apiKeySecret = "claude-api-provider-secret-must-not-escape"
  const bearerSecret = "claude-bearer-provider-secret-must-not-escape"
  const wrongClaudeRouting = "wrong-claude-routing-must-not-escape"
  const allStaticSecrets = [providerSecret, apiKeySecret, bearerSecret, wrongClaudeRouting]

  await mkdir(codexHome, { recursive: true, mode: 0o700 })
  await mkdir(claudeHome, { recursive: true, mode: 0o700 })
  await mkdir(join(trapHome, ".muxvia/state"), { recursive: true, mode: 0o700 })
  await mkdir(trapCodex, { recursive: true, mode: 0o700 })
  await mkdir(trapClaude, { recursive: true, mode: 0o700 })
  await writeFile(codexConfigPath, '# operator comment survives\nunrelated = "keep-me"\n', { mode: 0o600 })
  const originalClaudeSettings = JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] },
    env: { OPERATOR_UNRELATED: "keep-me" },
  })
  await writeFile(claudeSettingsPath, originalClaudeSettings, { mode: 0o640 })
  await chmod(claudeSettingsPath, 0o640)
  await writeFile(join(trapCodex, "config.toml"), 'trap = "codex"\n', { mode: 0o600 })
  await writeFile(join(trapClaude, "settings.json"), '{"trap":"claude"}\n', { mode: 0o600 })
  await writeFile(join(trapHome, ".muxvia/state/trap"), "trap\n", { mode: 0o600 })
  await chmod(fakeCodex, 0o755)
  await chmod(fakeClaude, 0o755)
  const trapFingerprint = await controlledTreeFingerprint(trapHome)

  let codexRoutingCredential: string | undefined
  let claudeRoutingCredential: string | undefined
  let requestAuditFailure: string | undefined
  const claudeRequestProjections: ClaudeRequestProjection[] = []
  const upstream = await startFakeUpstream(providerSecret, (request) => {
    try {
      const path = request.path.split("?", 1)[0]
      if (path === "/v1/responses") {
        auditCapturedRequestAndProject(request, {
          providerCredential: providerSecret,
          forbiddenRoutingCredentials: [
            wrongClaudeRouting,
            ...(codexRoutingCredential ? [codexRoutingCredential] : []),
            ...(claudeRoutingCredential ? [claudeRoutingCredential] : []),
          ],
          authorization: "expected-provider",
        })
      } else {
        claudeRequestProjections.push(auditClaudeRequestAndProject(request, {
          apiKeyCredential: apiKeySecret,
          bearerCredential: bearerSecret,
          forbiddenRoutingCredentials: [
            wrongClaudeRouting,
            ...(codexRoutingCredential ? [codexRoutingCredential] : []),
            ...(claudeRoutingCredential ? [claudeRoutingCredential] : []),
          ],
        }))
      }
    } catch (error) {
      requestAuditFailure ??= fixedClaudeRequestAuditDiagnostic(error)
    }
  }, {
    apiKeyCredentials: [apiKeySecret],
    bearerCredentials: [bearerSecret],
  })

  const services: Array<{ child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> }> = []
  const rpcStreams: Buffer[][] = []
  const decodedFrames: unknown[] = []
  const views: TargetView[] = []
  const selectedFrames: string[] = []
  const nativeFrames: string[] = []
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  let codexSession: TargetSession | undefined
  let claudeSession: TargetSession | undefined
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let rendererAudit: ReturnType<typeof createRendererAudit> | undefined
  let unsubscribes: Array<() => void> = []

  const startService = async (shutdownFile: string, label: string) => {
    const serviceEnvironment: NodeJS.ProcessEnv = {
      HOME: trapHome,
      CODEX_HOME: trapCodex,
      CLAUDE_CONFIG_DIR: trapClaude,
      PATH: `${dirname(fakeCodex)}:/usr/bin:/bin`,
      MUXVIA_INTEGRATION_TEST: "1",
    }
    serviceEnvironment.HOME = userHome
    delete serviceEnvironment.CODEX_HOME
    delete serviceEnvironment.CLAUDE_CONFIG_DIR
    const child = spawn(serviceBinary, [
      "--home", muxviaHome,
      "--test-shutdown-file", shutdownFile,
      "--test-codex-executable", fakeCodex,
      "--test-claude-executable", fakeClaude,
    ], {
      cwd: root,
      env: serviceEnvironment,
      stdio: ["ignore", "pipe", "pipe"],
    })
    const record = { child, output: captureProcessOutput(child) }
    services.push(record)
    await waitFor(async () => {
      if (child.exitCode !== null) {
        scanProcessOutputNoSecrets(record.output.streams, allStaticSecrets)
        const output = Buffer.concat(record.output.streams.flat()).toString("utf8")
        const category = output.includes("another Routing Service")
          ? "lock-collision"
          : output.includes("state is unavailable")
            ? "state"
            : output.includes("control transport failed")
              ? "control"
              : output.includes("I/O failed")
                ? "io"
                : output.includes("test-only Routing Service options")
                  ? "integration-guard"
                  : "unknown"
        throw new Error(`claude-routing-service-exited:${label}:${child.exitCode}:${category}`)
      }
      try { return (await stat(socketPath)).isSocket() } catch { return false }
    }, `Claude tracer ${label} control socket`)
    return record
  }
  const connect = async (release: string) => {
    const stream: Buffer[] = []
    const decoder = new FrameDecoder()
    rpcStreams.push(stream)
    return await RpcClient.connect(socketPath, release, undefined, (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => {
        const bytes = Buffer.from(chunk)
        stream.push(bytes)
        for (const frame of decoder.push(bytes)) {
          assertSecretSafeStructured(frame, [
            ...allStaticSecrets,
            ...(codexRoutingCredential ? [codexRoutingCredential] : []),
            ...(claudeRoutingCredential ? [claudeRoutingCredential] : []),
          ], "claude-decoded-rpc-frame")
          decodedFrames.push(frame)
        }
      })
      return socket
    })
  }
  const openSessions = async (release: string) => {
    codexClient = await connect(`${release}-codex`)
    claudeClient = await connect(`${release}-claude`)
    codexSession = await codexClient.openTarget("codex")
    claudeSession = await claudeClient.openTarget("claude", {
      claudeConfigDir: null,
      selectorState: "unset",
      hostManagedState: "unmanaged",
      cwd: root,
    })
    const collect = (view: TargetView) => {
      assertSecretSafeStructured(view, [
        ...allStaticSecrets,
        ...(codexRoutingCredential ? [codexRoutingCredential] : []),
        ...(claudeRoutingCredential ? [claudeRoutingCredential] : []),
      ], "claude-target-view")
      views.push(view)
    }
    collect(codexSession.get() as TargetView)
    collect(claudeSession.get() as TargetView)
    unsubscribes = [
      codexSession.subscribe((view) => collect(view as TargetView)),
      claudeSession.subscribe((view) => collect(view as TargetView)),
    ]
  }
  const closeSessions = async () => {
    unsubscribes.splice(0).forEach((unsubscribe) => unsubscribe())
    await Promise.all([codexSession?.close(), claudeSession?.close()])
    codexSession = undefined
    claudeSession = undefined
    codexClient = undefined
    claudeClient = undefined
  }
  const stopService = async (
    record: { child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> },
    shutdownFile: string,
  ) => {
    await writeFile(shutdownFile, "shutdown\n", { mode: 0o600 })
    const result = await Promise.race([record.output.completed, Bun.sleep(deadlineMs).then(() => undefined)])
    if (!result) throw new Error("claude-routing-service-shutdown-timeout")
    expect(result).toEqual({ code: 0, signal: null })
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
  }
  const leader = (key: string) => {
    setup!.mockInput.pressKey("x", { ctrl: true })
    setup!.mockInput.pressKey(key)
  }
  const assertAuditHealthy = () => {
    if (requestAuditFailure) throw new Error(requestAuditFailure)
  }

  try {
    const firstService = await startService(firstShutdown, "first")
    await openSessions("claude-e2e")

    const codexSaved = await actSecretSafe(codexSession!, {
      kind: "create-provider",
      name: "Codex Baseline",
      baseUrl: upstream.baseUrl,
      model: "gpt-codex-baseline",
      credential: { kind: "replace", value: providerSecret },
      authentication: "openai-bearer",
      presetKey: null,
    }, allStaticSecrets, "codex-baseline-save")
    const codexProvider = codexSaved.view.providers[0]!
    const codexApplied = await actSecretSafe(codexSession!, {
      kind: "activate-provider",
      providerId: codexProvider.id,
      mode: "takeover",
    }, allStaticSecrets, "codex-baseline-takeover")
    const codexConfig = extractManagedConfig(await readFile(codexConfigPath, "utf8"), "gpt-codex-baseline")
    codexRoutingCredential = codexConfig.credential
    const codexServingResponse = await chunkedPost(codexConfig.endpoint, codexConfig.credential)
    expect(codexServingResponse.status).toBe(201)
    expect(codexServingResponse.body).toBe(SSE_BYTES.join(""))
    const codexServing = await waitForSecretSafeSession(
      codexSession!,
      (view) => view.servingProviderId === codexProvider.id,
      [...allStaticSecrets, codexConfig.credential],
      "codex-authenticated-serving-baseline",
    )
    const codexDatabaseWithoutPublicationSequence = () =>
      readStableTargetDatabaseFingerprint(databasePath, "codex", false)
    const codexViewWithoutPublicationSequence = () =>
      stableTargetViewFingerprint(codexSession!.get(), false)
    const codexBaseline = {
      endpoint: codexApplied.view.takeover.endpoint!,
      credentialDigest: sensitiveDigest(codexConfig.credential),
      snapshotId: codexApplied.view.activatedSnapshot!.id,
      config: await safeFileFingerprint(codexConfigPath),
      database: codexDatabaseWithoutPublicationSequence(),
      view: stableTargetViewFingerprint(codexServing, false),
    }

    setup = await testRender(() => <App sessions={{ codex: codexSession!, claude: claudeSession! }} />, {
      width: 100,
      height: 28,
      useThread: false,
      kittyKeyboard: true,
    })
    rendererAudit = createRendererAudit(setup)
    rendererAudit.start()
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.waitForFrame((frame) => frame.includes("Claude Code") && frame.includes("Run a target action"))

    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API (Messages)"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API key"))
    await setup.mockInput.typeText("Claude API Key")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(upstream.baseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("claude-api-model")
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(apiKeySecret)
    captureSecretSafeFrame(setup, selectedFrames, allStaticSecrets, "claude-api-key-entry")
    setup.mockInput.pressEnter()
    await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.providers.length === 1,
      allStaticSecrets,
      "claude-api-key-save",
    )
    const apiProvider = claudeSession!.get().providers[0]!
    expect(apiProvider).toMatchObject({
      name: "Claude API Key",
      authentication: "anthropic-api-key",
      protocol: "anthropic-messages",
      credential: "present",
    })

    const beforeInspection = await readOnlyStateFingerprint(databasePath, claudeSettingsPath)
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude API Key"))
    setup.mockInput.pressEnter()
    await waitFor(() => claudeRequestProjections.length >= 1, "Claude automatic discovery")
    assertAuditHealthy()
    expect(claudeRequestProjections[0]).toMatchObject({
      method: "GET", pathClass: "models", authentication: "api-key",
    })
    assertExactClaudeDiscoveryProjection(claudeRequestProjections[0]!)
    await waitForSecretSafeFrame(setup, (frame) => frame.includes("2 models available"), allStaticSecrets, "claude-auto-discovery")
    expect(await readOnlyStateFingerprint(databasePath, claudeSettingsPath)).toEqual(beforeInspection)
    leader("f")
    await waitFor(() => claudeRequestProjections.length >= 2, "Claude explicit discovery")
    assertAuditHealthy()
    expect(claudeRequestProjections[1]).toMatchObject({
      method: "GET", pathClass: "models", authentication: "api-key",
    })
    assertExactClaudeDiscoveryProjection(claudeRequestProjections[1]!)
    expect(await readOnlyStateFingerprint(databasePath, claudeSettingsPath)).toEqual(beforeInspection)
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action") && !frame.includes("Enter save"))
    await setup.renderOnce()
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Providers") && frame.includes("Claude API Key"))
    leader("t")
    await waitFor(() => claudeRequestProjections.length >= 3, "Claude reachability")
    assertAuditHealthy()
    expect(claudeRequestProjections[2]).toMatchObject({ authentication: "absent", pathClass: "reachability" })
    expect(await readOnlyStateFingerprint(databasePath, claudeSettingsPath)).toEqual(beforeInspection)
    expect(claudeSession!.get().routeHealth).toEqual({ state: "unobserved" })
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action") && !frame.includes("Providers"))
    await setup.renderOnce()

    await setup.mockInput.typeText("/takeover")
    setup.mockInput.pressEnter()
    const apiApplied = await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.takeover.state === "active",
      allStaticSecrets,
      "claude-api-key-takeover",
    )
    const appliedFrame = await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Mode       Takeover")
        && frame.includes("Current Target Provider  Claude API Key")
        && frame.includes("Restart Claude Code to use the managed configuration."),
      allStaticSecrets,
      "claude-api-key-applied",
    )
    selectedFrames.push(appliedFrame)
    const settingsBytes = await readFile(claudeSettingsPath, "utf8")
    const claudeManaged = extractClaudeManagedSettings(settingsBytes, "claude-api-model", [
      ...allStaticSecrets,
      codexConfig.credential,
    ])
    claudeRoutingCredential = claudeManaged.credential
    auditClaudeSettingsSecrets(settingsBytes, claudeManaged.credential, [
      ...allStaticSecrets,
      codexConfig.credential,
    ])
    expect((await stat(claudeSettingsPath)).mode & 0o777).toBe(0o640)
    expect(apiApplied.activatedSnapshot).toMatchObject({
      providerId: apiProvider.id,
      model: "claude-api-model",
      protocol: "anthropic-messages",
      authentication: "anthropic-api-key",
    })
    expect(claudeManaged.endpoint).not.toBe(codexBaseline.endpoint)
    expect(sensitiveDigest(claudeManaged.credential)).not.toBe(codexBaseline.credentialDigest)
    if (codexDatabaseWithoutPublicationSequence() !== codexBaseline.database) {
      throw new Error("claude-tracer-mutated-codex-database")
    }
    if (codexViewWithoutPublicationSequence() !== codexBaseline.view) {
      throw new Error("claude-tracer-published-codex-view")
    }
    if (canonicalJson(await safeFileFingerprint(codexConfigPath)) !== canonicalJson(codexBaseline.config)) {
      throw new Error("claude-tracer-mutated-codex-config")
    }

    const callsBeforeRejected = upstream.calls.length
    expect((await claudePost(claudeManaged.endpoint, wrongClaudeRouting, "/v1/messages", { messages: [] })).status).toBe(401)
    expect((await claudePost(claudeManaged.endpoint, codexConfig.credential, "/v1/messages", { messages: [] })).status).toBe(401)
    expect(upstream.calls.length).toBe(callsBeforeRejected)
    expect((await chunkedPost(codexConfig.endpoint, claudeManaged.credential)).status).toBe(401)
    expect(upstream.calls.length).toBe(callsBeforeRejected)

    const message = await claudePost(
      claudeManaged.endpoint,
      claudeManaged.credential,
      "/v1/messages?beta=true&fixture=exact",
      {
        model: "client-must-be-replaced",
        messages: [{ role: "user", content: "hello" }],
        tools: [{
          name: "fixture_tool",
          input_schema: { type: "object", properties: { value: { type: "string" } }, required: ["value"] },
        }],
        future_extension: { retained: true, nested: { version: 7 } },
      },
      {
        "anthropic-beta": ["tools-2025-01-01", "context-1m-2025-08-07"],
        "x-claude-code-fixture": "claude-contract-request",
      },
    )
    if (message.status === 422) {
      const reported = String(message.headers["x-fixture-contract-error"] ?? "")
      const category = ["query", "tools", "unknown", "beta"].includes(reported) ? reported : "version"
      throw new Error(`claude-fixture-contract-${category}`)
    }
    expect(message.status).toBe(200)
    assertClaudeUpstreamResponseHeaders(message.headers)
    expect(JSON.parse(message.body)).toMatchObject({ id: "msg_fixture", type: "message" })
    const counted = await claudePost(
      claudeManaged.endpoint,
      claudeManaged.credential,
      "/v1/messages/count_tokens",
      { model: "wrong", messages: [] },
    )
    expect(counted.status).toBe(200)
    assertClaudeUpstreamResponseHeaders(counted.headers)
    expect(JSON.parse(counted.body)).toEqual({ input_tokens: 7 })
    const errored = await claudePost(
      claudeManaged.endpoint,
      claudeManaged.credential,
      "/v1/messages",
      { messages: [], fixture_error: true },
    )
    expect(errored.status).toBe(429)
    assertClaudeUpstreamResponseHeaders(errored.headers)
    expect(errored.body).toBe('{"type":"error","error":{"type":"rate_limit_error","message":"fixture"}}')
    assertAuditHealthy()
    assertExactClaudeMessagesProjection(claudeRequestProjections[3]!)
    expect(claudeRequestProjections.slice(3, 6)).toMatchObject([
      { pathClass: "messages", authentication: "api-key", model: "claude-api-model" },
      { pathClass: "count-tokens", authentication: "api-key", model: "claude-api-model" },
      { pathClass: "messages", authentication: "api-key", model: "claude-api-model", error: true },
    ])

    const delayedResponse = fetch(`${claudeManaged.endpoint}/v1/messages`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${claudeManaged.credential}`,
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
      },
      body: JSON.stringify({ messages: [], stream: true, fixture_delay: true }),
    })
    await upstream.waitForDelayedStart()

    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API (Messages)"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API key"))
    await setup.mockInput.typeText("Claude Bearer")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(upstream.baseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("claude-bearer-model")
    setup.mockInput.pressTab()
    leader("h")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(bearerSecret)
    captureSecretSafeFrame(setup, selectedFrames, [
      ...allStaticSecrets,
      claudeManaged.credential,
      codexConfig.credential,
    ], "claude-bearer-entry")
    setup.mockInput.pressEnter()
    await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.providers.length === 2,
      [...allStaticSecrets, claudeManaged.credential, codexConfig.credential],
      "claude-bearer-save",
    )
    const bearerProvider = claudeSession!.get().providers.find((provider) => provider.name === "Claude Bearer")!
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude Bearer"))
    setup.mockInput.pressKey("down")
    leader("o")
    const bearerApplied = await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.currentProviderId === bearerProvider.id,
      [...allStaticSecrets, claudeManaged.credential, codexConfig.credential],
      "claude-bearer-switch",
    )
    expect(bearerApplied.takeover.endpoint).toBe(claudeManaged.endpoint)
    const switchedSettingsBytes = await readFile(claudeSettingsPath, "utf8")
    const switchedSettings = extractClaudeManagedSettings(switchedSettingsBytes, "claude-bearer-model", [
      ...allStaticSecrets,
      codexConfig.credential,
    ])
    auditClaudeSettingsSecrets(switchedSettingsBytes, claudeManaged.credential, [
      ...allStaticSecrets,
      codexConfig.credential,
    ])
    expect(switchedSettings.credential).toBe(claudeManaged.credential)
    expect(bearerApplied.activatedSnapshot!.id).not.toBe(apiApplied.activatedSnapshot!.id)

    const bearerResponse = await claudePost(
      claudeManaged.endpoint,
      claudeManaged.credential,
      "/v1/messages",
      { messages: [], future_extension: "new-snapshot" },
    )
    expect(bearerResponse.status).toBe(200)
    assertClaudeUpstreamResponseHeaders(bearerResponse.headers)
    upstream.releaseDelayed()
    const oldStream = await delayedResponse
    expect(oldStream.status).toBe(200)
    expect(oldStream.headers.get("x-upstream")).toBe("claude-fixture")
    expect(await oldStream.text()).toBe(CLAUDE_SSE_BYTES.join(""))
    await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.servingProviderId === apiProvider.id,
      [...allStaticSecrets, claudeManaged.credential, codexConfig.credential],
      "claude-pinned-api-key-serving",
    )
    const failbackResponse = await claudePost(
      claudeManaged.endpoint,
      claudeManaged.credential,
      "/v1/messages",
      { messages: [], future_extension: "post-pinned-failback" },
    )
    expect(failbackResponse.status).toBe(200)
    assertAuditHealthy()
    const delayedProjection = claudeRequestProjections.find((projection) => projection.delayed)
    expect(delayedProjection).toMatchObject({
      pathClass: "messages", authentication: "api-key", model: "claude-api-model", stream: true,
    })
    expect(claudeRequestProjections.at(-1)).toMatchObject({
      pathClass: "messages", authentication: "bearer", model: "claude-bearer-model",
      unknownDigest: sensitiveDigest('"post-pinned-failback"'),
    })
    await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.servingProviderId === bearerProvider.id,
      [...allStaticSecrets, claudeManaged.credential, codexConfig.credential],
      "claude-bearer-serving",
    )

    expect(await safeFileFingerprint(codexConfigPath)).toEqual(codexBaseline.config)
    if (codexDatabaseWithoutPublicationSequence() !== codexBaseline.database) {
      throw new Error("claude-tracer-mutated-codex-database-after-hot-switch")
    }
    expect(codexSession!.get()).toMatchObject({
      currentProviderId: codexProvider.id,
      servingProviderId: codexProvider.id,
      activatedSnapshot: { id: codexBaseline.snapshotId },
      takeover: { endpoint: codexBaseline.endpoint },
    })
    const codexResponse = await chunkedPost(codexConfig.endpoint, codexConfig.credential)
    expect(codexResponse.status).toBe(201)
    expect(codexResponse.body).toBe(SSE_BYTES.join(""))
    await waitForSecretSafeSession(
      codexSession!,
      (view) => view.servingProviderId === codexProvider.id,
      [...allStaticSecrets, claudeManaged.credential, codexConfig.credential],
      "codex-baseline-serving",
    )
    expect(await safeFileFingerprint(codexConfigPath)).toEqual(codexBaseline.config)
    if (codexDatabaseWithoutPublicationSequence() !== codexBaseline.database) {
      throw new Error("claude-tracer-mutated-codex-database-after-serving")
    }
    if (codexViewWithoutPublicationSequence() !== codexBaseline.view) {
      throw new Error("claude-tracer-published-codex-view-after-serving")
    }
    expect(codexSession!.get()).toMatchObject({
      currentProviderId: codexProvider.id,
      servingProviderId: codexProvider.id,
      activatedSnapshot: { id: codexBaseline.snapshotId },
      takeover: { endpoint: codexBaseline.endpoint },
    })
    expect(claudeSession!.get().servingProviderId).toBe(bearerProvider.id)

    setup.mockInput.pressCtrlC()
    await waitFor(() => setup!.renderer.isDestroyed, "Claude tracer renderer close")
    rendererAudit.stop()
    nativeFrames.push(...rendererAudit.frames())
    setup = undefined
    rendererAudit = undefined
    await closeSessions()
    const afterCloseClaude = await claudePost(
      claudeManaged.endpoint,
      claudeManaged.credential,
      "/v1/messages",
      { messages: [] },
    )
    const afterCloseCodex = await chunkedPost(codexConfig.endpoint, codexConfig.credential)
    expect(afterCloseClaude.status).toBe(200)
    expect(afterCloseCodex.status).toBe(201)
    expect(firstService.child.exitCode).toBeNull()
    expect(await safeFileFingerprint(codexConfigPath)).toEqual(codexBaseline.config)
    if (codexDatabaseWithoutPublicationSequence() !== codexBaseline.database) {
      throw new Error("claude-tracer-mutated-codex-database-after-close")
    }

    await stopService(firstService, firstShutdown)
    await expect(claudePost(claudeManaged.endpoint, claudeManaged.credential, "/v1/messages", { messages: [] })).rejects.toBeDefined()
    await expect(chunkedPost(codexConfig.endpoint, codexConfig.credential)).rejects.toBeDefined()

    const secondService = await startService(secondShutdown, "restart")
    await openSessions("claude-e2e-restart")
    expect(codexSession!.get()).toMatchObject({
      currentProviderId: codexProvider.id,
      activatedSnapshot: { id: codexBaseline.snapshotId },
      takeover: { endpoint: codexBaseline.endpoint },
    })
    if (codexDatabaseWithoutPublicationSequence() !== codexBaseline.database) {
      throw new Error("claude-tracer-mutated-codex-database-after-restart")
    }
    expect(await safeFileFingerprint(codexConfigPath)).toEqual(codexBaseline.config)
    expect(claudeSession!.get()).toMatchObject({
      currentProviderId: bearerProvider.id,
      activatedSnapshot: { id: bearerApplied.activatedSnapshot!.id, model: "claude-bearer-model" },
      takeover: { endpoint: claudeManaged.endpoint },
    })
    expect(extractManagedConfig(await readFile(codexConfigPath, "utf8"), "gpt-codex-baseline")).toEqual(codexConfig)
    const restartedSettingsBytes = await readFile(claudeSettingsPath, "utf8")
    expect(extractClaudeManagedSettings(restartedSettingsBytes, "claude-bearer-model", [
      ...allStaticSecrets,
      codexConfig.credential,
    ]).credential).toBe(claudeManaged.credential)
    auditClaudeSettingsSecrets(restartedSettingsBytes, claudeManaged.credential, [
      ...allStaticSecrets,
      codexConfig.credential,
    ])
    expect((await claudePost(claudeManaged.endpoint, claudeManaged.credential, "/v1/messages", { messages: [] })).status).toBe(200)
    expect((await chunkedPost(codexConfig.endpoint, codexConfig.credential)).status).toBe(201)
    if (codexDatabaseWithoutPublicationSequence() !== codexBaseline.database) {
      throw new Error("claude-tracer-mutated-codex-database-after-restart-serving")
    }

    const database = new Database(databasePath, { readonly: true })
    try {
      const credentialRows = database.query(`SELECT p.target, p.name, c.bearer_token AS secret
        FROM providers p JOIN credentials c ON c.id = p.credential_id ORDER BY p.target, p.position`).all() as Array<{
          target: string; name: string; secret: string
        }>
      expect(credentialRows.map(({ target, name, secret }) => ({
        target, name, digest: sensitiveDigest(secret),
      }))).toEqual([
        { target: "claude", name: "Claude API Key", digest: sensitiveDigest(apiKeySecret) },
        { target: "claude", name: "Claude Bearer", digest: sensitiveDigest(bearerSecret) },
        { target: "codex", name: "Codex Baseline", digest: sensitiveDigest(providerSecret) },
      ])
      const routes = database.query("SELECT target, routing_credential AS secret FROM target_route_state ORDER BY target").all() as Array<{
        target: string; secret: string
      }>
      expect(routes.map(({ target, secret }) => ({ target, digest: sensitiveDigest(secret) }))).toEqual([
        { target: "claude", digest: sensitiveDigest(claudeManaged.credential) },
        { target: "codex", digest: sensitiveDigest(codexConfig.credential) },
      ])
      const snapshots = database.query(`SELECT s.target, p.name,
        s.provider_bearer_token AS secret FROM activated_snapshots s
        JOIN providers p ON p.id = s.provider_id ORDER BY s.target, p.position`).all() as Array<{
          target: string; name: string; secret: string
        }>
      expect(snapshots.map(({ target, name, secret }) => ({
        target, name, digest: sensitiveDigest(secret),
      }))).toEqual([
        { target: "claude", name: "Claude API Key", digest: sensitiveDigest(apiKeySecret) },
        { target: "claude", name: "Claude Bearer", digest: sensitiveDigest(bearerSecret) },
        { target: "codex", name: "Codex Baseline", digest: sensitiveDigest(providerSecret) },
      ])
      const receipts = database.query("SELECT outcome_json FROM action_receipts ORDER BY target, committed_revision").all()
      scanNoSecrets(receipts, [
        ...allStaticSecrets,
        claudeManaged.credential,
        codexConfig.credential,
      ], "claude-action-receipts")
    } finally {
      database.close()
    }
    auditSqliteSecretLocations(databasePath, {
      providerSecrets: [
        { target: "claude", name: "Claude API Key", secret: apiKeySecret },
        { target: "claude", name: "Claude Bearer", secret: bearerSecret },
        { target: "codex", name: "Codex Baseline", secret: providerSecret },
      ],
      routingSecrets: [
        { target: "claude", secret: claudeManaged.credential },
        { target: "codex", secret: codexConfig.credential },
      ],
    })

    await writeFile(secondShutdown, "shutdown\n", { mode: 0o600 })
    const finalResult = await Promise.race([secondService.output.completed, Bun.sleep(deadlineMs).then(() => undefined)])
    if (!finalResult) throw new Error("claude-final-drain-timeout")
    expect(finalResult).toEqual({ code: 0, signal: null })
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    await expect(claudePost(claudeManaged.endpoint, claudeManaged.credential, "/v1/messages", { messages: [] })).rejects.toBeDefined()
    await expect(chunkedPost(codexConfig.endpoint, codexConfig.credential)).rejects.toBeDefined()
    const assertSessionDrained = async (session: TargetSession, target: "codex" | "claude") => {
      let rejected = false
      try {
        await Promise.race([
          session.act({
            kind: "reorder-providers",
            providerIds: session.get().providers.map(({ id }) => id),
          }),
          Bun.sleep(1_000).then(() => { throw new Error("drained-session-action-timeout") }),
        ])
      } catch (error) {
        scanNoSecrets([asyncErrorSurface(error)], [
          ...allStaticSecrets,
          claudeManaged.credential,
          codexConfig.credential,
        ], `drained-${target}-session-error`)
        rejected = true
      }
      if (!rejected) throw new Error(`drained-${target}-session-accepted-action`)
    }
    await assertSessionDrained(codexSession!, "codex")
    await assertSessionDrained(claudeSession!, "claude")

    expect((await stat(databasePath)).mode & 0o777).toBe(0o600)
    expect((await stat(muxviaHome)).mode & 0o777).toBe(0o700)
  } finally {
    const completeSecrets = () => [
      ...allStaticSecrets,
      ...(claudeRoutingCredential ? [claudeRoutingCredential] : []),
      ...(codexRoutingCredential ? [codexRoutingCredential] : []),
    ]
    await runClaudeSecurityFinalizer([
      { name: "release-delayed", run: () => upstream.releaseDelayed() },
      { name: "native-recorder-drain", run: () => {
        if (!rendererAudit) return
        rendererAudit.stop()
        nativeFrames.push(...rendererAudit.frames())
      } },
      { name: "renderer-destroy", run: () => {
        if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
      } },
      { name: "session-close", run: closeSessions },
      { name: "shutdown-signal-first", run: () => writeFile(firstShutdown, "shutdown\n", { mode: 0o600 }) },
      { name: "shutdown-signal-second", run: () => writeFile(secondShutdown, "shutdown\n", { mode: 0o600 }) },
      { name: "process-kill", run: () => {
        for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
      } },
      { name: "process-output-drain", run: async () => {
        const results = await Promise.allSettled(services.map(({ output }) => Promise.race([
          output.completed,
          Bun.sleep(deadlineMs).then(() => { throw new Error("process-output-drain-timeout") }),
        ])))
        if (results.some((result) => result.status === "rejected")) {
          throw new Error("process-output-drain-incomplete")
        }
      } },
      { name: "upstream-drain", run: () => upstream.stop() },
      { name: "codex-credential-recovery", run: async () => {
        if (codexRoutingCredential) return
        try {
          const raw = await readFile(codexConfigPath, "utf8")
          scanNoSecrets([raw], allStaticSecrets, "claude-final-codex-config-raw")
          codexRoutingCredential = raw.match(/X-Muxvia-Routing-Credential"\s*=\s*"([a-f0-9]{64})"/)?.[1]
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error
        }
      } },
      { name: "claude-credential-recovery", run: async () => {
        if (claudeRoutingCredential) return
        try {
          const raw = await readFile(claudeSettingsPath, "utf8")
          claudeRoutingCredential = auditAndExtractClaudeRoutingCredential(raw, [
            ...allStaticSecrets,
            ...(codexRoutingCredential ? [codexRoutingCredential] : []),
          ]).credential
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error
        }
      } },
      { name: "upstream-request-audit", run: assertAuditHealthy },
      { name: "raw-rpc", run: () => {
        if (rpcStreams.some((stream) => stream.length > 0)) {
          scanRawRpcFramesNoSecrets(rpcStreams, completeSecrets())
        }
      } },
      { name: "zero-secret-observations", run: () => {
        scanNoSecrets(
          [decodedFrames, views, selectedFrames, nativeFrames],
          completeSecrets(),
          "claude-zero-secret-observations",
        )
      } },
      { name: "process-output", run: () => {
        scanProcessOutputNoSecrets(services.map(({ output }) => output.streams).flat(), completeSecrets())
      } },
      { name: "trap", run: async () => {
        if (await controlledTreeFingerprint(trapHome) !== trapFingerprint) {
          throw new Error("claude-environment-trap-mutated")
        }
      } },
    ])
  }
}, 60_000)

test("real processes prove Codex direct activation is control-only and survives restart", async () => {
  const root = await mkdtemp(join(tmpdir(), "muxvia-direct-e2e-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const codexHome = join(userHome, ".codex")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const configPath = join(codexHome, "config.toml")
  const authPath = join(codexHome, "auth.json")
  const directModel = "gpt-direct"
  const editedModel = "gpt-direct-edited"
  const directSecrets = [providerSecret, wrongRoutingSecret, authSentinel]
  await mkdir(codexHome, { recursive: true, mode: 0o700 })
  await writeFile(configPath, [
    "# operator comment survives",
    'unrelated = "keep-me"',
    "",
    "[operator_settings]",
    'theme = "dark"',
    "nested = { enabled = true, count = 2 }",
    "",
  ].join("\n"), { mode: 0o640 })
  await chmod(configPath, 0o640)
  await writeFile(authPath, `{"tokens":"${authSentinel}"}\n`, { mode: 0o600 })
  await chmod(authPath, 0o600)
  await chmod(fakeCodex, 0o755)
  const originalConfigMode = (await stat(configPath)).mode & 0o777
  const authFingerprint = await safeFileFingerprint(authPath)
  assertExactFileMode(originalConfigMode, 0o640)
  expect(authFingerprint.mode).toBe(0o600)

  const requestAudit = createCapturedRequestAudit({
    providerCredential: providerSecret,
    forbiddenRoutingCredentials: () => [wrongRoutingSecret, authSentinel],
    expectedRequestCount: 0,
  })
  const upstream = await startFakeUpstream(providerSecret, requestAudit.observe)
  const rpcStreams: Buffer[][] = []
  const outboundAudits: ReturnType<typeof createOutboundOperationAudit>[] = []
  const outboundOperationKinds: string[] = []
  const decodedInboundFrames: unknown[] = []
  const views: TargetView[] = []
  const selectedRenderedFrames: string[] = []
  const nativeRenderedFrames: string[] = []
  const services: Array<{
    child: ReturnType<typeof spawn>
    output: ReturnType<typeof captureProcessOutput>
  }> = []
  let client: RpcClient | undefined
  let session: TargetSession | undefined
  let unsubscribe: (() => void) | undefined
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let rendererAudit: ReturnType<typeof createRendererAudit> | undefined

  const collectView = (view: TargetView, label: string) => {
    assertSecretSafeStructured(view, directSecrets, label)
    views.push(view)
  }

  const startService = async () => {
    const child = spawn(serviceBinary, [
      "--home", muxviaHome,
      "--test-codex-executable", fakeCodex,
    ], {
      cwd: root,
      env: {
        HOME: userHome,
        PATH: `${dirname(fakeCodex)}:/usr/bin:/bin`,
        MUXVIA_INTEGRATION_TEST: "1",
      },
      stdio: ["ignore", "pipe", "pipe"],
    })
    const record = { child, output: captureProcessOutput(child) }
    services.push(record)
    await waitFor(async () => {
      try {
        await stat(socketPath)
        return true
      } catch { return false }
    }, "Direct control socket")
    return record
  }
  const connect = async (release: string) => {
    const stream: Buffer[] = []
    const decoder = new FrameDecoder()
    rpcStreams.push(stream)
    return await RpcClient.connect(socketPath, release, undefined, (path) => {
      const socket = createConnection({ path })
      const outboundAudit = createOutboundOperationAudit()
      outboundAudits.push(outboundAudit)
      const write = socket.write.bind(socket)
      socket.write = ((chunk: Uint8Array, callback?: (error?: Error | null) => void) => {
        const beforeCount = outboundAudit.operationKinds.length
        outboundAudit.observe(chunk)
        outboundOperationKinds.push(...outboundAudit.operationKinds.slice(beforeCount))
        return write(chunk, callback)
      }) as typeof socket.write
      socket.on("data", (chunk) => {
        const bytes = Buffer.from(chunk)
        stream.push(bytes)
        for (const frame of decoder.push(bytes)) {
          assertSecretSafeStructured(frame, directSecrets, "direct-decoded-inbound-frame")
          decodedInboundFrames.push(frame)
        }
      })
      return socket
    })
  }
  const waitForCleanServiceExit = async (
    record: { child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> },
    label: string,
  ) => {
    const result = await Promise.race([
      record.output.completed,
      Bun.sleep(deadlineMs).then(() => undefined),
    ])
    if (!result) throw new Error(`Timed out waiting for ${label}`)
    expect(result).toEqual({ code: 0, signal: null })
    expect(record.child.exitCode).toBe(0)
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
  }
  const leader = (key: string) => {
    setup!.mockInput.pressKey("x", { ctrl: true })
    setup!.mockInput.pressKey(key)
  }
  const closeControlPlane = async () => {
    setup!.mockInput.pressCtrlC()
    await waitFor(() => setup!.renderer.isDestroyed, "Direct renderer destroy")
    rendererAudit!.stop()
    const recordedFrames = rendererAudit!.frames()
    assertSecretSafeStructured(recordedFrames, directSecrets, "direct-native-renderer-frames")
    nativeRenderedFrames.push(...recordedFrames)
    setup = undefined
    rendererAudit = undefined
    unsubscribe?.()
    unsubscribe = undefined
    await session!.close()
    session = undefined
    client = undefined
  }

  try {
    const firstService = await startService()
    client = await connect("direct-e2e")
    session = await client.openTarget("codex")
    collectView(session.get() as TargetView, "direct-initial-view")
    unsubscribe = session.subscribe((view) => collectView(view, "direct-subscribed-view"))
    setup = await testRender(() => <App session={session!} />, {
      width: 80,
      height: 28,
      useThread: false,
      kittyKeyboard: true,
    })
    rendererAudit = createRendererAudit(setup)
    rendererAudit.start()
    await setup.renderOnce()
    captureFrame(setup, selectedRenderedFrames)
    await setup.mockInput.typeText("/codex")
    setup.mockInput.pressEnter()
    const codexFrame = await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    selectedRenderedFrames.push(codexFrame)

    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("OpenAI API (Responses)"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter save"))
    await setup.mockInput.typeText("Direct Provider")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(upstream.baseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(directModel)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(providerSecret)
    await setup.renderOnce()
    captureSecretSafeFrame(
      setup,
      selectedRenderedFrames,
      directSecrets,
      "direct-provider-credential-entry",
    )
    setup.mockInput.pressEnter()
    const savedView = await waitForSecretSafeSession(
      session,
      (view) => view.providers.length === 1,
      directSecrets,
      "direct-provider-save",
    )
    await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Provider saved: Direct Provider"),
      directSecrets,
      "direct-provider-saved",
    )
    const savedProvider = savedView.providers[0]!
    assertSecretSafeStructured(savedProvider, directSecrets, "direct-saved-provider", (safeProvider) => {
      expect(safeProvider).toMatchObject({
        name: "Direct Provider",
        baseUrl: upstream.baseUrl,
        model: directModel,
        routingRequirement: "direct-compatible",
        credential: "present",
        completeness: "complete",
      })
    })
    expect(upstream.calls.length === 0).toBeTrue()

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    const providerPickerFrame = await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Providers") && frame.includes("Direct Provider"),
      directSecrets,
      "direct-provider-picker",
    )
    selectedRenderedFrames.push(providerPickerFrame)
    leader("a")
    const directView = await waitForSecretSafeSession(
      session,
      (view) => view.mode === "direct",
      directSecrets,
      "direct-activation",
    )
    const directFrame = await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Mode       Direct")
        && frame.includes("Current Target Provider  Direct Provider")
        && frame.includes("Direct Activation applied: Direct Provider")
        && frame.includes("Restart Codex to use the managed configuration."),
      directSecrets,
      "direct-activation-result",
    )
    selectedRenderedFrames.push(directFrame)

    collectView(directView as TargetView, "direct-applied-view")
    assertSecretSafeStructured(directView, directSecrets, "direct-applied-view", (safeView) => {
      expect(safeView).toMatchObject({
        mode: "direct",
        takeover: { state: "inactive", endpoint: null },
        currentProviderId: savedProvider.id,
        servingProviderId: null,
        managedConfiguration: { state: "applied", path: configPath, restartRequired: true },
        recovery: { state: "committed" },
        activatedSnapshot: { providerId: savedProvider.id, model: directModel },
      })
    })

    const directConfigBytes = await readFile(configPath)
    const expectedDirectConfig = [
      "# operator comment survives",
      'unrelated = "keep-me"',
      `model = "${directModel}"`,
      'model_provider = "muxvia_codex"',
      "",
      "[operator_settings]",
      'theme = "dark"',
      "nested = { enabled = true, count = 2 }",
      "",
      "[model_providers]",
      "",
      "[model_providers.muxvia_codex]",
      'name = "Muxvia Direct"',
      `base_url = "${upstream.baseUrl}"`,
      'wire_api = "responses"',
      `http_headers = { Authorization = "Bearer ${providerSecret}" }`,
      "supports_websockets = false",
      "",
    ].join("\n")
    assertExactDirectManagedConfig(directConfigBytes.toString("utf8"), expectedDirectConfig)
    assertExactFileMode((await stat(configPath)).mode & 0o777, originalConfigMode)
    expect(await safeFileFingerprint(authPath)).toEqual(authFingerprint)

    const directDatabase = new Database(databasePath, { readonly: true })
    const route = directDatabase.query(`SELECT current_provider_id AS currentProviderId,
      serving_provider_id IS NULL AS servingAbsent,
      takeover_state AS takeoverState,
      route_port IS NULL AS routePortAbsent,
      routing_credential IS NULL AS routingCredentialAbsent,
      activated_snapshot_id AS snapshotId,
      managed_config_path AS managedConfigPath,
      recovery_state AS recoveryState
      FROM target_route_state WHERE target = 'codex'`).get() as Record<string, unknown>
    const snapshot = directDatabase.query(`SELECT id, provider_id AS providerId, base_url AS baseUrl,
      model, provider_bearer_token = ? AS providerCredentialMatches, epoch
      FROM activated_snapshots WHERE target = 'codex'`).get(providerSecret) as Record<string, unknown>
    directDatabase.close()
    expect(route).toMatchObject({
      currentProviderId: savedProvider.id,
      servingAbsent: 1,
      takeoverState: "inactive",
      routePortAbsent: 1,
      routingCredentialAbsent: 1,
      snapshotId: directView.activatedSnapshot!.id,
      managedConfigPath: configPath,
      recoveryState: "clean",
    })
    expect(snapshot).toMatchObject({
      id: directView.activatedSnapshot!.id,
      providerId: savedProvider.id,
      baseUrl: upstream.baseUrl,
      model: directModel,
      providerCredentialMatches: 1,
    })
    if (!firstService.child.pid) throw new Error("direct-service-pid-missing")
    await assertNoTcpListeners(firstService.child.pid)
    expect((await stat(socketPath)).isSocket()).toBeTrue()

    const updateOutcome = await actSecretSafe(session, {
      kind: "update-provider",
      providerId: savedProvider.id,
      providerRevision: savedProvider.providerRevision,
      name: "Direct Provider Edited",
      baseUrl: `${upstream.baseUrl}/edited`,
      model: editedModel,
      credential: { kind: "keep" },
    }, directSecrets, "direct-provider-edit")
    collectView(updateOutcome.view, "direct-provider-edit-view")
    assertSecretSafeStructured(updateOutcome.view, directSecrets, "direct-provider-edit-view", (safeView) => {
      expect(safeView.providers[0]).toMatchObject({
        id: savedProvider.id,
        baseUrl: `${upstream.baseUrl}/edited`,
        model: editedModel,
      })
      expect(safeView.activatedSnapshot).toMatchObject({
        id: directView.activatedSnapshot!.id,
        providerId: savedProvider.id,
        model: directModel,
      })
    })
    expect(sensitiveDigest(await readFile(configPath))).toBe(sensitiveDigest(directConfigBytes))

    const immutableDatabase = new Database(databasePath, { readonly: true })
    const immutableSnapshot = immutableDatabase.query(`SELECT id, provider_id AS providerId,
      base_url AS baseUrl, model, provider_bearer_token = ? AS providerCredentialMatches
      FROM activated_snapshots WHERE target = 'codex'`).get(providerSecret) as Record<string, unknown>
    const editedDeclaration = immutableDatabase.query(`SELECT id, base_url AS baseUrl, model
      FROM providers WHERE id = ?`).get(savedProvider.id) as Record<string, unknown>
    immutableDatabase.close()
    expect(immutableSnapshot).toMatchObject({
      id: directView.activatedSnapshot!.id,
      providerId: savedProvider.id,
      baseUrl: upstream.baseUrl,
      model: directModel,
      providerCredentialMatches: 1,
    })
    expect(editedDeclaration).toMatchObject({
      id: savedProvider.id,
      baseUrl: `${upstream.baseUrl}/edited`,
      model: editedModel,
    })
    assertSecretSafeStructured(session.get(), directSecrets, "direct-post-edit-view", (safeView) => {
      expect(safeView.takeover.endpoint).toBeNull()
    })
    expect(upstream.calls.length === 0).toBeTrue()

    await closeControlPlane()
    await waitForCleanServiceExit(firstService, "Direct Routing Service idle exit")

    const restartedService = await startService()
    client = await connect("direct-e2e-restart")
    session = await client.openTarget("codex")
    const restartedView = session.get()
    collectView(restartedView as TargetView, "direct-restarted-view")
    assertSecretSafeStructured(restartedView, directSecrets, "direct-restarted-view", (safeView) => {
      expect(safeView).toMatchObject({
        managementRevision: updateOutcome.view.managementRevision,
        mode: "direct",
        takeover: { state: "inactive", endpoint: null },
        currentProviderId: savedProvider.id,
        servingProviderId: null,
        managedConfiguration: { state: "applied", path: configPath, restartRequired: true },
        activatedSnapshot: {
          id: directView.activatedSnapshot!.id,
          providerId: savedProvider.id,
          model: directModel,
        },
      })
    })
    expect(restartedView.service.epoch === directView.service.epoch).toBeFalse()
    unsubscribe = session.subscribe((view) => collectView(view, "direct-restarted-subscribed-view"))
    setup = await testRender(() => <App session={session!} />, {
      width: 80,
      height: 28,
      useThread: false,
      kittyKeyboard: true,
    })
    rendererAudit = createRendererAudit(setup)
    rendererAudit.start()
    await setup.renderOnce()
    await setup.mockInput.typeText("/codex")
    setup.mockInput.pressEnter()
    const restartedFrame = await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Mode       Direct")
        && frame.includes("Current Target Provider  Direct Provider Edited")
        && frame.includes(`Activated Snapshot  Direct Provider Edited · ${directModel}`)
        && frame.includes("Restart Codex to use the managed configuration."),
      directSecrets,
      "direct-restarted-render",
    )
    selectedRenderedFrames.push(restartedFrame)
    expect(restartedView.takeover.endpoint).toBeNull()
    if (!restartedService.child.pid) throw new Error("direct-service-pid-missing")
    await assertNoTcpListeners(restartedService.child.pid)
    expect((await stat(socketPath)).isSocket()).toBeTrue()
    expect(sensitiveDigest(await readFile(configPath))).toBe(sensitiveDigest(directConfigBytes))
    assertExactFileMode((await stat(configPath)).mode & 0o777, originalConfigMode)
    expect(await safeFileFingerprint(authPath)).toEqual(authFingerprint)
    expect(upstream.calls.length === 0).toBeTrue()

    await closeControlPlane()
    await waitForCleanServiceExit(restartedService, "restarted Direct Routing Service idle exit")

    await upstream.quiesce()
    requestAudit.assertComplete(upstream.calls.length)
    expect(upstream.calls.length === 0).toBeTrue()
    const receiptDatabase = new Database(databasePath, { readonly: true })
    const receipts = receiptDatabase.query(`SELECT action_kind AS actionKind,
      outcome_json AS outcomeJson FROM action_receipts ORDER BY committed_revision, action_id`).all()
    assertSecretSafeStructured(receipts, directSecrets, "direct-action-receipts")
    const finalRoute = receiptDatabase.query(`SELECT takeover_state AS takeoverState,
      route_port IS NULL AS routePortAbsent,
      routing_credential IS NULL AS routingCredentialAbsent
      FROM target_route_state WHERE target = 'codex'`).get()
    receiptDatabase.close()
    expect(receipts.some((receipt) => (receipt as { actionKind: string }).actionKind === "activate-provider")).toBeTrue()
    expect(finalRoute).toEqual({
      takeoverState: "inactive",
      routePortAbsent: 1,
      routingCredentialAbsent: 1,
    })
    for (const audit of outboundAudits) audit.finish()
    expect(outboundOperationKinds.some((kind) =>
      kind === "discover-models" || kind === "check-reachability"
    )).toBeFalse()
    scanRawRpcFramesNoSecrets(rpcStreams, directSecrets)
    scanNoSecrets(
      [decodedInboundFrames, receipts, views, selectedRenderedFrames, nativeRenderedFrames],
      directSecrets,
      "direct-inbound-and-rendered-surfaces",
    )
    scanProcessOutputNoSecrets(services.map(({ output }) => output.streams).flat(), directSecrets)
  } finally {
    rendererAudit?.stop()
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    unsubscribe?.()
    await session?.close().catch(() => {})
    await client?.close().catch(() => {})
    for (const { child } of services) {
      if (child.exitCode === null) child.kill("SIGKILL")
    }
    await Promise.all(services.map(({ output }) => output.completed.catch(() => undefined)))
    await upstream.stop()
  }
})

test("real processes prove Claude Direct authentication replacement and natural idle restart", async () => {
  const root = await mkdtemp(join(tmpdir(), "m6-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const codexHome = join(userHome, ".codex")
  const claudeHome = join(userHome, ".claude")
  const settingsPath = join(claudeHome, "settings.json")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const trapHome = join(root, "outside-home-traps")
  const inheritedTraps = {
    HOME: trapHome,
    CODEX_HOME: join(trapHome, ".codex"),
    CLAUDE_CONFIG_DIR: join(trapHome, ".claude"),
  }
  const bearerSecret = "claude-direct-bearer-secret-must-not-escape"
  const apiKeySecret = "claude-direct-api-key-secret-must-not-escape"
  const priorBearer = "claude-direct-prior-bearer-must-not-escape"
  const priorApiKey = "claude-direct-prior-api-key-must-not-escape"
  const bearerBaseUrl = "https://claude-bearer-direct.invalid/v1"
  const apiKeyBaseUrl = "https://claude-api-key-direct.invalid/v1"
  const bearerModel = "claude-direct-bearer-model"
  const apiKeyModel = "claude-direct-api-key-model"
  const directSecrets = [bearerSecret, apiKeySecret, priorBearer, priorApiKey]
  const bearerSettings: ClaudeDirectSettingsExpectation = {
    authentication: "anthropic-bearer",
    baseUrl: bearerBaseUrl,
    credential: bearerSecret,
    model: bearerModel,
  }
  const apiKeySettings: ClaudeDirectSettingsExpectation = {
    authentication: "anthropic-api-key",
    baseUrl: apiKeyBaseUrl,
    credential: apiKeySecret,
    model: apiKeyModel,
  }
  const sqliteSecretPolicy: SqliteSecretPolicy = {
    providerSecrets: [
      { target: "claude", name: "Claude Direct Bearer", secret: bearerSecret },
      { target: "claude", name: "Claude Direct API Key", secret: apiKeySecret },
      { target: "claude", name: "Prior Bearer", secret: priorBearer },
      { target: "claude", name: "Prior API Key", secret: priorApiKey },
    ],
    routingSecrets: [],
  }

  await mkdir(codexHome, { recursive: true, mode: 0o700 })
  await mkdir(claudeHome, { recursive: true, mode: 0o700 })
  await mkdir(join(trapHome, ".muxvia/state"), { recursive: true, mode: 0o700 })
  await mkdir(join(trapHome, ".codex"), { recursive: true, mode: 0o700 })
  await mkdir(join(trapHome, ".claude"), { recursive: true, mode: 0o700 })
  await writeFile(join(trapHome, ".muxvia/state/canary"), "muxvia-trap\n", { mode: 0o600 })
  await writeFile(join(trapHome, ".codex/config.toml"), 'trap = "codex"\n', { mode: 0o600 })
  await writeFile(join(trapHome, ".claude/settings.json"), '{"trap":"claude"}\n', { mode: 0o600 })
  await writeFile(settingsPath, JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] },
    env: {
      OPERATOR_UNRELATED: "keep-me",
      ANTHROPIC_AUTH_TOKEN: priorBearer,
      ANTHROPIC_API_KEY: priorApiKey,
    },
  }), { mode: 0o640 })
  await chmod(settingsPath, 0o640)
  await chmod(fakeCodex, 0o755)
  await chmod(fakeClaude, 0o755)
  const originalMode = (await stat(settingsPath)).mode & 0o777
  const trapFingerprint = await controlledTreeFingerprint(trapHome)
  assertExactFileMode(originalMode, 0o640)

  const services: Array<{
    child: ReturnType<typeof spawn>
    output: ReturnType<typeof captureProcessOutput>
  }> = []
  const rpcStreams: Buffer[][] = []
  const decodedFrames: unknown[] = []
  const views: TargetView[] = []
  const selectedFrames: string[] = []
  const nativeFrames: string[] = []
  const outboundAudits: ReturnType<typeof createOutboundOperationAudit>[] = []
  const outboundOperationKinds: string[] = []
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  let codexSession: TargetSession | undefined
  let claudeSession: TargetSession | undefined
  let unsubscribes: Array<() => void> = []
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let rendererAudit: ReturnType<typeof createRendererAudit> | undefined
  let settingsPhase: ClaudeDirectSettingsAuditPhase = {
    kind: "unmanaged",
    priorAuthToken: priorBearer,
    priorApiKey,
  }

  const collectView = (view: TargetView, label: string) => {
    assertSecretSafeStructured(view, directSecrets, label)
    views.push(view)
  }
  const startService = async (label: string) => {
    const inheritedEnvironment: NodeJS.ProcessEnv = {
      ...process.env,
      ...inheritedTraps,
      PATH: `${dirname(fakeClaude)}:/usr/bin:/bin`,
      MUXVIA_INTEGRATION_TEST: "1",
    }
    assertChildVisibleEnvironment(inheritedTraps, inheritedEnvironment, trapHome, trapHome)
    const environment: NodeJS.ProcessEnv = { ...inheritedEnvironment, HOME: userHome }
    delete environment.CODEX_HOME
    delete environment.CLAUDE_CONFIG_DIR
    const child = spawn(serviceBinary, [
      "--home", muxviaHome,
      "--test-codex-executable", fakeCodex,
      "--test-claude-executable", fakeClaude,
    ], {
      cwd: root,
      env: environment,
      stdio: ["ignore", "pipe", "pipe"],
    })
    const record = { child, output: captureProcessOutput(child) }
    services.push(record)
    await waitFor(async () => {
      if (child.exitCode !== null) {
        scanProcessOutputNoSecrets(record.output.streams, directSecrets)
        const output = Buffer.concat(record.output.streams.flat()).toString("utf8")
        const category = output.includes("another Routing Service")
          ? "lock-collision"
          : output.includes("state is unavailable")
            ? "state"
            : output.includes("control transport failed")
              ? "control"
              : output.includes("I/O failed")
                ? "io"
                : output.includes("test-only Routing Service options")
                  ? "integration-guard"
                  : "unknown"
        throw new Error(`claude-direct-service-exited-before-connect:${label}:${category}`)
      }
      try { return (await stat(socketPath)).isSocket() } catch { return false }
    }, `Claude Direct ${label} control socket`)
    return record
  }
  const connect = async (release: string) => {
    const stream: Buffer[] = []
    const decoder = new FrameDecoder()
    rpcStreams.push(stream)
    return await RpcClient.connect(socketPath, release, undefined, (path) => {
      const socket = createConnection({ path })
      const outboundAudit = createOutboundOperationAudit()
      outboundAudits.push(outboundAudit)
      const write = socket.write.bind(socket)
      socket.write = ((chunk: Uint8Array, callback?: (error?: Error | null) => void) => {
        const beforeCount = outboundAudit.operationKinds.length
        outboundAudit.observe(chunk)
        outboundOperationKinds.push(...outboundAudit.operationKinds.slice(beforeCount))
        return write(chunk, callback)
      }) as typeof socket.write
      socket.on("data", (chunk) => {
        const bytes = Buffer.from(chunk)
        stream.push(bytes)
        for (const frame of decoder.push(bytes)) {
          assertSecretSafeStructured(frame, directSecrets, "claude-direct-decoded-frame")
          decodedFrames.push(frame)
        }
      })
      return socket
    })
  }
  const openSessions = async (release: string) => {
    codexClient = await connect(`${release}-codex`)
    claudeClient = await connect(`${release}-claude`)
    codexSession = await codexClient.openTarget("codex")
    claudeSession = await claudeClient.openTarget("claude", {
      claudeConfigDir: null,
      selectorState: "unset",
      hostManagedState: "unmanaged",
      cwd: root,
    })
    collectView(codexSession.get() as TargetView, "claude-direct-codex-view")
    collectView(claudeSession.get() as TargetView, "claude-direct-claude-view")
    unsubscribes = [
      codexSession.subscribe((view) => collectView(view as TargetView, "claude-direct-codex-push")),
      claudeSession.subscribe((view) => collectView(view as TargetView, "claude-direct-claude-push")),
    ]
  }
  const closeSessions = async () => {
    unsubscribes.splice(0).forEach((unsubscribe) => unsubscribe())
    const sessions = [codexSession, claudeSession]
    const clients = [codexClient, claudeClient]
    codexSession = undefined
    claudeSession = undefined
    codexClient = undefined
    claudeClient = undefined
    await Promise.all(sessions.map((session) => session?.close().catch(() => undefined)))
    await Promise.all(clients.map((client) => client?.close().catch(() => undefined)))
  }
  const closeRenderer = async () => {
    setup!.mockInput.pressCtrlC()
    await waitFor(() => setup!.renderer.isDestroyed, "Claude Direct renderer destroy")
    rendererAudit!.stop()
    const frames = rendererAudit!.frames()
    scanNoSecrets(frames, directSecrets, "claude-direct-native-renderer-frames")
    nativeFrames.push(...frames)
    setup = undefined
    rendererAudit = undefined
  }
  const waitForCleanExit = async (
    record: { child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> },
    label: string,
  ) => {
    const result = await Promise.race([
      record.output.completed,
      Bun.sleep(deadlineMs).then(() => undefined),
    ])
    if (!result) throw new Error(`claude-direct-natural-exit-timeout:${label}`)
    if (result.code !== 0 || result.signal !== null || record.child.exitCode !== 0) {
      throw new Error(`claude-direct-natural-exit-failed:${label}`)
    }
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
  }
  const leader = (key: string) => {
    setup!.mockInput.pressKey("x", { ctrl: true })
    setup!.mockInput.pressKey(key)
  }

  try {
    const firstService = await startService("first")
    await openSessions("claude-direct-first")
    setup = await testRender(() => <App sessions={{ codex: codexSession!, claude: claudeSession! }} />, {
      width: 100,
      height: 28,
      useThread: false,
      kittyKeyboard: true,
    })
    rendererAudit = createRendererAudit(setup)
    rendererAudit.start()
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Claude Code") && frame.includes("Run a target action"),
      directSecrets,
      "claude-direct-target",
    )

    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await waitForSecretSafeFrame(setup, (frame) => frame.includes("Anthropic API (Messages)"), directSecrets, "claude-direct-bearer-preset")
    setup.mockInput.pressEnter()
    await waitForSecretSafeFrame(setup, (frame) => frame.includes("Anthropic API key"), directSecrets, "claude-direct-bearer-editor")
    await setup.mockInput.typeText("Claude Direct Bearer")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(bearerBaseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(bearerModel)
    setup.mockInput.pressTab()
    leader("h")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(bearerSecret)
    captureSecretSafeFrame(setup, selectedFrames, directSecrets, "claude-direct-bearer-entry")
    setup.mockInput.pressEnter()
    const bearerSaved = await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.providers.length === 1,
      directSecrets,
      "claude-direct-bearer-save",
    )
    const bearerProvider = bearerSaved.providers[0]!
    assertSecretSafeStructured(bearerProvider, directSecrets, "claude-direct-bearer-provider", (provider) => {
      expect(provider).toMatchObject({
        name: "Claude Direct Bearer",
        baseUrl: bearerBaseUrl,
        model: bearerModel,
        protocol: "anthropic-messages",
        authentication: "anthropic-bearer",
        routingRequirement: "direct-compatible",
        credential: "present",
        completeness: "complete",
      })
    })
    await waitFor(() => views.some((view) => view.target === "claude" && view.providers.length === 1), "Claude Direct bearer save push")

    const bearerFrameStart = decodedFrames.length
    await setup.mockInput.typeText("/direct")
    settingsPhase = { kind: "direct", expected: bearerSettings }
    setup.mockInput.pressEnter()
    const bearerView = await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.mode === "direct" && view.currentProviderId === bearerProvider.id,
      directSecrets,
      "claude-direct-bearer-activation",
    )
    const bearerAppliedFrame = await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Mode       Direct")
        && frame.includes("Current Target Provider  Claude Direct Bearer")
        && frame.includes("Direct Activation applied: Claude Direct Bearer")
        && frame.includes("Restart Claude Code to use the managed configuration."),
      directSecrets,
      "claude-direct-bearer-applied",
    )
    selectedFrames.push(bearerAppliedFrame)
    await assertClaudeDirectResponseAndPush(decodedFrames, bearerFrameStart, {
      providerId: bearerProvider.id,
      model: bearerModel,
      settingsPath,
    }, directSecrets, "claude-direct-bearer")
    assertExactClaudeDirectSettings(await readFile(settingsPath, "utf8"), bearerSettings, [
      apiKeySecret,
      priorBearer,
      priorApiKey,
    ])
    assertExactFileMode((await stat(settingsPath)).mode & 0o777, originalMode)
    assertSecretSafeStructured(bearerView, directSecrets, "claude-direct-bearer-view", (view) => {
      expect(view).toMatchObject({
        target: "claude",
        mode: "direct",
        takeover: { state: "inactive", endpoint: null },
        currentProviderId: bearerProvider.id,
        servingProviderId: null,
        managedConfiguration: { state: "applied", path: settingsPath, restartRequired: true },
        recovery: { state: "committed" },
        activatedSnapshot: {
          providerId: bearerProvider.id,
          model: bearerModel,
          protocol: "anthropic-messages",
          authentication: "anthropic-bearer",
        },
      })
    })
    if (!firstService.child.pid) throw new Error("claude-direct-first-service-pid-missing")
    await assertNoTcpListeners(firstService.child.pid)

    const bearerSettingsFingerprint = await safeFileFingerprint(settingsPath)
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await waitForSecretSafeFrame(setup, (frame) => frame.includes("Anthropic API (Messages)"), directSecrets, "claude-direct-api-preset")
    setup.mockInput.pressEnter()
    await waitForSecretSafeFrame(setup, (frame) => frame.includes("Anthropic API key"), directSecrets, "claude-direct-api-editor")
    await setup.mockInput.typeText("Claude Direct API Key")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(apiKeyBaseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(apiKeyModel)
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(apiKeySecret)
    captureSecretSafeFrame(setup, selectedFrames, directSecrets, "claude-direct-api-entry")
    setup.mockInput.pressEnter()
    const apiSaved = await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.providers.length === 2,
      directSecrets,
      "claude-direct-api-save",
    )
    const apiKeyProvider = apiSaved.providers.find((provider) => provider.name === "Claude Direct API Key")!
    assertSecretSafeStructured(apiKeyProvider, directSecrets, "claude-direct-api-provider", (provider) => {
      expect(provider).toMatchObject({
        baseUrl: apiKeyBaseUrl,
        model: apiKeyModel,
        protocol: "anthropic-messages",
        authentication: "anthropic-api-key",
        routingRequirement: "direct-compatible",
        credential: "present",
        completeness: "complete",
      })
    })
    await waitFor(() => views.some((view) => view.target === "claude" && view.providers.length === 2), "Claude Direct API-key save push")
    expect(await safeFileFingerprint(settingsPath)).toEqual(bearerSettingsFingerprint)
    assertSecretSafeStructured(apiSaved.activatedSnapshot, directSecrets, "claude-direct-immutable-bearer-snapshot", (snapshot) => {
      expect(snapshot).toMatchObject({
        id: bearerView.activatedSnapshot!.id,
        providerId: bearerProvider.id,
        model: bearerModel,
      })
    })

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await waitForSecretSafeFrame(
      setup,
      (frame) => frame.includes("Providers") && frame.includes("Claude Direct API Key"),
      directSecrets,
      "claude-direct-api-picker",
    )
    setup.mockInput.pressKey("down")
    const apiFrameStart = decodedFrames.length
    settingsPhase = { kind: "direct", expected: apiKeySettings }
    leader("a")
    const apiKeyView = await waitForSecretSafeSession(
      claudeSession!,
      (view) => view.mode === "direct" && view.currentProviderId === apiKeyProvider.id,
      directSecrets,
      "claude-direct-api-activation",
    )
    await setup.renderOnce()
    const apiAppliedFrame = captureSecretSafeFrame(
      setup,
      selectedFrames,
      directSecrets,
      "claude-direct-api-applied",
    )
    const missingApiFrameEvidence = [
      ["Mode       Direct", "mode"],
      ["Current Target Provider  Claude Direct API Key", "current"],
      ["Restart Claude Code to use the managed configuration.", "restart"],
    ].find(([expected]) => !apiAppliedFrame.includes(expected!))
    if (missingApiFrameEvidence) {
      throw new Error(`claude-direct-api-frame-missing:${missingApiFrameEvidence[1]}`)
    }
    await assertClaudeDirectResponseAndPush(decodedFrames, apiFrameStart, {
      providerId: apiKeyProvider.id,
      model: apiKeyModel,
      settingsPath,
    }, directSecrets, "claude-direct-api-key")
    const finalSettingsBytes = await readFile(settingsPath, "utf8")
    assertExactClaudeDirectSettings(finalSettingsBytes, apiKeySettings, [
      bearerSecret,
      priorBearer,
      priorApiKey,
    ])
    assertExactFileMode((await stat(settingsPath)).mode & 0o777, originalMode)
    assertSecretSafeStructured(apiKeyView, directSecrets, "claude-direct-api-view", (view) => {
      expect(view).toMatchObject({
        target: "claude",
        mode: "direct",
        takeover: { state: "inactive", endpoint: null },
        currentProviderId: apiKeyProvider.id,
        servingProviderId: null,
        managedConfiguration: { state: "applied", path: settingsPath, restartRequired: true },
        activatedSnapshot: {
          providerId: apiKeyProvider.id,
          model: apiKeyModel,
          protocol: "anthropic-messages",
          authentication: "anthropic-api-key",
        },
      })
    })
    expect(apiKeyView.activatedSnapshot!.id).not.toBe(bearerView.activatedSnapshot!.id)

    auditSqliteSecretLocations(databasePath, sqliteSecretPolicy)
    const firstEpochDatabase = new Database(databasePath, { readonly: true })
    const firstEpochRoute = firstEpochDatabase.query(`SELECT current_provider_id AS currentProviderId,
      serving_provider_id IS NULL AS servingAbsent, takeover_state AS takeoverState,
      route_port IS NULL AS routePortAbsent, routing_credential IS NULL AS routingCredentialAbsent,
      activated_snapshot_id AS snapshotId, managed_config_path AS managedConfigPath,
      managed_config_version AS managedConfigVersion, recovery_state AS recoveryState
      FROM target_route_state WHERE target = 'claude'`).get() as Record<string, unknown>
    const firstEpochSnapshots = firstEpochDatabase.query(`SELECT provider_id AS providerId, model,
      authentication, provider_bearer_token AS secret FROM activated_snapshots
      WHERE target = 'claude' ORDER BY rowid`).all() as Array<Record<string, unknown> & { secret: string }>
    firstEpochDatabase.close()
    expect(firstEpochRoute).toEqual({
      currentProviderId: apiKeyProvider.id,
      servingAbsent: 1,
      takeoverState: "inactive",
      routePortAbsent: 1,
      routingCredentialAbsent: 1,
      snapshotId: apiKeyView.activatedSnapshot!.id,
      managedConfigPath: settingsPath,
      managedConfigVersion: 2,
      recoveryState: "clean",
    })
    const firstEpochSnapshotProjection = firstEpochSnapshots.map(({ secret, ...snapshot }) => ({
      ...snapshot,
      secretDigest: sensitiveDigest(secret),
    })) as Array<Record<string, unknown>>
    expect(firstEpochSnapshotProjection).toEqual([
      {
        providerId: bearerProvider.id,
        model: bearerModel,
        authentication: "anthropic-bearer",
        secretDigest: sensitiveDigest(bearerSecret),
      },
      {
        providerId: apiKeyProvider.id,
        model: apiKeyModel,
        authentication: "anthropic-api-key",
        secretDigest: sensitiveDigest(apiKeySecret),
      },
    ])
    await assertNoTcpListeners(firstService.child.pid)
    expect(await controlledTreeFingerprint(trapHome)).toBe(trapFingerprint)

    const firstEpoch = apiKeyView.service.epoch
    await closeRenderer()
    await closeSessions()
    await waitForCleanExit(firstService, "first")

    const secondService = await startService("restart")
    await openSessions("claude-direct-restart")
    const restartedView = claudeSession!.get()
    assertSecretSafeStructured(restartedView, directSecrets, "claude-direct-restarted-view", (view) => {
      expect(view).toMatchObject({
        target: "claude",
        mode: "direct",
        takeover: { state: "inactive", endpoint: null },
        currentProviderId: apiKeyProvider.id,
        servingProviderId: null,
        managedConfiguration: { state: "applied", path: settingsPath, restartRequired: true },
        activatedSnapshot: {
          id: apiKeyView.activatedSnapshot!.id,
          providerId: apiKeyProvider.id,
          model: apiKeyModel,
          protocol: "anthropic-messages",
          authentication: "anthropic-api-key",
        },
      })
    })
    expect(restartedView.service.epoch).not.toBe(firstEpoch)
    assertExactClaudeDirectSettings(await readFile(settingsPath, "utf8"), apiKeySettings, [
      bearerSecret,
      priorBearer,
      priorApiKey,
    ])
    assertExactFileMode((await stat(settingsPath)).mode & 0o777, originalMode)
    if (!secondService.child.pid) throw new Error("claude-direct-second-service-pid-missing")
    await assertNoTcpListeners(secondService.child.pid)
    expect(await controlledTreeFingerprint(trapHome)).toBe(trapFingerprint)

    await closeSessions()
    await waitForCleanExit(secondService, "restart")

    auditSqliteSecretLocations(databasePath, sqliteSecretPolicy)
    const finalDatabase = new Database(databasePath, { readonly: true })
    const receipts = finalDatabase.query(`SELECT action_kind AS actionKind,
      outcome_json AS outcomeJson FROM action_receipts
      WHERE target = 'claude' ORDER BY committed_revision, action_id`).all()
    const recoveryPayloads = finalDatabase.query(`SELECT payload_json AS payloadJson
      FROM activation_recovery WHERE target = 'claude' ORDER BY created_revision`).all() as Array<{ payloadJson: string }>
    const finalRoute = finalDatabase.query(`SELECT takeover_state AS takeoverState,
      route_port IS NULL AS routePortAbsent, routing_credential IS NULL AS routingCredentialAbsent,
      managed_config_version AS managedConfigVersion
      FROM target_route_state WHERE target = 'claude'`).get()
    finalDatabase.close()
    scanNoSecrets(receipts, directSecrets, "claude-direct-action-receipts")
    expect(receipts.filter((receipt) => (receipt as { actionKind: string }).actionKind === "activate-provider")).toHaveLength(2)
    expect(finalRoute).toEqual({
      takeoverState: "inactive",
      routePortAbsent: 1,
      routingCredentialAbsent: 1,
      managedConfigVersion: 2,
    })
    expect(recoveryPayloads).toHaveLength(2)
    for (const { payloadJson } of recoveryPayloads) {
      const payload = JSON.parse(payloadJson) as {
        ownership_version?: unknown
        before?: { ownership_version?: unknown }
        desired?: { ownership_version?: unknown; owned?: { auth_token?: unknown; api_key?: unknown } }
      }
      if (
        payload.ownership_version !== 2
        || payload.before?.ownership_version !== 2
        || payload.desired?.ownership_version !== 2
      ) throw new Error("claude-direct-recovery-ownership-version-mismatch")
      const desiredCredentialCount = [
        payload.desired.owned?.auth_token,
        payload.desired.owned?.api_key,
      ].filter((value) => typeof value === "string").length
      if (desiredCredentialCount !== 1) throw new Error("claude-direct-recovery-dual-credential")
    }
    for (const audit of outboundAudits) audit.finish()
    if (outboundOperationKinds.some((kind) => kind === "discover-models" || kind === "check-reachability")) {
      throw new Error("claude-direct-unexpected-inspection-operation")
    }
    scanRawRpcFramesNoSecrets(rpcStreams, directSecrets)
    scanNoSecrets(
      [decodedFrames, views, receipts, selectedFrames, nativeFrames],
      directSecrets,
      "claude-direct-observed-surfaces",
    )
    scanProcessOutputNoSecrets(services.map(({ output }) => output.streams).flat(), directSecrets)
    expect(await controlledTreeFingerprint(trapHome)).toBe(trapFingerprint)
  } finally {
    await runClaudeSecurityFinalizer([
      { name: "native-recorder-drain", run: () => {
        if (!rendererAudit) return
        rendererAudit.stop()
        nativeFrames.push(...rendererAudit.frames())
      } },
      { name: "renderer-destroy", run: () => {
        if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
      } },
      { name: "session-close", run: closeSessions },
      { name: "process-kill", run: () => {
        for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
      } },
      { name: "process-output-drain", run: async () => {
        const results = await Promise.allSettled(services.map(({ output }) => Promise.race([
          output.completed,
          Bun.sleep(deadlineMs).then(() => { throw new Error("claude-direct-output-drain-timeout") }),
        ])))
        if (results.some((result) => result.status === "rejected")) {
          throw new Error("claude-direct-output-drain-incomplete")
        }
      } },
      { name: "raw-rpc", run: () => {
        if (rpcStreams.some((stream) => stream.length > 0)) {
          scanRawRpcFramesNoSecrets(rpcStreams, directSecrets)
        }
      } },
      { name: "observed-surfaces", run: () => {
        scanNoSecrets(
          [decodedFrames, views, selectedFrames, nativeFrames],
          directSecrets,
          "claude-direct-final-observed-surfaces",
        )
      } },
      { name: "process-output", run: () => {
        scanProcessOutputNoSecrets(services.map(({ output }) => output.streams).flat(), directSecrets)
      } },
      { name: "sqlite-secret-locations", run: async () => {
        try { await stat(databasePath) } catch (error) {
          if ((error as NodeJS.ErrnoException).code === "ENOENT") return
          throw error
        }
        auditSqliteSecretLocations(databasePath, sqliteSecretPolicy)
      } },
      { name: "settings", run: async () => {
        await auditClaudeDirectSettingsFile(settingsPath, settingsPhase, directSecrets, originalMode)
      } },
      { name: "trap", run: async () => {
        await assertControlledTreeUnchanged(
          trapHome,
          trapFingerprint,
          "claude-direct-environment-trap-mutated",
        )
      } },
    ])
  }
}, 60_000)

test("real processes prove the complete Target Provider workflow without leaking secrets or mutating inspections", async () => {
  const root = await mkdtemp(join(tmpdir(), "muxvia-e2e-"))
  roots.push(root)
  const userHome = join(root, "home")
  const operatorHomeCanary = join(root, "operator-home-canary")
  const codexHomeCanary = join(operatorHomeCanary, ".codex")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const configPath = join(userHome, ".codex/config.toml")
  const shutdownFile = join(root, "shutdown")
  await mkdir(join(operatorHomeCanary, ".muxvia/state"), { recursive: true, mode: 0o700 })
  await mkdir(codexHomeCanary, { recursive: true, mode: 0o700 })
  await writeFile(join(codexHomeCanary, "config.toml"), 'canary = "operator-codex-home"\n', { mode: 0o600 })
  await writeFile(join(operatorHomeCanary, ".muxvia/state/canary"), "operator muxvia state\n", { mode: 0o600 })
  const operatorHomeCanaryBefore = await controlledTreeFingerprint(operatorHomeCanary)
  await mkdir(join(userHome, ".codex"), { recursive: true, mode: 0o700 })
  await writeFile(configPath, '# operator comment survives\nunrelated = "keep-me"\n', { mode: 0o600 })
  await chmod(fakeCodex, 0o755)
  let routingCredentialForRequestAudit: string | undefined
  const requestAudit = createCapturedRequestAudit({
    providerCredential: providerSecret,
    forbiddenRoutingCredentials: () => routingCredentialForRequestAudit
      ? [wrongRoutingSecret, routingCredentialForRequestAudit]
      : [wrongRoutingSecret],
    expectedRequestCount: 6,
  })
  const upstream = await startFakeUpstream(providerSecret, requestAudit.observe)
  const rpcStreams: Buffer[][] = []
  const outboundOperationKinds: string[] = []
  const outboundAudits: ReturnType<typeof createOutboundOperationAudit>[] = []
  const decodedInboundFrames: unknown[] = []
  const views: TargetView[] = []
  const selectedRenderedFrames: string[] = []
  const readOnlyInspections: string[] = []
  let service: ReturnType<typeof spawn> | undefined
  let processOutput: ReturnType<typeof captureProcessOutput> | undefined
  let client: RpcClient | undefined
  let session: TargetSession | undefined
  let unsubscribe: (() => void) | undefined
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let rendererAudit: ReturnType<typeof createRendererAudit> | undefined
  try {
    const inheritedEnvironmentTraps: NodeJS.ProcessEnv = {
      HOME: operatorHomeCanary,
      CODEX_HOME: codexHomeCanary,
    }
    const serviceEnvironment: NodeJS.ProcessEnv = {
      ...inheritedEnvironmentTraps,
      PATH: `${dirname(fakeCodex)}:/usr/bin:/bin`,
      MUXVIA_INTEGRATION_TEST: "1",
    }
    serviceEnvironment.HOME = userHome
    delete serviceEnvironment.CODEX_HOME
    service = spawn(serviceBinary, [
      "--home", muxviaHome,
      "--test-shutdown-file", shutdownFile,
      "--test-codex-executable", fakeCodex,
    ], {
      cwd: root,
      env: serviceEnvironment,
      stdio: ["ignore", "pipe", "pipe"],
    })
    processOutput = captureProcessOutput(service)

    await waitFor(async () => {
      try {
        await stat(socketPath)
        return true
      } catch { return false }
    }, "control socket")

    const connect = async (release: string) => {
      const stream: Buffer[] = []
      const decoder = new FrameDecoder()
      rpcStreams.push(stream)
      return await RpcClient.connect(socketPath, release, undefined, (path) => {
        const socket = createConnection({ path })
        const outboundAudit = createOutboundOperationAudit()
        outboundAudits.push(outboundAudit)
        const write = socket.write.bind(socket)
        socket.write = ((chunk: Uint8Array, callback?: (error?: Error | null) => void) => {
          const beforeCount = outboundAudit.operationKinds.length
          outboundAudit.observe(chunk)
          outboundOperationKinds.push(...outboundAudit.operationKinds.slice(beforeCount))
          return write(chunk, callback)
        }) as typeof socket.write
        socket.on("data", (chunk) => {
          const bytes = Buffer.from(chunk)
          stream.push(bytes)
          decodedInboundFrames.push(...decoder.push(bytes))
        })
        return socket
      })
    }
    const assertReadOnlyInspection = async (label: string, before: unknown) => {
      expect(await readOnlyStateFingerprint(databasePath, configPath)).toEqual(before)
      readOnlyInspections.push(label)
    }
    const leader = (key: string) => {
      setup!.mockInput.pressKey("x", { ctrl: true })
      setup!.mockInput.pressKey(key)
    }
    const openProviderPicker = async () => {
      for (let pass = 0; pass < 4; pass++) await Promise.resolve()
      await setup!.renderOnce()
      await setup!.mockInput.typeText("/providers")
      setup!.mockInput.pressEnter()
      return await setup!.waitForFrame((frame) => frame.includes("Providers"))
    }

    client = await connect("e2e")
    session = await client.openTarget("codex")
    views.push(session.get() as TargetView)
    unsubscribe = session.subscribe((view) => views.push(view))
    setup = await testRender(() => <App session={session!} />, {
      width: 80,
      height: 24,
      useThread: false,
      kittyKeyboard: true,
    })
    rendererAudit = createRendererAudit(setup)
    rendererAudit.start()
    await setup.renderOnce()
    await setup.mockInput.typeText("/codex")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))

    // 1. A name-only declaration is valid but Incomplete in exactly three ways.
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("OpenAI API (Responses)"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter save"))
    await setup.mockInput.typeText("Original Provider")
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers.length === 1, "name-only Provider save")
    await setup.waitForFrame((frame) => frame.includes("Provider saved: Original Provider"))
    captureFrame(setup, selectedRenderedFrames)
    const incomplete = session.get().providers[0]!
    expect(incomplete).toMatchObject({
      name: "Original Provider",
      completeness: "incomplete",
      missingFields: ["base-url", "model", "credential"],
    })

    // 2. The first saved-state inspection sees only the incomplete saved declaration.
    const incompletePickerFrame = await openProviderPicker()
    expect(incompletePickerFrame.includes("Incomplete")).toBeTrue()
    selectedRenderedFrames.push(incompletePickerFrame)
    const incompleteInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    setup.mockInput.pressEnter()
    const missingCredentialFrame = await setup.waitForFrame((frame) => frame.includes("Credential missing"))
    selectedRenderedFrames.push(missingCredentialFrame)
    await assertReadOnlyInspection("incomplete automatic discovery", incompleteInspectionBefore)
    expect(upstream.calls.length === 0).toBeTrue()

    // Typing endpoint/model/credential never starts discovery; save is the first declaration mutation.
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(upstream.baseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("manual-before-discovery")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(providerSecret)
    await setup.renderOnce()
    const formFrame = captureFrame(setup, selectedRenderedFrames)
    expect(formFrame.includes(providerSecret)).toBeFalse()
    expect(upstream.calls.length === 0).toBeTrue()
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers[0]?.completeness === "complete", "complete Provider save")
    const completeFrame = await setup.waitForFrame((frame) => frame.includes("Provider saved: Original Provider"))
    selectedRenderedFrames.push(completeFrame)

    // Reopening now performs automatic discovery with the newly saved endpoint and Credential Reference.
    await openProviderPicker()
    const savedInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    const automaticInboundStart = decodedInboundFrames.length
    requestAudit.expectNext("expected-provider")
    setup.mockInput.pressEnter()
    await upstream.waitForCallCount(1)
    expect(requestAudit.projection(0)).toEqual(expectedModelRequestProjection)
    await waitForInboundResult(
      decodedInboundFrames,
      automaticInboundStart,
      { resultKind: "model-discovery" },
      "automatic-model-discovery",
    )
    const automaticModelsFrame = await setup.waitForFrame((frame) => frame.includes("2 models available"))
    selectedRenderedFrames.push(automaticModelsFrame)
    await assertReadOnlyInspection("complete automatic discovery", savedInspectionBefore)

    // 3. Explicit discovery is a second read-only request; selecting is local until save.
    const explicitInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    const explicitInboundStart = decodedInboundFrames.length
    requestAudit.expectNext("expected-provider")
    leader("f")
    await upstream.waitForCallCount(2)
    expect(requestAudit.projection(1)).toEqual(expectedModelRequestProjection)
    await waitForInboundResult(
      decodedInboundFrames,
      explicitInboundStart,
      { resultKind: "model-discovery" },
      "explicit-model-discovery",
    )
    const explicitModelsFrame = await setup.waitForFrame((frame) => frame.includes("2 models available"))
    selectedRenderedFrames.push(explicitModelsFrame)
    await assertReadOnlyInspection("explicit discovery", explicitInspectionBefore)
    leader("m")
    const modelPickerFrame = await setup.waitForFrame((frame) =>
      frame.includes("gpt-fixture-a") && frame.includes("gpt-fixture-b")
    )
    selectedRenderedFrames.push(modelPickerFrame)
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("gpt-fixture-b") && !frame.includes("Select Model"))
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers[0]?.model === "gpt-fixture-b", "discovered model save")
    await setup.waitForFrame((frame) => frame.includes("Provider saved: Original Provider"))

    const originalId = session.get().providers[0]!.id

    // 4. The safe Preset is copy-on-create and typing its draft makes no network request.
    const presetOutboundStart = outboundOperationKinds.length
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("OpenAI API (Responses)"))
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    const presetEditor = await setup.waitForFrame((frame) => frame.includes("https://api.openai.com/v1"))
    selectedRenderedFrames.push(presetEditor)
    const callsBeforePresetTyping = upstream.calls.length
    await setup.mockInput.typeText("Preset Provider")
    await setup.renderOnce()
    captureFrame(setup, selectedRenderedFrames)
    expect(upstream.calls.length === callsBeforePresetTyping).toBeTrue()
    const presetOutboundOperations = outboundOperationKinds.slice(presetOutboundStart)
    expect(presetOutboundOperations.length === 0).toBeTrue()
    expect(presetOutboundOperations.some((kind) =>
      kind === "discover-models" || kind === "check-reachability"
    )).toBeFalse()
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers.length === 2, "Preset Provider save")
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    const preset = session.get().providers.find((provider) => provider.name === "Preset Provider")!
    expect(preset).toMatchObject({
      baseUrl: "https://api.openai.com/v1",
      provenance: { kind: "preset", key: "openai-api-responses" },
      completeness: "incomplete",
      credential: "missing",
    })

    // 5. Duplicate once without a Credential Reference and once with explicit reuse.
    await openProviderPicker()
    leader("c")
    await setup.waitForFrame((frame) => frame.includes("Reuse Credential Reference?"))
    setup.mockInput.pressKey("n")
    await setup.waitForFrame((frame) => frame.includes("Original Provider Copy"))
    await setup.mockInput.typeText(" Without")
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers.length === 3, "without-credential duplicate save")
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    captureFrame(setup, selectedRenderedFrames)

    await openProviderPicker()
    leader("c")
    await setup.waitForFrame((frame) => frame.includes("Reuse Credential Reference?"))
    setup.mockInput.pressKey("y")
    await setup.waitForFrame((frame) => frame.includes("Original Provider Copy"))
    await setup.mockInput.typeText(" Shared")
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers.length === 4, "shared-credential duplicate save")
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    captureFrame(setup, selectedRenderedFrames)

    const withoutCredential = session.get().providers.find((provider) => provider.name.endsWith("Without"))!
    const sharedCredential = session.get().providers.find((provider) => provider.name.endsWith("Shared"))!
    expect(withoutCredential).toMatchObject({ credential: "missing", completeness: "incomplete" })
    expect(sharedCredential).toMatchObject({ credential: "present", completeness: "complete" })
    const referencesBeforeDelete = credentialReferences(databasePath)
    const originalCredentialId = referencesBeforeDelete.find(({ name }) => name === "Original Provider")!.credentialId
    expect(originalCredentialId).not.toBeNull()
    expect(referencesBeforeDelete.find(({ name }) => name.endsWith("Shared"))!.credentialId).toBe(originalCredentialId)
    expect(referencesBeforeDelete.find(({ name }) => name.endsWith("Without"))!.credentialId).toBeNull()

    // 6. Reorder through the named command and reconnect over a fresh production UDS session.
    await openProviderPicker()
    for (let step = 0; step < 3; step++) {
      setup.mockInput.pressKey("down")
      await setup.renderOnce()
    }
    leader("u")
    const persistedOrder = [
      "Original Provider",
      "Original Provider Copy Shared",
      "Preset Provider",
      "Original Provider Copy Without",
    ]
    await waitForSession(
      session,
      (view) => view.providers.map(({ name }) => name).join("|") === persistedOrder.join("|"),
      "persisted Provider order",
    )
    await setup.renderOnce()
    captureFrame(setup, selectedRenderedFrames)
    const reconnectClient = await connect("e2e-reconnect")
    const reconnected = await reconnectClient.openTarget("codex")
    views.push(reconnected.get() as TargetView)
    expect(reconnected.get().providers.map(({ name }) => name)).toEqual(persistedOrder)
    await reconnected.close()

    // 7. Apply Takeover to the original Provider, then edit only its declaration.
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    await setup.mockInput.typeText("/takeover")
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.takeover.state === "active", "Takeover activation")
    const activeFrame = await setup.waitForFrame((frame) =>
      frame.includes("Mode       Takeover") && frame.includes("Current Target Provider  Original Provider")
    )
    selectedRenderedFrames.push(activeFrame)

    const activatedConfigBytes = await readFile(configPath)
    const managed = extractManagedConfig(activatedConfigBytes.toString("utf8"), "gpt-fixture-b")
    routingCredentialForRequestAudit = managed.credential
    const actualRoutingSplit = Math.floor(managed.credential.length / 2)
    let actualRoutingDiagnostic = ""
    try {
      scanProcessOutputNoSecrets([[
        Buffer.from(managed.credential.slice(0, actualRoutingSplit)),
        Buffer.from(managed.credential.slice(actualRoutingSplit)),
      ]], [managed.credential])
    } catch (error) {
      actualRoutingDiagnostic = error instanceof Error ? error.message : String(error)
    }
    expect(actualRoutingDiagnostic === "secret-scan-failed:process-output-stream-0").toBeTrue()
    expect(actualRoutingDiagnostic.includes(managed.credential)).toBeFalse()
    expect(session.get().currentProviderId).toBe(originalId)
    expect(session.get().activatedSnapshot).toMatchObject({ providerId: originalId, model: "gpt-fixture-b" })
    const wrong = await chunkedPost(managed.endpoint, wrongRoutingSecret)
    expect(wrong.status).toBe(401)
    expect(upstream.calls.length === 2).toBeTrue()

    await openProviderPicker()
    for (let step = 0; step < 2; step++) {
      setup.mockInput.pressKey("up")
      await setup.renderOnce()
    }
    const activeEditorInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    const activeAutomaticInboundStart = decodedInboundFrames.length
    requestAudit.expectNext("expected-provider")
    setup.mockInput.pressEnter()
    await upstream.waitForCallCount(3)
    expect(requestAudit.projection(2)).toEqual(expectedModelRequestProjection)
    await waitForInboundResult(
      decodedInboundFrames,
      activeAutomaticInboundStart,
      { resultKind: "model-discovery" },
      "active-model-discovery",
    )
    await setup.waitForFrame((frame) => frame.includes("2 models available"))
    await assertReadOnlyInspection("active declaration automatic discovery", activeEditorInspectionBefore)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("/edited")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("-declared")
    expect(upstream.calls.length === 3).toBeTrue()
    setup.mockInput.pressEnter()
    await waitForSession(
      session,
      (view) => view.providers.find(({ id }) => id === originalId)?.baseUrl.endsWith("/edited") === true,
      "active Provider declaration edit",
    )
    const editedOriginal = session.get().providers.find(({ id }) => id === originalId)!
    expect(editedOriginal).toMatchObject({
      baseUrl: `${upstream.baseUrl}/edited`,
      model: "gpt-fixture-b-declared",
    })
    expect(session.get().activatedSnapshot).toMatchObject({ providerId: originalId, model: "gpt-fixture-b" })
    expect(sensitiveDigest(await readFile(configPath)) === sensitiveDigest(activatedConfigBytes)).toBeTrue()

    requestAudit.expectNext("expected-provider")
    const valid = await chunkedPost(managed.endpoint, managed.credential)
    await upstream.waitForCallCount(4)
    expect(requestAudit.projection(3)).toEqual(expectedResponseRequestProjection)
    expect(valid.status).toBe(201)
    const responseContentType = valid.headers["content-type"]
    const responseIsEventStream = Array.isArray(responseContentType)
      ? responseContentType.some((value) => value.startsWith("text/event-stream"))
      : responseContentType?.startsWith("text/event-stream") === true
    expect(responseIsEventStream).toBeTrue()
    expect(valid.body === SSE_BYTES.join("")).toBeTrue()
    await waitFor(() => views.some((view) => view.servingProviderId !== null), "Serving push")
    await setup.renderOnce()
    const servedFrame = captureFrame(setup, selectedRenderedFrames)
    expect(servedFrame.includes("Serving Provider  Original Provider")).toBeTrue()

    // 8. Reachability is an unauthenticated, headers-only 401 observation with no state change.
    await openProviderPicker()
    const reachabilityBefore = await readOnlyStateFingerprint(databasePath, configPath)
    const reachabilityInboundStart = decodedInboundFrames.length
    requestAudit.expectNext("absent")
    leader("t")
    await upstream.waitForCallCount(5)
    expect(requestAudit.projection(4)).toEqual(expectedReachabilityRequestProjection)
    await waitForInboundResult(
      decodedInboundFrames,
      reachabilityInboundStart,
      { resultKind: "reachability" },
      "reachability",
    )
    const reachabilityFrame = await setup.waitForFrame((frame) =>
      frame.includes("Reachable") && frame.includes("HTTP 401")
    )
    selectedRenderedFrames.push(reachabilityFrame)
    await assertReadOnlyInspection("reachability", reachabilityBefore)

    // 9. Active deletion is rejected; deleting the inactive shared duplicate preserves its credential.
    const activeDeleteRenderStart = rendererAudit.frames().length
    leader("d")
    const activeDeleteConfirmation = await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    selectedRenderedFrames.push(activeDeleteConfirmation)
    const activeDeleteInboundStart = decodedInboundFrames.length
    setup.mockInput.pressKey("y")
    await waitForInboundResult(
      decodedInboundFrames,
      activeDeleteInboundStart,
      { errorCode: "provider-referenced" },
      "active-provider-delete",
    )
    await setup.renderOnce()
    const activeDeleteRejected = captureFrame(setup, selectedRenderedFrames)
    const activeDeleteRenderedFrames = rendererAudit.frames().slice(activeDeleteRenderStart)
    expect(activeDeleteRenderedFrames.some((frame) => frame.includes("Delete Provider?"))).toBeTrue()
    expect(activeDeleteRejected.includes("Delete Provider?")).toBeFalse()
    expect(activeDeleteRenderedFrames.length >= 2).toBeTrue()
    expect(session.get().providers).toHaveLength(4)
    expect(session.get().providers.find(({ id }) => id === originalId)?.activeReferences).toEqual([
      "current",
      "activated-snapshot",
      "activated-route-plan",
    ])

    setup.mockInput.pressKey("down")
    leader("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await waitForSession(session, (view) => view.providers.length === 3, "inactive duplicate delete")
    expect(session.get().providers.some(({ name }) => name.endsWith("Shared"))).toBeFalse()
    const referencesAfterDelete = credentialReferences(databasePath)
    expect(referencesAfterDelete.find(({ name }) => name === "Original Provider")!.credentialId).toBe(originalCredentialId)
    await setup.renderOnce()
    captureFrame(setup, selectedRenderedFrames)

    // 10. Closing the Control Plane does not stop an active Routing Service.
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    setup.mockInput.pressCtrlC()
    await setup.waitFor(() => setup!.renderer.isDestroyed)
    rendererAudit.stop()
    const allRenderedFrames = rendererAudit.frames()
    setup = undefined
    unsubscribe()
    unsubscribe = undefined
    await session.close()
    session = undefined
    client = undefined
    requestAudit.expectNext("expected-provider")
    const second = await chunkedPost(managed.endpoint, managed.credential)
    await upstream.waitForCallCount(6)
    expect(requestAudit.projection(5)).toEqual(expectedResponseRequestProjection)
    expect(second.status).toBe(201)
    expect(service.exitCode).toBeNull()

    await writeFile(shutdownFile, "shutdown\n")
    const processResult = await processOutput.completed
    expect(processResult).toEqual({ code: 0, signal: null })
    expect(service.exitCode).toBe(0)
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    await expect(chunkedPost(managed.endpoint, managed.credential)).rejects.toBeDefined()
    await upstream.quiesce()
    requestAudit.assertComplete(upstream.calls.length)

    const receiptDatabase = new Database(databasePath, { readonly: true })
    const receipts = receiptDatabase
      .query("SELECT outcome_json FROM action_receipts ORDER BY committed_revision")
      .all()
    receiptDatabase.close()
    const secrets = [providerSecret, wrongRoutingSecret, managed.credential]
    for (const audit of outboundAudits) audit.finish()
    scanRawRpcFramesNoSecrets(rpcStreams, secrets)
    scanNoSecrets(
      [decodedInboundFrames, receipts, views, selectedRenderedFrames, allRenderedFrames],
      secrets,
      "inbound-and-rendered-surfaces",
    )
    scanProcessOutputNoSecrets(processOutput.streams, secrets)
    expect(readOnlyInspections).toEqual([
      "incomplete automatic discovery",
      "complete automatic discovery",
      "explicit discovery",
      "active declaration automatic discovery",
      "reachability",
    ])
    expect(allRenderedFrames.length).toBeGreaterThan(selectedRenderedFrames.length)

    expect(await controlledTreeFingerprint(operatorHomeCanary)).toBe(operatorHomeCanaryBefore)
  } finally {
    rendererAudit?.stop()
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    unsubscribe?.()
    await session?.close().catch(() => {})
    await client?.close().catch(() => {})
    await writeFile(shutdownFile, "shutdown\n").catch(() => {})
    if (service && service.exitCode === null) {
      const closed = await Promise.race([
        processOutput!.completed.then(() => true, () => true),
        Bun.sleep(deadlineMs).then(() => false),
      ])
      if (!closed) service.kill("SIGKILL")
    }
    await processOutput?.completed.catch(() => {})
    await upstream.stop()
  }
})

test("real processes prove failover routes, pinned epochs, stale health, and target isolation", async () => {
  const root = await mkdtemp(join(tmpdir(), "fh-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const codexConfig = join(userHome, ".codex/config.toml")
  const claudeSettings = join(userHome, ".claude/settings.json")
  const firstShutdown = join(root, "shutdown-first")
  const targetNames = ["codex", "claude"] as const
  type FailoverTarget = typeof targetNames[number]
  type FailoverFixture = Awaited<ReturnType<typeof startFailoverFixtureUpstream>>
  type TargetFixtures = { primary: FailoverFixture; fallbackA: FailoverFixture; fallbackB: FailoverFixture }

  const fixtureSecrets = {
    codex: {
      primary: "failover-codex-primary-provider-secret",
      fallbackA: "failover-codex-fallback-a-provider-secret",
      fallbackB: "failover-codex-fallback-b-provider-secret",
    },
    claude: {
      primary: "failover-claude-primary-provider-secret",
      fallbackA: "failover-claude-fallback-a-provider-secret",
      fallbackB: "failover-claude-fallback-b-provider-secret",
    },
  } as const
  const fixtures = {
    codex: {
      primary: await startFailoverFixtureUpstream("codex", fixtureSecrets.codex.primary),
      fallbackA: await startFailoverFixtureUpstream("codex", fixtureSecrets.codex.fallbackA),
      fallbackB: await startFailoverFixtureUpstream("codex", fixtureSecrets.codex.fallbackB),
    },
    claude: {
      primary: await startFailoverFixtureUpstream("claude", fixtureSecrets.claude.primary),
      fallbackA: await startFailoverFixtureUpstream("claude", fixtureSecrets.claude.fallbackA),
      fallbackB: await startFailoverFixtureUpstream("claude", fixtureSecrets.claude.fallbackB),
    },
  } satisfies Record<FailoverTarget, TargetFixtures>
  const allFixtures = targetNames.flatMap((target) => Object.values(fixtures[target]))
  const providerSecrets = targetNames.flatMap((target) => Object.values(fixtureSecrets[target]))
  const routingSecrets = new Set<string>()
  const publicSecrets = () => [...providerSecrets, ...routingSecrets]
  const services: Array<{ child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> }> = []
  const readinessSockets: Array<ReturnType<typeof createConnection>> = []
  const rpcStreams: Buffer[][] = []
  const decodedFrames: unknown[] = []
  const observedViews: TargetView[] = []
  const observedOutcomes: unknown[] = []
  const nativeFrames: string[] = []
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  let codexSession: TargetSession | undefined
  let claudeSession: TargetSession | undefined
  let subscriptions: Array<() => void> = []
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let recorder: ReturnType<typeof createRendererAudit> | undefined

  await mkdir(dirname(codexConfig), { recursive: true, mode: 0o700 })
  await mkdir(dirname(claudeSettings), { recursive: true, mode: 0o700 })
  await writeFile(codexConfig, '# operator comment survives\nunrelated = "keep-me"\n', { mode: 0o600 })
  await writeFile(claudeSettings, JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] },
    env: { OPERATOR_UNRELATED: "keep-me" },
  }, null, 2), { mode: 0o640 })
  await chmod(fakeCodex, 0o755)
  await chmod(fakeClaude, 0o755)

  const startService = async (label: string, shutdownFile?: string) => {
    const args = [
      "--home", muxviaHome,
      "--test-codex-executable", fakeCodex,
      "--test-claude-executable", fakeClaude,
    ]
    if (shutdownFile) args.push("--test-shutdown-file", shutdownFile)
    const child = spawn(serviceBinary, args, {
      cwd: root,
      env: { HOME: userHome, PATH: `${dirname(fakeCodex)}:/usr/bin:/bin`, MUXVIA_INTEGRATION_TEST: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    })
    const output = captureProcessOutput(child)
    const service = { child, output }
    services.push(service)
    const exitedBeforeReady = output.completed.then(({ code }) => {
      scanProcessOutputNoSecrets(output.streams, publicSecrets())
      throw new Error(`failover-routing-service-exited:${label}:${code}`)
    }) as Promise<never>
    await waitForEventTurnReadiness(
      () => probeUnixSocket(socketPath, readinessSockets),
      exitedBeforeReady,
      `failover-${label}-service`,
    )
    return service
  }
  const connect = async (release: string) => {
    const chunks: Buffer[] = []
    const decoder = new FrameDecoder()
    rpcStreams.push(chunks)
    const client = await eventBarrier(RpcClient.connect(socketPath, release, undefined, (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => {
        const bytes = Buffer.from(chunk)
        chunks.push(bytes)
        for (const frame of decoder.push(bytes)) {
          assertSecretSafeStructured(frame, publicSecrets(), "failover-rpc-frame")
          decodedFrames.push(frame)
        }
      })
      return socket
    }), `${release}-connect`)
    readinessSockets.splice(0).forEach((socket) => socket.destroy())
    return client
  }
  const openSessions = async (release: string) => {
    codexClient = await connect(`${release}-codex`)
    claudeClient = await connect(`${release}-claude`)
    codexSession = await eventBarrier(codexClient.openTarget("codex"), `${release}-codex-open`)
    claudeSession = await eventBarrier(claudeClient.openTarget("claude", {
      claudeConfigDir: null,
      selectorState: "unset",
      hostManagedState: "unmanaged",
      cwd: root,
    }), `${release}-claude-open`)
    const collect = (view: TargetView) => {
      assertSecretSafeStructured(view, publicSecrets(), "failover-target-view")
      observedViews.push(view)
    }
    collect(codexSession.get() as TargetView)
    collect(claudeSession.get() as TargetView)
    subscriptions = [
      codexSession.subscribe((view) => collect(view as TargetView)),
      claudeSession.subscribe((view) => collect(view as TargetView)),
    ]
  }
  const closeSessions = async () => {
    subscriptions.splice(0).forEach((unsubscribe) => unsubscribe())
    const sessions = [codexSession, claudeSession]
    const clients = [codexClient, claudeClient]
    codexSession = undefined
    claudeSession = undefined
    codexClient = undefined
    claudeClient = undefined
    await Promise.all(sessions.map((session) => session?.close().catch(() => undefined)))
    await Promise.all(clients.map((client) => client?.close().catch(() => undefined)))
  }
  const sessionFor = (target: FailoverTarget): TargetSession => {
    const session = target === "codex" ? codexSession : claudeSession
    if (!session) throw new Error(`failover-session-missing:${target}`)
    return session
  }
  const safeAct = async (target: FailoverTarget, action: OrdinaryTargetAction, label: string) => {
    const outcome = await actSecretSafe(sessionFor(target), action, publicSecrets(), `failover-${target}-${label}`)
    observedOutcomes.push(outcome)
    return outcome
  }
  const post = async (target: FailoverTarget, route: { endpoint: string; credential: string }) => {
    return target === "codex"
      ? await chunkedPost(route.endpoint, route.credential)
      : await claudePost(route.endpoint, route.credential, "/v1/messages", {
        model: "failover-claude-primary", messages: [], max_tokens: 16, stream: true,
      })
  }
  const readRoute = async (target: FailoverTarget) => {
    const route = target === "codex"
      ? extractManagedConfig(await readFile(codexConfig, "utf8"), "failover-codex-primary")
      : extractClaudeManagedSettings(
        await readFile(claudeSettings, "utf8"), "failover-claude-primary", providerSecrets,
      )
    routingSecrets.add(route.credential)
    return route
  }
  const providerByName = (target: FailoverTarget, name: string) => {
    const provider = sessionFor(target).get().providers.find((candidate) => candidate.name === name)
    if (!provider) throw new Error(`failover-provider-missing:${target}:${name}`)
    return provider
  }
  const applyPlan = async (target: FailoverTarget, providerNames: readonly string[]) => {
    const members = providerNames.map((name) => {
      const provider = providerByName(target, name)
      return { providerId: provider.id, providerRevision: provider.providerRevision }
    })
    await safeAct(target, { kind: "save-failover-draft", members }, "save-route")
    const draftRevision = sessionFor(target).get().failover?.draftRevision
    if (!draftRevision) throw new Error(`failover-draft-revision-missing:${target}`)
    await safeAct(target, { kind: "apply-failover-chain", draftRevision }, "apply-route")
  }
  const waitForServing = async (
    target: FailoverTarget,
    providerId: string,
    label: string,
  ) => await waitForSecretSafeSession(
    sessionFor(target),
    (view) => view.servingProviderId === providerId,
    publicSecrets(),
    `failover-${target}-${label}`,
  )
  const assertResponse = (target: FailoverTarget, result: Awaited<ReturnType<typeof chunkedPost>>) => {
    const valid = target === "codex"
      ? result.status === 201 && result.body === SSE_BYTES.join("")
      : result.status === 200 && result.body === CLAUDE_SSE_BYTES.join("")
    if (!valid) throw new Error(`failover-response-invalid:${target}`)
  }
  const auditUpstreamRequests = () => {
    for (const target of targetNames) {
      for (const fixture of Object.values(fixtures[target])) {
        for (const call of fixture.calls) {
          auditCapturedRequestAndProject(call, {
            providerCredential: fixture.credential,
            forbiddenRoutingCredentials: [...routingSecrets],
            authorization: "expected-provider",
          })
          scanNoSecrets([call], [
            ...publicSecrets().filter((secret) => secret !== fixture.credential),
          ], `failover-upstream-${target}`)
          const expectedPath = target === "codex" ? "/v1/responses" : "/v1/messages"
          if (call.method !== "POST" || call.path.split("?", 1)[0] !== expectedPath) {
            throw new Error(`failover-upstream-request-mismatch:${target}`)
          }
        }
      }
    }
  }
  const renderRoutes = async () => {
    setup = await testRender(() => <App sessions={{ codex: codexSession!, claude: claudeSession! }} />, {
      width: 100,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    recorder = createRendererAudit(setup)
    recorder.start()
    for (const [index, target] of targetNames.entries()) {
      if (index > 0) {
        setup.mockInput.pressEscape()
        await waitForSecretSafeFrame(
          setup,
          (value) => value.includes("Choose a target"),
          publicSecrets(),
          `failover-${target}-home`,
        )
      }
      setup.mockInput.pressKey(index === 0 ? "1" : "2")
      await waitForSecretSafeFrame(
        setup,
        (value) => value.includes(target === "codex" ? "Codex CLI" : "Claude Code")
          && value.includes("Run a target action"),
        publicSecrets(),
        `failover-${target}-page`,
      )
      await setup.mockInput.typeText("/route")
      setup.mockInput.pressEnter()
      const frame = await waitForSecretSafeFrame(
        setup,
        (value) => value.includes("Failover Route") && value.includes("Stale"),
        publicSecrets(),
        `failover-${target}-route`,
      )
      nativeFrames.push(frame)
      if (!frame.includes("Current:") || !frame.includes("Serving:") || !frame.includes("Fallback")) {
        throw new Error(`failover-route-frame-incomplete:${target}`)
      }
      setup.mockInput.pressEscape()
      await waitForSecretSafeFrame(
        setup,
        (value) => value.includes("Run a target action") && !value.includes("Failover Route"),
        publicSecrets(),
        `failover-${target}-route-closed`,
      )
    }
  }
  const closeRenderer = () => {
    recorder?.stop()
    if (recorder) nativeFrames.push(...recorder.frames())
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    setup = undefined
    recorder = undefined
  }

  try {
    const first = await startService("first", firstShutdown)
    await openSessions("failover-first")
    const routes = {} as Record<FailoverTarget, { endpoint: string; credential: string }>
    for (const target of targetNames) {
      const targetFixtures = fixtures[target]
      for (const [role, fixture] of Object.entries(targetFixtures) as Array<[keyof TargetFixtures, FailoverFixture]>) {
        const name = `${target === "codex" ? "Codex" : "Claude"} ${role}`
        await safeAct(target, {
          kind: "create-provider",
          name,
          baseUrl: fixture.baseUrl,
          model: `failover-${target}-${role === "primary" ? "primary" : role === "fallbackA" ? "fallback-a" : "fallback-b"}`,
          credential: { kind: "replace", value: fixture.credential },
          authentication: target === "codex" ? "openai-bearer" : "anthropic-bearer",
          presetKey: null,
        }, `create-${role}`)
      }
      const primaryName = `${target === "codex" ? "Codex" : "Claude"} primary`
      await safeAct(target, {
        kind: "activate-provider",
        providerId: providerByName(target, primaryName).id,
        mode: "takeover",
      }, "activate-primary")
      routes[target] = await readRoute(target)
      await applyPlan(target, [primaryName, `${target === "codex" ? "Codex" : "Claude"} fallbackA`])
    }

    for (const target of targetNames) {
      const session = sessionFor(target)
      const targetFixtures = fixtures[target]
      const primary = providerByName(target, `${target === "codex" ? "Codex" : "Claude"} primary`)
      const fallbackA = providerByName(target, `${target === "codex" ? "Codex" : "Claude"} fallbackA`)
      const fallbackB = providerByName(target, `${target === "codex" ? "Codex" : "Claude"} fallbackB`)
      const peer = target === "codex" ? "claude" : "codex"
      const peerBefore = stableTargetViewFingerprint(sessionFor(peer).get())
      const current = session.get().currentProviderId
      const firstPlanEpoch = session.get().failover?.activePlan?.epoch
      if (!firstPlanEpoch || current !== primary.id) throw new Error(`failover-initial-plan-invalid:${target}`)

      targetFixtures.primary.setBehavior("retryable-failure")
      targetFixtures.fallbackA.setBehavior("success")
      assertResponse(target, await post(target, routes[target]))
      await waitForServing(target, fallbackA.id, "fallback-serving")
      if (session.get().currentProviderId !== primary.id || session.get().routeHealth.state !== "degraded") {
        throw new Error(`failover-current-serving-projection-invalid:${target}`)
      }

      targetFixtures.primary.setBehavior("success")
      assertResponse(target, await post(target, routes[target]))
      await waitForServing(target, primary.id, "primary-failback")
      if (session.get().currentProviderId !== primary.id) throw new Error(`failover-current-changed:${target}`)

      const held = targetFixtures.primary.holdNext("retryable-failure")
      const fallbackACalls = targetFixtures.fallbackA.calls.length
      const fallbackBCalls = targetFixtures.fallbackB.calls.length
      const pinnedRequest = post(target, routes[target])
      await eventBarrier(held.started, `failover-${target}-held-primary`)
      await applyPlan(target, [
        `${target === "codex" ? "Codex" : "Claude"} primary`,
        `${target === "codex" ? "Codex" : "Claude"} fallbackB`,
      ])
      const nextPlanEpoch = session.get().failover?.activePlan?.epoch
      if (!nextPlanEpoch || nextPlanEpoch === firstPlanEpoch) throw new Error(`failover-plan-epoch-not-replaced:${target}`)
      held.release()
      assertResponse(target, await eventBarrier(pinnedRequest, `failover-${target}-pinned-request`))
      await targetFixtures.fallbackA.waitForCallCount(fallbackACalls + 1)
      if (targetFixtures.fallbackB.calls.length !== fallbackBCalls) {
        throw new Error(`failover-pinned-request-used-new-plan:${target}`)
      }
      if (session.get().failover?.activePlan?.epoch !== nextPlanEpoch) {
        throw new Error(`failover-active-plan-overwritten:${target}`)
      }

      targetFixtures.primary.setBehavior("retryable-failure")
      targetFixtures.fallbackB.setBehavior("success")
      assertResponse(target, await post(target, routes[target]))
      await waitForServing(target, fallbackB.id, "new-plan-fallback")
      const beforeReachability = stableTargetViewFingerprint(session.get())
      const reachability = await eventBarrier(
        session.checkReachability(primary.id, primary.providerRevision),
        `failover-${target}-reachability`,
      )
      assertSecretSafeStructured(reachability, publicSecrets(), `failover-${target}-reachability`)
      if (stableTargetViewFingerprint(session.get()) !== beforeReachability) {
        throw new Error(`failover-reachability-mutated-health:${target}`)
      }
      if (stableTargetViewFingerprint(sessionFor(peer).get()) !== peerBefore) {
        throw new Error(`failover-peer-target-mutated:${target}`)
      }
    }

    closeRenderer()
    await closeSessions()
    await writeFile(firstShutdown, "shutdown\n")
    const firstExit = await eventBarrier(first.output.completed, "failover-first-exit")
    if (firstExit.code !== 0 || firstExit.signal !== null) throw new Error("failover-first-exit-invalid")
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })

    const firstEpochs = Object.fromEntries(targetNames.map((target) => [target, observedViews
      .filter((view) => view.target === target).at(-1)!.service.epoch])) as Record<FailoverTarget, string>
    const second = await startService("restart")
    await openSessions("failover-restart")
    for (const target of targetNames) {
      const view = sessionFor(target).get()
      const activeMembers = new Set(view.failover?.activePlan?.members.map(({ providerId }) => providerId))
      const memberHealth = view.providers
        .filter(({ id }) => activeMembers.has(id))
        .map(({ routeHealth }) => routeHealth?.state)
      if (view.service.epoch === firstEpochs[target]
        || view.routeHealth.state !== "stale"
        || memberHealth.length !== 2
        || memberHealth.some((state) => state !== "stale")) {
        throw new Error(`failover-restart-stale-projection-invalid:${target}`)
      }
    }

    await renderRoutes()
    closeRenderer()
    for (const target of targetNames) {
      const session = sessionFor(target)
      const primary = providerByName(target, `${target === "codex" ? "Codex" : "Claude"} primary`)
      const primaryCalls = fixtures[target].primary.calls.length
      fixtures[target].primary.setBehavior("success")
      assertResponse(target, await post(target, routes[target]))
      await fixtures[target].primary.waitForCallCount(primaryCalls + 1)
      await waitForServing(target, primary.id, "restart-primary")
      if (session.get().routeHealth.state !== "healthy" || session.get().currentProviderId !== primary.id) {
        throw new Error(`failover-restart-eligibility-not-reset:${target}`)
      }
      await safeAct(target, { kind: "disable-takeover" }, "disable-takeover")
      const disabled = session.get()
      if (disabled.currentProviderId !== null
        || disabled.activatedSnapshot !== null
        || disabled.takeover.state !== "inactive"
        || disabled.failover?.activePlan !== null) {
        throw new Error(`failover-disable-did-not-clear-route:${target}`)
      }
    }

    await closeSessions()
    const naturalExit = await Promise.race([
      second.output.completed,
      Bun.sleep(deadlineMs).then(() => undefined),
    ])
    if (!naturalExit || naturalExit.code !== 0 || naturalExit.signal !== null) {
      throw new Error("failover-natural-exit-failed")
    }
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })

    const database = new Database(databasePath, { readonly: true })
    const receipts = database.query("SELECT outcome_json AS outcome FROM action_receipts ORDER BY committed_revision, action_id").all()
    database.close()
    scanNoSecrets([receipts], publicSecrets(), "failover-action-receipts")
    scanRawRpcFramesNoSecrets(rpcStreams, publicSecrets())
    scanNoSecrets([decodedFrames, observedViews, observedOutcomes, nativeFrames], publicSecrets(), "failover-public-surfaces")
    scanProcessOutputNoSecrets(services.flatMap(({ output }) => output.streams), publicSecrets())
    auditUpstreamRequests()
  } finally {
    closeRenderer()
    readinessSockets.splice(0).forEach((socket) => socket.destroy())
    for (const fixture of allFixtures) fixture.releaseHeld()
    await closeSessions()
    await writeFile(firstShutdown, "shutdown\n").catch(() => {})
    for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
    await Promise.all(services.map(({ output }) => Promise.race([
      output.completed.catch(() => undefined),
      Bun.sleep(deadlineMs),
    ])))
    await Promise.all(allFixtures.map((fixture) => fixture.stop()))
  }
}, 120_000)

test("real processes prove Universal Provider synchronization across both Targets", async () => {
  const root = await mkdtemp(join(tmpdir(), "mu-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const codexConfig = join(userHome, ".codex/config.toml")
  const claudeSettings = join(userHome, ".claude/settings.json")
  const firstShutdown = join(root, "shutdown-first")
  const sourceSecret = "universal-source-secret-must-not-escape"
  const replacementSecret = "universal-replacement-secret-must-not-escape"
  const upstream = await startFakeUpstream(sourceSecret, undefined, { bearerCredentials: [sourceSecret] })
  const replacementUpstream = await startFakeUpstream(replacementSecret, undefined, { bearerCredentials: [replacementSecret] })
  const services: Array<{ child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> }> = []
  const rpcStreams: Buffer[][] = []
  const decodedFrames: unknown[] = []
  const nativeFrames: string[] = []
  const routingSecrets = new Set<string>()
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  let universalClient: RpcClient | undefined
  let codexSession: TargetSession | undefined
  let claudeSession: TargetSession | undefined
  let universalSession: UniversalProviderSession | undefined
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let recorder: ReturnType<typeof createRendererAudit> | undefined

  await mkdir(dirname(codexConfig), { recursive: true, mode: 0o700 })
  await mkdir(dirname(claudeSettings), { recursive: true, mode: 0o700 })
  await writeFile(codexConfig, '# operator comment survives\nunrelated = "keep-me"\n', { mode: 0o600 })
  await writeFile(claudeSettings, JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] },
    env: { OPERATOR_UNRELATED: "keep-me" },
  }, null, 2), { mode: 0o640 })
  await chmod(fakeCodex, 0o755)
  await chmod(fakeClaude, 0o755)

  const secrets = () => [sourceSecret, replacementSecret, ...routingSecrets]
  const startService = async (label: string, shutdownFile?: string) => {
    const args = [
      "--home", muxviaHome,
      "--test-codex-executable", fakeCodex,
      "--test-claude-executable", fakeClaude,
    ]
    if (shutdownFile) args.push("--test-shutdown-file", shutdownFile)
    const child = spawn(serviceBinary, args, {
      cwd: root,
      env: { HOME: userHome, PATH: `${dirname(fakeCodex)}:/usr/bin:/bin`, MUXVIA_INTEGRATION_TEST: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    })
    const record = { child, output: captureProcessOutput(child) }
    services.push(record)
    await waitFor(async () => {
      if (child.exitCode !== null) throw new Error(`universal-provider-service-exited:${label}`)
      try { return (await stat(socketPath)).isSocket() } catch { return false }
    }, `Universal Provider ${label} socket`)
    return record
  }
  const connect = async (release: string) => {
    const chunks: Buffer[] = []
    const decoder = new FrameDecoder()
    rpcStreams.push(chunks)
    return await RpcClient.connect(socketPath, release, undefined, (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => {
        const bytes = Buffer.from(chunk)
        chunks.push(bytes)
        for (const frame of decoder.push(bytes)) {
          assertSecretSafeStructured(frame, secrets(), "universal-provider-rpc-frame")
          decodedFrames.push(frame)
        }
      })
      return socket
    })
  }
  const openSessions = async (release: string) => {
    codexClient = await connect(`${release}-codex`)
    claudeClient = await connect(`${release}-claude`)
    universalClient = await connect(`${release}-catalog`)
    codexSession = await codexClient.openTarget("codex")
    claudeSession = await claudeClient.openTarget("claude", {
      claudeConfigDir: null,
      selectorState: "unset",
      hostManagedState: "unmanaged",
      cwd: root,
    })
    universalSession = await universalClient.openUniversalProviders({
      claudeConfigDir: null,
      selectorState: "unset",
      blockingSelector: null,
      hostManagedState: "unmanaged",
      cwd: root,
    })
  }
  const closeSessions = async () => {
    const sessions = [codexSession, claudeSession, universalSession]
    const clients = [codexClient, claudeClient, universalClient]
    codexSession = undefined
    claudeSession = undefined
    universalSession = undefined
    codexClient = undefined
    claudeClient = undefined
    universalClient = undefined
    await Promise.all(sessions.map((session) => session?.close().catch(() => undefined)))
    await Promise.all(clients.map((client) => client?.close().catch(() => undefined)))
  }
  const waitForCatalog = async (
    predicate: (view: ReturnType<UniversalProviderSession["get"]>) => boolean,
    label: string,
  ) => {
    if (predicate(universalSession!.get())) return
    await new Promise<void>((resolveWait, reject) => {
      let unsubscribe = () => {}
      const timeout = setTimeout(() => {
        unsubscribe()
        reject(new Error(`universal-provider-catalog-timeout:${label}`))
      }, deadlineMs)
      const finish = () => {
        clearTimeout(timeout)
        unsubscribe()
        resolveWait()
      }
      unsubscribe = universalSession!.subscribe((view) => { if (predicate(view)) finish() })
      if (predicate(universalSession!.get())) finish()
    })
  }
  const renderApp = async () => {
    setup = await testRender(() => <App
      sessions={{ codex: codexSession!, claude: claudeSession! }}
      universalSession={universalSession!}
    />, { width: 100, height: 30, useThread: false, kittyKeyboard: true })
    recorder = createRendererAudit(setup)
    recorder.start()
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
  }
  const closeRenderer = () => {
    recorder?.stop()
    const frames = recorder?.frames() ?? []
    scanNoSecrets(frames, secrets(), "universal-provider-renderer")
    nativeFrames.push(...frames)
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    setup = undefined
    recorder = undefined
  }
  const leader = (key: string) => {
    setup!.mockInput.pressKey("x", { ctrl: true })
    setup!.mockInput.pressKey(key)
  }
  const openUniversalPicker = async () => {
    await setup!.mockInput.typeText("/universal-providers")
    setup!.mockInput.pressEnter()
    return await setup!.waitForFrame((frame) => frame.includes("Universal Providers"))
  }
  const openProviderPicker = async () => {
    await setup!.mockInput.typeText("/providers")
    setup!.mockInput.pressEnter()
    return await setup!.waitForFrame((frame) => frame.includes("Providers"))
  }
  const fixedProblemCode = (error: unknown, label: string) => {
    assertSecretSafeStructured(error, secrets(), `universal-provider-${label}`)
    if (!error || typeof error !== "object" || !("code" in error) || typeof error.code !== "string") {
      throw new Error(`universal-provider-problem-missing:${label}`)
    }
    return error.code
  }
  const catalog = () => {
    if (!universalSession) throw new Error("universal-provider-catalog-session-missing")
    return universalSession
  }
  const codex = () => {
    if (!codexSession) throw new Error("universal-provider-codex-session-missing")
    return codexSession
  }
  const claude = () => {
    if (!claudeSession) throw new Error("universal-provider-claude-session-missing")
    return claudeSession
  }
  const ui = () => {
    if (!setup) throw new Error("universal-provider-renderer-missing")
    return setup
  }

  try {
    const firstService = await startService("first", firstShutdown)
    await openSessions("first")

    // A Preset-backed declaration starts pending and materializes one stable Generated Provider per Target only on explicit sync.
    const createdOutcome = await catalog().act({
      kind: "create-universal-provider",
      name: "Shared Frontier",
      baseUrl: upstream.baseUrl,
      credential: { kind: "replace", value: sourceSecret },
      presetKey: "openai-api-responses",
      targets: [
        { target: "codex", enabled: true, model: "universal-codex-v1", authentication: "openai-bearer", routingRequirement: "direct-compatible" },
        { target: "claude", enabled: true, model: "universal-claude-v1", authentication: "anthropic-bearer", routingRequirement: "direct-compatible" },
      ],
    })
    const created = createdOutcome.view.providers[0]!
    if (created.provenance?.key !== "openai-api-responses"
      || !created.targets.every((target) => target.synchronization === "pending" && target.generatedProviderId === null)) {
      throw new Error("universal-provider-created-state-invalid")
    }
    await renderApp()
    const pendingFrame = await openUniversalPicker()
    if (!pendingFrame.includes("Codex CLI · Pending · Enabled") || !pendingFrame.includes("Claude Code · Pending · Enabled")) {
      throw new Error("universal-provider-pending-rails-missing")
    }
    ui().mockInput.pressEscape()
    await catalog().act({
      kind: "synchronize-universal-provider",
      providerId: created.id,
      providerRevision: created.providerRevision,
    })
    await waitForCatalog((view) => view.providers[0]?.targets.every((target) =>
      target.synchronization === "current" && target.generatedProviderId !== null) === true, "initial sync")
    const synchronized = catalog().get().providers[0]!
    const generatedIds = Object.fromEntries(synchronized.targets.map((target) => [target.target, target.generatedProviderId!])) as Record<"codex" | "claude", string>
    await waitForSession(codex(), (view) => view.providers.some((provider) => provider.id === generatedIds.codex), "Codex Generated Provider")
    await waitForSession(claude(), (view) => view.providers.some((provider) => provider.id === generatedIds.claude), "Claude Generated Provider")

    // The Target editor changes only the Overlay; duplicating a Generated Provider creates a detached ordinary Provider.
    await openProviderPicker()
    ui().mockInput.pressEnter()
    await ui().waitForFrame((frame) => frame.includes("Generated Provider Target Overlay"))
    await ui().mockInput.typeText("-overlay")
    ui().mockInput.pressEnter()
    await waitForSession(codex(), (view) => view.providers.find((provider) => provider.id === generatedIds.codex)?.model.endsWith("-overlay") === true, "Generated Overlay edit")
    await waitForCatalog((view) => view.providers[0]?.targets.find((target) => target.target === "codex")?.model.endsWith("-overlay") === true, "Overlay catalog update")
    const generatedCodex = codex().get().providers.find((provider) => provider.id === generatedIds.codex)!
    let duplicated: Awaited<ReturnType<TargetSession["act"]>>
    try {
      duplicated = await codex().act({
        kind: "duplicate-provider",
        sourceProviderId: generatedCodex.id,
        sourceProviderRevision: generatedCodex.providerRevision,
        name: `${generatedCodex.name} Detached`,
        baseUrl: generatedCodex.baseUrl,
        model: generatedCodex.model,
        credential: { kind: "without" },
      })
    } catch (error) {
      assertSecretSafeStructured(error, secrets(), "universal-provider-duplicate-error")
      throw new Error("universal-provider-duplicate-failed")
    }
    let detachedCodex = duplicated.view.providers.find((provider) => !provider.generated)!
    if (!detachedCodex || detachedCodex.provenance?.kind === "universal-provider") {
      throw new Error("universal-provider-duplicate-not-detached")
    }
    const completedDetached = await codex().act({
      kind: "update-provider",
      providerId: detachedCodex.id,
      providerRevision: detachedCodex.providerRevision,
      name: detachedCodex.name,
      baseUrl: detachedCodex.baseUrl,
      model: detachedCodex.model,
      authentication: detachedCodex.authentication,
      routingRequirement: detachedCodex.routingRequirement,
      credential: { kind: "replace", value: sourceSecret },
    })
    detachedCodex = completedDetached.view.providers.find((provider) => provider.id === detachedCodex.id)!

    // Both generated declarations activate immutable Takeover snapshots.
    try {
      await codex().act({ kind: "activate-provider", providerId: generatedIds.codex, mode: "takeover" })
    } catch (error) {
      assertSecretSafeStructured(error, secrets(), "universal-provider-codex-activation-error")
      throw new Error("universal-provider-codex-activation-failed")
    }
    try {
      await claude().act({ kind: "activate-provider", providerId: generatedIds.claude, mode: "takeover" })
    } catch (error) {
      assertSecretSafeStructured(error, secrets(), "universal-provider-claude-activation-error")
      throw new Error("universal-provider-claude-activation-failed")
    }
    const codexRoute = extractManagedConfig(await readFile(codexConfig, "utf8"), "universal-codex-v1-overlay")
    const claudeRoute = extractClaudeManagedSettings(await readFile(claudeSettings, "utf8"), "universal-claude-v1", [sourceSecret, replacementSecret])
    routingSecrets.add(codexRoute.credential)
    routingSecrets.add(claudeRoute.credential)
    const codexConfigDigest = sensitiveDigest(await readFile(codexConfig))!
    const claudeSettingsDigest = sensitiveDigest(await readFile(claudeSettings))!
    const snapshotIds = {
      codex: codex().get().activatedSnapshot?.id,
      claude: claude().get().activatedSnapshot?.id,
    }
    if (!snapshotIds.codex || !snapshotIds.claude) throw new Error("universal-provider-snapshot-missing")

    // Restart preserves source/generated identities, revisions, current routes, and snapshots.
    closeRenderer()
    await closeSessions()
    await writeFile(firstShutdown, "shutdown\n")
    const firstExit = await eventBarrier(firstService.output.completed, "universal-provider-first-exit")
    if (firstExit.code !== 0 || firstExit.signal !== null) throw new Error("universal-provider-first-exit-failed")
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    const finalService = await startService("restart")
    await openSessions("restart")
    await renderApp()
    const reopened = catalog().get().providers[0]!
    if (reopened.id !== synchronized.id || reopened.targets.some((target) => target.generatedProviderId !== generatedIds[target.target])) {
      throw new Error("universal-provider-restart-identity-mismatch")
    }
    if (codex().get().activatedSnapshot?.id !== snapshotIds.codex || claude().get().activatedSnapshot?.id !== snapshotIds.claude) {
      throw new Error("universal-provider-restart-snapshot-mismatch")
    }

    // Source synchronization updates declarations while active config and traffic stay pinned to the immutable snapshots.
    await catalog().act({
      kind: "update-universal-provider",
      providerId: reopened.id,
      providerRevision: reopened.providerRevision,
      name: "Shared Frontier v2",
      baseUrl: replacementUpstream.baseUrl,
      credential: { kind: "replace", value: replacementSecret },
      targets: reopened.targets.map(({ target, enabled, model, authentication, routingRequirement }) => ({
        target, enabled, model: `${model}-declared`, authentication, routingRequirement,
      })),
    })
    const pending = catalog().get().providers[0]!
    await openUniversalPicker()
    const editedFrame = await ui().waitForFrame((frame) => frame.includes("Shared Frontier v2") && frame.includes("Pending"))
    nativeFrames.push(editedFrame)
    ui().mockInput.pressEscape()
    await catalog().act({
      kind: "synchronize-universal-provider",
      providerId: pending.id,
      providerRevision: pending.providerRevision,
    })
    await waitForCatalog((view) => view.providers[0]?.targets.every((target) => target.synchronization === "current") === true, "updated sync")
    if (sensitiveDigest(await readFile(codexConfig)) !== codexConfigDigest
      || sensitiveDigest(await readFile(claudeSettings)) !== claudeSettingsDigest) {
      throw new Error("universal-provider-active-config-not-pinned")
    }
    if (codex().get().activatedSnapshot?.id !== snapshotIds.codex || claude().get().activatedSnapshot?.id !== snapshotIds.claude) {
      throw new Error("universal-provider-active-snapshot-not-pinned")
    }
    const codexTraffic = await chunkedPost(codexRoute.endpoint, codexRoute.credential)
    const claudeTraffic = await claudePost(claudeRoute.endpoint, claudeRoute.credential, "/v1/messages", { messages: [], stream: true })
    if (codexTraffic.status !== 201 || claudeTraffic.status !== 200 || replacementUpstream.calls.length !== 0) {
      throw new Error("universal-provider-active-traffic-not-pinned")
    }

    // Referenced disable and delete both reject before any generated removal.
    const current = catalog().get().providers[0]!
    let disableCode = ""
    try {
      await catalog().act({
        kind: "update-universal-provider",
        providerId: current.id,
        providerRevision: current.providerRevision,
        name: current.name,
        baseUrl: current.baseUrl,
        credential: { kind: "keep" },
        targets: current.targets.map(({ target, model, authentication, routingRequirement }) => ({
          target, enabled: false, model, authentication, routingRequirement,
        })),
      })
    } catch (error) { disableCode = fixedProblemCode(error, "disable") }
    if (disableCode !== "generated-provider-referenced") throw new Error("universal-provider-disable-not-blocked")
    let deleteCode = ""
    try {
      await catalog().act({ kind: "delete-universal-provider", providerId: current.id, providerRevision: current.providerRevision })
    } catch (error) { deleteCode = fixedProblemCode(error, "delete") }
    if (deleteCode !== "generated-provider-referenced") throw new Error("universal-provider-delete-not-blocked")
    const blocked = catalog().get().providers[0]!
    if (!blocked.targets.every((target) => target.activeReferences.includes("current")
      && target.activeReferences.includes("activated-snapshot"))) {
      throw new Error("universal-provider-blocker-list-incomplete")
    }

    // Restore exits both active Takeovers; detached Direct replacements then keep Generated identities unreferenced.
    const codexRestore = await codex().previewReconciliation("restore")
    await codex().applyReconciliation({
      strategy: "restore",
      observationToken: codexRestore.observationToken,
    })
    const claudeRestore = await claude().previewReconciliation("restore")
    await claude().applyReconciliation({
      strategy: "restore",
      observationToken: claudeRestore.observationToken,
    })
    await codex().act({ kind: "activate-provider", providerId: detachedCodex.id, mode: "direct" })
    const claudeFallback = await claude().act({
      kind: "create-provider",
      name: "Claude Detached Fallback",
      baseUrl: upstream.baseUrl,
      model: "claude-fallback",
      credential: { kind: "replace", value: sourceSecret },
      authentication: "anthropic-bearer",
      presetKey: null,
    })
    const detachedClaude = claudeFallback.view.providers.find((provider) => provider.name === "Claude Detached Fallback")!
    await claude().act({ kind: "activate-provider", providerId: detachedClaude.id, mode: "direct" })
    closeRenderer()
    await universalSession!.close()
    universalSession = undefined
    universalClient = undefined
    universalClient = await connect("released-catalog")
    universalSession = await universalClient.openUniversalProviders({
      claudeConfigDir: null,
      selectorState: "unset",
      blockingSelector: null,
      hostManagedState: "unmanaged",
      cwd: root,
    })
    await renderApp()
    if (!catalog().get().providers[0]?.targets.every((target) => target.activeReferences.length === 0)) {
      throw new Error("universal-provider-references-not-released")
    }
    const released = catalog().get().providers[0]!
    const disabled = await catalog().act({
      kind: "update-universal-provider",
      providerId: released.id,
      providerRevision: released.providerRevision,
      name: released.name,
      baseUrl: released.baseUrl,
      credential: { kind: "keep" },
      targets: released.targets.map(({ target, model, authentication, routingRequirement }) => ({
        target, enabled: false, model, authentication, routingRequirement,
      })),
    })
    const disabledProvider = disabled.view.providers[0]!
    await catalog().act({
      kind: "synchronize-universal-provider",
      providerId: disabledProvider.id,
      providerRevision: disabledProvider.providerRevision,
    })
    await waitForSession(codex(), (view) => !view.providers.some((provider) => provider.id === generatedIds.codex), "Codex generated removal")
    await waitForSession(claude(), (view) => !view.providers.some((provider) => provider.id === generatedIds.claude), "Claude generated removal")
    const removable = catalog().get().providers[0]!
    await catalog().act({ kind: "delete-universal-provider", providerId: removable.id, providerRevision: removable.providerRevision })
    if (catalog().get().providers.length !== 0) throw new Error("universal-provider-source-delete-failed")

    const database = new Database(databasePath, { readonly: true })
    try {
      const counts = database.query(`SELECT
        (SELECT COUNT(*) FROM universal_providers) AS providers,
        (SELECT COUNT(*) FROM universal_provider_targets) AS targets,
        (SELECT COUNT(*) FROM universal_credentials) AS credentials,
        (SELECT COUNT(*) FROM providers WHERE generated_owner_id IS NOT NULL) AS generated,
        (SELECT COUNT(*) FROM universal_action_receipts) AS receipts`).get() as Record<string, number>
      if (counts.providers !== 0 || counts.targets !== 0 || counts.credentials !== 0 || counts.generated !== 0 || (counts.receipts ?? 0) < 6) {
        throw new Error("universal-provider-final-database-state-invalid")
      }
    } finally { database.close() }

    closeRenderer()
    await closeSessions()
    const naturalExit = await Promise.race([finalService.output.completed, Bun.sleep(deadlineMs).then(() => undefined)])
    if (!naturalExit || naturalExit.code !== 0 || naturalExit.signal !== null) throw new Error("universal-provider-natural-exit-failed")
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    await upstream.quiesce()
    await replacementUpstream.quiesce()
    scanRawRpcFramesNoSecrets(rpcStreams, secrets())
    scanNoSecrets([decodedFrames, nativeFrames], secrets(), "universal-provider-observed-surfaces")
    scanProcessOutputNoSecrets(services.map(({ output }) => output.streams).flat(), secrets())
  } finally {
    closeRenderer()
    await closeSessions()
    await writeFile(firstShutdown, "shutdown\n").catch(() => {})
    for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
    await Promise.all(services.map(({ output }) => Promise.race([output.completed.catch(() => undefined), Bun.sleep(deadlineMs)])))
    await upstream.stop()
    await replacementUpstream.stop()
  }
}, 70_000)

test("real processes preserve priced Request History without retaining successful payloads", async () => {
  const targets = ["codex", "claude"] as const
  type RecordTarget = typeof targets[number]
  const root = await mkdtemp(join(tmpdir(), "rr-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const codexConfig = join(userHome, ".codex/config.toml")
  const claudeSettings = join(userHome, ".claude/settings.json")
  const firstShutdown = join(root, "shutdown-first")
  const exactModels = {
    codex: "gpt-5.6",
    claude: "claude-sonnet-4-5",
  } as const
  const unpricedModels = {
    codex: "request-record-unpriced-codex",
    claude: "request-record-unpriced-claude",
  } as const
  const fixtureSecrets = {
    codex: {
      priced: "request-record-codex-priced-provider-secret",
      fallback: "request-record-codex-fallback-provider-secret",
      unpriced: "request-record-codex-unpriced-provider-secret",
    },
    claude: {
      priced: "request-record-claude-priced-provider-secret",
      fallback: "request-record-claude-fallback-provider-secret",
      unpriced: "request-record-claude-unpriced-provider-secret",
    },
  } as const
  const successBodySecrets = targets.flatMap((target) => [
    `request-record-${target}-priced-success-body-secret`,
    `request-record-${target}-fallback-success-body-secret`,
    `request-record-${target}-unpriced-success-body-secret`,
  ])
  const successHeaderSecrets = targets.flatMap((target) => [
    `request-record-${target}-priced-success-header-secret`,
    `request-record-${target}-fallback-success-header-secret`,
    `request-record-${target}-unpriced-success-header-secret`,
  ])
  const requestBodySecrets = {
    codex: "request-record-codex-cancel-request-body-secret",
    claude: "request-record-claude-cancel-request-body-secret",
  } as const
  const retainedMarkers = {
    codex: "retained-codex-failure-marker",
    claude: "retained-claude-failure-marker",
  } as const
  const fixtures = {
    codex: {
      priced: await startRequestRecordFixtureUpstream("codex", fixtureSecrets.codex.priced),
      fallback: await startRequestRecordFixtureUpstream("codex", fixtureSecrets.codex.fallback),
      unpriced: await startRequestRecordFixtureUpstream("codex", fixtureSecrets.codex.unpriced),
    },
    claude: {
      priced: await startRequestRecordFixtureUpstream("claude", fixtureSecrets.claude.priced),
      fallback: await startRequestRecordFixtureUpstream("claude", fixtureSecrets.claude.fallback),
      unpriced: await startRequestRecordFixtureUpstream("claude", fixtureSecrets.claude.unpriced),
    },
  }
  const allFixtures = targets.flatMap((target) => Object.values(fixtures[target]))
  const providerSecrets = targets.flatMap((target) => Object.values(fixtureSecrets[target]))
  const routingSecrets: Record<RecordTarget, Set<string>> = {
    codex: new Set<string>(),
    claude: new Set<string>(),
  }
  const nonPersistedSecrets = [
    ...successBodySecrets,
    ...successHeaderSecrets,
    ...Object.values(requestBodySecrets),
  ]
  const allRoutingSecrets = () => targets.flatMap((target) => [...routingSecrets[target]])
  const publicSecrets = () => [...providerSecrets, ...allRoutingSecrets(), ...nonPersistedSecrets]
  const services: Array<{ child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcessOutput> }> = []
  const readinessSockets: Array<ReturnType<typeof createConnection>> = []
  const rpcStreams: Buffer[][] = []
  const decodedFrames: unknown[] = []
  const observedViews: TargetView[] = []
  const renderedFrames: string[] = []
  const frozenPricingBefore = {} as Record<RecordTarget, { recordId: string; snapshot: string }>
  const sessions: Partial<Record<RecordTarget, TargetSession>> = {}
  const clients: Partial<Record<RecordTarget, RpcClient>> = {}
  const unsubscribes: Array<() => void> = []
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let recorder: ReturnType<typeof createRendererAudit> | undefined

  await mkdir(dirname(codexConfig), { recursive: true, mode: 0o700 })
  await mkdir(dirname(claudeSettings), { recursive: true, mode: 0o700 })
  await writeFile(codexConfig, '# operator comment survives\nunrelated = "keep-me"\n', { mode: 0o600 })
  await writeFile(claudeSettings, JSON.stringify({
    operator: { theme: "dark", hooks: ["keep"] },
    env: { OPERATOR_UNRELATED: "keep-me" },
  }, null, 2), { mode: 0o640 })
  await chmod(claudeSettings, 0o640)
  await chmod(fakeCodex, 0o755)
  await chmod(fakeClaude, 0o755)

  const startService = async (label: string, shutdownFile?: string) => {
    const args = [
      "--home", muxviaHome,
      "--test-codex-executable", fakeCodex,
      "--test-claude-executable", fakeClaude,
    ]
    if (shutdownFile) args.push("--test-shutdown-file", shutdownFile)
    const child = spawn(serviceBinary, args, {
      cwd: root,
      env: { HOME: userHome, PATH: `${dirname(fakeCodex)}:/usr/bin:/bin`, MUXVIA_INTEGRATION_TEST: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    })
    const output = captureProcessOutput(child)
    services.push({ child, output })
    const exitedBeforeReady = output.completed.then(({ code }) => {
      scanProcessOutputNoSecrets(output.streams, publicSecrets())
      const stderr = Buffer.concat(output.streams[1]).toString("utf8")
      const classification = stderr.includes("Routing Service state is unavailable")
        ? "state"
        : stderr.includes("another Routing Service owns this Muxvia Home")
          ? "lock"
          : stderr.includes("Routing Service control transport failed")
            ? "control"
            : "unknown"
      throw new Error(`request-record-service-exited:${label}:${code}:${classification}`)
    }) as Promise<never>
    await waitForEventTurnReadiness(
      () => probeUnixSocket(socketPath, readinessSockets),
      exitedBeforeReady,
      `request-record-${label}-service`,
    )
    return { child, output }
  }
  const connect = async (target: RecordTarget, release: string) => {
    const chunks: Buffer[] = []
    const decoder = new FrameDecoder()
    rpcStreams.push(chunks)
    const client = await eventBarrier(RpcClient.connect(socketPath, release, undefined, (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => {
        const bytes = Buffer.from(chunk)
        chunks.push(bytes)
        for (const frame of decoder.push(bytes)) {
          assertSecretSafeStructured(frame, publicSecrets(), `request-record-${target}-rpc-frame`)
          decodedFrames.push(frame)
        }
      })
      return socket
    }), `request-record-${target}-${release}-connect`)
    readinessSockets.splice(0).forEach((socket) => socket.destroy())
    return client
  }
  const openSessions = async (release: string) => {
    for (const target of targets) {
      const client = await connect(target, `${release}-${target}`)
      clients[target] = client
      const session = target === "codex"
        ? await eventBarrier(client.openTarget(target), `request-record-${release}-${target}-open`)
        : await eventBarrier(client.openTarget(target, {
            claudeConfigDir: null,
            selectorState: "unset",
            hostManagedState: "unmanaged",
            cwd: root,
          }), `request-record-${release}-${target}-open`)
      sessions[target] = session
      const collect = (view: Readonly<TargetView>) => {
        assertSecretSafeStructured(view, publicSecrets(), `request-record-${target}-view`)
        observedViews.push(view as TargetView)
      }
      collect(session.get())
      unsubscribes.push(session.subscribe(collect))
    }
  }
  const closeSessions = async () => {
    unsubscribes.splice(0).forEach((unsubscribe) => unsubscribe())
    const ownedSessions = targets.map((target) => sessions[target])
    const ownedClients = targets.map((target) => clients[target])
    for (const target of targets) {
      delete sessions[target]
      delete clients[target]
    }
    await Promise.all(ownedSessions.map((session) => session?.close().catch(() => undefined)))
    await Promise.all(ownedClients.map((client) => client?.close().catch(() => undefined)))
  }
  const sessionFor = (target: RecordTarget) => {
    const session = sessions[target]
    if (!session) throw new Error(`request-record-session-missing:${target}`)
    return session
  }
  const providerByName = (target: RecordTarget, name: string) => {
    const provider = sessionFor(target).get().providers.find((candidate) => candidate.name === name)
    if (!provider) throw new Error(`request-record-provider-missing:${target}:${name}`)
    return provider
  }
  const safeAct = async (target: RecordTarget, action: OrdinaryTargetAction, label: string) =>
    await actSecretSafe(sessionFor(target), action, publicSecrets(), `request-record-${target}-${label}`)
  const applyPlan = async (target: RecordTarget, names: readonly string[]) => {
    await safeAct(target, {
      kind: "save-failover-draft",
      members: names.map((name) => {
        const provider = providerByName(target, name)
        return { providerId: provider.id, providerRevision: provider.providerRevision }
      }),
    }, "save-plan")
    const draftRevision = sessionFor(target).get().failover?.draftRevision
    if (!draftRevision) throw new Error(`request-record-draft-revision-missing:${target}`)
    await safeAct(target, { kind: "apply-failover-chain", draftRevision }, "apply-plan")
    const epoch = sessionFor(target).get().failover?.activePlan?.epoch
    if (!epoch) throw new Error(`request-record-plan-epoch-missing:${target}`)
    return epoch
  }
  const readRoute = async (target: RecordTarget, model: string) => {
    const route = target === "codex"
      ? extractManagedConfig(await readFile(codexConfig, "utf8"), model)
      : extractClaudeManagedSettings(await readFile(claudeSettings, "utf8"), model, providerSecrets)
    routingSecrets[target].add(route.credential)
    return route
  }
  const post = async (target: RecordTarget, route: { endpoint: string; credential: string }) => target === "codex"
    ? await chunkedPost(route.endpoint, route.credential)
    : await claudePost(route.endpoint, route.credential, "/v1/messages", {
        model: "request-record-request-model",
        messages: [],
        max_tokens: 16,
        stream: true,
      })
  const waitForRecordCount = async (target: RecordTarget, count: number) => {
    let page: Awaited<ReturnType<TargetSession["listRequestRecords"]>> | undefined
    await waitFor(async () => {
      page = await sessionFor(target).listRequestRecords({ limit: 100 })
      assertSecretSafeStructured(page, publicSecrets(), `request-record-${target}-page-wait`)
      return page.records.length >= count
    }, `request-record-${target}-count-${count}`)
    return page!
  }
  const collectPages = async (target: RecordTarget) => {
    const records: Array<Awaited<ReturnType<TargetSession["listRequestRecords"]>>["records"][number]> = []
    let beforeCursor: string | undefined
    do {
      const page = await sessionFor(target).listRequestRecords({
        limit: 2,
        ...(beforeCursor === undefined ? {} : { beforeCursor }),
      })
      assertSecretSafeStructured(page, publicSecrets(), `request-record-${target}-paged-history`)
      records.push(...page.records)
      beforeCursor = page.nextCursor ?? undefined
    } while (beforeCursor !== undefined)
    return records
  }
  const readFrozenPricingSnapshot = (target: RecordTarget, recordId: string) => {
    const database = new Database(databasePath, { readonly: true })
    try {
      database.exec("PRAGMA busy_timeout = 5000")
      const snapshot = database.query(`
        SELECT catalog_version AS catalogVersion,
               source,
               source_model AS sourceModel,
               input_nano_usd_per_million AS inputNanoUsdPerMillion,
               output_nano_usd_per_million AS outputNanoUsdPerMillion,
               cache_read_multiplier_ppm AS cacheReadMultiplierPpm,
               cache_creation_multiplier_ppm AS cacheCreationMultiplierPpm,
               priced_at_unix_ms AS pricedAtUnixMs,
               estimated_cost_nano_usd AS estimatedCostNanoUsd
        FROM pricing_snapshots
        WHERE request_record_id = ?`).get(recordId) as Record<string, string | number> | null
      assertSecretSafeStructured(snapshot, publicSecrets(), `request-record-pricing-snapshot-${recordId}`)
      if (!snapshot) throw new Error("request-record-pricing-snapshot-missing")
      if (
        snapshot.catalogVersion !== "models.dev-d2ec701bace7"
        || snapshot.sourceModel !== exactModels[target]
        || typeof snapshot.estimatedCostNanoUsd !== "number"
        || snapshot.estimatedCostNanoUsd <= 0
      ) throw new Error(`request-record-pricing-snapshot-invalid:${target}`)
      return canonicalJson(snapshot)
    } finally {
      database.close()
    }
  }
  const renderHistory = async () => {
    setup = await testRender(() => <App sessions={{ codex: sessions.codex!, claude: sessions.claude! }} />, {
      width: 100,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    recorder = createRendererAudit(setup)
    recorder.start()
    for (const [index, target] of targets.entries()) {
      if (index > 0) {
        setup.mockInput.pressEscape()
        await waitForSecretSafeFrame(
          setup,
          (frame) => frame.includes("Choose a target"),
          publicSecrets(),
          `request-record-${target}-home`,
        )
      }
      setup.mockInput.pressKey(index === 0 ? "1" : "2")
      await waitForSecretSafeFrame(
        setup,
        (frame) => frame.includes(target === "codex" ? "Codex CLI" : "Claude Code")
          && frame.includes("Run a target action"),
        publicSecrets(),
        `request-record-${target}-target`,
      )
      await setup.mockInput.typeText("/activity")
      setup.mockInput.pressEnter()
      const listFrame = await waitForSecretSafeFrame(
        setup,
        (frame) => frame.includes("Request History")
          && frame.includes("Estimated")
          && frame.includes("Unpriced"),
        publicSecrets(),
        `request-record-${target}-activity-list`,
      )
      renderedFrames.push(listFrame)
      setup.mockInput.pressKey("down")
      await setup.renderOnce()
      const detailResultsBefore = decodedFrames.filter((frame) => (
        frame as { result?: { kind?: unknown } }
      ).result?.kind === "request-record-detail").length
      setup.mockInput.pressEnter()
      await waitFor(() => decodedFrames.filter((frame) => (
        frame as { result?: { kind?: unknown } }
      ).result?.kind === "request-record-detail").length === detailResultsBefore + 1,
      `request-record-${target}-activity-detail-response`)
      for (let pass = 0; pass < 4; pass++) await Promise.resolve()
      await setup.renderOnce()
      const detailFrame = captureSecretSafeFrame(
        setup,
        renderedFrames,
        publicSecrets(),
        `request-record-${target}-activity-detail`,
      )
      if (!detailFrame.includes("sensitive request") || !detailFrame.includes("truncated")) {
        const selectedSuccess = detailFrame.includes("Successful records retain no response payload")
        const displayClipped = detailFrame.includes("Display clipped to this terminal")
        const historyOpen = detailFrame.includes("Request History")
        const detailLoading = detailFrame.includes("Loading retained failure detail")
        const markerVisible = detailFrame.includes(retainedMarkers[target])
        throw new Error(`request-record-activity-detail-mismatch:${target}:${selectedSuccess}:${displayClipped}:${historyOpen}:${detailLoading}:${markerVisible}`)
      }
      renderedFrames.push(detailFrame)
      setup.mockInput.pressEscape()
      await waitForSecretSafeFrame(
        setup,
        (frame) => frame.includes("Run a target action") && !frame.includes("Request History"),
        publicSecrets(),
        `request-record-${target}-activity-close`,
      )
    }
  }
  const renderHistoryThroughRealMuxvia = async () => {
    for (const target of targets) {
      if (clients[target]?.serviceMetadata.release !== "0.1.0") {
        throw new Error(`request-record-real-muxvia-service-release-invalid:${target}`)
      }
    }
    const output: Uint8Array[] = []
    const terminal = new Bun.Terminal({
      cols: 100,
      rows: 30,
      name: "xterm-256color",
      data: (_terminal, data) => { output.push(Uint8Array.from(data)) },
    })
    const env: Record<string, string | undefined> = {
      ...process.env,
      HOME: userHome,
      CODEX_HOME: join(userHome, ".codex"),
      PATH: `${dirname(fakeCodex)}:/usr/bin:/bin`,
      TERM: "xterm-256color",
    }
    delete env.OTUI_NO_NATIVE_RENDER
    const proc = Bun.spawn([
      "/usr/bin/env",
      process.execPath,
      "run",
      "--preload=@opentui/solid/preload",
      "--jsx-import-source=@opentui/solid",
      controlPlaneEntry,
      "--service", serviceBinary,
      "--socket", socketPath,
    ], {
      cwd: root,
      env,
      terminal,
    })
    const rendered = () => new TextDecoder().decode(Buffer.concat(output.map((chunk) => Buffer.from(chunk))))
    try {
      try {
        await waitForSecretSafeText(
          rendered,
          (screen) => screen.includes("Choose a target"),
          publicSecrets(),
          "request-record-real-muxvia-home",
        )
      } catch (error) {
        if (error instanceof Error
          && error.message === "secret-scan-failed:request-record-real-muxvia-home-text") {
          throw error
        }
        const screen = rendered()
        scanNoSecrets([screen], publicSecrets(), "request-record-real-muxvia-startup")
        const classes = [
          "service-unavailable", "Connection", "Terminal", "TypeError", "Error", "ENOENT",
          "unsupported", "failed", "Cannot", "not found", "module", "package", "command",
          "file", "@opentui", "@opentui/core-darwin", "@opentui/solid", "solid-js",
          "index.tsx", "hand-over",
        ].map((marker) => screen.includes(marker)).join(":")
        throw new Error(`request-record-real-muxvia-startup-invalid:${proc.exitCode}:${screen.includes("Codex CLI")}:${screen.includes("Claude Code")}:${screen.includes("Choose a target")}:${screen.length > 0}:${classes}`)
      }
      terminal.write("1")
      await waitForSecretSafeText(
        rendered,
        (screen) => screen.includes("Run a target action"),
        publicSecrets(),
        "request-record-real-muxvia-target",
      )
      terminal.write("/activity\r")
      await waitForSecretSafeText(
        rendered,
        (screen) => screen.includes("Request History")
          && screen.includes("Estimated")
          && screen.includes("Unpriced"),
        publicSecrets(),
        "request-record-real-muxvia-activity",
      )
      const screen = rendered()
      scanNoSecrets([screen], publicSecrets(), "request-record-real-muxvia-renderer")
      renderedFrames.push(screen)
      proc.kill("SIGINT")
      const exit = await Promise.race([
        proc.exited,
        Bun.sleep(deadlineMs).then(() => undefined),
      ])
      if (exit !== 0) throw new Error("request-record-real-muxvia-exit-invalid")
    } finally {
      if (proc.exitCode === null) proc.kill("SIGKILL")
      await proc.exited.catch(() => {})
      terminal.close()
    }
  }
  const closeRenderer = () => {
    recorder?.stop()
    if (recorder) renderedFrames.push(...recorder.frames())
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    setup = undefined
    recorder = undefined
  }

  try {
    const first = await startService("first", firstShutdown)
    await openSessions("first")
    const planEpochs = {} as Record<RecordTarget, { priced: string; unpriced: string }>
    const routes = {} as Record<RecordTarget, { endpoint: string; credential: string }>

    for (const target of targets) {
      const label = target === "codex" ? "Codex" : "Claude"
      const targetFixtures = fixtures[target]
      const targetSecrets = fixtureSecrets[target]
      for (const [role, model] of [
        ["priced", exactModels[target]],
        ["fallback", exactModels[target]],
        ["unpriced", unpricedModels[target]],
      ] as const) {
        await safeAct(target, {
          kind: "create-provider",
          name: `${label} ${role}`,
          baseUrl: targetFixtures[role].baseUrl,
          model,
          credential: { kind: "replace", value: targetSecrets[role] },
          authentication: target === "codex" ? "openai-bearer" : "anthropic-bearer",
          presetKey: null,
        }, `create-${role}`)
      }
      const pricedName = `${label} priced`
      const fallbackName = `${label} fallback`
      await safeAct(target, {
        kind: "activate-provider",
        providerId: providerByName(target, pricedName).id,
        mode: "takeover",
      }, "activate-priced")
      routes[target] = await readRoute(target, exactModels[target])
      planEpochs[target] = {
        priced: await applyPlan(target, [pricedName, fallbackName]),
        unpriced: "",
      }

      const pricedBody = `request-record-${target}-priced-success-body-secret`
      const pricedHeader = `request-record-${target}-priced-success-header-secret`
      targetFixtures.priced.setBehavior({ kind: "success", bodySecret: pricedBody, headerSecret: pricedHeader })
      const pricedResponse = await post(target, routes[target])
      if (
        pricedResponse.status !== (target === "codex" ? 201 : 200)
        || pricedResponse.headers["x-request-record-success"] !== pricedHeader
        || !pricedResponse.body.includes(pricedBody)
      ) throw new Error(`request-record-priced-response-mismatch:${target}`)
      await waitForRecordCount(target, 1)

      targetFixtures.priced.setBehavior({ kind: "retryable-failure" })
      const fallbackBody = `request-record-${target}-fallback-success-body-secret`
      const fallbackHeader = `request-record-${target}-fallback-success-header-secret`
      targetFixtures.fallback.setBehavior({ kind: "success", bodySecret: fallbackBody, headerSecret: fallbackHeader })
      const fallbackResponse = await post(target, routes[target])
      if (
        fallbackResponse.status !== (target === "codex" ? 201 : 200)
        || fallbackResponse.headers["x-request-record-success"] !== fallbackHeader
        || !fallbackResponse.body.includes(fallbackBody)
      ) throw new Error(`request-record-fallback-response-mismatch:${target}`)
      const pricedHistory = await waitForRecordCount(target, 2)
      const pricedRecord = pricedHistory.records.find((record) => record.providerName === pricedName)
      if (!pricedRecord) throw new Error(`request-record-priced-history-missing:${target}`)
      frozenPricingBefore[target] = {
        recordId: pricedRecord.id,
        snapshot: readFrozenPricingSnapshot(target, pricedRecord.id),
      }

      const unpricedName = `${label} unpriced`
      await safeAct(target, {
        kind: "activate-provider",
        providerId: providerByName(target, unpricedName).id,
        mode: "takeover",
      }, "activate-unpriced")
      routes[target] = await readRoute(target, unpricedModels[target])
      planEpochs[target].unpriced = await applyPlan(target, [unpricedName])
      const unpricedBody = `request-record-${target}-unpriced-success-body-secret`
      const unpricedHeader = `request-record-${target}-unpriced-success-header-secret`
      targetFixtures.unpriced.setBehavior({ kind: "success", bodySecret: unpricedBody, headerSecret: unpricedHeader })
      const unpricedResponse = await post(target, routes[target])
      if (
        unpricedResponse.status !== (target === "codex" ? 201 : 200)
        || unpricedResponse.headers["x-request-record-success"] !== unpricedHeader
        || !unpricedResponse.body.includes(unpricedBody)
      ) throw new Error(`request-record-unpriced-response-mismatch:${target}`)
      await waitForRecordCount(target, 3)

      const retainedPayload = JSON.stringify({
        marker: retainedMarkers[target],
        providerCredential: targetSecrets.unpriced,
        routingCredential: routes[target].credential,
        payload: "x".repeat(70_000),
      })
      targetFixtures.unpriced.setBehavior({ kind: "retained-failure", payload: retainedPayload })
      const failedResponse = await post(target, routes[target])
      if (failedResponse.status !== 429 || failedResponse.body.length <= 65_536) {
        throw new Error(`request-record-failure-response-mismatch:${target}`)
      }
      await waitForRecordCount(target, 4)

      const priced = providerByName(target, pricedName)
      await safeAct(target, {
        kind: "update-provider",
        providerId: priced.id,
        providerRevision: priced.providerRevision,
        name: `${label} priced edited`,
        baseUrl: priced.baseUrl,
        model: `${exactModels[target]}-edited`,
        credential: { kind: "keep" },
        authentication: priced.authentication,
        routingRequirement: priced.routingRequirement,
      }, "edit-historical-provider")

      targetFixtures.unpriced.setBehavior({ kind: "cancelled-stream" })
      const cancelled = cancelRoutedPost(
        target,
        routes[target].endpoint,
        routes[target].credential,
        requestBodySecrets[target],
      )
      await eventBarrier(targetFixtures.unpriced.waitForCancellationStart(), `request-record-${target}-cancel-start`)
      const cancelledStatus = await eventBarrier(cancelled, `request-record-${target}-cancel-client`)
      if (cancelledStatus !== 200) throw new Error(`request-record-cancel-status-invalid:${target}`)
    }

    await closeSessions()
    await writeFile(firstShutdown, "shutdown\n")
    const firstExit = await eventBarrier(first.output.completed, "request-record-first-exit")
    if (firstExit.code !== 0 || firstExit.signal !== null) throw new Error("request-record-first-exit-invalid")
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })

    const second = await startService("restart")
    await openSessions("restart")
    for (const target of targets) {
      const label = target === "codex" ? "Codex" : "Claude"
      const records = await collectPages(target)
      if (records.length !== 5) throw new Error(`request-record-history-count-invalid:${target}`)
      const expected = [
        ["cancelled", `${label} unpriced`, unpricedModels[target], planEpochs[target].unpriced],
        ["upstream-error", `${label} unpriced`, unpricedModels[target], planEpochs[target].unpriced],
        ["success", `${label} unpriced`, unpricedModels[target], planEpochs[target].unpriced],
        ["success", `${label} fallback`, exactModels[target], planEpochs[target].priced],
        ["success", `${label} priced`, exactModels[target], planEpochs[target].priced],
      ] as const
      for (const [index, record] of records.entries()) {
        const row = expected[index]!
        if (
          record.outcome !== row[0]
          || record.providerName !== row[1]
          || record.model !== row[2]
          || record.planEpoch !== row[3]
          || "errorPayload" in record
        ) throw new Error(`request-record-history-row-invalid:${target}:${index}`)
      }
      if (
        records[0]!.hasErrorPayload
        || !records[1]!.hasErrorPayload
        || !records[1]!.errorPayloadTruncated
        || records.slice(2).some((record) => record.hasErrorPayload)
      ) throw new Error(`request-record-payload-metadata-invalid:${target}`)
      if (
        records[0]!.estimatedCostNanoUsd !== null
        || records[1]!.estimatedCostNanoUsd !== null
        || records[2]!.estimatedCostNanoUsd !== null
        || !records[3]!.estimatedCostNanoUsd
        || !records[4]!.estimatedCostNanoUsd
      ) throw new Error(`request-record-pricing-projection-invalid:${target}`)
      const failureDetail = await sessionFor(target).inspectRequestRecord(records[1]!.id)
      assertSecretSafeStructured(failureDetail, publicSecrets(), `request-record-${target}-failure-detail`)
      if (
        failureDetail.errorPayload?.length !== 65_536
        || !failureDetail.errorPayload.includes(retainedMarkers[target])
        || failureDetail.record.errorPayloadTruncated !== true
        || failureDetail.errorPayloadSensitive !== true
      ) throw new Error(`request-record-failure-detail-invalid:${target}`)
      if (
        records[4]!.id !== frozenPricingBefore[target].recordId
        || readFrozenPricingSnapshot(target, records[4]!.id) !== frozenPricingBefore[target].snapshot
      ) throw new Error(`request-record-frozen-pricing-invalid:${target}`)
    }

    await renderHistory()
    closeRenderer()
    await renderHistoryThroughRealMuxvia()

    for (const target of targets) {
      await safeAct(target, { kind: "disable-takeover" }, "disable-takeover")
    }
    const finalRoutes = { ...routes }
    await closeSessions()
    const naturalExit = await Promise.race([
      second.output.completed,
      Bun.sleep(deadlineMs).then(() => undefined),
    ])
    if (!naturalExit || naturalExit.code !== 0 || naturalExit.signal !== null) {
      throw new Error("request-record-natural-exit-failed")
    }
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    for (const target of targets) {
      await expect(post(target, finalRoutes[target])).rejects.toBeDefined()
    }

    const database = new Database(databasePath, { readonly: true })
    try {
      const counts = database.query(`SELECT
        (SELECT COUNT(*) FROM request_records) AS records,
        (SELECT COUNT(*) FROM pricing_snapshots) AS pricing,
        (SELECT COUNT(*) FROM request_records WHERE outcome = 'success' AND error_payload IS NOT NULL) AS successfulPayloads,
        (SELECT COUNT(*) FROM request_records WHERE outcome = 'upstream-error' AND length(error_payload) = 65536 AND error_payload_truncated = 1) AS cappedFailures`).get() as Record<string, number>
      if (counts.records !== 10 || counts.pricing !== 4 || counts.successfulPayloads !== 0 || counts.cappedFailures !== 2) {
        throw new Error("request-record-final-database-counts-invalid")
      }
      const definitions = database.query(`SELECT name FROM sqlite_master
        WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name`).all() as Array<{ name: string }>
      for (const { name } of definitions) {
        const quoted = `"${name.replaceAll('"', '""')}"`
        scanNoSecrets(database.query(`SELECT * FROM ${quoted}`).all(), nonPersistedSecrets, `request-record-database-${name}`)
      }
    } finally { database.close() }
    auditSqliteSecretLocations(databasePath, {
      providerSecrets: targets.flatMap((target) => {
        const label = target === "codex" ? "Codex" : "Claude"
        return [
          { target, name: `${label} priced edited`, secret: fixtureSecrets[target].priced },
          { target, name: `${label} fallback`, secret: fixtureSecrets[target].fallback },
          { target, name: `${label} unpriced`, secret: fixtureSecrets[target].unpriced },
        ]
      }),
      routingSecrets: targets.flatMap((target) => [...routingSecrets[target]].map((secret) => ({ target, secret }))),
    })
    scanRawRpcFramesNoSecrets(rpcStreams, publicSecrets())
    scanNoSecrets([decodedFrames, observedViews, renderedFrames], publicSecrets(), "request-record-public-surfaces")
    scanProcessOutputNoSecrets(services.flatMap(({ output }) => output.streams), publicSecrets())
    for (const target of targets) {
      for (const fixture of Object.values(fixtures[target])) {
        scanNoSecrets(fixture.calls, allRoutingSecrets(), `request-record-${target}-upstream-routing-secret`)
        if (fixture.calls.some((call) => call.authorization !== `Bearer ${fixture.credential}`)) {
          throw new Error(`request-record-upstream-credential-invalid:${target}`)
        }
      }
    }
  } finally {
    closeRenderer()
    readinessSockets.splice(0).forEach((socket) => socket.destroy())
    await closeSessions()
    await writeFile(firstShutdown, "shutdown\n").catch(() => {})
    for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
    await Promise.all(services.map(({ output }) => Promise.race([
      output.completed.catch(() => undefined),
      Bun.sleep(deadlineMs),
    ])))
    await Promise.all(allFixtures.map((fixture) => fixture.stop()))
  }
}, 70_000)
