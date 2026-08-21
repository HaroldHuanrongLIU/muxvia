import { expect, test } from "bun:test"
import { readFile } from "node:fs/promises"
import { fileURLToPath } from "node:url"

import { FrameDecoder, encodeFrame } from "../src/control/framing"
import {
  parseClientFrame,
  parseServerFrame,
  parseTargetAction,
  parseTargetView,
  parseSubscriptionAccountAction,
  parseUniversalProviderAction,
} from "../src/control/types"
import type { TargetAction, TargetView } from "../src/control/types"

const fixtures = new URL("../../../protocol/fixtures/", import.meta.url)

async function readFixture(name: string): Promise<unknown> {
  return JSON.parse(await readFile(fileURLToPath(new URL(name, fixtures)), "utf8"))
}

test.each([
  ["hello.json", parseClientFrame],
  ["initial-target-view.json", parseTargetView],
  ["failover-target-view.json", parseTargetView],
  ["save-provider.json", parseTargetAction],
  ["create-subscription-bridge-provider.json", parseTargetAction],
  ["reorder-providers.json", parseTargetAction],
  ["delete-provider.json", parseTargetAction],
  ["duplicate-provider.json", parseTargetAction],
  ["save-failover-draft.json", parseTargetAction],
  ["apply-failover-chain.json", parseTargetAction],
  ["disable-takeover.json", parseTargetAction],
  ["discover-models.json", parseClientFrame],
  ["check-reachability.json", parseClientFrame],
  ["cancel-inspection.json", parseClientFrame],
  ["probe-compatibility.json", parseClientFrame],
  ["list-request-records.json", parseClientFrame],
  ["inspect-request-record.json", parseClientFrame],
  ["open-universal-providers.json", parseClientFrame],
  ["prepare-handover.json", parseClientFrame],
  ["force-stop.json", parseClientFrame],
  ["compatibility-probe.json", parseServerFrame],
  ["request-record-page.json", parseServerFrame],
  ["request-record-detail.json", parseServerFrame],
  ["handover-prepared.json", parseServerFrame],
  ["force-stop-accepted.json", parseServerFrame],
  ["universal-provider-catalog.json", parseServerFrame],
  ["universal-provider-act.json", parseClientFrame],
  ["universal-provider-outcome.json", parseServerFrame],
  ["universal-provider-view.json", parseServerFrame],
  ["preview-reconciliation.json", parseServerFrame],
  ["apply-reconciliation.json", parseTargetAction],
  ["resolve-compatibility.json", parseTargetAction],
  ["create-universal-provider.json", parseUniversalProviderAction],
  ["update-universal-provider.json", parseUniversalProviderAction],
  ["duplicate-universal-provider.json", parseUniversalProviderAction],
  ["delete-universal-provider.json", parseUniversalProviderAction],
  ["synchronize-universal-provider.json", parseUniversalProviderAction],
  ["open-subscription-accounts.json", parseClientFrame],
  ["subscription-account-catalog.json", parseServerFrame],
  ["set-default-subscription-account.json", parseSubscriptionAccountAction],
  ["start-device-authorization.json", parseClientFrame],
  ["device-authorization-challenge.json", parseServerFrame],
  ["poll-device-authorization.json", parseClientFrame],
  ["device-authorization-poll.json", parseServerFrame],
  ["preview-default-subscription-account.json", parseClientFrame],
  ["subscription-default-preview.json", parseServerFrame],
  ["subscription-account-act.json", parseClientFrame],
  ["subscription-account-outcome.json", parseServerFrame],
  ["preview-provider-import.json", parseClientFrame],
  ["preview-cc-switch-sql-import.json", parseClientFrame],
  ["confirm-provider-import.json", parseClientFrame],
  ["confirm-cc-switch-sql-import.json", parseClientFrame],
  ["export-provider-configuration.json", parseClientFrame],
  ["provider-import-preview.json", parseServerFrame],
  ["cc-switch-sql-import-preview.json", parseServerFrame],
  ["provider-import-outcome.json", parseServerFrame],
  ["cc-switch-sql-import-outcome.json", parseServerFrame],
  ["migrated-usage-activity-page.json", parseServerFrame],
  ["provider-configuration-export.json", parseServerFrame],
] as const)("round-trips %s as its protocol type", async (name, parse) => {
  const value = await readFixture(name)
  expect(JSON.parse(JSON.stringify(parse(value)))).toEqual(value)
})

