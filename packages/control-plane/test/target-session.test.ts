import { afterEach, expect, test } from "bun:test"
import { mkdtemp, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { createServer, type Server, type Socket } from "node:net"

import { encodeFrame, FrameDecoder } from "../src/control/framing"
import { RpcClient } from "../src/control/rpc-client"
import type {
  ClientFrame,
  ClaudePreflightContext,
  CompatibilityProbe,
  OrdinaryTargetAction,
  ProviderConfigurationExport,
  ProviderImportChoice,
  ProviderImportOutcome,
  ProviderImportPreview,
  ReconciliationPreview,
  RequestRecordDetail,
  RequestRecordPage,
  ServerFrame,
  TargetAction,
  TargetView,
} from "../src/control/types"

const roots: string[] = []

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

const serviceEpoch = "00000000-0000-4000-8000-000000000001"

function viewAtRevision(revision: number, sequence = revision, target: TargetView["target"] = "codex"): TargetView {
  return {
    target,
    managementRevision: revision,
    viewSequence: sequence,
    service: { epoch: serviceEpoch, state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: revision === 0 ? [] : [{
      id: `provider-${revision}`,
      position: 0,
      providerRevision: 1,
      name: `Provider ${revision}`,
      baseUrl: "https://provider.example/v1",
      model: `model-${revision}`,
      protocol: target === "claude" ? "anthropic-messages" : "openai-responses",
      authentication: target === "claude" ? "anthropic-api-key" : "openai-bearer",
      routingRequirement: "direct-compatible",
      credential: "present",
      completeness: "complete",
      missingFields: [],
      provenance: null,
      generated: false,
      universalProviderId: null,
      synchronization: null,
      ownership: {
        name: "target-provider", baseUrl: "target-provider", model: "target-provider",
        protocol: "target-fixed", authentication: "target-provider",
        routingRequirement: "target-provider", credential: "target-provider",
      },
      routeHealth: { state: "unobserved" },
      activeReferences: [],
    }],
    providerPresets: [],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    failover: { draftRevision: 1, draftMembers: [], activePlan: null },
    problems: [],
  }
}

function reconciliationPreview(
  target: TargetView["target"] = "codex",
  strategy: ReconciliationPreview["strategy"] = "reapply",
): ReconciliationPreview {
  return {
    observationToken: "00000000-0000-4000-8000-000000000701",
    target,
    strategy,
    managementRevision: 1,
    compatibility: {
      version: target === "claude" ? "2.1.0" : "1.2.0",
      classification: "tested",
      acknowledgementRequired: false,
    },
    shadowSources: [],
    changes: [{ field: "provider", state: "changed" }],
    providerEffect: "keep-current",
    restartRequired: true,
    unobservableRuntimeBoundary: true,
  }
}

class ScriptedServer {
  readonly frames: ClientFrame[] = []
  #server: Server
  #socket?: Socket
  #waiters: Array<() => void> = []

  private constructor(server: Server) {
    this.#server = server
  }

  static async start(): Promise<{ server: ScriptedServer; path: string }> {
    const root = await mkdtemp(join(tmpdir(), "muxvia-target-session-"))
    roots.push(root)
    const path = join(root, "control.sock")
    let scripted!: ScriptedServer
    const server = createServer((socket) => scripted.#accept(socket))
    scripted = new ScriptedServer(server)
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject)
      server.listen(path, resolve)
    })
    return { server: scripted, path }
  }

  #accept(socket: Socket): void {
    this.#socket = socket
    const decoder = new FrameDecoder()
    socket.on("data", (chunk) => {
      if (typeof chunk === "string") throw new Error("unexpected text chunk")
      for (const value of decoder.push(chunk)) {
        const frame = value as ClientFrame
        this.frames.push(frame)
        if (frame.type === "hello") {
          this.send({
            type: "hello-ack",
            rpc: { major: 1, minor: 0 },
            release: "routing-test",
            serviceEpoch,
            frameLimit: 1_048_576,
          })
        }
        for (const wake of this.#waiters.splice(0)) wake()
      }
    })
  }

  async waitForRequests(count: number): Promise<void> {
    while (this.requests().length < count) {
      await new Promise<void>((resolve) => this.#waiters.push(resolve))
    }
  }

  async waitForFrames(count: number): Promise<void> {
    while (this.frames.length < count) {
      await new Promise<void>((resolve) => this.#waiters.push(resolve))
    }
  }

  requests(): Extract<ClientFrame, { type: "request" }>[] {
    return this.frames.filter((frame): frame is Extract<ClientFrame, { type: "request" }> => frame.type === "request")
  }

  receivedActionCount(): number {
    return this.requests().filter((frame) => frame.operation.kind === "act").length
  }

  cancels(): Extract<ClientFrame, { type: "cancel" }>[] {
    return this.frames.filter(
      (frame): frame is Extract<ClientFrame, { type: "cancel" }> => frame.type === "cancel",
    )
  }

  replyOpen(index: number, view: TargetView): void {
    const frame = this.requests()[index]!
    this.send({ type: "response", requestId: frame.requestId, result: { kind: "target-view", view } })
  }

  replyApplied(view: TargetView): void {
    const frame = this.requests().filter((request) => request.operation.kind === "act").at(-1)!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "action-outcome", outcome: { status: "applied", view } },
    })
    this.push(view)
  }

  replyStale(view: TargetView, code = "stale-revision"): void {
    const frame = this.requests().filter((request) => request.operation.kind === "act").at(-1)!
    this.send({
      type: "error",
      requestId: frame.requestId,
      problem: { code, message: "Target state changed" },
      authoritativeView: view,
    })
  }

  replyDiscovery(index: number, models: string[]): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: {
        kind: "model-discovery",
        result: {
          status: "success",
          models: models.map((id) => ({ id, displayName: null })),
          attempts: 1,
          elapsedMs: 1,
          endpointOrigin: "https://draft.example",
        },
      },
    })
  }

  replyPreview(index: number, preview: ReconciliationPreview): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "reconciliation-preview", preview },
    })
  }

  replyCompatibilityProbe(index: number, probe: CompatibilityProbe): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "compatibility-probe", probe },
    })
  }

  replyRequestRecordPage(index: number, page: RequestRecordPage): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "request-record-page", page },
    })
  }

  replyRequestRecordDetail(index: number, detail: RequestRecordDetail): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "request-record-detail", detail },
    })
  }

  replyProviderImportPreview(index: number, preview: ProviderImportPreview): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "provider-import-preview", preview },
    })
  }

  replyProviderImportOutcome(index: number, outcome: ProviderImportOutcome): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "provider-import-outcome", outcome },
    })
  }

  replyProviderConfigurationExport(index: number, exportValue: ProviderConfigurationExport): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "provider-configuration-export", export: exportValue },
    })
  }

  replyHandoverPrepared(index: number, release: string): void {
    const frame = this.requests()[index]!
    this.send({
      type: "response",
      requestId: frame.requestId,
      result: { kind: "handover-prepared", release },
    })
  }

  replyWithTargetView(index: number, view: TargetView): void {
    const frame = this.requests()[index]!
    this.send({ type: "response", requestId: frame.requestId, result: { kind: "target-view", view } })
  }

  push(view: TargetView): void {
    this.send({ type: "target-view", view })
  }

  send(frame: ServerFrame): void {
    this.#socket!.write(encodeFrame(frame))
  }

  async close(): Promise<void> {
    this.#socket?.destroy()
    await new Promise<void>((resolve) => this.#server.close(() => resolve()))
  }
}

