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
import type { TargetSession } from "../src/control/target-session"
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

function extractManagedConfig(text: string, expectedModel: string): { endpoint: string; credential: string } {
  expect(text).toContain("# operator comment survives")
  expect(text).toContain('unrelated = "keep-me"')
  expect(text).toContain(`model = "${expectedModel}"`)
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

function scanNoSecrets(values: unknown[], secrets: readonly string[] = [providerSecret, wrongRoutingSecret]): void {
  const joined = values.map((value) => typeof value === "string" ? value : JSON.stringify(value)).join("\n")
  for (const secret of secrets) expect(joined).not.toContain(secret)
}

function scanRawRpcFramesNoSecrets(
  streams: readonly (readonly Buffer[])[],
  secrets: readonly string[] = [providerSecret, wrongRoutingSecret],
): void {
  expect(streams.reduce((count, frames) => count + frames.length, 0)).toBeGreaterThan(0)
  const decodedFrames = streams.flatMap((frames) => {
    const decoder = new FrameDecoder()
    const decoded = frames.flatMap((frame) => decoder.push(frame))
    decoder.finish()
    return decoded
  })
  scanNoSecrets(decodedFrames, secrets)
}

function readDatabaseProjection(path: string): unknown {
  const database = new Database(path, { readonly: true })
  try {
    const queries = [
      ["metadata", "SELECT key, value FROM metadata ORDER BY key"],
      ["credentials", "SELECT id, target FROM credentials ORDER BY id"],
      ["providers", `SELECT id, target, position, provider_revision, name, base_url, model, protocol,
        credential_id, provenance_kind, provenance_key, generated_owner_id FROM providers ORDER BY position`],
      ["target-route", `SELECT target, management_revision, view_sequence, current_provider_id,
        serving_provider_id, takeover_state, route_port, activated_snapshot_id, managed_config_path,
        recovery_state FROM target_route_state ORDER BY target`],
      ["target-problems", "SELECT target, code, message FROM target_problems ORDER BY target, code"],
      ["snapshots", `SELECT id, target, provider_id, base_url, model, epoch
        FROM activated_snapshots ORDER BY id`],
      ["receipts", `SELECT action_id, action_kind, committed_revision, outcome_json
        FROM action_receipts ORDER BY committed_revision, action_id`],
      ["recovery", `SELECT id, target, action_id, config_path, state, created_revision
        FROM activation_recovery ORDER BY created_revision, id`],
    ] as const
    return queries.map(([name, sql]) => [name, database.query(sql).all()])
  } finally {
    database.close()
  }
}

async function readManagedConfigurationFingerprint(path: string): Promise<string> {
  try {
    return Buffer.from(await readFile(path)).toString("base64")
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

function credentialReferences(databasePath: string): Array<{ name: string; credentialId: string | null }> {
  const database = new Database(databasePath, { readonly: true })
  try {
    return database.query(`SELECT name, credential_id AS credentialId
      FROM providers ORDER BY position`).all() as Array<{ name: string; credentialId: string | null }>
  } finally {
    database.close()
  }
}

function captureFrame(
  setup: Awaited<ReturnType<typeof testRender>>,
  renderedFrames: string[],
  activities: string[],
): string {
  const frame = setup.captureCharFrame()
  renderedFrames.push(frame)
  activities.push(...frame.split("\n").map((line) => line.trim()).filter((line) =>
    /Provider saved:|Target Takeover applied:|Target state updated/.test(line)
  ))
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
  const upstream = await startFakeUpstream(providerSecret)
  const logs: string[] = []
  const rpcStreams: Buffer[][] = []
  const decodedInboundFrames: unknown[] = []
  const views: TargetView[] = []
  const activities: string[] = []
  const renderedFrames: string[] = []
  const readOnlyInspections: string[] = []
  let service: ReturnType<typeof spawn> | undefined
  let client: RpcClient | undefined
  let session: TargetSession | undefined
  let unsubscribe: (() => void) | undefined
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

    const connect = async (release: string) => {
      const stream: Buffer[] = []
      const decoder = new FrameDecoder()
      rpcStreams.push(stream)
      return await RpcClient.connect(socketPath, release, undefined, (path) => {
        const socket = createConnection({ path })
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
    captureFrame(setup, renderedFrames, activities)
    const incomplete = session.get().providers[0]!
    expect(incomplete).toMatchObject({
      name: "Original Provider",
      completeness: "incomplete",
      missingFields: ["base-url", "model", "credential"],
    })

    // 2. The first saved-state inspection sees only the incomplete saved declaration.
    const incompletePickerFrame = await openProviderPicker()
    expect(incompletePickerFrame).toContain("Incomplete")
    renderedFrames.push(incompletePickerFrame)
    const incompleteInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    setup.mockInput.pressEnter()
    const missingCredentialFrame = await setup.waitForFrame((frame) => frame.includes("Credential missing"))
    renderedFrames.push(missingCredentialFrame)
    await assertReadOnlyInspection("incomplete automatic discovery", incompleteInspectionBefore)
    expect(upstream.calls).toHaveLength(0)

    // Typing endpoint/model/credential never starts discovery; save is the first declaration mutation.
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(upstream.baseUrl)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("manual-before-discovery")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(providerSecret)
    await setup.renderOnce()
    const formFrame = captureFrame(setup, renderedFrames, activities)
    expect(formFrame.includes(providerSecret)).toBeFalse()
    expect(upstream.calls).toHaveLength(0)
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers[0]?.completeness === "complete", "complete Provider save")
    const completeFrame = await setup.waitForFrame((frame) => frame.includes("Provider saved: Original Provider"))
    renderedFrames.push(completeFrame)

    // Reopening now performs automatic discovery with the newly saved endpoint and Credential Reference.
    await openProviderPicker()
    const savedInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    setup.mockInput.pressEnter()
    await upstream.waitForCallCount(1)
    const automaticModelsFrame = await setup.waitForFrame((frame) => frame.includes("2 models available"))
    renderedFrames.push(automaticModelsFrame)
    await assertReadOnlyInspection("complete automatic discovery", savedInspectionBefore)
    expect(upstream.calls[0]).toMatchObject({
      method: "GET",
      path: "/v1/models",
      authorization: `Bearer ${providerSecret}`,
      body: "",
    })

    // 3. Explicit discovery is a second read-only request; selecting is local until save.
    const explicitInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    const explicitInboundStart = decodedInboundFrames.length
    leader("f")
    await upstream.waitForCallCount(2)
    await setup.waitFor(() => decodedInboundFrames.slice(explicitInboundStart).some((frame) =>
      JSON.stringify(frame).includes('"kind":"model-discovery"')
    ))
    const explicitModelsFrame = await setup.waitForFrame((frame) => frame.includes("2 models available"))
    renderedFrames.push(explicitModelsFrame)
    await assertReadOnlyInspection("explicit discovery", explicitInspectionBefore)
    leader("m")
    const modelPickerFrame = await setup.waitForFrame((frame) =>
      frame.includes("gpt-fixture-a") && frame.includes("gpt-fixture-b")
    )
    renderedFrames.push(modelPickerFrame)
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("gpt-fixture-b") && !frame.includes("Select Model"))
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers[0]?.model === "gpt-fixture-b", "discovered model save")
    await setup.waitForFrame((frame) => frame.includes("Provider saved: Original Provider"))

    const originalId = session.get().providers[0]!.id

    // 4. The safe Preset is copy-on-create and typing its draft makes no network request.
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("OpenAI API (Responses)"))
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    const presetEditor = await setup.waitForFrame((frame) => frame.includes("https://api.openai.com/v1"))
    renderedFrames.push(presetEditor)
    const callsBeforePresetTyping = upstream.calls.length
    await setup.mockInput.typeText("Preset Provider")
    await setup.renderOnce()
    captureFrame(setup, renderedFrames, activities)
    expect(upstream.calls).toHaveLength(callsBeforePresetTyping)
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
    captureFrame(setup, renderedFrames, activities)

    await openProviderPicker()
    leader("c")
    await setup.waitForFrame((frame) => frame.includes("Reuse Credential Reference?"))
    setup.mockInput.pressKey("y")
    await setup.waitForFrame((frame) => frame.includes("Original Provider Copy"))
    await setup.mockInput.typeText(" Shared")
    setup.mockInput.pressEnter()
    await waitForSession(session, (view) => view.providers.length === 4, "shared-credential duplicate save")
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    captureFrame(setup, renderedFrames, activities)

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
    captureFrame(setup, renderedFrames, activities)
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
    renderedFrames.push(activeFrame)

    const activatedConfigBytes = await readFile(configPath)
    const managed = extractManagedConfig(activatedConfigBytes.toString("utf8"), "gpt-fixture-b")
    expect(session.get().currentProviderId).toBe(originalId)
    expect(session.get().activatedSnapshot).toMatchObject({ providerId: originalId, model: "gpt-fixture-b" })
    const wrong = await chunkedPost(managed.endpoint, wrongRoutingSecret)
    expect(wrong.status).toBe(401)
    expect(upstream.calls).toHaveLength(2)

    await openProviderPicker()
    for (let step = 0; step < 2; step++) {
      setup.mockInput.pressKey("up")
      await setup.renderOnce()
    }
    const activeEditorInspectionBefore = await readOnlyStateFingerprint(databasePath, configPath)
    setup.mockInput.pressEnter()
    await upstream.waitForCallCount(3)
    await setup.waitForFrame((frame) => frame.includes("2 models available"))
    await assertReadOnlyInspection("active declaration automatic discovery", activeEditorInspectionBefore)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("/edited")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("-declared")
    expect(upstream.calls).toHaveLength(3)
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
    expect(await readFile(configPath)).toEqual(activatedConfigBytes)

    const valid = await chunkedPost(managed.endpoint, managed.credential)
    await upstream.waitForCallCount(4)
    expect(valid.status).toBe(201)
    expect(valid.headers["content-type"]).toStartWith("text/event-stream")
    expect(valid.body).toBe(SSE_BYTES.join(""))
    expect(upstream.calls[3]).toMatchObject({
      method: "POST",
      authorization: `Bearer ${providerSecret}`,
      contentType: "application/json",
      testHeader: "preserved",
      body: '{"model":"gpt-test","input":"hello"}',
      path: "/v1/responses",
    })
    await waitFor(() => views.some((view) => view.servingProviderId !== null), "Serving push")
    await setup.renderOnce()
    const servedFrame = captureFrame(setup, renderedFrames, activities)
    expect(servedFrame.includes("Serving Provider  Original Provider")).toBeTrue()

    // 8. Reachability is an unauthenticated, headers-only 401 observation with no state change.
    await openProviderPicker()
    const reachabilityBefore = await readOnlyStateFingerprint(databasePath, configPath)
    leader("t")
    await upstream.waitForCallCount(5)
    const reachabilityFrame = await setup.waitForFrame((frame) =>
      frame.includes("Reachable") && frame.includes("HTTP 401")
    )
    renderedFrames.push(reachabilityFrame)
    await assertReadOnlyInspection("reachability", reachabilityBefore)
    expect(upstream.calls[4]).toMatchObject({
      method: "GET",
      path: "/v1/edited",
      authorization: null,
      body: "",
    })

    // 9. Active deletion is rejected; deleting the inactive shared duplicate preserves its credential.
    const activeDeleteFrameStart = decodedInboundFrames.length
    leader("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => decodedInboundFrames.slice(activeDeleteFrameStart).some((frame) =>
      JSON.stringify(frame).includes("provider-referenced")
    ))
    expect(session.get().providers).toHaveLength(4)
    expect(session.get().providers.find(({ id }) => id === originalId)?.activeReferences).toEqual([
      "current",
      "activated-snapshot",
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
    captureFrame(setup, renderedFrames, activities)

    // 10. Closing the Control Plane does not stop an active Routing Service.
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    setup.mockInput.pressCtrlC()
    await setup.waitFor(() => setup!.renderer.isDestroyed)
    setup = undefined
    unsubscribe()
    unsubscribe = undefined
    await session.close()
    session = undefined
    client = undefined
    const second = await chunkedPost(managed.endpoint, managed.credential)
    await upstream.waitForCallCount(6)
    expect(second.status).toBe(201)
    expect(service.exitCode).toBeNull()
    expect(upstream.calls[5]).toMatchObject({
      method: "POST",
      path: "/v1/responses",
      authorization: `Bearer ${providerSecret}`,
    })

    await writeFile(shutdownFile, "shutdown\n")
    await waitFor(() => service!.exitCode !== null, "drained test shutdown")
    expect(service.exitCode).toBe(0)
    await expect(stat(socketPath)).rejects.toMatchObject({ code: "ENOENT" })
    await expect(chunkedPost(managed.endpoint, managed.credential)).rejects.toBeDefined()

    const receiptDatabase = new Database(databasePath, { readonly: true })
    const receipts = receiptDatabase
      .query("SELECT outcome_json FROM action_receipts ORDER BY committed_revision")
      .all()
    receiptDatabase.close()
    const secrets = [providerSecret, wrongRoutingSecret, managed.credential]
    scanRawRpcFramesNoSecrets(rpcStreams, secrets)
    scanNoSecrets([decodedInboundFrames, receipts, views, activities, renderedFrames, logs], secrets)
    expect(readOnlyInspections).toEqual([
      "incomplete automatic discovery",
      "complete automatic discovery",
      "explicit discovery",
      "active declaration automatic discovery",
      "reachability",
    ])
    expect(activities.length).toBeGreaterThan(0)

    for (const call of upstream.calls) {
      expect(JSON.stringify(call)).not.toContain(wrongRoutingSecret)
      expect(JSON.stringify(call)).not.toContain(managed.credential)
      const headersWithoutAuthorization = { ...call.headers, authorization: null }
      scanNoSecrets([{ ...call, authorization: null, headers: headersWithoutAuthorization }], [providerSecret])
      const expectsProviderAuthorization = call.path.endsWith("/models") || call.method === "POST"
      expect(call.authorization).toBe(expectsProviderAuthorization ? `Bearer ${providerSecret}` : null)
      expect(call.headers.authorization ?? null).toBe(expectsProviderAuthorization ? `Bearer ${providerSecret}` : null)
    }
    expect(await controlledTreeFingerprint(operatorHomeCanary)).toBe(operatorHomeCanaryBefore)
  } finally {
    if (setup && !setup.renderer.isDestroyed) setup.renderer.destroy()
    unsubscribe?.()
    await session?.close().catch(() => {})
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