test("Provider Transfer contracts are preview-first, closed, and secret-free", async () => {
  for (const [name, parse, surface] of [
    ["preview-provider-import.json", parseClientFrame, "operation"],
    ["preview-cc-switch-sql-import.json", parseClientFrame, "operation"],
    ["confirm-provider-import.json", parseClientFrame, "operation"],
    ["confirm-cc-switch-sql-import.json", parseClientFrame, "operation"],
    ["export-provider-configuration.json", parseClientFrame, "operation"],
    ["provider-import-preview.json", parseServerFrame, "result"],
    ["cc-switch-sql-import-preview.json", parseServerFrame, "result"],
    ["provider-import-outcome.json", parseServerFrame, "result"],
    ["cc-switch-sql-import-outcome.json", parseServerFrame, "result"],
    ["provider-configuration-export.json", parseServerFrame, "result"],
  ] as const) {
    const value = await readFixture(name) as any
    const invalid = structuredClone(value)
    invalid[surface].additiveSecret = "PROVIDER_TRANSFER_ADDITIVE_SECRET_16001"
    expect(() => parse(invalid)).toThrow()
    if (surface === "result") {
      expect(JSON.stringify(parse(value))).not.toContain("provider-import-secret-must-not-escape")
    }
  }

  const exportFrame = await readFixture("provider-configuration-export.json") as any
  const parsedExport = parseServerFrame(exportFrame) as any
  expect(JSON.stringify(parsedExport)).not.toMatch(
    /token|recovery|activatedSnapshot|activatedRoutePlan/i,
  )
  expect([
    ...parsedExport.result.export.universalProviders,
    ...parsedExport.result.export.targetProviders,
  ].every((declaration) => declaration.credential === "missing")).toBeTrue()
  const secretBearing = structuredClone(exportFrame)
  secretBearing.result.export.targetProviders[0].credential = "present"
  expect(() => parseServerFrame(secretBearing)).toThrow()
})

test("Request Record protocol and JSON schema are closed and bounded", async () => {
  const list = await readFixture("list-request-records.json") as any
  const detail = await readFixture("request-record-detail.json") as any
  for (const [value, parse, surface] of [
    [list, parseClientFrame, "operation"],
    [detail, parseServerFrame, "result"],
  ] as const) {
    const invalid = structuredClone(value)
    invalid[surface].credential = "REQUEST_HISTORY_PROTOCOL_SECRET_13101"
    expect(() => parse(invalid)).toThrow()
  }

  const zeroLimit = structuredClone(list)
  zeroLimit.operation.limit = 0
  expect(() => parseClientFrame(zeroLimit)).toThrow()

  const schema = JSON.parse(await readFile(
    fileURLToPath(new URL("../../../protocol/control-v1.schema.json", import.meta.url)),
    "utf8",
  ))
  const branch = (definition: string, discriminator: string) => schema.$defs[definition].oneOf.find(
    (candidate: any) => candidate.properties.kind.const === discriminator,
  )
  for (const [definition, discriminator] of [
    ["controlOperation", "list-request-records"],
    ["controlOperation", "inspect-request-record"],
    ["controlResult", "request-record-page"],
    ["controlResult", "request-record-detail"],
  ] as const) {
    expect(branch(definition, discriminator)?.additionalProperties).toBeFalse()
  }
  expect(schema.$defs.requestRecordOutcome.enum).toEqual([
    "success", "upstream-error", "semantic-error", "transport-error",
    "route-unavailable", "cancelled", "stream-error",
  ])
  for (const definition of [
    "requestUsage", "requestRecordSummary", "pricingSnapshot",
    "requestRecordPage", "requestRecordDetail",
  ]) {
    expect(schema.$defs[definition].additionalProperties).toBeFalse()
  }
})

test("Subscription Bridge Target Provider contract is closed and credentialless", async () => {
  const action = await readFixture("create-subscription-bridge-provider.json") as any
  expect(parseTargetAction(action)).toEqual(action)

  const additive = structuredClone(action)
  additive.accessToken = "SUBSCRIPTION_BRIDGE_PROTOCOL_SECRET_12802"
  expect(JSON.stringify(parseTargetAction(additive))).not.toContain("SUBSCRIPTION_BRIDGE_PROTOCOL_SECRET_12802")

  const schema = JSON.parse(await readFile(
    fileURLToPath(new URL("../../../protocol/control-v1.schema.json", import.meta.url)),
    "utf8",
  ))
  const branch = schema.$defs.targetAction.oneOf.find(
    (candidate: any) => candidate.properties?.kind?.const === "create-provider",
  )
  expect(branch.properties.authentication.enum).toContain("codex-subscription")
  expect(branch.properties.presetKey.oneOf).toContainEqual({ const: "codex-subscription-bridge" })
  expect(schema.$defs.activatedSnapshot.properties.authentication.enum).toContain("codex-subscription")
  expect(schema.$defs.providerPreset.oneOf).toEqual([
    {
      type: "object",
      properties: {
        key: { const: "openai-api-responses" },
        baseUrl: { const: "https://api.openai.com/v1" },
        model: { const: "" },
        protocol: { const: "openai-responses" },
        authentication: { const: "openai-bearer" },
      },
      required: ["key", "baseUrl", "model", "protocol", "authentication"],
    },
    {
      type: "object",
      properties: {
        key: { const: "anthropic-api-messages" },
        baseUrl: { const: "https://api.anthropic.com/v1" },
        model: { const: "" },
        protocol: { const: "anthropic-messages" },
        authentication: { const: "anthropic-api-key" },
      },
      required: ["key", "baseUrl", "model", "protocol", "authentication"],
    },
    {
      type: "object",
      properties: {
        key: { const: "codex-subscription-bridge" },
        baseUrl: { const: "https://chatgpt.com/backend-api/codex" },
        model: { const: "" },
        protocol: { const: "anthropic-messages" },
        authentication: { const: "codex-subscription" },
      },
      required: ["key", "baseUrl", "model", "protocol", "authentication"],
    },
  ])
})

