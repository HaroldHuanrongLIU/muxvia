import { expect, test } from "bun:test"
import { readFile } from "node:fs/promises"
import { fileURLToPath } from "node:url"

import { FrameDecoder, encodeFrame } from "../src/control/framing"
import {
  parseClientFrame,
  parseServerFrame,
  parseTargetAction,
  parseTargetView,
} from "../src/control/types"
import type { TargetView } from "../src/control/types"

const fixtures = new URL("../../../protocol/fixtures/", import.meta.url)

async function readFixture(name: string): Promise<unknown> {
  return JSON.parse(await readFile(fileURLToPath(new URL(name, fixtures)), "utf8"))
}

test.each([
  ["hello.json", parseClientFrame],
  ["initial-target-view.json", parseTargetView],
  ["save-provider.json", parseTargetAction],
  ["reorder-providers.json", parseTargetAction],
  ["delete-provider.json", parseTargetAction],
  ["duplicate-provider.json", parseTargetAction],
  ["discover-models.json", parseClientFrame],
  ["check-reachability.json", parseClientFrame],
  ["cancel-inspection.json", parseClientFrame],
  ["preview-reconciliation.json", parseServerFrame],
  ["apply-reconciliation.json", parseTargetAction],
] as const)("round-trips %s as its protocol type", async (name, parse) => {
  const value = await readFixture(name)
  expect(JSON.parse(JSON.stringify(parse(value)))).toEqual(value)
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
    generated: false,
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
    problems: [],
  } satisfies TargetView
  expect(parseTargetView(view)).toEqual(view)

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