async function openScriptedSession(initial: TargetView) {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const opening = client.openTarget(initial.target)
  await server.waitForRequests(1)
  server.replyOpen(0, initial)
  const session = await opening
  return { session, server }
}

test("RpcClient exposes exact negotiated service metadata", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")

  expect(client.serviceMetadata).toEqual({
    release: "routing-test",
    serviceEpoch,
    rpc: { major: 1, minor: 0 },
  })

  await client.close()
  await server.close()
})

test("a TargetSession lists and inspects target-bound immutable request history", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const recordId = "00000000-0000-4000-8000-000000000901"
  const summary = {
    id: recordId,
    planId: "00000000-0000-4000-8000-000000000902",
    planEpoch: "00000000-0000-4000-8000-000000000903",
    providerId: "00000000-0000-4000-8000-000000000904",
    providerName: "Recorded Provider",
    model: "recorded-model",
    protocol: "openai-responses" as const,
    startedAtUnixMs: 100,
    finishedAtUnixMs: 125,
    latencyMs: 25,
    outcome: "upstream-error" as const,
    httpStatus: 429,
    usage: null,
    estimatedCostNanoUsd: null,
    hasErrorPayload: true,
    errorPayloadTruncated: false,
  }
  const listing = session.listRequestRecords({ limit: 17, beforeCursor: "opaque-before" })
  await server.waitForRequests(2)
  expect(server.requests()[1]!.operation).toEqual({
    kind: "list-request-records",
    target: "codex",
    limit: 17,
    beforeCursor: "opaque-before",
  })
  server.replyRequestRecordPage(1, {
    target: "codex",
    records: [summary],
    nextCursor: "opaque-next",
  })
  const page = await listing
  expect(page.records[0]).toEqual(summary)
  expect(Object.isFrozen(page)).toBe(true)
  expect(Object.isFrozen(page.records)).toBe(true)
  expect(Object.isFrozen(page.records[0])).toBe(true)

  const inspecting = session.inspectRequestRecord(recordId)
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toEqual({
    kind: "inspect-request-record",
    target: "codex",
    recordId,
  })
  server.replyRequestRecordDetail(2, {
    target: "codex",
    record: summary,
    pricingSnapshot: null,
    errorPayload: "sanitized failure",
    errorPayloadSensitive: true,
  })
  const detail = await inspecting
  expect(detail.errorPayload).toBe("sanitized failure")
  expect(Object.isFrozen(detail)).toBe(true)
  expect(Object.isFrozen(detail.record)).toBe(true)

  await session.close()
  await server.close()
})