test("Subscription Account contracts reject secret-bearing additive fields", async () => {
  const action = await readFixture("set-default-subscription-account.json") as any
  action.refreshToken = "SUBSCRIPTION_PROTOCOL_SECRET_11701"
  expect(() => parseSubscriptionAccountAction(action)).toThrow()

  const catalog = await readFixture("subscription-account-catalog.json") as any
  catalog.result.view.accounts[0].accessToken = "SUBSCRIPTION_PROTOCOL_SECRET_11702"
  expect(() => parseServerFrame(catalog)).toThrow()
})

test("Failover Chain actions are closed and revision bound", async () => {
  const save = await readFixture("save-failover-draft.json") as any
  const apply = await readFixture("apply-failover-chain.json") as any
  expect(parseTargetAction(save)).toEqual(save)
  expect(parseTargetAction(apply)).toEqual(apply)

  for (const value of [save, apply]) {
    const invalid = structuredClone(value)
    invalid.additiveSecret = "FAILOVER_PROTOCOL_SECRET_86421"
    expect(() => parseTargetAction(invalid)).toThrow()
  }
  const invalidMember = structuredClone(save)
  invalidMember.members[0].providerRevision = 0
  expect(() => parseTargetAction(invalidMember)).toThrow()
  const invalidDraft = structuredClone(apply)
  invalidDraft.draftRevision = 0
  expect(() => parseTargetAction(invalidDraft)).toThrow()

  const schema = JSON.parse(await readFile(
    fileURLToPath(new URL("../../../protocol/control-v1.schema.json", import.meta.url)),
    "utf8",
  ))
  for (const discriminator of ["save-failover-draft", "apply-failover-chain"]) {
    const branch = schema.$defs.targetAction.oneOf.find(
      (candidate: any) => candidate.properties?.kind?.const === discriminator,
    )
    expect(branch).toBeDefined()
    expect(branch.additionalProperties).toBeFalse()
  }
})

test("lifecycle contracts are closed and secret-free", async () => {
  const disabled = await readFixture("disable-takeover.json") as any
  const prepared = await readFixture("prepare-handover.json") as any
  const accepted = await readFixture("handover-prepared.json") as any
  const forced = await readFixture("force-stop.json") as any
  const forceAccepted = await readFixture("force-stop-accepted.json") as any

  expect(parseTargetAction(disabled)).toEqual(disabled)
  expect(parseClientFrame(prepared)).toEqual(prepared)
  expect(parseServerFrame(accepted)).toEqual(accepted)
  expect(parseClientFrame(forced)).toEqual(forced)
  expect(parseServerFrame(forceAccepted)).toEqual(forceAccepted)
  expect(() => parseClientFrame({
    ...forced,
    operation: { ...forced.operation, acknowledgement: "yes" },
  })).toThrow()
  expect(() => parseServerFrame({
    ...forceAccepted,
    result: { ...forceAccepted.result, warning: "yes" },
  })).toThrow()

  for (const [value, branch, parse] of [
    [disabled, disabled, parseTargetAction],
    [prepared, prepared.operation, parseClientFrame],
    [accepted, accepted.result, parseServerFrame],
    [forced, forced.operation, parseClientFrame],
    [forceAccepted, forceAccepted.result, parseServerFrame],
  ] as const) {
    const invalid = structuredClone(value)
    const invalidBranch = branch === disabled
      ? invalid
      : "operation" in invalid
        ? invalid.operation
        : invalid.result
    invalidBranch.additiveSecret = "LIFECYCLE_PROTOCOL_SECRET_40391"
    expect(() => parse(invalid)).toThrow()
  }
})

