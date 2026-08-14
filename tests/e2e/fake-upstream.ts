import { createServer } from "node:http"

export const SSE_BYTES = [
  "event: response.output_text.delta\ndata: {\"delta\":\"hel\"}\n\n",
  "event: response.output_text.delta\ndata: {\"delta\":\"lo\"}\n\n",
  "event: response.completed\ndata: {}\n\n",
] as const

export interface CapturedRequest {
  authorization: string | null
  headers: Record<string, string | null>
  contentType: string | null
  method: string
  testHeader: string | null
  body: string
  path: string
}

function header(value: string | string[] | undefined): string | null {
  return Array.isArray(value) ? value.join(", ") : value ?? null
}

export async function startFakeUpstream(expectedProviderCredential: string) {
  const calls: CapturedRequest[] = []
  const callWaiters = new Set<() => void>()
  const server = createServer(async (request, response) => {
    const body: Buffer[] = []
    for await (const chunk of request) body.push(Buffer.from(chunk))
    const headers = Object.fromEntries(Object.entries(request.headers).map(([name, value]) => [
      name,
      header(value),
    ]))
    calls.push({
      authorization: header(request.headers.authorization),
      headers,
      contentType: header(request.headers["content-type"]),
      method: request.method ?? "",
      testHeader: header(request.headers["x-test-preserved"]),
      body: Buffer.concat(body).toString("utf8"),
      path: request.url ?? "",
    })
    for (const notify of callWaiters) notify()

    if (request.method === "GET" && request.url === "/v1/models") {
      if (request.headers.authorization !== `Bearer ${expectedProviderCredential}`) {
        response.writeHead(403).end()
        return
      }
      response.writeHead(200, { "content-type": "application/json" })
      response.end(JSON.stringify({
        data: [
          { id: "gpt-fixture-b", owned_by: "fixture" },
          { id: "gpt-fixture-a" },
          { id: "gpt-fixture-b", owned_by: "duplicate" },
        ],
      }))
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
    stop: async () => {
      server.closeAllConnections()
      await new Promise<void>((resolve) => server.close(() => resolve()))
    },
  }
}
