import { createServer } from "node:http"

export const SSE_BYTES = [
  "event: response.output_text.delta\ndata: {\"delta\":\"hel\"}\n\n",
  "event: response.output_text.delta\ndata: {\"delta\":\"lo\"}\n\n",
  "event: response.completed\ndata: {}\n\n",
] as const

export interface CapturedRequest {
  authorization: string | null
  contentType: string | null
  testHeader: string | null
  body: string
  path: string
}

function header(value: string | string[] | undefined): string | null {
  return Array.isArray(value) ? value.join(", ") : value ?? null
}

export async function startFakeUpstream() {
  const calls: CapturedRequest[] = []
  const server = createServer(async (request, response) => {
    const body: Buffer[] = []
    for await (const chunk of request) body.push(Buffer.from(chunk))
    calls.push({
      authorization: header(request.headers.authorization),
      contentType: header(request.headers["content-type"]),
      testHeader: header(request.headers["x-test-preserved"]),
      body: Buffer.concat(body).toString("utf8"),
      path: request.url ?? "",
    })
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
    stop: async () => {
      server.closeAllConnections()
      await new Promise<void>((resolve) => server.close(() => resolve()))
    },
  }
}