test("Failover Chain view schema is closed and complete", async () => {
  const schema = JSON.parse(await readFile(
    fileURLToPath(new URL("../../../protocol/control-v1.schema.json", import.meta.url)),
    "utf8",
  ))
  expect(schema.$defs.targetView.properties.failover.$ref).toBe("#/$defs/failoverView")
  expect(schema.$defs.targetView.required).toContain("failover")
  expect(schema.$defs.provider.properties.routeHealth.$ref).toBe("#/$defs/routeHealth")
  expect(schema.$defs.provider.required).toContain("routeHealth")
  expect(schema.$defs.routeHealth.properties.state.enum).toEqual([
    "unobserved", "healthy", "degraded", "unavailable", "stale",
  ])
  expect(schema.$defs.failoverView.additionalProperties).toBeFalse()
  expect(schema.$defs.activatedRoutePlan.additionalProperties).toBeFalse()

  const missingFailover = await readFixture("failover-target-view.json") as any
  delete missingFailover.failover
  expect(() => parseTargetView(missingFailover)).toThrow()

  const missingProviderHealth = await readFixture("failover-target-view.json") as any
  delete missingProviderHealth.providers[0].routeHealth
  expect(() => parseTargetView(missingProviderHealth)).toThrow()
})

test("Universal Provider JSON schema exposes the complete closed catalog contract", async () => {
  const schema = JSON.parse(await readFile(
    fileURLToPath(new URL("../../../protocol/control-v1.schema.json", import.meta.url)),
    "utf8",
  ))
  const hasBranch = (definition: string, discriminator: string) =>
    schema.$defs[definition]?.oneOf?.some((branch: any) =>
      branch.properties?.kind?.const === discriminator || branch.properties?.type?.const === discriminator)

  expect(hasBranch("controlOperation", "open-universal-providers")).toBeTrue()
  expect(hasBranch("controlOperation", "universal-provider-act")).toBeTrue()
  expect(hasBranch("controlResult", "universal-provider-catalog")).toBeTrue()
  expect(hasBranch("controlResult", "universal-provider-outcome")).toBeTrue()
  expect(hasBranch("serverFrame", "universal-provider-view")).toBeTrue()
  expect(schema.$defs.universalProviderAction).toBeDefined()
  expect(schema.$defs.universalProviderCatalog).toBeDefined()
  expect(schema.oneOf).toContainEqual({ $ref: "#/$defs/universalProviderAction" })
  expect(schema.$defs.universalProviderPresetTarget.properties.authentication.enum)
    .not.toContain("codex-subscription")

  const action = await readFixture("create-universal-provider.json") as Record<string, unknown>
  action.additiveSecret = "UNIVERSAL_ADDITIVE_SECRET_99310"
  expect(() => parseUniversalProviderAction(action)).toThrow()

  const invalidPreset = await readFixture("create-universal-provider.json") as Record<string, unknown>
  invalidPreset.presetKey = "unstable-user-defined-preset"
  expect(() => parseUniversalProviderAction(invalidPreset)).toThrow()
})

// Catches a parser mutation that accepts arbitrary reconciliation values or lets
// additive secret-bearing fields survive its typed public projection.
test("reconciliation contracts are closed, validated, and secret-free", async () => {
  const preview = await readFixture("preview-reconciliation.json") as {
    result: {
      preview: { changes: Array<Record<string, unknown>>; providerCredential?: string }
    }
  }
  const result = preview.result.preview
  result.providerCredential = "preview-secret-must-not-escape"
  result.changes[0]!.rawConfiguration = "preview-secret-must-not-escape"

  const parsed = parseServerFrame(preview)
  const serialized = JSON.stringify(parsed)
  expect(serialized).not.toContain("preview-secret-must-not-escape")
  expect(JSON.parse(serialized)).toEqual(await readFixture("preview-reconciliation.json"))

  const validPreview = await readFixture("preview-reconciliation.json") as any
  const previewMutations: Array<(value: any) => void> = [
    (value) => { value.result.preview.observationToken = "not-a-uuid" },
    (value) => { value.result.preview.managementRevision = 0 },
    (value) => { value.result.preview.compatibility.classification = "arbitrary" },
    (value) => { value.result.preview.shadowSources[0] = "arbitrary-source" },
    (value) => { value.result.preview.changes[0].field = "arbitrary-field" },
    (value) => { value.result.preview.changes[0].state = "arbitrary-state" },
  ]
  for (const mutate of previewMutations) {
    const invalid = structuredClone(validPreview)
    mutate(invalid)
    expect(() => parseServerFrame(invalid)).toThrow()
  }
  expect(() => parseClientFrame({
    type: "request", requestId: "preview", operation: {
      kind: "preview-reconciliation", target: "codex", strategy: "automatic",
    },
  })).toThrow()
  expect(() => parseTargetAction({
    kind: "reconcile", strategy: "restore", observationToken: "not-a-uuid",
  })).toThrow()
  expect(() => parseTargetAction({
    kind: "reconcile", strategy: "automatic", observationToken: "00000000-0000-4000-8000-000000000701",
  })).toThrow()
})

