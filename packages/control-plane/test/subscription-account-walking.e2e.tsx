/** @jsxImportSource @opentui/solid */
import { Database } from "bun:sqlite"
import { afterEach, expect, test } from "bun:test"
import { TestRecorder } from "@opentui/core/testing"
import { testRender } from "@opentui/solid"
import { spawn } from "node:child_process"
import { createServer, type IncomingMessage, type ServerResponse } from "node:http"
import { createConnection } from "node:net"
import { chmod, mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"
import { finished } from "node:stream/promises"

import { RpcClient } from "../src/control/rpc-client"
import type { SubscriptionAccountSession, SubscriptionPlatformEffects } from "../src/control/subscription-account-session"
import type { TargetSession } from "../src/control/target-session"
import type { SubscriptionAccountCatalogView } from "../src/control/types"
import { App } from "../src/ui/app"
import { auditSecretFreeDiagnostic, waitForSecretFreeFrame } from "./secret-audit"

const repoRoot = resolve(import.meta.dir, "../../..")
const serviceBinary = resolve(repoRoot, "target/debug/muxvia-routing")
const fakeCodex = resolve(repoRoot, "tests/e2e/fixtures/fake-codex")
const fakeClaude = resolve(repoRoot, "tests/e2e/fixtures/fake-claude")
const deadlineMs = 10_000
const roots: string[] = []

const privateSecrets = [
  "REMOTE_DEVICE_ACCOUNT_A_12101",
  "REMOTE_DEVICE_ACCOUNT_B_12102",
  "REMOTE_DEVICE_REAUTHORIZE_B_12103",
  "REMOTE_DEVICE_EXPIRED_12104",
  "REMOTE_DEVICE_CANCELLED_12105",
  "AUTHORIZATION_CODE_A_12106",
  "AUTHORIZATION_CODE_B_12107",
  "AUTHORIZATION_CODE_REAUTH_B_12108",
  "SERVER_VERIFIER_A_12109",
  "SERVER_VERIFIER_B_12110",
  "SERVER_VERIFIER_REAUTH_B_12111",
  "ACCESS_TOKEN_A_12112",
  "ACCESS_TOKEN_B_12113",
  "ACCESS_TOKEN_REAUTH_B_12114",
  "REFRESH_TOKEN_A_12115",
  "REFRESH_TOKEN_B_12116",
  "REFRESH_TOKEN_REAUTH_B_12117",
] as const
const providerSecrets = ["PROVIDER_ACCOUNT_CODEX_12118", "PROVIDER_ACCOUNT_CLAUDE_12119"] as const

afterEach(async () => {
  for (const root of roots.splice(0)) await rm(root, { recursive: true, force: true })
})

async function waitFor(predicate: () => boolean | Promise<boolean>, label: string): Promise<void> {
  const deadline = Date.now() + deadlineMs
  while (!(await predicate())) {
    if (Date.now() >= deadline) throw new Error(`subscription-wait-failed:${label}`)
    await Bun.sleep(10)
  }
}

async function waitForCatalog(
  session: SubscriptionAccountSession,
  predicate: (view: Readonly<SubscriptionAccountCatalogView>) => boolean,
  label: string,
): Promise<void> {
  if (predicate(session.get())) return
  await new Promise<void>((resolveWait, reject) => {
    let unsubscribe = () => {}
    const timeout = setTimeout(() => {
      unsubscribe()
      reject(new Error(`subscription-catalog-wait-failed:${label}`))
    }, deadlineMs)
    const finish = () => {
      clearTimeout(timeout)
      unsubscribe()
      resolveWait()
    }
    unsubscribe = session.subscribe((view) => { if (predicate(view)) finish() })
    if (predicate(session.get())) finish()
  })
}

function assertSecretFree(value: unknown, secrets: readonly string[], label: string): void {
  try {
    auditSecretFreeDiagnostic(value, secrets, label)
    const rendered = typeof value === "string" ? value : JSON.stringify(value)
    for (const secret of secrets) {
      const numeric = [...Buffer.from(secret)].join(",")
      if (rendered.includes(secret) || rendered.includes(numeric)) {
        throw new Error("matched")
      }
    }
  } catch {
    throw new Error(`subscription-secret-scan-failed:${label}`)
  }
}

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`
  if (value && typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>)
      .sort(([left], [right]) => left.localeCompare(right))
    return `{${entries.map(([key, entry]) => `${JSON.stringify(key)}:${canonicalJson(entry)}`).join(",")}}`
  }
  return JSON.stringify(value)
}

test("Subscription Account tracer scans literal and numeric-byte private surfaces", () => {
  const secret = "SUBSCRIPTION_TRACER_CONTROLLED_SECRET_12120"
  const customError = Object.assign(new Error("safe"), { stack: `at ${secret}`, custom: secret })
  for (const surface of [
    secret,
    Buffer.from(secret),
    { nested: secret },
    customError,
    new AggregateError([new Error(secret)], "safe"),
    [...Buffer.from(secret)],
  ]) {
    let diagnostic = ""
    try { assertSecretFree(surface, [secret], "controlled-mutation") }
    catch (error) { diagnostic = error instanceof Error ? error.message : "" }
    expect(diagnostic).toBe("subscription-secret-scan-failed:controlled-mutation")
    expect(diagnostic).not.toContain(secret)
  }
})

function jwt(accountId: string, email: string): string {
  return `e30.${Buffer.from(JSON.stringify({ chatgpt_account_id: accountId, email })).toString("base64url")}.signature`
}

function readBody(request: IncomingMessage): Promise<string> {
  return new Promise((resolveBody, reject) => {
    const chunks: Buffer[] = []
    request.on("data", (chunk) => chunks.push(Buffer.from(chunk)))
    request.once("end", () => resolveBody(Buffer.concat(chunks).toString("utf8")))
    request.once("error", reject)
  })
}

function jsonResponse(response: ServerResponse, status: number, value: unknown): void {
  const body = JSON.stringify(value)
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": Buffer.byteLength(body),
    connection: "close",
  })
  response.end(body)
}

class DeviceAuthorityFixture {
  readonly requests: Array<{ path: string; body: string }> = []
  readonly pollCounts = new Map<string, number>()
  readonly cancelledPollStarted: Promise<void>
  #resolveCancelledPollStarted!: () => void
  #releaseCancelledPoll!: () => void
  #cancelledPollRelease: Promise<void>
  #starts = 0
  #server = createServer((request, response) => {
    void this.#handle(request, response).catch(() => {
      if (!response.headersSent) jsonResponse(response, 500, {})
      else response.destroy()
    })
  })

  constructor() {
    this.cancelledPollStarted = new Promise((resolveStarted) => { this.#resolveCancelledPollStarted = resolveStarted })
    this.#cancelledPollRelease = new Promise((resolveRelease) => { this.#releaseCancelledPoll = resolveRelease })
  }

  async start(): Promise<string> {
    await new Promise<void>((resolveListen, reject) => {
      this.#server.once("error", reject)
      this.#server.listen(0, "127.0.0.1", resolveListen)
    })
    const address = this.#server.address()
    if (!address || typeof address === "string") throw new Error("subscription-authority-address-invalid")
    return `http://127.0.0.1:${address.port}`
  }

  releaseCancelledPoll(): void { this.#releaseCancelledPoll() }

  async close(): Promise<void> {
    this.#releaseCancelledPoll()
    await new Promise<void>((resolveClose) => this.#server.close(() => resolveClose()))
  }

  async #handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const body = await readBody(request)
    const path = request.url ?? ""
    this.requests.push({ path, body })
    if (path === "/api/accounts/deviceauth/usercode") {
      this.#starts++
      const challenges = [
        [privateSecrets[0], "AAAA-BBBB"],
        [privateSecrets[1], "CCCC-DDDD"],
        [privateSecrets[2], "EEEE-FFFF"],
        [privateSecrets[3], "GGGG-HHHH"],
        [privateSecrets[4], "IIII-JJJJ"],
      ] as const
      const challenge = challenges[this.#starts - 1]
      if (!challenge) return jsonResponse(response, 500, {})
      return jsonResponse(response, 200, {
        device_auth_id: challenge[0],
        user_code: challenge[1],
        interval: 5,
        expires_in: 900,
      })
    }
    if (path === "/api/accounts/deviceauth/token") {
      const parsed = JSON.parse(body) as { device_auth_id?: string }
      const device = parsed.device_auth_id ?? ""
      const count = (this.pollCounts.get(device) ?? 0) + 1
      this.pollCounts.set(device, count)
      if (device === privateSecrets[3]) return jsonResponse(response, 410, {})
      if (device === privateSecrets[4]) {
        if (count === 1) {
          this.#resolveCancelledPollStarted()
          await this.#cancelledPollRelease
          return jsonResponse(response, 403, {})
        }
        return jsonResponse(response, 410, {})
      }
      if (count === 1) return jsonResponse(response, 403, {})
      const authorized = new Map<string, readonly [string, string]>([
        [privateSecrets[0], [privateSecrets[5], privateSecrets[8]]],
        [privateSecrets[1], [privateSecrets[6], privateSecrets[9]]],
        [privateSecrets[2], [privateSecrets[7], privateSecrets[10]]],
      ] as const).get(device)
      if (!authorized) return jsonResponse(response, 500, {})
      return jsonResponse(response, 200, {
        authorization_code: authorized[0],
        code_verifier: authorized[1],
      })
    }
    if (path === "/oauth/token") {
      const form = new URLSearchParams(body)
      if (form.get("grant_type") === "refresh_token") {
        if (form.get("refresh_token") === privateSecrets[15]) return jsonResponse(response, 401, {})
        if (form.get("refresh_token") === privateSecrets[14]) {
          return jsonResponse(response, 200, {
            access_token: privateSecrets[11],
            refresh_token: privateSecrets[14],
            id_token: jwt("account-a", "a@example.test"),
            expires_in: 3600,
          })
        }
        return jsonResponse(response, 500, {})
      }
      const tokens = new Map<string, readonly [string, string, string, string]>([
        [privateSecrets[5], ["account-a", "a@example.test", privateSecrets[11], privateSecrets[14]]],
        [privateSecrets[6], ["account-b", "b@example.test", privateSecrets[12], privateSecrets[15]]],
        [privateSecrets[7], ["account-b", "b@example.test", privateSecrets[13], privateSecrets[16]]],
      ] as const).get(form.get("code") ?? "")
      if (!tokens) return jsonResponse(response, 500, {})
      return jsonResponse(response, 200, {
        access_token: tokens[2],
        refresh_token: tokens[3],
        id_token: jwt(tokens[0], tokens[1]),
        expires_in: 3600,
      })
    }
    jsonResponse(response, 404, {})
  }
}

type BridgeUpstreamRequest = { path: string; headers: Record<string, string | string[] | undefined>; body: string }

class SubscriptionBridgeUpstreamFixture {
  readonly requests: BridgeUpstreamRequest[] = []
  #holdNext = false
  #resumeHeld?: () => void
  #finishHeld?: () => void
  #server = createServer((request, response) => {
    void this.#handle(request, response).catch(() => response.destroy())
  })

  async start(): Promise<string> {
    await new Promise<void>((resolveListen, reject) => {
      this.#server.once("error", reject)
      this.#server.listen(0, "127.0.0.1", resolveListen)
    })
    const address = this.#server.address()
    if (!address || typeof address === "string") throw new Error("subscription-bridge-upstream-address-invalid")
    return `http://127.0.0.1:${address.port}`
  }

  holdNextResponse(): void {
    this.#holdNext = true
  }

  resumeHeldResponse(): void {
    const resume = this.#resumeHeld
    this.#resumeHeld = undefined
    if (!resume) throw new Error("subscription-bridge-held-response-missing")
    resume()
  }

  finishHeldResponse(): void {
    const finish = this.#finishHeld
    this.#finishHeld = undefined
    if (!finish) throw new Error("subscription-bridge-held-response-missing")
    finish()
  }

  async close(): Promise<void> {
    await new Promise<void>((resolveClose) => this.#server.close(() => resolveClose()))
  }

  async #handle(request: IncomingMessage, response: ServerResponse): Promise<void> {
    const body = await readBody(request)
    const path = request.url ?? ""
    this.requests.push({ path, headers: request.headers, body })
    if (path === "/backend-api/codex/responses") {
      response.writeHead(200, { "content-type": "text/event-stream" })
      const stream = await readFile(resolve(repoRoot, "crates/routing-service/tests/fixtures/subscription-bridge/responses-stream.input.sse"))
      if (this.#holdNext) {
        this.#holdNext = false
        const split = Math.min(stream.length, 420)
        response.write(stream.subarray(0, split))
        this.#resumeHeld = () => { response.write(stream.subarray(split)) }
        this.#finishHeld = () => { response.end() }
        return
      }
      for (let offset = 0; offset < stream.length; offset += 37) {
        response.write(stream.subarray(offset, offset + 37))
      }
      response.end()
      return
    }
    if (path === "/messages") {
      response.writeHead(200, { "content-type": "text/event-stream" })
      response.end([
        "event: message_start",
        'data: {"type":"message_start","message":{"id":"native-fallback","type":"message","role":"assistant","model":"native","usage":{"input_tokens":1,"output_tokens":0}}}',
        "",
        "event: content_block_start",
        'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}',
        "",
        "event: content_block_delta",
        'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"native fallback"}}',
        "",
        "event: content_block_stop",
        'data: {"type":"content_block_stop","index":0}',
        "",
        "event: message_delta",
        'data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":2}}',
        "",
        "event: message_stop",
        'data: {"type":"message_stop"}',
        "",
      ].join("\n"))
      return
    }
    jsonResponse(response, 404, {})
  }
}

