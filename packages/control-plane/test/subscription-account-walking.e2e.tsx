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
  let rendered = ""
  try { rendered = typeof value === "string" ? value : JSON.stringify(value) }
  catch { throw new Error(`subscription-secret-scan-failed:${label}`) }
  for (const secret of secrets) {
    const numeric = [...Buffer.from(secret)].join(",")
    if (rendered.includes(secret) || rendered.includes(numeric)) {
      throw new Error(`subscription-secret-scan-failed:${label}`)
    }
  }
}

test("Subscription Account tracer scans literal and numeric-byte private surfaces", () => {
  const secret = "SUBSCRIPTION_TRACER_CONTROLLED_SECRET_12120"
  for (const surface of [secret, Buffer.from(secret), { nested: secret }]) {
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