test("compatibility Probe and Resolve contracts are exact closed and secret-free", async () => {
  const request = await readFixture("probe-compatibility.json") as any
  const response = await readFixture("compatibility-probe.json") as any
  const resolution = await readFixture("resolve-compatibility.json") as any
  for (const [value, mutate, parse] of [
    [request, (copy: any) => { copy.operation.additiveSecret = "COMPATIBILITY_PROTOCOL_SECRET_98711" }, parseClientFrame],
    [response, (copy: any) => { copy.result.additiveSecret = "COMPATIBILITY_PROTOCOL_SECRET_98711" }, parseServerFrame],
    [response, (copy: any) => { copy.result.probe.additiveSecret = "COMPATIBILITY_PROTOCOL_SECRET_98711" }, parseServerFrame],
    [resolution, (copy: any) => { copy.additiveSecret = "COMPATIBILITY_PROTOCOL_SECRET_98711" }, parseTargetAction],
  ] as const) {
    const invalid = structuredClone(value)
    mutate(invalid)
    expect(() => parse(invalid)).toThrow()
  }
  expect(() => parseClientFrame({
    type: "request", requestId: "legacy", operation: { kind: "preview-compatibility", target: "codex" },
  })).toThrow()
  expect(() => parseTargetAction({ kind: "acknowledge-compatibility", version: "0.42.0" })).toThrow()

  const schema = JSON.parse(await readFile(fileURLToPath(new URL("../../../protocol/control-v1.schema.json", import.meta.url)), "utf8"))
  const branch = (definition: string, discriminator: string) => schema.$defs[definition].oneOf.find(
    (candidate: any) => candidate.properties.kind.const === discriminator,
  )
  expect(branch("controlOperation", "probe-compatibility")?.additionalProperties).toBeFalse()
  expect(branch("controlResult", "compatibility-probe")?.additionalProperties).toBeFalse()
  expect(branch("targetAction", "resolve-compatibility")?.additionalProperties).toBeFalse()
  expect(schema.$defs.controlProblem.properties.source.enum).toEqual([
    "control-plane-context", "user-settings", "managed-settings",
    "shared-project-settings", "local-project-settings", "codex-profile",
    "claude-managed", "claude-shared", "claude-project", "claude-local",
    "claude-selector", "claude-host-managed",
  ])
})

test("round-trips target-scoped reconciliation preview requests with Claude context", () => {
  const frame = {
    type: "request",
    requestId: "reconciliation-preview",
    operation: {
      kind: "preview-reconciliation",
      target: "claude",
      strategy: "adopt",
      claudeContext: {
        claudeConfigDir: "/tmp/claude-home",
        selectorState: "disabled",
        hostManagedState: "unmanaged",
        cwd: "/tmp/project",
      },
    },
  } as const
  expect(parseClientFrame(frame)).toEqual(frame)
})

test("round-trips draft discovery sources and view-free inspection results", () => {
  const sources = [
    { kind: "missing" },
    { kind: "ephemeral", value: "test-ephemeral-value" },
    {
      kind: "saved",
      providerId: "00000000-0000-4000-8000-000000000101",
      providerRevision: 7,
    },
  ] as const
  for (const credentialSource of sources) {
    const frame = {
      type: "request",
      requestId: "discover-draft",
      operation: {
        kind: "discover-models",
        target: "codex",
        source: {
          kind: "draft",
          baseUrl: "https://draft.example/v1",
          authentication: "openai-bearer",
          credentialSource,
        },
      },
    } as const
    expect(parseClientFrame(frame)).toEqual(frame)
  }

  const frames: unknown[] = [
    {
      type: "response",
      requestId: "discover-draft",
      result: {
        kind: "model-discovery",
        result: {
          status: "success",
          models: [{ id: "model-a", displayName: "Owner A" }],
          attempts: 1,
          elapsedMs: 4,
          endpointOrigin: "https://provider.example",
        },
      },
    },
    {
      type: "response",
      requestId: "reachability",
      result: {
        kind: "reachability",
        result: {
          status: "reachable",
          httpStatus: 503,
          ttfbMs: 12,
          checkedAtUnixMs: 1_775_000_000_000,
          retryCount: 0,
          slow: false,
          endpointOrigin: "https://provider.example",
        },
      },
    },
  ]
  for (const frame of frames) {
    const parsed = parseServerFrame(frame)
    expect(parsed as unknown).toEqual(frame)
    expect(JSON.stringify(parsed)).not.toContain("TargetView")
    expect((parsed as { result: Record<string, unknown> }).result).not.toHaveProperty("view")
  }
})

test.each([600, 999])("accepts Reachability HTTP status %d", (httpStatus) => {
  const frame = {
    type: "response",
    requestId: `reachability-${httpStatus}`,
    result: {
      kind: "reachability",
      result: {
        status: "reachable",
        httpStatus,
        ttfbMs: 12,
        checkedAtUnixMs: 1_775_000_000_000,
        retryCount: 0,
        slow: false,
        endpointOrigin: "https://provider.example",
      },
    },
  } as const
  expect(parseServerFrame(frame)).toEqual(frame)
})