function captureProcess(child: ReturnType<typeof spawn>) {
  const stdout: Buffer[] = []
  const stderr: Buffer[] = []
  child.stdout?.on("data", (chunk) => stdout.push(Buffer.from(chunk)))
  child.stderr?.on("data", (chunk) => stderr.push(Buffer.from(chunk)))
  const completed = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolveExit, reject) => {
    child.once("error", reject)
    child.once("close", (code, signal) => resolveExit({ code, signal }))
  }).then(async (result) => {
    await Promise.all([
      child.stdout ? finished(child.stdout).catch(() => undefined) : undefined,
      child.stderr ? finished(child.stderr).catch(() => undefined) : undefined,
    ])
    return result
  })
  return { stdout, stderr, completed }
}

test("real processes prove Subscription Account authorization and persistence", async () => {
  const root = await mkdtemp(join(tmpdir(), "mx-sa-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const accountPath = join(muxviaHome, "state/subscription-accounts.json")
  const firstShutdown = join(root, "shutdown-first")
  const secondShutdown = join(root, "shutdown-second")
  const authority = new DeviceAuthorityFixture()
  const authorityOrigin = await authority.start()
  await chmod(fakeCodex, 0o755)
  await chmod(fakeClaude, 0o755)
  await mkdir(userHome, { recursive: true, mode: 0o700 })

  const services: Array<{ child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcess> }> = []
  const rpcChunks: Buffer[] = []
  const renderedFrames: string[] = []
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  let accountClient: RpcClient | undefined
  let codexSession: TargetSession | undefined
  let claudeSession: TargetSession | undefined
  let accountSession: SubscriptionAccountSession | undefined
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let recorder: TestRecorder | undefined

  const startService = async (label: string, options?: { shutdown?: string; refresh?: string }) => {
    const args = [
      "--home", muxviaHome,
      "--test-codex-executable", fakeCodex,
      "--test-claude-executable", fakeClaude,
      "--test-device-authority-origin", authorityOrigin,
    ]
    if (options?.shutdown) args.push("--test-shutdown-file", options.shutdown)
    if (options?.refresh) args.push("--test-refresh-subscription-account", options.refresh)
    const child = spawn(serviceBinary, args, {
      cwd: root,
      env: { ...process.env, HOME: userHome, MUXVIA_INTEGRATION_TEST: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    })
    const output = captureProcess(child)
    services.push({ child, output })
    await waitFor(async () => {
      if (child.exitCode !== null) throw new Error(`subscription-service-exited:${label}`)
      try { return (await stat(socketPath)).isSocket() } catch { return false }
    }, `${label}-service-ready`)
    return { child, output }
  }
  const connectClient = async (release: string) => await RpcClient.connect(
    socketPath,
    release,
    undefined,
    (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => rpcChunks.push(Buffer.from(chunk)))
      return socket
    },
  )
  const openSessions = async (label: string) => {
    codexClient = await connectClient(`${label}-codex`)
    claudeClient = await connectClient(`${label}-claude`)
    accountClient = await connectClient(`${label}-accounts`)
    codexSession = await codexClient.openTarget("codex")
    claudeSession = await claudeClient.openTarget("claude", {
      claudeConfigDir: null,
      selectorState: "unset",
      blockingSelector: null,
      hostManagedState: "unmanaged",
      cwd: root,
    })
    accountSession = await accountClient.openSubscriptionAccounts()
  }
  const closeSessions = async () => {
    const sessions = [codexSession, claudeSession, accountSession]
    const clients = [codexClient, claudeClient, accountClient]
    codexSession = undefined
    claudeSession = undefined
    accountSession = undefined
    codexClient = undefined
    claudeClient = undefined
    accountClient = undefined
    await Promise.all(sessions.map((session) => session?.close().catch(() => undefined)))
    await Promise.all(clients.map((client) => client?.close().catch(() => undefined)))
  }
  const closeRenderer = () => {
    recorder?.stop()
    renderedFrames.push(...(recorder?.recordedFrames.map(({ frame }) => frame) ?? []))
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    setup = undefined
    recorder = undefined
  }
  const renderAccounts = async (
    effects: SubscriptionPlatformEffects,
    target: "codex" | "claude" = "codex",
  ) => {
    setup = await testRender(() => <App
      sessions={{ codex: codexSession!, claude: claudeSession! }}
      subscriptionAccountSession={accountSession!}
      subscriptionEffects={effects}
    />, { width: 100, height: 30, useThread: false, kittyKeyboard: true })
    recorder = new TestRecorder(setup.renderer)
    recorder.rec()
    await setup.renderOnce()
    setup.mockInput.pressKey(target === "codex" ? "1" : "2")
    await setup.renderOnce()
    await setup.mockInput.typeText("/accounts")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Subscription Accounts"))
  }
  const stopService = async (
    service: Awaited<ReturnType<typeof startService>>,
    shutdown?: string,
  ) => {
    closeRenderer()
    if (shutdown) await writeFile(shutdown, "shutdown\n")
    await closeSessions()
    const exit = await Promise.race([
      service.output.completed,
      Bun.sleep(deadlineMs).then(() => undefined),
    ])
    if (!exit || exit.code !== 0 || exit.signal !== null) throw new Error("subscription-service-exit-invalid")
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
  }

  const copied: string[] = []
  const opened: string[] = []
  const effects: SubscriptionPlatformEffects = {
    copyUserCode: async (code) => { copied.push(code); return true },
    openVerificationUrl: async (url) => { opened.push(url); return false },
    wait: async (_milliseconds, signal) => {
      if (signal.aborted) throw Object.assign(new Error("cancelled"), { code: "cancelled" })
      await Promise.resolve()
    },
  }

  try {
    const first = await startService("first", { shutdown: firstShutdown })
    await openSessions("first")
    const codexCreated = await codexSession!.act({
      kind: "create-provider",
      name: "Codex Subscription",
      baseUrl: "https://subscription.invalid/v1",
      model: "subscription-codex",
      credential: { kind: "replace", value: providerSecrets[0] },
      authentication: "openai-bearer",
      presetKey: null,
    })
    const claudeCreated = await claudeSession!.act({
      kind: "create-provider",
      name: "Claude Subscription",
      baseUrl: "https://subscription.invalid",
      model: "subscription-claude",
      credential: { kind: "replace", value: providerSecrets[1] },
      authentication: "anthropic-bearer",
      presetKey: null,
    })
    const codexProvider = codexCreated.view.providers[0]!
    const claudeProvider = claudeCreated.view.providers[0]!
    await renderAccounts(effects)

    setup!.mockInput.pressKey("a")
    await setup!.waitForFrame((frame) => frame.includes("AAAA-BBBB"))
    await waitForCatalog(accountSession!, (view) => view.accounts.length === 1, "first account")
    await setup!.waitForFrame((frame) => frame.includes("Authorization complete"))
    setup!.mockInput.pressKey("a")
    await setup!.waitForFrame((frame) => frame.includes("CCCC-DDDD"))
    await waitForCatalog(accountSession!, (view) => view.accounts.length === 2, "second account")
    await setup!.waitForFrame((frame) => frame.includes("b@example.test"))
    if (copied.slice(0, 2).join(",") !== "AAAA-BBBB,CCCC-DDDD"
      || opened.slice(0, 2).some((url) => url !== "https://auth.openai.com/codex/device")) {
      throw new Error("subscription-copy-open-attempt-mismatch")
    }

    closeRenderer()
    await accountSession!.act({
      kind: "bind-provider-fixed",
      target: "codex",
      providerId: codexProvider.id,
      providerRevision: codexProvider.providerRevision,
      accountId: "account-a",
    })
    await accountSession!.act({
      kind: "bind-provider-follow-default",
      target: "claude",
      providerId: claudeProvider.id,
      providerRevision: claudeProvider.providerRevision,
    })
    const defaultPreview = await accountSession!.previewDefault("account-b")
    await accountSession!.act({
      kind: "set-default-account",
      accountId: "account-b",
      previewToken: defaultPreview.previewToken,
    })
    const beforeRestart = structuredClone(accountSession!.get())

    await stopService(first, firstShutdown)
    const accountBytes = await readFile(accountPath)
    const accountMetadata = await stat(accountPath)
    const accountText = accountBytes.toString("utf8")
    if ((accountMetadata.mode & 0o777) !== 0o600
      || !accountText.includes(privateSecrets[14])
      || !accountText.includes(privateSecrets[15])
      || accountText.includes(privateSecrets[11])
      || accountText.includes(privateSecrets[12])) {
      throw new Error("subscription-private-file-contract-mismatch")
    }
    assertSecretFree((await readFile(databasePath)).toString("latin1"), privateSecrets, "sqlite-private-material")

    const second = await startService("restart", { shutdown: secondShutdown })
    await openSessions("restart")
    if (JSON.stringify(accountSession!.get()) !== JSON.stringify(beforeRestart)) {
      throw new Error("subscription-restart-catalog-mismatch")
    }
    await accountSession!.act({ kind: "delete-account", accountId: "account-a" })
    const afterDelete = accountSession!.get()
    const fixed = afterDelete.bindings.find((binding) => binding.providerId === codexProvider.id)
    const followed = afterDelete.bindings.find((binding) => binding.providerId === claudeProvider.id)
    if (fixed?.binding.kind !== "fixed" || fixed.binding.accountId !== "account-a"
      || fixed.resolution.state !== "missing"
      || followed?.binding.kind !== "follow-default"
      || followed.resolution.accountId !== "account-b") {
      throw new Error("subscription-dangling-binding-contract-mismatch")
    }
    await stopService(second, secondShutdown)

    const third = await startService("refresh-rejection", { refresh: "account-b" })
    await openSessions("refresh-rejection")
    if (accountSession!.get().accounts[0]?.state !== "needs-reauthorization") {
      throw new Error("subscription-permanent-refresh-rejection-not-durable")
    }
    const bindingMetadataBeforeReauthorization = accountSession!.get().bindings.map(({ resolution: _, ...binding }) => binding)
    await renderAccounts(effects)
    setup!.mockInput.pressKey("r")
    await setup!.waitForFrame((frame) => frame.includes("EEEE-FFFF"))
    await waitForCatalog(accountSession!, (view) => view.accounts[0]?.state === "authorized", "same identity reauthorization")
    const bindingMetadataAfterReauthorization = accountSession!.get().bindings.map(({ resolution: _, ...binding }) => binding)
    if (JSON.stringify(bindingMetadataAfterReauthorization) !== JSON.stringify(bindingMetadataBeforeReauthorization)) {
      throw new Error("subscription-reauthorization-changed-bindings")
    }

    const expiredChallenge = await accountSession!.startDeviceAuthorization()
    if ((await accountSession!.pollDeviceAuthorization(expiredChallenge.flowId)).status !== "expired") {
      throw new Error("subscription-expired-flow-misclassified")
    }
    const cancelledChallenge = await accountSession!.startDeviceAuthorization()
    const cancellation = new AbortController()
    const cancelledPoll = accountSession!.pollDeviceAuthorization(cancelledChallenge.flowId, cancellation.signal)
    await authority.cancelledPollStarted
    cancellation.abort()
    await expect(cancelledPoll).rejects.toMatchObject({ code: "cancelled" })
    authority.releaseCancelledPoll()
    await waitFor(() => authority.pollCounts.get(privateSecrets[4]) === 1, "cancelled remote poll completion")
    if ((await accountSession!.pollDeviceAuthorization(cancelledChallenge.flowId)).status !== "expired") {
      throw new Error("subscription-cancelled-flow-was-revoked")
    }

    closeRenderer()
    await closeSessions()
    const finalExit = await Promise.race([
      third.output.completed,
      Bun.sleep(deadlineMs).then(() => undefined),
    ])
    if (!finalExit || finalExit.code !== 0 || finalExit.signal !== null) {
      throw new Error("subscription-natural-exit-failed")
    }
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })

    const finalDocument = JSON.parse((await readFile(accountPath)).toString("utf8")) as {
      accounts: Record<string, { state: string; refresh_token: string }>
      default_account_id?: string
    }
    if (Object.keys(finalDocument.accounts).join(",") !== "account-b"
      || finalDocument.accounts["account-b"]?.state !== "authorized"
      || finalDocument.accounts["account-b"]?.refresh_token !== privateSecrets[16]
      || finalDocument.default_account_id !== "account-b") {
      throw new Error("subscription-final-private-state-mismatch")
    }
    const database = new Database(databasePath, { readonly: true })
    const bindings = database.query("SELECT target, provider_id, binding_kind, account_id FROM subscription_provider_bindings ORDER BY target").all()
    const receipts = database.query("SELECT action_json, outcome_json FROM subscription_account_action_receipts ORDER BY committed_revision").all()
    database.close()
    if (bindings.length !== 2 || receipts.length < 4) throw new Error("subscription-durable-history-incomplete")

    assertSecretFree(Buffer.concat(rpcChunks).toString("latin1"), privateSecrets, "raw-rpc")
    assertSecretFree(renderedFrames, [...privateSecrets, ...providerSecrets], "renderer")
    for (const service of services) {
      assertSecretFree(
        Buffer.concat([...service.output.stdout, ...service.output.stderr]).toString("latin1"),
        [...privateSecrets, ...providerSecrets],
        "process-output",
      )
    }
    assertSecretFree(receipts, [...privateSecrets, ...providerSecrets], "sqlite-receipts")
    if (!authority.requests.some(({ body }) => body.includes(`refresh_token=${privateSecrets[15]}`))) {
      throw new Error("subscription-refresh-rejection-request-missing")
    }
  } finally {
    closeRenderer()
    authority.releaseCancelledPoll()
    await closeSessions()
    for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
    await Promise.all(services.map(({ output }) => Promise.race([output.completed.catch(() => undefined), Bun.sleep(deadlineMs)])))
    assertSecretFree(Buffer.concat(rpcChunks).toString("latin1"), privateSecrets, "final-raw-rpc")
    assertSecretFree(renderedFrames, [...privateSecrets, ...providerSecrets], "final-renderer")
    for (const service of services) {
      assertSecretFree(
        Buffer.concat([...service.output.stdout, ...service.output.stderr]).toString("latin1"),
        [...privateSecrets, ...providerSecrets],
        "final-process-output",
      )
    }
    try {
      assertSecretFree(
        (await readFile(databasePath)).toString("latin1"),
        privateSecrets,
        "final-sqlite-private-material",
      )
    } catch (error) {
      if (!(error && typeof error === "object" && "code" in error && error.code === "ENOENT")) throw error
    }
    await authority.close()
  }
}, 120_000)

