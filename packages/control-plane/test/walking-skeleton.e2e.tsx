/** @jsxImportSource @opentui/solid */
import { Database } from "bun:sqlite"
import { afterEach, expect, test } from "bun:test"
import { TestRecorder } from "@opentui/core/testing"
import { testRender } from "@opentui/solid"
import { createHash } from "node:crypto"
import { spawn } from "node:child_process"
import { createConnection } from "node:net"
import { request as httpRequest } from "node:http"
import { chmod, mkdir, mkdtemp, readFile, readdir, rm, stat, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { dirname, join, resolve } from "node:path"
import { finished } from "node:stream/promises"

import { RpcClient } from "../src/control/rpc-client"
import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { App } from "../src/ui/app"
import type { TargetSession } from "../src/control/target-session"
import { parseClientFrame, type TargetView } from "../src/control/types"
import { SSE_BYTES, startFakeUpstream, type CapturedRequest } from "../../../tests/e2e/fake-upstream"

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
      request.headers.authorization ?? null,
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
    const receipts = database.query(`SELECT action_id, action_kind, committed_revision,
      outcome_json AS outcomeJson FROM action_receipts ORDER BY committed_revision, action_id`
    ).all() as Array<Record<string, unknown> & { outcomeJson: string }>
    const recovery = database.query(`SELECT id, target, action_id, config_path,
      file_identity_json AS fileIdentityJson, before_owned_json AS beforeOwnedJson,
      desired_owned_json AS desiredOwnedJson, state, created_revision
      FROM activation_recovery ORDER BY created_revision, id`).all() as Array<Record<string, unknown> & {
        fileIdentityJson: string
        beforeOwnedJson: string
        desiredOwnedJson: string
      }>
    return [
      ["metadata", database.query("SELECT key, value FROM metadata ORDER BY key").all()],
      ["credentials", credentials.map(({ bearerToken, ...row }) => ({
        ...row,
        bearerTokenDigest: sensitiveDigest(bearerToken),
      }))],
      ["providers", database.query(`SELECT id, target, position, provider_revision, name, base_url, model, protocol,
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
        beforeOwnedJson,
        desiredOwnedJson,
        ...row
      }) => ({
        ...row,
        fileIdentityJsonDigest: sensitiveDigest(fileIdentityJson),
        beforeOwnedJsonDigest: sensitiveDigest(beforeOwnedJson),
        desiredOwnedJsonDigest: sensitiveDigest(desiredOwnedJson),
      }))],
    ]
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
        (id, target, provider_id, base_url, model, provider_bearer_token, epoch)
        VALUES ('snapshot-id', 'codex', 'provider-id', 'https://snapshot.invalid/v1', 'model',
          'snapshot-sensitive-one', 'epoch');
      UPDATE target_route_state SET routing_credential = 'routing-sensitive-one' WHERE target = 'codex';
      INSERT INTO activation_recovery
        (id, target, action_id, config_path, file_identity_json, before_owned_json,
          desired_owned_json, state, created_revision)
        VALUES ('recovery-id', 'codex', 'action-id', '/controlled/config',
          'identity-sensitive-one', 'before-sensitive-one', 'desired-sensitive-one', 'pending', 1);
    `)
    await writeFile(configPath, "managed-sensitive-one")

    const mutations = [
      "UPDATE credentials SET bearer_token = 'credential-sensitive-two'",
      "UPDATE target_route_state SET routing_credential = 'routing-sensitive-two' WHERE target = 'codex'",
      "UPDATE activated_snapshots SET provider_bearer_token = 'snapshot-sensitive-two'",
      "UPDATE activation_recovery SET file_identity_json = 'identity-sensitive-two'",
      "UPDATE activation_recovery SET before_owned_json = 'before-sensitive-two'",
      "UPDATE activation_recovery SET desired_owned_json = 'desired-sensitive-two'",
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
    requestAudit.expectNext("expected-provider")
    setup.mockInput.pressEnter()
    await upstream.waitForCallCount(1)
    expect(requestAudit.projection(0)).toEqual(expectedModelRequestProjection)
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
    await setup.waitFor(() => decodedInboundFrames.slice(explicitInboundStart).some((frame) =>
      JSON.stringify(frame).includes('"kind":"model-discovery"')
    ))
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
    requestAudit.expectNext("expected-provider")
    setup.mockInput.pressEnter()
    await upstream.waitForCallCount(3)
    expect(requestAudit.projection(2)).toEqual(expectedModelRequestProjection)
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
    requestAudit.expectNext("absent")
    leader("t")
    await upstream.waitForCallCount(5)
    expect(requestAudit.projection(4)).toEqual(expectedReachabilityRequestProjection)
    const reachabilityFrame = await setup.waitForFrame((frame) =>
      frame.includes("Reachable") && frame.includes("HTTP 401")
    )
    selectedRenderedFrames.push(reachabilityFrame)
    await assertReadOnlyInspection("reachability", reachabilityBefore)

    // 9. Active deletion is rejected; deleting the inactive shared duplicate preserves its credential.
    const activeDeleteFrameStart = decodedInboundFrames.length
    const activeDeleteRenderStart = rendererAudit.frames().length
    leader("d")
    const activeDeleteConfirmation = await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    selectedRenderedFrames.push(activeDeleteConfirmation)
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => decodedInboundFrames.slice(activeDeleteFrameStart).some((frame) =>
      JSON.stringify(frame).includes("provider-referenced")
    ))
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