test("decodes a frame split across arbitrary chunks", () => {
  const frame = encodeFrame({ type: "hello", rpc: { major: 1, minor: 0 }, release: "test" })
  const decoder = new FrameDecoder()
  expect(decoder.push(frame.subarray(0, 2))).toEqual([])
  expect(decoder.push(frame.subarray(2, 7))).toEqual([])
  expect(decoder.push(frame.subarray(7))).toEqual([
    { type: "hello", rpc: { major: 1, minor: 0 }, release: "test" },
  ])
})

test("rejects the advertised length before allocating an oversized body", () => {
  const decoder = new FrameDecoder()
  const prefix = new Uint8Array([0, 16, 0, 1])
  expect(() => decoder.push(prefix)).toThrow("frame-too-large")
})

test("ignores unknown additive fields in an action envelope", () => {
  expect(parseClientFrame({
    type: "request",
    requestId: "request-1",
    operation: {
      kind: "act",
      target: "codex",
      actionId: "00000000-0000-4000-8000-000000000005",
      expectedRevision: 0,
      action: { kind: "save-provider" },
      futureField: "ignored",
    },
  })).toEqual({
    type: "request",
    requestId: "request-1",
    operation: {
      kind: "act",
      target: "codex",
      actionId: "00000000-0000-4000-8000-000000000005",
      expectedRevision: 0,
      action: { kind: "save-provider" },
    },
  })
})

test("round-trips schema-v3 Provider declarations, Presets, and credential intent", () => {
  const provider = {
    id: "00000000-0000-4000-8000-000000000101",
    position: 0,
    providerRevision: 1,
    name: "Direct Provider",
    baseUrl: "https://provider.example/v1",
    model: "model-a",
    protocol: "openai-responses",
    authentication: "openai-bearer",
    routingRequirement: "direct-compatible",
    credential: "present",
    completeness: "complete",
    missingFields: [],
    provenance: null,
    importProvenance: {
      sourceProduct: "target-cli",
      sourceTarget: "codex",
      sourceIdentifier: "config.toml:profile.current",
      configurationFingerprint: "a".repeat(64),
    },
    importedCurrent: true,
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
  } satisfies TargetView["providers"][number]
  const view = {
    target: "codex",
    managementRevision: 0,
    viewSequence: 0,
    service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: [provider],
    providerPresets: [{
      key: "openai-api-responses",
      baseUrl: "https://api.openai.com/v1",
      model: "",
      protocol: "openai-responses",
      authentication: "openai-bearer",
    }],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    failover: { draftRevision: 1, draftMembers: [], activePlan: null },
    problems: [],
  } satisfies TargetView
  expect(parseTargetView(view)).toEqual(view)

  expect(() => parseTargetView({
    ...view,
    providers: [{
      ...provider,
      importProvenance: { ...provider.importProvenance, additiveSecret: "must-not-pass" },
    }],
  })).toThrow()
  expect(() => parseTargetView({
    ...view,
    providers: [{
      ...provider,
      importProvenance: {
        sourceProduct: "target-cli",
        sourceTarget: "codex",
        sourceIdentifier: "config.toml:profile.current",
      },
    }],
  })).toThrow()

  expect(parseTargetAction({
    kind: "create-provider",
    name: "Incomplete",
    baseUrl: "",
    model: "",
    credential: { kind: "remove" },
    presetKey: "openai-api-responses",
  })).toEqual({
    kind: "create-provider",
    name: "Incomplete",
    baseUrl: "",
    model: "",
    credential: { kind: "remove" },
    presetKey: "openai-api-responses",
  })

  expect(parseTargetAction({
    kind: "duplicate-provider",
    sourceProviderId: "00000000-0000-4000-8000-000000000101",
    sourceProviderRevision: 1,
    name: "Copied Provider",
    baseUrl: "https://copied.example/v1",
    model: "copied-model",
    credential: { kind: "reuse-source" },
  })).toEqual({
    kind: "duplicate-provider",
    sourceProviderId: "00000000-0000-4000-8000-000000000101",
    sourceProviderRevision: 1,
    name: "Copied Provider",
    baseUrl: "https://copied.example/v1",
    model: "copied-model",
    credential: { kind: "reuse-source" },
  })
})