test("a TargetSession owns the closed preview-confirm-export Provider Transfer workflow", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(0))
  const source = {
    kind: "cc-switch-sql",
    path: "/operator/selected/cc-switch-export.sql",
  } as const
  const previewing = session.previewProviderImport(source)
  await server.waitForRequests(2)
  expect(server.requests()[1]!.operation).toEqual({
    kind: "preview-provider-import",
    target: "codex",
    source,
  })
  const preview: ProviderImportPreview = {
    previewToken: "00000000-0000-4000-8000-000000000161",
    source: { product: "cc-switch", target: "codex" },
    candidates: [{
      kind: "target-provider",
      candidateId: "00000000-0000-4000-8000-000000000162",
      target: "codex",
      name: "Relay",
      baseUrl: "https://relay.example/v1",
      model: "gpt-relay",
      protocol: "openai-responses",
      authentication: "openai-bearer",
      routingRequirement: "direct-compatible",
      credential: "present",
      importedCurrent: false,
      exactMatches: [],
    }],
    historicalUsage: {
      recordCount: 5,
      startDate: "2025-12-31",
      endDate: "2026-01-01",
      estimatedStorageBytes: 512,
      selectedByDefault: false,
    },
  }
  server.replyProviderImportPreview(1, preview)
  const receivedPreview = await previewing
  expect(receivedPreview).toEqual(preview)
  expect(Object.isFrozen(receivedPreview.candidates[0])).toBe(true)

  const choices: ProviderImportChoice[] = [{
    candidateId: preview.candidates[0]!.candidateId,
    resolution: { kind: "create" },
  }]
  const confirming = session.confirmProviderImport(preview.previewToken, choices, true)
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toEqual({
    kind: "confirm-provider-import",
    target: "codex",
    previewToken: preview.previewToken,
    choices,
    includeHistoricalUsage: true,
  })
  const outcome: ProviderImportOutcome = {
    records: [{
      kind: "target-provider",
      candidateId: preview.candidates[0]!.candidateId,
      resolution: "created",
      target: "codex",
      providerId: "00000000-0000-4000-8000-000000000163",
    }],
    historicalUsageImportedRecords: 5,
  }
  server.replyProviderImportOutcome(2, outcome)
  expect(await confirming).toEqual(outcome)

  const exporting = session.exportProviderConfiguration()
  await server.waitForRequests(4)
  expect(server.requests()[3]!.operation).toEqual({
    kind: "export-provider-configuration",
    target: "codex",
  })
  const exportValue: ProviderConfigurationExport = {
    format: "muxvia-provider-configuration",
    version: 1,
    universalProviders: [],
    targetProviders: [],
    failoverDrafts: [
      { target: "codex", providerSourceIds: [] },
      { target: "claude", providerSourceIds: [] },
    ],
  }
  server.replyProviderConfigurationExport(3, exportValue)
  const receivedExport = await exporting
  expect(receivedExport).toEqual(exportValue)
  expect(Object.isFrozen(receivedExport.failoverDrafts)).toBe(true)

  await session.close()
  await expect(session.previewProviderImport(source)).rejects.toMatchObject({ code: "connection-closed" })
  await expect(session.confirmProviderImport(preview.previewToken, [])).rejects.toMatchObject({ code: "connection-closed" })
  await expect(session.exportProviderConfiguration()).rejects.toMatchObject({ code: "connection-closed" })
  await server.close()
})

test("request history is cancellable and rejects mismatched Target or record responses", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const controller = new AbortController()
  const cancelled = session.listRequestRecords({ limit: 10 }, controller.signal)
  await server.waitForRequests(2)
  const cancelledRequest = server.requests()[1]!
  controller.abort()
  await expect(cancelled).rejects.toMatchObject({ code: "cancelled" })
  await server.waitForFrames(4)
  expect(server.cancels()).toContainEqual({ type: "cancel", requestId: cancelledRequest.requestId })

  const wrongPage = session.listRequestRecords({ limit: 1 })
  await server.waitForRequests(3)
  server.replyRequestRecordPage(2, { target: "claude", records: [], nextCursor: null })
  await expect(wrongPage).rejects.toMatchObject({
    code: "invalid-response",
    message: "Request record page did not match Target",
  })

  const requestedId = "00000000-0000-4000-8000-000000000911"
  const wrongDetail = session.inspectRequestRecord(requestedId)
  await server.waitForRequests(4)
  server.replyRequestRecordDetail(3, {
    target: "codex",
    record: {
      id: "00000000-0000-4000-8000-000000000912",
      planId: "00000000-0000-4000-8000-000000000913",
      planEpoch: "00000000-0000-4000-8000-000000000914",
      providerId: null,
      providerName: null,
      model: "recorded-model",
      protocol: "openai-responses",
      startedAtUnixMs: 100,
      finishedAtUnixMs: 125,
      latencyMs: 25,
      outcome: "route-unavailable",
      httpStatus: null,
      usage: null,
      estimatedCostNanoUsd: null,
      hasErrorPayload: false,
      errorPayloadTruncated: false,
    },
    pricingSnapshot: null,
    errorPayload: null,
    errorPayloadSensitive: false,
  })
  await expect(wrongDetail).rejects.toMatchObject({
    code: "invalid-response",
    message: "Request record detail did not match request",
  })

  const wrongKind = session.listRequestRecords({ limit: 1 })
  await server.waitForRequests(5)
  server.replyWithTargetView(4, viewAtRevision(1))
  await expect(wrongKind).rejects.toMatchObject({
    code: "invalid-response",
    message: "Request record page did not match Target",
  })

  await session.close()
  await expect(session.listRequestRecords({ limit: 1 })).rejects.toMatchObject({ code: "connection-closed" })
  await expect(session.inspectRequestRecord(requestedId)).rejects.toMatchObject({ code: "connection-closed" })
  expect(server.requests()).toHaveLength(5)
  await server.close()
})

