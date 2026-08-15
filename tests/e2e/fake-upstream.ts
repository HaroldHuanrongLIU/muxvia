import { createServer } from "node:http"

export const SSE_BYTES = [
  "event: response.output_text.delta\ndata: {\"delta\":\"hel\"}\n\n",
  "event: response.output_text.delta\ndata: {\"delta\":\"lo\"}\n\n",
  "event: response.completed\ndata: {}\n\n",
] as const

export const CLAUDE_SSE_BYTES = [
  "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
  "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hello\"}}\n\n",
  "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
] as const

export interface CapturedRequest {
  authorization: string | null
  apiKey?: string | null
  headers: Record<string, string | null>
  contentType: string | null
  method: string
  testHeader: string | null
  body: string
  path: string
}

export interface FakeUpstreamOptions {
  apiKeyCredentials?: readonly string[]
  bearerCredentials?: readonly string[]
}

function header(value: string | string[] | undefined): string | null {
  return Array.isArray(value) ? value.join(", ") : value ?? null
}

export async function startFakeUpstream(
  expectedProviderCredential: string,
  onCapturedRequest?: (request: CapturedRequest) => void,
  options: FakeUpstreamOptions = {},
) {
  const calls: CapturedRequest[] = []
  const callWaiters = new Set<() => void>()
  const handlerIdleWaiters = new Set<() => void>()
  const handlerCountWaiters = new Set<() => void>()
  let activeHandlers = 0
  let quiescePromise: Promise<void> | undefined
  let delayedStartedResolve = () => {}
  let delayedReleaseResolve = () => {}
  const delayedStarted = new Promise<void>((resolve) => { delayedStartedResolve = resolve })
  const delayedRelease = new Promise<void>((resolve) => { delayedReleaseResolve = resolve })
  const server = createServer(async (request, response) => {
    activeHandlers++
    response.once("close", () => {
      activeHandlers--
      if (activeHandlers === 0) {
        for (const resolveIdle of handlerIdleWaiters) resolveIdle()
        handlerIdleWaiters.clear()
      }
    })
    for (const notify of handlerCountWaiters) notify()
    const body: Buffer[] = []
    for await (const chunk of request) body.push(Buffer.from(chunk))
    const headers = Object.fromEntries(Object.entries(request.headers).map(([name, value]) => [
      name,
      header(value),
    ]))
    const capturedRequest = {
      authorization: header(request.headers.authorization),
      apiKey: header(request.headers["x-api-key"]),
      headers,
      contentType: header(request.headers["content-type"]),
      method: request.method ?? "",
      testHeader: header(request.headers["x-test-preserved"]),
      body: Buffer.concat(body).toString("utf8"),
      path: request.url ?? "",
    }
    onCapturedRequest?.(capturedRequest)
    calls.push(capturedRequest)
    for (const notify of callWaiters) notify()

    if (request.method === "GET" && request.url?.startsWith("/v1/models")) {
      const authorized = request.headers.authorization === `Bearer ${expectedProviderCredential}`
        || options.bearerCredentials?.includes(header(request.headers.authorization)?.replace(/^Bearer /, "") ?? "")
        || options.apiKeyCredentials?.includes(header(request.headers["x-api-key"]) ?? "")
      if (!authorized) {
        response.writeHead(403).end()
        return
      }
      response.writeHead(200, { "content-type": "application/json" })
      response.end(JSON.stringify({
        data: [
          { id: "gpt-fixture-b", owned_by: "fixture", display_name: "Fixture B" },
          { id: "gpt-fixture-a", display_name: "Fixture A" },
          { id: "gpt-fixture-b", owned_by: "duplicate", display_name: "Fixture B duplicate" },
        ],
        has_more: false,
      }))
      return
    }
    if (request.method === "POST" && request.url?.startsWith("/v1/messages")) {
      const apiKey = header(request.headers["x-api-key"])
      const bearer = header(request.headers.authorization)?.replace(/^Bearer /, "") ?? null
      const authorized = (apiKey !== null && options.apiKeyCredentials?.includes(apiKey))
        || (bearer !== null && options.bearerCredentials?.includes(bearer))
      if (!authorized) {
        response.writeHead(403).end()
        return
      }
      const parsed = JSON.parse(capturedRequest.body) as Record<string, unknown>
      if (request.url.startsWith("/v1/messages/count_tokens")) {
        response.writeHead(200, { "content-type": "application/json", "x-upstream": "claude-fixture" })
        response.end('{"input_tokens":7}')
        return
      }
      if (parsed.fixture_error === true) {
        response.writeHead(429, { "content-type": "application/json", "x-upstream": "claude-fixture" })
        response.end('{"type":"error","error":{"type":"rate_limit_error","message":"fixture"}}')
        return
      }
      if (parsed.stream === true) {
        response.writeHead(200, { "content-type": "text/event-stream", "x-upstream": "claude-fixture" })
        response.write(CLAUDE_SSE_BYTES[0])
        if (parsed.fixture_delay === true) {
          delayedStartedResolve()
          await delayedRelease
        }
        for (const part of CLAUDE_SSE_BYTES.slice(1)) response.write(part)
        response.end()
        return
      }
      response.writeHead(200, { "content-type": "application/json", "x-upstream": "claude-fixture" })
      response.end('{"id":"msg_fixture","type":"message","role":"assistant","content":[]}')
      return
    }
    if (request.method === "GET") {
      response.writeHead(401, { "x-fixture-reachability": "reachable" }).end()
      return
    }
    if (request.method !== "POST" || request.url !== "/v1/responses") {
      response.writeHead(404).end()
      return
    }
    if (request.headers.authorization !== `Bearer ${expectedProviderCredential}`) {
      response.writeHead(403).end()
      return
    }
    response.writeHead(201, { "content-type": "text/event-stream", "x-upstream": "fixture" })
    for (const part of SSE_BYTES) response.write(part)
    response.end()
  })
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject)
    server.listen(0, "127.0.0.1", () => resolve())
  })
  const address = server.address()
  if (!address || typeof address === "string") throw new Error("fake upstream did not bind TCP")
  const waitForHandlersToDrain = () => activeHandlers === 0
    ? Promise.resolve()
    : new Promise<void>((resolve) => handlerIdleWaiters.add(resolve))
  const quiesce = () => {
    if (!quiescePromise) {
      quiescePromise = (async () => {
        await waitForHandlersToDrain()
        await new Promise<void>((resolve) => server.close(() => resolve()))
        await waitForHandlersToDrain()
      })()
    }
    return quiescePromise
  }
  return {
    calls,
    baseUrl: `http://127.0.0.1:${address.port}/v1`,
    waitForCallCount: async (count: number) => {
      if (calls.length >= count) return
      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => {
          callWaiters.delete(notify)
          reject(new Error(`Timed out waiting for fake upstream call ${count}; saw ${calls.length}`))
        }, 10_000)
        const notify = () => {
          if (calls.length < count) return
          clearTimeout(timeout)
          callWaiters.delete(notify)
          resolve()
        }
        callWaiters.add(notify)
      })
    },
    waitForActiveHandlerCount: async (count: number) => {
      if (activeHandlers >= count) return
      await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => {
          handlerCountWaiters.delete(notify)
          reject(new Error(`Timed out waiting for fake upstream active handler ${count}; saw ${activeHandlers}`))
        }, 10_000)
        const notify = () => {
          if (activeHandlers < count) return
          clearTimeout(timeout)
          handlerCountWaiters.delete(notify)
          resolve()
        }
        handlerCountWaiters.add(notify)
      })
    },
    waitForDelayedStart: () => delayedStarted,
    releaseDelayed: () => delayedReleaseResolve(),
    quiesce,
    stop: async () => {
      const quiesced = quiesce()
      server.closeAllConnections()
      await quiesced
    },
  }
}