test("round-trips generated Provider ownership and Target Overlay edits as a closed contract", () => {
  const generated = {
    id: "00000000-0000-4000-8000-000000000108",
    position: 0,
    providerRevision: 3,
    name: "Universal Generated",
    baseUrl: "https://universal.example/v1",
    model: "overlay-model",
    protocol: "openai-responses",
    authentication: "openai-bearer",
    routingRequirement: "takeover-required",
    credential: "present",
    completeness: "complete",
    missingFields: [],
    provenance: { kind: "universal-provider", key: "00000000-0000-4000-8000-000000000801" },
    generated: true,
    universalProviderId: "00000000-0000-4000-8000-000000000801",
    synchronization: "current",
    ownership: {
      name: "universal-provider",
      baseUrl: "universal-provider",
      model: "target-overlay",
      protocol: "target-fixed",
      authentication: "target-overlay",
      routingRequirement: "target-overlay",
      credential: "universal-provider",
    },
    routeHealth: { state: "unobserved" },
    activeReferences: ["activated-route-plan"],
  } satisfies TargetView["providers"][number]
  const view = {
    target: "codex",
    managementRevision: 4,
    viewSequence: 4,
    service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: [generated],
    providerPresets: [],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    failover: { draftRevision: 1, draftMembers: [], activePlan: null },
    problems: [],
  }
  expect(parseTargetView(view).providers[0]).toEqual(generated)

  const overlayEdit = {
    kind: "update-provider",
    providerId: generated.id,
    providerRevision: generated.providerRevision,
    name: generated.name,
    baseUrl: generated.baseUrl,
    model: "overlay-two",
    credential: { kind: "keep" },
    authentication: "openai-bearer",
    routingRequirement: "direct-compatible",
  } satisfies TargetAction
  expect(parseTargetAction(overlayEdit)).toEqual(overlayEdit)

  expect(() => parseTargetView({
    ...view,
    providers: [{ ...generated, ownership: { ...generated.ownership, model: "universal-provider" } }],
  })).not.toThrow()
  expect(() => parseTargetView({
    ...view,
    providers: [{ ...generated, ownership: { ...generated.ownership, model: "future-owner" } }],
  })).toThrow()
})