test("a TargetSession exposes target-bound native usage lifecycle operations", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const listing = session.listUsageActivity({ limit: 7, beforeCursor: "usage-before" })
  await server.waitForRequests(2)
  expect(server.requests()[1]!.operation).toEqual({
    kind: "list-usage-activity",
    target: "codex",
    limit: 7,
    beforeCursor: "usage-before",
  })
  server.send({
    type: "response",
    requestId: server.requests()[1]!.requestId,
    result: {
      kind: "usage-activity-page",
      page: {
        target: "codex",
        entries: [],
        nextCursor: null,
        detailedRetentionDays: 30,
        catalogVersion: "release-pinned",
      },
    },
  })
  expect(Object.isFrozen(await listing)).toBe(true)

  const refreshing = session.refreshNativeUsage()
  await server.waitForRequests(3)
  server.send({
    type: "response",
    requestId: server.requests()[2]!.requestId,
    result: { kind: "native-usage-refresh", refresh: { target: "codex", importedRecords: 2, scannedFiles: 1 } },
  })
  await expect(refreshing).resolves.toMatchObject({ importedRecords: 2 })

  const retaining = session.setUsageRetention(14)
  await server.waitForRequests(4)
  expect(server.requests()[3]!.operation).toEqual({ kind: "set-usage-retention", target: "codex", detailedRetentionDays: 14 })
  server.send({
    type: "response",
    requestId: server.requests()[3]!.requestId,
    result: {
      kind: "usage-retention-outcome",
      outcome: { target: "codex", detailedRetentionDays: 14, rolledUpDays: 1, prunedRequestRecords: 1, prunedNativeUsageRecords: 1 },
    },
  })
  await expect(retaining).resolves.toMatchObject({ detailedRetentionDays: 14 })

  const clearing = session.clearUsage()
  await server.waitForRequests(5)
  server.send({
    type: "response",
    requestId: server.requests()[4]!.requestId,
    result: {
      kind: "usage-clear-outcome",
      outcome: { target: "codex", clearedRequestRecords: 1, clearedNativeUsageRecords: 2, clearedDailyRollups: 3, clearedImportCursors: 4 },
    },
  })
  await expect(clearing).resolves.toMatchObject({ clearedImportCursors: 4 })

  const updating = session.updatePricingCatalog()
  await server.waitForRequests(6)
  server.send({
    type: "response",
    requestId: server.requests()[5]!.requestId,
    result: {
      kind: "pricing-catalog-update-outcome",
      outcome: { target: "codex", catalogVersion: "models.dev-sha256:abc", source: "models.dev", backfilledRequestRecords: 1, backfilledNativeUsageRecords: 2 },
    },
  })
  await expect(updating).resolves.toMatchObject({ backfilledNativeUsageRecords: 2 })

  await session.close()
  await server.close()
})

test("RpcClient prepares one closed compatible handover request", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")

  const preparing = client.prepareHandover(
    "/opt/muxvia/muxvia-routing-next",
    "routing-next",
  )
  await server.waitForRequests(1)
  expect(server.requests()[0]!.operation).toEqual({
    kind: "prepare-handover",
    candidatePath: "/opt/muxvia/muxvia-routing-next",
    expectedRelease: "routing-next",
  })
  server.replyHandoverPrepared(0, "routing-next")
  await expect(preparing).resolves.toEqual({ release: "routing-next" })

  await client.close()
  await server.close()
})

function assertCapturedCreateProvider(
  frame: Extract<ClientFrame, { type: "request" }> | undefined,
  expected: {
    expectedRevision: number
    name: string
    baseUrl: string
    model: string
    credential: string
  },
): void {
  const operation = frame?.operation
  if (operation?.kind !== "act") {
    throw new Error("Queued ordinary action did not match captured input")
  }
  const action = operation.action as TargetAction
  if (action.kind !== "create-provider") {
    throw new Error("Queued ordinary action did not match captured input")
  }
  const matches = operation.expectedRevision === expected.expectedRevision
    && action.name === expected.name
    && action.baseUrl === expected.baseUrl
    && action.model === expected.model
    && action.credential.kind === "replace"
    && action.credential.value === expected.credential
    && action.presetKey === null
  if (!matches) throw new Error("Queued ordinary action did not match captured input")
}

function ordinaryActionTypeExcludesCompatibilityResolution(
  session: Awaited<ReturnType<typeof openScriptedSession>>["session"],
): void {
  // @ts-expect-error Compatibility Resolve is available only through resolveCompatibility.
  void session.act({ kind: "resolve-compatibility", version: "codex-forged-type" })
}

void ordinaryActionTypeExcludesCompatibilityResolution

test("a Claude session captures its target and preflight context for gap refresh", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const claudeContext = {
    claudeConfigDir: null,
    selectorState: "disabled" as const,
    hostManagedState: "unmanaged" as const,
    cwd: "/tmp/project",
  }
  const opening = client.openTarget("claude", claudeContext)
  await server.waitForRequests(1)
  server.replyOpen(0, viewAtRevision(0, 0, "claude"))
  const session = await opening
  const action = session.act({
    kind: "create-provider", name: "Claude", baseUrl: "https://api.anthropic.com/v1", model: "claude-test",
    credential: { kind: "replace", value: "not-serialized" }, presetKey: "anthropic-api-messages",
  })
  await server.waitForRequests(2)
  expect(server.requests().map((request) =>
    "target" in request.operation ? request.operation.target : null)).toEqual(["claude", "claude"])
  server.replyApplied(viewAtRevision(1, 1, "claude"))
  await action
  server.push(viewAtRevision(3, 3, "claude"))
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toEqual({
    kind: "open-target",
    target: "claude",
    claudeContext,
  })
  server.replyOpen(2, viewAtRevision(3, 3, "claude"))
  await session.close()
  await server.close()
})