test("real processes prove the Claude Codex Subscription Bridge end to end", async () => {
  const root = await mkdtemp(join(tmpdir(), "mx-sb-"))
  roots.push(root)
  const userHome = join(root, "home")
  const muxviaHome = join(userHome, ".muxvia")
  const socketPath = join(muxviaHome, "run/control.sock")
  const databasePath = join(muxviaHome, "state/muxvia.db")
  const accountPath = join(muxviaHome, "state/subscription-accounts.json")
  const settingsPath = join(userHome, ".claude/settings.json")
  const firstShutdown = join(root, "shutdown-first")
  const secondShutdown = join(root, "shutdown-second")
  const authority = new DeviceAuthorityFixture()
  const bridgeUpstream = new SubscriptionBridgeUpstreamFixture()
  const authorityOrigin = await authority.start()
  const upstreamOrigin = await bridgeUpstream.start()
  await chmod(fakeCodex, 0o755)
  await chmod(fakeClaude, 0o755)
  await mkdir(userHome, { recursive: true, mode: 0o700 })
  await mkdir(join(userHome, ".claude"), { recursive: true, mode: 0o700 })
  const originalSettings = Buffer.from(JSON.stringify({
    bridgeUnrelated: "BRIDGE_UNRELATED_SETTINGS_SECRET_12204",
  }, null, 2))
  await writeFile(settingsPath, originalSettings, { mode: 0o640 })
  await chmod(settingsPath, 0o640)

  const bridgeSecrets = [
    ...privateSecrets,
    "NATIVE_BRIDGE_FALLBACK_SECRET_12201",
    "FIXTURE_REQUEST_CONTENT_6521",
    "BRIDGE_UNRELATED_SETTINGS_SECRET_12204",
  ] as const
  const services: Array<{ child: ReturnType<typeof spawn>; output: ReturnType<typeof captureProcess> }> = []
  const rpcChunks: Buffer[] = []
  const renderedFrames: string[] = []
  const routingSecrets: string[] = []
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  let accountClient: RpcClient | undefined
  let codexSession: TargetSession | undefined
  let claudeSession: TargetSession | undefined
  let accountSession: SubscriptionAccountSession | undefined
  let setup: Awaited<ReturnType<typeof testRender>> | undefined
  let recorder: TestRecorder | undefined

  const startService = async (label: string, options?: { shutdown?: string; refresh?: string }) => {
    const args = [
      "--home", muxviaHome,
      "--test-codex-executable", fakeCodex,
      "--test-claude-executable", fakeClaude,
      "--test-device-authority-origin", authorityOrigin,
      "--test-subscription-bridge-origin", upstreamOrigin,
    ]
    if (options?.shutdown) args.push("--test-shutdown-file", options.shutdown)
    if (options?.refresh) args.push("--test-refresh-subscription-account", options.refresh)
    const child = spawn(serviceBinary, args, {
      cwd: root,
      env: { ...process.env, HOME: userHome, MUXVIA_INTEGRATION_TEST: "1" },
      stdio: ["ignore", "pipe", "pipe"],
    })
    const output = captureProcess(child)
    services.push({ child, output })
    await waitFor(async () => {
      if (child.exitCode !== null) throw new Error(`subscription-bridge-service-exited:${label}`)
      try { return (await stat(socketPath)).isSocket() } catch { return false }
    }, `${label}-service-ready`)
    return { child, output }
  }
  const connectClient = async (release: string) => await RpcClient.connect(
    socketPath,
    release,
    undefined,
    (path) => {
      const socket = createConnection({ path })
      socket.on("data", (chunk) => {
        assertSecretFree(chunk, [...bridgeSecrets, ...routingSecrets], "bridge-live-raw-rpc")
        rpcChunks.push(Buffer.from(chunk))
      })
      return socket
    },
  )
  const openSessions = async (label: string) => {
    codexClient = await connectClient(`${label}-codex`)
    claudeClient = await connectClient(`${label}-claude`)
    accountClient = await connectClient(`${label}-accounts`)
    codexSession = await codexClient.openTarget("codex")
    claudeSession = await claudeClient.openTarget("claude", {
      claudeConfigDir: null,
      selectorState: "unset",
      blockingSelector: null,
      hostManagedState: "unmanaged",
      cwd: root,
    })
    accountSession = await accountClient.openSubscriptionAccounts()
  }
  const closeSessions = async () => {
    const sessions = [codexSession, claudeSession, accountSession]
    const clients = [codexClient, claudeClient, accountClient]
    codexSession = undefined
    claudeSession = undefined
    accountSession = undefined
    codexClient = undefined
    claudeClient = undefined
    accountClient = undefined
    await Promise.all(sessions.map((session) => session?.close().catch(() => undefined)))
    await Promise.all(clients.map((client) => client?.close().catch(() => undefined)))
  }
  const closeRenderer = () => {
    recorder?.stop()
    renderedFrames.push(...(recorder?.recordedFrames.map(({ frame }) => frame) ?? []))
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    setup = undefined
    recorder = undefined
  }
  const stopService = async (service: Awaited<ReturnType<typeof startService>>, shutdown: string) => {
    closeRenderer()
    await writeFile(shutdown, "shutdown\n")
    await closeSessions()
    const exit = await Promise.race([service.output.completed, Bun.sleep(deadlineMs).then(() => undefined)])
    if (!exit || exit.code !== 0 || exit.signal !== null) throw new Error("subscription-bridge-service-exit-invalid")
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
  }
  const renderWorkflow = async () => {
    const effects: SubscriptionPlatformEffects = {
      copyUserCode: async () => true,
      openVerificationUrl: async () => true,
      wait: async (_milliseconds, signal) => {
        if (signal.aborted) throw Object.assign(new Error("cancelled"), { code: "cancelled" })
        await Promise.resolve()
      },
    }
    setup = await testRender(() => <App
      sessions={{ codex: codexSession!, claude: claudeSession! }}
      subscriptionAccountSession={accountSession!}
      subscriptionEffects={effects}
    />, { width: 110, height: 38, useThread: false, kittyKeyboard: true })
    recorder = new TestRecorder(setup.renderer)
    recorder.rec()
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.renderOnce()
  }
  const routingAccess = async () => {
    const settings = JSON.parse((await readFile(settingsPath)).toString("utf8")) as {
      env?: { ANTHROPIC_AUTH_TOKEN?: string }
    }
    const endpoint = claudeSession!.get().takeover.endpoint
    const token = settings.env?.ANTHROPIC_AUTH_TOKEN
    if (!endpoint || !token) throw new Error("subscription-bridge-route-access-missing")
    if (!routingSecrets.includes(token)) routingSecrets.push(token)
    return { endpoint, token }
  }
  const sendFixture = async (fixture: "messages-text.input.json" | "messages-tools.input.json") => {
    const { endpoint, token } = await routingAccess()
    const body = await readFile(resolve(repoRoot, `crates/routing-service/tests/fixtures/subscription-bridge/${fixture}`))
    return await fetch(`${endpoint}/v1/messages`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${token}`,
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
        "x-session-id": "bridge-session-safe",
      },
      body,
    })
  }
  const assertBridgeRequest = async (index: number, accountId: string, accessToken: string, expectedFixture: string) => {
    const request = bridgeUpstream.requests[index]
    if (!request) throw new Error("subscription-bridge-upstream-request-missing")
    const expected = JSON.parse((await readFile(resolve(
      repoRoot,
      `crates/routing-service/tests/fixtures/subscription-bridge/${expectedFixture}`,
    ))).toString("utf8")) as Record<string, unknown>
    expected.model = "gpt-5.6"
    const actualBody = JSON.parse(request.body) as Record<string, unknown>
    const expectedSession = expectedFixture.startsWith("messages-text") ? "metadata-session" : "session-from-user"
    const safeHeaders = { ...request.headers }
    if (safeHeaders.authorization === `Bearer ${accessToken}`) safeHeaders.authorization = "<approved-access-token>"
    const safeBody = canonicalJson(actualBody) === canonicalJson(expected) ? "<approved-request-body>" : request.body
    assertSecretFree(
      { path: request.path, headers: safeHeaders, body: safeBody },
      [...bridgeSecrets, ...routingSecrets],
      `bridge-upstream-${index}`,
    )
    const checks = {
      path: request.path === "/backend-api/codex/responses",
      authorization: request.headers.authorization === `Bearer ${accessToken}`,
      account: request.headers["chatgpt-account-id"] === accountId,
      originator: request.headers.originator === "codex_cli_rs",
      version: request.headers.version === "0.144.1",
      contentType: request.headers["content-type"] === "application/json",
      session: request.headers.session_id === expectedSession,
      requestId: request.headers["x-client-request-id"] === expectedSession,
      window: request.headers["x-codex-window-id"] === `${expectedSession}:0`,
      userAgent: request.headers["user-agent"] === undefined,
      body: canonicalJson(actualBody) === canonicalJson(expected),
    }
    const failed = Object.entries(checks).filter(([, matched]) => !matched).map(([name]) => name)
    if (failed.length > 0) throw new Error(`subscription-bridge-upstream-contract-mismatch:${failed.join(",")}`)
  }
  const assertNativeRequest = async (index: number, expectedFixture: string) => {
    const request = bridgeUpstream.requests[index]
    if (!request) throw new Error("subscription-bridge-native-request-missing")
    const expected = JSON.parse((await readFile(resolve(
      repoRoot,
      `crates/routing-service/tests/fixtures/subscription-bridge/${expectedFixture}`,
    ))).toString("utf8")) as Record<string, unknown>
    expected.model = "claude-native"
    const actualBody = JSON.parse(request.body) as Record<string, unknown>
    const safeHeaders = { ...request.headers }
    if (safeHeaders["x-api-key"] === "NATIVE_BRIDGE_FALLBACK_SECRET_12201") {
      safeHeaders["x-api-key"] = "<approved-provider-credential>"
    }
    const safeBody = canonicalJson(actualBody) === canonicalJson(expected) ? "<approved-request-body>" : request.body
    assertSecretFree(
      { path: request.path, headers: safeHeaders, body: safeBody },
      [...bridgeSecrets, ...routingSecrets],
      `bridge-native-upstream-${index}`,
    )
    if (request.path !== "/messages"
      || request.headers["x-api-key"] !== "NATIVE_BRIDGE_FALLBACK_SECRET_12201"
      || canonicalJson(actualBody) !== canonicalJson(expected)) {
      throw new Error("subscription-bridge-native-upstream-contract-mismatch")
    }
  }
  const assertConverted = async (response: Response) => {
    const bytes = Buffer.from(await response.arrayBuffer())
    const expected = await readFile(resolve(
      repoRoot,
      "crates/routing-service/tests/fixtures/subscription-bridge/anthropic-stream.expected.sse",
    ))
    const events = (contents: Buffer) => contents.toString("utf8").trim().split("\n\n").map((block) => {
      const lines = block.split("\n")
      const event = lines.find((line) => line.startsWith("event: "))?.slice(7)
      const data = lines.find((line) => line.startsWith("data: "))?.slice(6)
      return event && data ? { event, data: JSON.parse(data) as unknown } : null
    })
    if (response.status !== 200) {
      throw new Error("subscription-bridge-converted-stream-status-mismatch")
    }
    const actualEvents = events(bytes)
    const expectedEvents = events(expected)
    if (canonicalJson(actualEvents) !== canonicalJson(expectedEvents)) {
      const mismatch = actualEvents.findIndex((event, index) => canonicalJson(event) !== canonicalJson(expectedEvents[index]))
      throw new Error(`subscription-bridge-converted-stream-mismatch:${actualEvents.length}:${expectedEvents.length}:${mismatch}`)
    }
  }

  try {
    const first = await startService("first", { shutdown: firstShutdown })
    await openSessions("first")
    await renderWorkflow()
    await setup!.mockInput.typeText("/provider")
    setup!.mockInput.pressEnter()
    await waitForSecretFreeFrame(
      setup!,
      (frame) => frame.includes("Codex Subscription Bridge"),
      [...bridgeSecrets, ...routingSecrets],
      "bridge-provider-picker",
    )
    setup!.mockInput.pressKey("down")
    setup!.mockInput.pressKey("down")
    setup!.mockInput.pressEnter()
    await waitForSecretFreeFrame(
      setup!,
      (frame) => frame.includes("Undocumented ChatGPT Codex interface"),
      [...bridgeSecrets, ...routingSecrets],
      "bridge-provider-form",
    )
    await setup!.mockInput.typeText("Subscription Bridge")
    setup!.mockInput.pressTab()
    await setup!.mockInput.typeText("gpt-5.6")
    setup!.mockInput.pressEnter()
    await waitFor(() => claudeSession!.get().providers.length === 1, "bridge-provider-created")
    const bridge = claudeSession!.get().providers[0]!
    await setup!.mockInput.typeText("/accounts")
    setup!.mockInput.pressEnter()
    await waitForSecretFreeFrame(
      setup!,
      (frame) => frame.includes("Subscription Accounts"),
      [...bridgeSecrets, ...routingSecrets],
      "bridge-account-overlay",
    )
    setup!.mockInput.pressKey("a")
    await waitForCatalog(accountSession!, (view) => view.accounts.length === 1, "bridge-account-a")
    setup!.mockInput.pressKey("a")
    await waitForCatalog(accountSession!, (view) => view.accounts.length === 2, "bridge-account-b")
    setup!.mockInput.pressKey("l")
    await waitForCatalog(accountSession!, (view) => view.bindings.length === 1, "bridge-follow-default")
    closeRenderer()

    let directDiagnostic = ""
    try {
      await claudeSession!.act({ kind: "activate-provider", providerId: bridge.id, mode: "direct" })
    } catch (error) {
      assertSecretFree(error, bridgeSecrets, "direct-deviation-error")
      directDiagnostic = typeof error === "object" && error !== null && "code" in error ? String(error.code) : ""
    }
    if (directDiagnostic !== "unsupported-activation-mode") throw new Error("subscription-bridge-direct-not-rejected")
    await claudeSession!.act({ kind: "activate-provider", providerId: bridge.id, mode: "takeover" })
    const nativeOutcome = await claudeSession!.act({
      kind: "create-provider",
      name: "Native Fallback",
      baseUrl: upstreamOrigin,
      model: "claude-native",
      credential: { kind: "replace", value: "NATIVE_BRIDGE_FALLBACK_SECRET_12201" },
      authentication: "anthropic-api-key",
      presetKey: null,
    })
    const native = nativeOutcome.view.providers.find((provider) => provider.name === "Native Fallback")!
    const draft = await claudeSession!.act({
      kind: "save-failover-draft",
      members: [
        { providerId: bridge.id, providerRevision: bridge.providerRevision },
        { providerId: native.id, providerRevision: native.providerRevision },
      ],
    })
    await claudeSession!.act({
      kind: "apply-failover-chain",
      draftRevision: draft.view.failover.draftRevision,
    })

    await assertConverted(await sendFixture("messages-text.input.json"))
    await assertBridgeRequest(0, "account-a", privateSecrets[11], "messages-text.expected.json")
    const preview = await accountSession!.previewDefault("account-b")
    await accountSession!.act({ kind: "set-default-account", accountId: "account-b", previewToken: preview.previewToken })
    await assertConverted(await sendFixture("messages-tools.input.json"))
    await assertBridgeRequest(1, "account-b", privateSecrets[12], "messages-tools.expected.json")
    await accountSession!.act({
      kind: "bind-provider-fixed",
      target: "claude",
      providerId: bridge.id,
      providerRevision: bridge.providerRevision,
      accountId: "account-a",
    })

    await stopService(first, firstShutdown)
    const second = await startService("restart", { shutdown: secondShutdown })
    await openSessions("restart")
    await assertConverted(await sendFixture("messages-text.input.json"))
    await assertBridgeRequest(2, "account-a", privateSecrets[11], "messages-text.expected.json")
    await accountSession!.act({ kind: "delete-account", accountId: "account-a" })
    const beforeFallback = bridgeUpstream.requests.length
    const fallback = await sendFixture("messages-text.input.json")
    const fallbackText = await fallback.text()
    const fallbackChecks = {
      status: fallback.status === 200,
      body: fallbackText.includes("native fallback"),
      attempts: bridgeUpstream.requests.length === beforeFallback + 1,
      path: bridgeUpstream.requests.at(-1)?.path === "/messages",
    }
    const failedFallback = Object.entries(fallbackChecks).filter(([, matched]) => !matched).map(([name]) => name)
    if (failedFallback.length > 0) {
      throw new Error(`subscription-bridge-fixed-delete-substituted-account:${failedFallback.join(",")}`)
    }
    await assertNativeRequest(beforeFallback, "messages-text.input.json")
    await accountSession!.act({
      kind: "bind-provider-fixed",
      target: "claude",
      providerId: bridge.id,
      providerRevision: bridge.providerRevision,
      accountId: "account-b",
    })
    await stopService(second, secondShutdown)

    const third = await startService("needs-reauthorization", { refresh: "account-b" })
    await openSessions("needs-reauthorization")
    if (accountSession!.get().accounts[0]?.state !== "needs-reauthorization") {
      throw new Error("subscription-bridge-needs-reauthorization-missing")
    }
    const beforeNeedsFallback = bridgeUpstream.requests.length
    const needsFallback = await sendFixture("messages-text.input.json")
    if (needsFallback.status !== 200 || bridgeUpstream.requests.length !== beforeNeedsFallback + 1
      || bridgeUpstream.requests.at(-1)?.path !== "/messages") {
      throw new Error("subscription-bridge-needs-reauthorization-did-not-fail-over")
    }
    await assertNativeRequest(beforeNeedsFallback, "messages-text.input.json")
    await renderWorkflow()
    await setup!.mockInput.typeText("/accounts")
    setup!.mockInput.pressEnter()
    await waitForSecretFreeFrame(
      setup!,
      (frame) => frame.includes("Needs Reauthorization"),
      [...bridgeSecrets, ...routingSecrets],
      "bridge-needs-reauthorization",
    )
    setup!.mockInput.pressKey("r")
    await waitForCatalog(accountSession!, (view) => view.accounts[0]?.state === "authorized", "bridge-reauthorized")
    closeRenderer()
    await assertConverted(await sendFixture("messages-text.input.json"))
    await assertBridgeRequest(bridgeUpstream.requests.length - 1, "account-b", privateSecrets[13], "messages-text.expected.json")

    const beforeDeviation = bridgeUpstream.requests.length
    const { endpoint, token } = await routingAccess()
    const countTokens = await fetch(`${endpoint}/v1/messages/count_tokens`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
      body: "{}",
    })
    if (countTokens.status !== 501 || await countTokens.text() !== "subscription-bridge-count-tokens-unsupported"
      || bridgeUpstream.requests.length !== beforeDeviation) {
      throw new Error("subscription-bridge-count-tokens-deviation-mismatch")
    }

    const beforeCancellation = bridgeUpstream.requests.length
    bridgeUpstream.holdNextResponse()
    const cancellationAccess = await routingAccess()
    const cancellationUrl = new URL(cancellationAccess.endpoint)
    const cancellationBody = await readFile(resolve(
      repoRoot,
      "crates/routing-service/tests/fixtures/subscription-bridge/messages-text.input.json",
    ))
    const cancellationSocket = createConnection({
      host: cancellationUrl.hostname,
      port: Number(cancellationUrl.port),
    })
    await new Promise<void>((resolveConnect, reject) => {
      cancellationSocket.once("connect", resolveConnect)
      cancellationSocket.once("error", reject)
    })
    const cancellationChunks: Buffer[] = []
    const firstDownstream = new Promise<void>((resolveFirst, reject) => {
      const onData = (chunk: Buffer) => {
        cancellationChunks.push(Buffer.from(chunk))
        if (Buffer.concat(cancellationChunks).includes(Buffer.from("event: message_start"))) {
          cancellationSocket.off("data", onData)
          resolveFirst()
        }
      }
      cancellationSocket.on("data", onData)
      cancellationSocket.once("error", reject)
    })
    cancellationSocket.write([
      "POST /v1/messages HTTP/1.1",
      `Host: ${cancellationUrl.host}`,
      `Authorization: Bearer ${cancellationAccess.token}`,
      "Content-Type: application/json",
      `Content-Length: ${cancellationBody.length}`,
      "Connection: close",
      "",
      "",
    ].join("\r\n"))
    cancellationSocket.write(cancellationBody)
    await Promise.race([
      firstDownstream,
      Bun.sleep(deadlineMs).then(() => { throw new Error("subscription-bridge-downstream-first-frame-timeout") }),
    ])
    if (bridgeUpstream.requests.length !== beforeCancellation + 1
      || bridgeUpstream.requests.at(-1)?.path !== "/backend-api/codex/responses") {
      throw new Error("subscription-bridge-cancellation-did-not-reach-bridge-upstream")
    }
    await assertBridgeRequest(
      beforeCancellation,
      "account-b",
      privateSecrets[13],
      "messages-text.expected.json",
    )
    const bytesBeforeCancel = cancellationChunks.reduce((total, chunk) => total + chunk.length, 0)
    cancellationSocket.destroy()
    await new Promise<void>((resolveClose) => cancellationSocket.once("close", resolveClose))
    bridgeUpstream.resumeHeldResponse()
    await new Promise<void>((resolveTurn) => setImmediate(resolveTurn))
    if (cancellationChunks.reduce((total, chunk) => total + chunk.length, 0) !== bytesBeforeCancel) {
      throw new Error("subscription-bridge-late-frame-after-cancel")
    }
    bridgeUpstream.finishHeldResponse()

    await claudeSession!.act({ kind: "disable-takeover" })
    closeRenderer()
    await closeSessions()
    const finalExit = await Promise.race([third.output.completed, Bun.sleep(deadlineMs).then(() => undefined)])
    if (!finalExit || finalExit.code !== 0 || finalExit.signal !== null) throw new Error("subscription-bridge-natural-exit-failed")
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    const listenerReachable = await Promise.race([
      new Promise<boolean>((resolveReachability) => {
        const socket = createConnection({
          host: cancellationUrl.hostname,
          port: Number(cancellationUrl.port),
        })
        socket.once("connect", () => {
          socket.destroy()
          resolveReachability(true)
        })
        socket.once("error", () => resolveReachability(false))
      }),
      Bun.sleep(deadlineMs).then(() => true),
    ])
    if (listenerReachable) throw new Error("subscription-bridge-listener-survived-natural-exit")

    const accountMetadata = await stat(accountPath)
    const settingsMetadata = await stat(settingsPath)
    if ((accountMetadata.mode & 0o777) !== 0o600) throw new Error("subscription-bridge-private-file-mode-mismatch")
    const restoredSettings = await readFile(settingsPath)
    if ((settingsMetadata.mode & 0o777) !== 0o640 || !restoredSettings.equals(originalSettings)) {
      throw new Error("subscription-bridge-settings-restore-mismatch")
    }
    const accountDocument = JSON.parse((await readFile(accountPath)).toString("utf8")) as {
      accounts?: Record<string, { refresh_token?: string; [key: string]: unknown }>
      [key: string]: unknown
    }
    const persistedAccount = accountDocument.accounts?.["account-b"]
    const safeAccountDocument = structuredClone(accountDocument)
    if (persistedAccount?.refresh_token === privateSecrets[16]
      && safeAccountDocument.accounts?.["account-b"]) {
      safeAccountDocument.accounts["account-b"].refresh_token = "<approved-refresh-token>"
    }
    assertSecretFree(safeAccountDocument, bridgeSecrets, "bridge-private-account-file")
    if (persistedAccount?.refresh_token !== privateSecrets[16]
      || Object.hasOwn(persistedAccount, "access_token")) {
      throw new Error("subscription-bridge-private-account-state-mismatch")
    }
    const databaseBytes = (await readFile(databasePath)).toString("latin1")
    assertSecretFree(databaseBytes, privateSecrets, "bridge-sqlite-access-token")
    assertSecretFree(
      Buffer.concat(rpcChunks).toString("latin1"),
      [...bridgeSecrets, ...routingSecrets],
      "bridge-raw-rpc",
    )
    assertSecretFree(renderedFrames, [...bridgeSecrets, ...routingSecrets], "bridge-renderer")
    for (const service of services) {
      assertSecretFree(
        Buffer.concat([...service.output.stdout, ...service.output.stderr]).toString("latin1"),
        [...bridgeSecrets, ...routingSecrets],
        "bridge-process-output",
      )
    }
  } finally {
    closeRenderer()
    await closeSessions()
    for (const { child } of services) if (child.exitCode === null) child.kill("SIGKILL")
    await Promise.all(services.map(({ output }) => Promise.race([output.completed.catch(() => undefined), Bun.sleep(deadlineMs)])))
    assertSecretFree(
      Buffer.concat(rpcChunks).toString("latin1"),
      [...bridgeSecrets, ...routingSecrets],
      "bridge-final-raw-rpc",
    )
    assertSecretFree(renderedFrames, [...bridgeSecrets, ...routingSecrets], "bridge-final-renderer")
    for (const service of services) {
      assertSecretFree(
        Buffer.concat([...service.output.stdout, ...service.output.stderr]).toString("latin1"),
        [...bridgeSecrets, ...routingSecrets],
        "bridge-final-process-output",
      )
    }
    await authority.close()
    await bridgeUpstream.close()
  }
}, 120_000)