test("round-trips Claude Messages declarations with explicit authentication and neutral Route Health", () => {
  const view = {
    target: "claude",
    managementRevision: 0,
    viewSequence: 0,
    service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: [{
      id: "00000000-0000-4000-8000-000000000101",
      position: 0,
      providerRevision: 1,
      name: "Anthropic API",
      baseUrl: "https://api.anthropic.com/v1",
      model: "claude-test",
      protocol: "anthropic-messages",
      authentication: "anthropic-api-key",
      routingRequirement: "takeover-required",
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
    providerPresets: [{
      key: "anthropic-api-messages",
      baseUrl: "https://api.anthropic.com/v1",
      model: "",
      protocol: "anthropic-messages",
      authentication: "anthropic-api-key",
    }],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    failover: { draftRevision: 1, draftMembers: [], activePlan: null },
    problems: [],
    futureField: "ignored",
  }

  const parsed = parseTargetView(view)
  expect(parsed.target).toBe("claude")
  expect(parsed.providers[0]?.protocol).toBe("anthropic-messages")
  expect(parsed.providers[0]?.authentication).toBe("anthropic-api-key")
  expect(parsed.routeHealth).toEqual({ state: "unobserved" })
  expect(JSON.parse(JSON.stringify(parsed))).not.toHaveProperty("futureField")
})

test.each(["direct", "takeover"] as const)("accepts activate-provider mode %s", (mode) => {
  const action = {
    kind: "activate-provider",
    providerId: "00000000-0000-4000-8000-000000000101",
    mode,
    futureField: "ignored",
  } as const
  expect(parseTargetAction(action)).toEqual({
    kind: "activate-provider",
    providerId: "00000000-0000-4000-8000-000000000101",
    mode,
  })
})

test("rejects an unknown activate-provider mode", () => {
  expect(() => parseTargetAction({
    kind: "activate-provider",
    providerId: "00000000-0000-4000-8000-000000000101",
    mode: "automatic",
  })).toThrow()
})

test("accepts additive fields while redacting credential replacement intent", () => {
  const parsed = parseTargetAction({
    kind: "create-provider",
    name: "Incomplete",
    baseUrl: "",
    model: "",
    credential: { kind: "replace", value: "credential-sentinel-must-not-escape" },
    routingRequirement: "takeover-required",
    futureField: "ignored",
  })
  expect(parsed).toEqual({
    kind: "create-provider",
    name: "Incomplete",
    baseUrl: "",
    model: "",
    credential: { kind: "replace", value: "credential-sentinel-must-not-escape" },
  })
})

test("rejects an action identifier that is not a UUID", () => {
  expect(() => parseClientFrame({
    type: "request",
    requestId: "request-1",
    operation: {
      kind: "act",
      target: "codex",
      actionId: "not-a-uuid",
      expectedRevision: 0,
      action: { kind: "save-provider" },
    },
  })).toThrow()
})

test("rejects incomplete or arbitrary Claude blocking selector projections", () => {
  for (const claudeContext of [
    { claudeConfigDir: null, selectorState: "enabled", hostManagedState: "unmanaged", cwd: "/safe" },
    { claudeConfigDir: null, selectorState: "unknown-nonempty", hostManagedState: "unmanaged", cwd: "/safe" },
    { claudeConfigDir: null, selectorState: "unset", hostManagedState: "managed", cwd: "/safe" },
    { claudeConfigDir: null, selectorState: "unset", blockingSelector: "CLAUDE_CODE_USE_VERTEX", hostManagedState: "unmanaged", cwd: "/safe" },
  ]) {
    expect(() => parseClientFrame({
      type: "request",
      requestId: "open-claude",
      operation: { kind: "open-target", target: "claude", claudeContext },
    })).toThrow()
  }

  expect(() => parseServerFrame({
    type: "error",
    requestId: "activate",
    problem: {
      code: "provider-mode-active",
      message: "fixed",
      source: "control-plane-context",
      selector: "ARBITRARY_SECRET_BEARING_SELECTOR",
    },
  })).toThrow()
})

test("JSON schema shares the exact closed Claude blocking selector enum", async () => {
  const schema = JSON.parse(await readFile(fileURLToPath(new URL("../../../protocol/control-v1.schema.json", import.meta.url)), "utf8"))
  expect(schema.$defs.claudeBlockingSelector.enum).toEqual([
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
  ])
  expect(schema.$defs.claudePreflightContext.properties.blockingSelector).toEqual({
    oneOf: [{ $ref: "#/$defs/claudeBlockingSelector" }, { type: "null" }],
  })
  expect(schema.$defs.controlProblem.properties.selector).toEqual({ $ref: "#/$defs/claudeBlockingSelector" })
  expect(schema.$defs.claudePreflightContext.allOf).toHaveLength(3)
})

test("rejects a hello acknowledgement outside RPC 1.0 or the fixed frame limit", () => {
  const helloAck = {
    type: "hello-ack",
    rpc: { major: 1, minor: 0 },
    release: "test",
    serviceEpoch: "00000000-0000-4000-8000-000000000001",
    frameLimit: 1_048_576,
  } as const

  expect(parseServerFrame(helloAck)).toEqual(helloAck)
  expect(() => parseServerFrame({ ...helloAck, rpc: { major: 1, minor: 1 } })).toThrow()
  expect(() => parseServerFrame({ ...helloAck, frameLimit: 1_048_575 })).toThrow()
})

test("drops additive secret fields from typed Target View projections", async () => {
  const view = await readFixture("initial-target-view.json") as Record<string, unknown>
  view.activatedSnapshot = {
    id: "00000000-0000-4000-8000-000000000002",
    providerId: "00000000-0000-4000-8000-000000000003",
    model: "gpt-test",
    protocol: "openai-responses",
    authentication: "openai-bearer",
    epoch: "00000000-0000-4000-8000-000000000004",
    providerCredential: "provider-secret-must-not-escape",
    routingCredential: "routing-secret-must-not-escape",
    authorization: "Bearer provider-secret-must-not-escape",
    recovery: { raw: "routing-secret-must-not-escape" },
  }
  view.problems = [{
    code: "invalid-action",
    message: "The action cannot be completed.",
    providerCredential: "provider-secret-must-not-escape",
    routingCredential: "routing-secret-must-not-escape",
  }]
  view.providerCredential = "provider-secret-must-not-escape"

  const serialized = JSON.stringify(parseTargetView(view))
  expect(serialized).not.toContain("provider-secret-must-not-escape")
  expect(serialized).not.toContain("routing-secret-must-not-escape")
  expect(JSON.parse(serialized).activatedSnapshot).toEqual({
    id: "00000000-0000-4000-8000-000000000002",
    providerId: "00000000-0000-4000-8000-000000000003",
    model: "gpt-test",
    protocol: "openai-responses",
    authentication: "openai-bearer",
    epoch: "00000000-0000-4000-8000-000000000004",
  })
})

test("encodes the frame length in big-endian order", () => {
  const frame = encodeFrame({ type: "hello" })
  expect([...frame.subarray(0, 4)]).toEqual([0, 0, 0, 16])
})

test("rejects invalid UTF-8 and JSON frame bodies", () => {
  const invalidUtf8 = new FrameDecoder()
  expect(() => invalidUtf8.push(new Uint8Array([0, 0, 0, 1, 0xff]))).toThrow("invalid-utf8")

  const invalidJson = new FrameDecoder()
  expect(() => invalidJson.push(new Uint8Array([0, 0, 0, 1, 0x7b]))).toThrow("invalid-json")
})

test("rejects a partial frame at end of stream", () => {
  const decoder = new FrameDecoder() as unknown as { push(chunk: Uint8Array): unknown[]; finish(): void }
  decoder.push(new Uint8Array([0, 0, 0, 4, 0x7b, 0x7d]))
  expect(() => decoder.finish()).toThrow("unexpected-eof")
})