test("a target session serializes actions and replaces stale state", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(0))
  const ordering: string[] = []
  session.subscribe(() => ordering.push("push"))
  const first = session.act({
    kind: "create-provider",
    name: "P",
    baseUrl: "https://p.test/v1",
    model: "m",
    credential: { kind: "replace", value: "s" },
    presetKey: null,
  }).then((outcome) => {
    ordering.push("resolved")
    return outcome
  })
  const second = session.act({ kind: "activate-provider", providerId: "p", mode: "takeover" })
  await server.waitForRequests(2)
  expect(server.receivedActionCount()).toBe(1)
  server.replyApplied(viewAtRevision(1))
  await first
  await Bun.sleep(10)
  expect(ordering).toEqual(["resolved", "push"])
  await server.waitForRequests(3)
  expect(server.receivedActionCount()).toBe(2)
  const actions = server.requests().filter((request) => request.operation.kind === "act")
  expect(actions[0]!.operation).toMatchObject({ expectedRevision: 0 })
  expect(actions[1]!.operation).toMatchObject({ expectedRevision: 1 })
  expect((actions[0]!.operation as { actionId: string }).actionId).not.toBe(
    (actions[1]!.operation as { actionId: string }).actionId,
  )
  server.replyStale(viewAtRevision(2))
  await expect(second).rejects.toMatchObject({ code: "stale-revision", retryable: true })
  expect(session.get().managementRevision).toBe(2)
  expect(server.receivedActionCount()).toBe(2)

  await session.close()
  await server.close()
})

test("act captures an ordinary action before its discriminator is mutated while queued", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const first = session.act({
    kind: "activate-provider",
    providerId: "provider-1",
    mode: "direct",
  })
  const mutable = {
    kind: "create-provider",
    name: "Captured Provider",
    baseUrl: "https://captured.example/v1",
    model: "captured-model",
    credential: { kind: "replace", value: "captured-credential" },
    presetKey: null,
  } satisfies OrdinaryTargetAction
  const queued = session.act(mutable)
  Object.assign(
    mutable as unknown as { kind: string; version?: string },
    { kind: "resolve-compatibility", version: "forged-after-call" },
  )

  await server.waitForRequests(2)
  server.replyApplied(viewAtRevision(2, 2))
  await first
  await server.waitForRequests(3)
  assertCapturedCreateProvider(server.requests()[2], {
    expectedRevision: 2,
    name: "Captured Provider",
    baseUrl: "https://captured.example/v1",
    model: "captured-model",
    credential: "captured-credential",
  })
  server.replyApplied(viewAtRevision(3, 3))
  await queued
  await session.close()
  await server.close()
})

test("act deeply captures nested ordinary action fields before queued execution", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const first = session.act({
    kind: "activate-provider",
    providerId: "provider-1",
    mode: "direct",
  })
  const mutable = {
    kind: "create-provider",
    name: "Nested Provider",
    baseUrl: "https://nested.example/v1",
    model: "nested-model",
    credential: { kind: "replace", value: "original-credential" },
    presetKey: null,
  } satisfies OrdinaryTargetAction
  const queued = session.act(mutable)
  mutable.credential.value = "mutated-credential"

  await server.waitForRequests(2)
  server.replyApplied(viewAtRevision(2, 2))
  await first
  await server.waitForRequests(3)
  assertCapturedCreateProvider(server.requests()[2], {
    expectedRevision: 2,
    name: "Nested Provider",
    baseUrl: "https://nested.example/v1",
    model: "nested-model",
    credential: "original-credential",
  })
  server.replyApplied(viewAtRevision(3, 3))
  await queued
  await session.close()
  await server.close()
})

test("failover draft and Apply actions are deeply captured at the TargetSession boundary", async () => {
  const initial = viewAtRevision(1)
  const { session, server } = await openScriptedSession(initial)
  const first = session.act({ kind: "activate-provider", providerId: "provider-1", mode: "direct" })
  const members = [{
    providerId: "00000000-0000-4000-8000-000000000061",
    providerRevision: 4,
  }]
  const save = session.act({ kind: "save-failover-draft", members })
  members[0]!.providerId = "00000000-0000-4000-8000-000000000099"
  members[0]!.providerRevision = 99

  await server.waitForRequests(2)
  server.replyApplied(viewAtRevision(2, 2))
  await first
  await server.waitForRequests(3)
  const saveOperation = server.requests()[2]!.operation
  expect(saveOperation).toMatchObject({
    kind: "act",
    expectedRevision: 2,
    action: {
      kind: "save-failover-draft",
      members: [{
        providerId: "00000000-0000-4000-8000-000000000061",
        providerRevision: 4,
      }],
    },
  })
  server.replyApplied(viewAtRevision(3, 3))
  await save

  const mutableApply = { kind: "apply-failover-chain", draftRevision: 7 } satisfies OrdinaryTargetAction
  const apply = session.act(mutableApply)
  mutableApply.draftRevision = 70
  await server.waitForRequests(4)
  expect(server.requests()[3]!.operation).toMatchObject({
    kind: "act",
    expectedRevision: 3,
    action: { kind: "apply-failover-chain", draftRevision: 7 },
  })
  server.replyApplied(viewAtRevision(4, 4))
  await apply
  await session.close()
  await server.close()
})

