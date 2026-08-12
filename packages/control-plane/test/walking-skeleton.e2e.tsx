/** @jsxImportSource @opentui/solid */
import { Database } from "bun:sqlite"
import { afterEach, expect, test } from "bun:test"
import { testRender } from "@opentui/solid"
import { spawn } from "node:child_process"
import { createConnection } from "node:net"
import { request as httpRequest } from "node:http"
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"

import { RpcClient } from "../src/control/rpc-client"
import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { App } from "../src/ui/app"
import type { TargetView } from "../src/control/types"
import { SSE_BYTES, startFakeUpstream } from "../../../tests/e2e/fake-upstream"

const providerSecret = "provider-secret-must-not-escape"
const wrongRoutingSecret = "routing-secret-must-not-escape"
const repoRoot = resolve(import.meta.dir, "../../..")
const serviceBinary = resolve(repoRoot, "target/debug/muxvia-routing")
const fakeCodex = resolve(repoRoot, "tests/e2e/fixtures/fake-codex")
const deadlineMs = 10_000
const roots: string[] = []

afterEach(async () => {
  for (const root of roots.splice(0)) await rm(root, { recursive: true, force: true })
})

async function controlledTreeFingerprint(path: string): Promise<string> {
  try {
    const own = await stat(path)
    const metadata = [own.mode, own.size, own.mtimeMs]
    if (!own.isDirectory()) {
      return JSON.stringify([metadata, Buffer.from(await readFile(path)).toString("base64")])
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

async function waitFor(predicate: () => boolean | Promise<boolean>, label: string): Promise<void> {
  const deadline = Date.now() + deadlineMs
  while (!(await predicate())) {
    if (Date.now() >= deadline) throw new Error(`Timed out waiting for ${label}`)
    await Bun.sleep(10)
  }
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

function extractManagedConfig(text: string): { endpoint: string; credential: string } {
  expect(text).toContain("# operator comment survives")
  expect(text).toContain('unrelated = "keep-me"')
  expect(text).toContain('model = "gpt-test"')
  expect(text).toContain('model_provider = "muxvia_codex"')
  expect(text).toContain('[model_providers.muxvia_codex]')
  expect(text).toContain('name = "Muxvia"')
  expect(text).toContain('wire_api = "responses"')
  expect(text).toContain("supports_websockets = false")
  const endpoint = text.match(/base_url\s*=\s*"(http:\/\/127\.0\.0\.1:\d+\/v1)"/)?.[1]
  const credential = text.match(/X-Muxvia-Routing-Credential"\s*=\s*"([a-f0-9]{64})"/)?.[1]
  if (!endpoint || !credential) throw new Error("managed endpoint or credential missing")
  return { endpoint, credential }
}

function scanNoSecrets(values: unknown[]): void {
  const joined = values.map((value) => typeof value === "string" ? value : JSON.stringify(value)).join("\n")
  expect(joined).not.toContain(providerSecret)
  expect(joined).not.toContain(wrongRoutingSecret)
}

function scanRawRpcFramesNoSecrets(frames: Buffer[]): void {
  expect(frames.length).toBeGreaterThan(0)
  const decoder = new FrameDecoder()
  const decodedFrames = frames.flatMap((frame) => decoder.push(frame))
  decoder.finish()
  scanNoSecrets(decodedFrames)
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
  expect(() => scanRawRpcFramesNoSecrets(splitEncodedFrameWithin(cleanFrame, "safe"))).not.toThrow()

  const providerFrame = encodeFrame({ type: "target-view", additiveDiagnostic: providerSecret })
  expect(() => scanRawRpcFramesNoSecrets(
    splitEncodedFrameWithin(providerFrame, providerSecret),
  )).toThrow()

  const routingFrame = encodeFrame({ type: "response", additiveDiagnostic: wrongRoutingSecret })
  expect(() => scanRawRpcFramesNoSecrets(
    splitEncodedFrameWithin(routingFrame, wrongRoutingSecret),
  )).toThrow()
})

test("real processes prove the Codex takeover walking skeleton", async () => {
  const root = await mkdtemp(join(tmpdir(), "muxvia-e2e-"))
  roots.push(root)
  const userHome = join(root, "home")
  const operatorHomeCanary = join(root, "operator-home-canary")
  const codexHomeCanary = join(operatorHomeCanary, ".codex")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const shutdownFile = join(root, "shutdown")
  await mkdir(join(operatorHomeCanary, ".muxvia/state"), { recursive: true, mode: 0o700 })
  await mkdir(codexHomeCanary, { recursive: true, mode: 0o700 })
  await writeFile(join(codexHomeCanary, "config.toml"), 'canary = "operator-codex-home"\n', { mode: 0o600 })
  await writeFile(join(operatorHomeCanary, ".muxvia/state/canary"), "operator muxvia state\n", { mode: 0o600 })
  const operatorHomeCanaryBefore = await controlledTreeFingerprint(operatorHomeCanary)
  await mkdir(join(userHome, ".codex"), { recursive: true, mode: 0o700 })
  await writeFile(join(userHome, ".codex/config.toml"), '# operator comment survives\nunrelated = "keep-me"\n', { mode: 0o600 })
  await chmod(fakeCodex, 0o755)
  const upstream = await startFakeUpstream()
  const logs: string[] = []
  const rpcFrames: Buffer[] = []
  const views: TargetView[] = []
  let service: ReturnType<typeof spawn> | undefined
  let client: RpcClient | undefined
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
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
    service.stdout?.on("data", (data) => logs.push(String(data)))
    service.stderr?.on("data", (data) => logs.push(String(data)))

    await waitFor(async () => {
      try {
        await stat(socketPath)
        return true
      } catch { return false }
    }, "control socket")

    client = await RpcClient.connect(socketPath, "e2e", undefined, (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => rpcFrames.push(Buffer.from(chunk)))
      return socket
    })
    const session = await client.openTarget("codex")
    views.push(session.get() as TargetView)
    const unsubscribe = session.subscribe((view) => views.push(view))
    setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false })
    await setup.renderOnce()
    setup.mockInput.pressKey("p")
    await setup.mockInput.typeText("Fixture Provider")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(upstream.baseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("gpt-test")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(providerSecret)
    await setup.renderOnce()
    const formFrame = setup.captureCharFrame()
    expect(formFrame.includes(providerSecret)).toBeFalse()
    setup.mockInput.pressEnter()
    await waitFor(() => session.get().providers.length === 1, "Provider save")
    await setup.renderOnce()
    const savedFrame = setup.captureCharFrame()
    expect(savedFrame.includes("Fixture Provider") && savedFrame.includes("Credential  Present")).toBeTrue()
    setup.mockInput.pressKey("a")
    await waitFor(() => session.get().takeover.state === "active", "Takeover activation")
    await setup.renderOnce()
    const activeFrame = setup.captureCharFrame()
    expect(activeFrame.includes("Mode       Takeover") && activeFrame.includes("Current    Fixture Provider")).toBeTrue()
    expect(activeFrame.includes("Restart Codex")).toBeTrue()

    const configText = await readFile(join(userHome, ".codex/config.toml"), "utf8")
    const managed = extractManagedConfig(configText)
    const wrong = await chunkedPost(managed.endpoint, wrongRoutingSecret)
    expect(wrong.status).toBe(401)
    expect(upstream.calls).toHaveLength(0)

    const valid = await chunkedPost(managed.endpoint, managed.credential)
    expect(valid.status).toBe(201)
    expect(valid.headers["content-type"]).toStartWith("text/event-stream")
    expect(valid.body).toBe(SSE_BYTES.join(""))
    expect(upstream.calls).toEqual([{
      authorization: `Bearer ${providerSecret}`,
      contentType: "application/json",
      testHeader: "preserved",
      body: '{"model":"gpt-test","input":"hello"}',
      path: "/v1/responses",
    }])
    await waitFor(() => views.some((view) => view.servingProviderId !== null), "Serving push")
    await setup.renderOnce()
    const servedFrame = setup.captureCharFrame()
    expect(servedFrame.includes("Serving    Fixture Provider")).toBeTrue()

    unsubscribe()
    setup.renderer.destroy()
    setup = undefined
    await session.close()
    client = undefined
    const second = await chunkedPost(managed.endpoint, managed.credential)
    expect(second.status).toBe(201)
    expect(service.exitCode).toBeNull()

    await writeFile(shutdownFile, "shutdown\n")
    await waitFor(() => service!.exitCode !== null, "drained test shutdown")
    expect(service.exitCode).toBe(0)
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    await expect(chunkedPost(managed.endpoint, managed.credential)).rejects.toBeDefined()

    const receiptDatabase = new Database(join(muxviaHome, "state/muxvia.db"), { readonly: true })
    const receipts = receiptDatabase
      .query("SELECT outcome_json FROM action_receipts ORDER BY committed_revision")
      .all()
    receiptDatabase.close()
    scanRawRpcFramesNoSecrets(rpcFrames)
    scanNoSecrets([formFrame, savedFrame, activeFrame, servedFrame, logs, views, receipts])
    expect(await controlledTreeFingerprint(operatorHomeCanary)).toBe(operatorHomeCanaryBefore)
  } finally {
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    await client?.close().catch(() => {})
    await writeFile(shutdownFile, "shutdown\n").catch(() => {})
    if (service && service.exitCode === null) {
      await Promise.race([
        new Promise<void>((resolveExit) => service!.once("exit", () => resolveExit())),
        Bun.sleep(deadlineMs).then(() => { service!.kill("SIGKILL") }),
      ])
    }
    await upstream.stop()
  }
})