test("a sequence gap issues one refresh and only complete increasing views notify", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1, 1))
  const received: number[] = []
  const unsubscribe = session.subscribe((view) => received.push(view.viewSequence))

  server.push(viewAtRevision(1, 1))
  server.push(viewAtRevision(2, 0))
  server.push(viewAtRevision(4, 4))
  server.push(viewAtRevision(5, 5))
  await server.waitForRequests(2)
  expect(server.requests().filter((request) => request.operation.kind === "open-target")).toHaveLength(2)
  expect(received).toEqual([])

  server.replyOpen(1, viewAtRevision(5, 5))
  await Bun.sleep(10)
  expect(session.get().viewSequence).toBe(5)
  expect(received).toEqual([5])

  server.push(viewAtRevision(6, 6))
  await Bun.sleep(10)
  expect(received).toEqual([5, 6])
  unsubscribe()
  server.push(viewAtRevision(7, 7))
  await Bun.sleep(10)
  expect(received).toEqual([5, 6])

  await session.close()
  await server.close()
})

test("close is idempotent, removes subscriptions, and closes the socket", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(0))
  let notifications = 0
  session.subscribe(() => notifications++)

  await session.close()
  await session.close()
  server.push(viewAtRevision(1))
  await Bun.sleep(10)
  expect(notifications).toBe(0)
  await expect(session.act({
    kind: "create-provider", name: "P", baseUrl: "https://p.test/v1", model: "m", credential: { kind: "replace", value: "s" }, presetKey: null,
  })).rejects.toMatchObject({ code: "connection-closed" })

  await server.close()
})

test("aborted discovery sends one cancel, ignores a late result, and preserves a newer push", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1, 1))
  const controller = new AbortController()
  const discovery = session.discoverModels({
    kind: "draft",
    baseUrl: "https://draft.example/v1",
    authentication: "openai-bearer",
    credentialSource: { kind: "ephemeral", value: "ephemeral-test-value" },
  }, controller.signal)
  await server.waitForRequests(2)
  const discoveryRequest = server.requests()[1]!

  server.push(viewAtRevision(2, 2))
  await Bun.sleep(10)
  controller.abort()
  await expect(discovery).rejects.toMatchObject({ code: "cancelled" })
  await server.waitForFrames(4)
  expect(server.cancels()).toEqual([{ type: "cancel", requestId: discoveryRequest.requestId }])

  server.replyDiscovery(1, ["late-model"])
  await Bun.sleep(10)
  expect(session.get()).toEqual(viewAtRevision(2, 2))
  expect(server.cancels()).toHaveLength(1)

  await session.close()
  await server.close()
})

test("explicit refresh accepts unsaved Blank and Preset drafts without Provider identity", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(0))
  const blank = session.discoverModels({
    kind: "draft",
    baseUrl: "",
    authentication: "openai-bearer",
    credentialSource: { kind: "missing" },
  })
  const preset = session.discoverModels({
    kind: "draft",
    baseUrl: "https://api.openai.com/v1",
    authentication: "openai-bearer",
    credentialSource: { kind: "ephemeral", value: "preset-test-value" },
  })
  await server.waitForRequests(3)

  const inspections = server.requests().slice(1)
  expect(inspections.map((request) => request.operation)).toEqual([
    {
      kind: "discover-models",
      target: "codex",
      source: {
        kind: "draft",
        baseUrl: "",
        authentication: "openai-bearer",
        credentialSource: { kind: "missing" },
      },
    },
    {
      kind: "discover-models",
      target: "codex",
      source: {
        kind: "draft",
        baseUrl: "https://api.openai.com/v1",
        authentication: "openai-bearer",
        credentialSource: { kind: "ephemeral", value: "preset-test-value" },
      },
    },
  ])
  expect(JSON.stringify(inspections.map((request) => request.operation))).not.toContain("providerId")

  server.replyDiscovery(1, [])
  server.replyDiscovery(2, ["model-a"])
  expect((await blank).status).toBe("success")
  expect((await preset).status).toBe("success")
  expect(session.get()).toEqual(viewAtRevision(0))

  await session.close()
  await server.close()
})

test("reconciliation preview captures target and Claude context without waiting for queued actions", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const claudeContext: ClaudePreflightContext = {
    claudeConfigDir: "/tmp/claude-home",
    selectorState: "disabled" as const,
    hostManagedState: "unmanaged" as const,
    cwd: "/tmp/project",
  }
  const capturedContext = structuredClone(claudeContext)
  const opening = client.openTarget("claude", claudeContext)
  await server.waitForRequests(1)
  server.replyOpen(0, viewAtRevision(1, 1, "claude"))
  const session = await opening
  claudeContext.claudeConfigDir = "/tmp/mutated-claude-home"
  claudeContext.cwd = "/tmp/mutated-project"

  const action = session.act({ kind: "activate-provider", providerId: "provider-1", mode: "direct" })
  const preview = session.previewReconciliation("reapply")
  await server.waitForRequests(3)
  const previewIndex = server.requests().findIndex(
    (request) => request.operation.kind === "preview-reconciliation",
  )
  expect(server.requests()[previewIndex]!.operation).toEqual({
    kind: "preview-reconciliation",
    target: "claude",
    strategy: "reapply",
    claudeContext: capturedContext,
  })

  const expected = reconciliationPreview("claude")
  server.replyPreview(previewIndex, expected)
  expect(await preview).toEqual(expected)
  server.replyApplied(viewAtRevision(2, 2, "claude"))
  await action
  await session.close()
  await server.close()
})

test("compatibility probe and resolution use the public target-scoped workflow", async () => {
  const initial = viewAtRevision(1)
  initial.problems = [{
    code: "compatibility-acknowledgement-required",
    message: "Acknowledge the exact version",
  }]
  const { session, server } = await openScriptedSession(initial)

  const inspecting = session.probeCompatibility()
  await server.waitForRequests(2)
  expect(server.requests()[1]!.operation).toEqual({
    kind: "probe-compatibility",
    target: "codex",
  })
  const probe: CompatibilityProbe = {
    target: "codex",
    managementRevision: 1,
    compatibility: {
      version: "codex-unknown-8.1",
      classification: "unknown-compatible",
      acknowledgementRequired: true,
    },
  }
  server.replyCompatibilityProbe(1, probe)
  const capturedProbe = await inspecting
  expect(capturedProbe).toEqual(probe)

  server.push(viewAtRevision(2, 2))
  await Bun.sleep(10)
  expect(session.get().managementRevision).toBe(2)

  const resolving = session.resolveCompatibility({
    version: "codex-unknown-8.1",
    managementRevision: capturedProbe.managementRevision,
  })
  await server.waitForRequests(3)
  server.replyStale(viewAtRevision(2, 2))
  await expect(resolving).rejects.toMatchObject({ code: "stale-revision" })

  const freshInspection = session.probeCompatibility()
  await server.waitForRequests(4)
  const freshProbe = { ...probe, managementRevision: 2 }
  server.replyCompatibilityProbe(3, freshProbe)
  const fresh = await freshInspection
  const succeeding = session.resolveCompatibility({
    version: fresh.compatibility.version,
    managementRevision: fresh.managementRevision,
  })
  await server.waitForRequests(5)
  const acknowledged = viewAtRevision(3, 3)
  server.replyApplied(acknowledged)
  expect((await succeeding).view).toEqual(acknowledged)
  expect(session.get()).toEqual(acknowledged)

  const operations = server.requests().filter((request) => request.operation.kind === "act")
  expect(operations.map((request) => request.operation)).toMatchObject([
    {
      kind: "act",
      target: "codex",
      expectedRevision: 1,
      action: { kind: "resolve-compatibility", version: "codex-unknown-8.1" },
    },
    {
      kind: "act",
      target: "codex",
      expectedRevision: 2,
      action: { kind: "resolve-compatibility", version: "codex-unknown-8.1" },
    },
  ])

  await session.close()
  await server.close()
})

test("compatibility Probe rejects a mismatched response with the canonical fixed diagnostic", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const pending = session.probeCompatibility()
  await server.waitForRequests(2)
  server.replyPreview(1, reconciliationPreview())
  await expect(pending).rejects.toMatchObject({
    code: "invalid-response",
    message: "Compatibility probe response did not match request",
  })
  await session.close()
  await server.close()
})

test("generic act rejects a forged compatibility resolution before writing a request", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  try {
    const rejected = session
      .act({ kind: "resolve-compatibility", version: "codex-forged-runtime" } as never)
      .then(
        () => ({ code: "unexpected-success", message: "unexpected success" }),
        (error: unknown) => error,
      )
    const result = await Promise.race([
      rejected,
      new Promise<{ code: string; message: string }>((resolve) => {
        setImmediate(() => resolve({ code: "timeout", message: "request did not reject locally" }))
      }),
    ])
    expect(server.requests()).toHaveLength(1)
    expect(result).toMatchObject({
      code: "unsupported-operation",
      message: "Compatibility resolution requires resolveCompatibility",
    })
    const ordinary = session.act({
      kind: "activate-provider",
      providerId: "provider-1",
      mode: "direct",
    })
    await server.waitForRequests(2)
    expect(server.requests()[1]!.operation).toMatchObject({
      kind: "act",
      expectedRevision: 1,
      action: { kind: "activate-provider", providerId: "provider-1", mode: "direct" },
    })
    server.replyApplied(viewAtRevision(2, 2))
    await expect(ordinary).resolves.toMatchObject({ status: "applied" })
  } finally {
    await session.close()
    await server.close()
  }
})

test("a queued compatibility resolution retains the Probe revision after an earlier action commits", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const earlier = session.act({ kind: "activate-provider", providerId: "provider-1", mode: "direct" })
  const resolving = session.resolveCompatibility({
    version: "codex-unknown-queued",
    managementRevision: 1,
  })
  await server.waitForRequests(2)
  expect(server.receivedActionCount()).toBe(1)
  server.replyApplied(viewAtRevision(2, 2))
  await earlier
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toMatchObject({
    kind: "act",
    expectedRevision: 1,
    action: { kind: "resolve-compatibility", version: "codex-unknown-queued" },
  })
  server.replyStale(viewAtRevision(2, 2))
  await expect(resolving).rejects.toMatchObject({ code: "stale-revision" })
  await session.close()
  await server.close()
})

test("reconciliation preview supports abort and validates its response kind", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const controller = new AbortController()
  const aborted = session.previewReconciliation("adopt", controller.signal)
  await server.waitForRequests(2)
  const request = server.requests()[1]!
  controller.abort()
  await expect(aborted).rejects.toMatchObject({ code: "cancelled" })
  await server.waitForFrames(4)
  expect(server.cancels()).toContainEqual({ type: "cancel", requestId: request.requestId })

  const invalid = session.previewReconciliation("restore")
  await server.waitForRequests(3)
  server.replyWithTargetView(2, viewAtRevision(2))
  await expect(invalid).rejects.toMatchObject({
    code: "invalid-response",
    message: "Reconciliation preview response did not match request",
  })
  expect(session.get().managementRevision).toBe(1)

  await session.close()
  await server.close()
})

test("reconciliation preview rejects a response for another target with a fixed diagnostic", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const pending = session.previewReconciliation("restore")
  await server.waitForRequests(2)
  const wrongTarget = reconciliationPreview("claude", "restore")
  server.replyPreview(1, wrongTarget)

  await expect(pending).rejects.toMatchObject({
    code: "invalid-response",
    message: "Reconciliation preview response did not match request",
  })
  expect(session.get().managementRevision).toBe(1)
  await session.close()
  await server.close()
})

test("reconciliation preview rejects another strategy with a fixed diagnostic", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const pending = session.previewReconciliation("reapply")
  await server.waitForRequests(2)
  const wrongStrategy = reconciliationPreview("codex", "adopt")
  server.replyPreview(1, wrongStrategy)

  await expect(pending).rejects.toMatchObject({
    code: "invalid-response",
    message: "Reconciliation preview response did not match request",
  })
  expect(session.get().managementRevision).toBe(1)
  await session.close()
  await server.close()
})

test("reconciliation preview is immutable and independent from later authoritative views", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const pending = session.previewReconciliation("reapply")
  await server.waitForRequests(2)
  server.replyPreview(1, reconciliationPreview())
  const preview = await pending

  expect(Object.isFrozen(preview)).toBe(true)
  expect(Object.isFrozen(preview.compatibility)).toBe(true)
  expect(Object.isFrozen(preview.changes)).toBe(true)
  expect(Object.isFrozen(preview.changes[0]!)).toBe(true)
  server.push(viewAtRevision(2, 2))
  await Bun.sleep(10)
  expect(preview.managementRevision).toBe(1)
  expect(preview.changes).toEqual([{ field: "provider", state: "changed" }])

  await session.close()
  await server.close()
})

test("reconciliation apply shares action serialization and replaces an authoritative stale view", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  const first = session.act({ kind: "activate-provider", providerId: "provider-1", mode: "direct" })
  const apply = session.applyReconciliation({
    strategy: "reapply",
    observationToken: "00000000-0000-4000-8000-000000000701",
  })
  await server.waitForRequests(2)
  expect(server.receivedActionCount()).toBe(1)

  server.replyApplied(viewAtRevision(2, 2))
  await first
  await server.waitForRequests(3)
  const actions = server.requests().filter((request) => request.operation.kind === "act")
  expect(actions[1]!.operation).toMatchObject({
    target: "codex",
    expectedRevision: 2,
    action: {
      kind: "reconcile",
      strategy: "reapply",
      observationToken: "00000000-0000-4000-8000-000000000701",
    },
  })
  expect((actions[1]!.operation as { actionId: string }).actionId).not.toBe(
    (actions[0]!.operation as { actionId: string }).actionId,
  )
  server.replyStale(viewAtRevision(3, 3), "stale-reconciliation-preview")
  await expect(apply).rejects.toMatchObject({ code: "stale-reconciliation-preview" })
  expect(session.get().managementRevision).toBe(3)

  await session.close()
  await server.close()
})

test("reconciliation methods reject after close", async () => {
  const { session, server } = await openScriptedSession(viewAtRevision(1))
  await session.close()

  await expect(session.previewReconciliation("restore")).rejects.toMatchObject({ code: "connection-closed" })
  await expect(session.applyReconciliation({
    strategy: "restore",
    observationToken: "00000000-0000-4000-8000-000000000701",
  })).rejects.toMatchObject({ code: "connection-closed" })
  await expect(session.probeCompatibility()).rejects.toMatchObject({ code: "connection-closed" })
  await expect(session.resolveCompatibility({
    version: "codex-unknown-closed",
    managementRevision: 1,
  })).rejects.toMatchObject({ code: "connection-closed" })
  expect(server.requests()).toHaveLength(1)
  await server.close()
})

test("a Claude reconciliation push gap refresh retains its captured context", async () => {
  const { server, path } = await ScriptedServer.start()
  const client = await RpcClient.connect(path, "control-test")
  const claudeContext: ClaudePreflightContext = {
    claudeConfigDir: null,
    selectorState: "disabled" as const,
    hostManagedState: "unmanaged" as const,
    cwd: "/tmp/reconciliation-project",
  }
  const capturedContext = structuredClone(claudeContext)
  const opening = client.openTarget("claude", claudeContext)
  await server.waitForRequests(1)
  server.replyOpen(0, viewAtRevision(1, 1, "claude"))
  const session = await opening
  claudeContext.claudeConfigDir = "/tmp/mutated-gap-home"
  claudeContext.cwd = "/tmp/mutated-gap-project"

  const apply = session.applyReconciliation({
    strategy: "adopt",
    observationToken: "00000000-0000-4000-8000-000000000701",
    acknowledgeVersion: "2.1.0",
  })
  await server.waitForRequests(2)
  expect(server.requests()[1]!.operation).toMatchObject({
    target: "claude",
    expectedRevision: 1,
    action: {
      kind: "reconcile",
      strategy: "adopt",
      observationToken: "00000000-0000-4000-8000-000000000701",
      acknowledgeVersion: "2.1.0",
    },
  })
  server.replyApplied(viewAtRevision(2, 2, "claude"))
  await apply
  server.push(viewAtRevision(4, 4, "claude"))
  await server.waitForRequests(3)
  expect(server.requests()[2]!.operation).toEqual({
    kind: "open-target",
    target: "claude",
    claudeContext: capturedContext,
  })
  server.replyOpen(2, viewAtRevision(4, 4, "claude"))

  await session.close()
  await server.close()
})
