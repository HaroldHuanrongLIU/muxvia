import { z } from "zod"

export const FRAME_LIMIT = 1_048_576

const rpcSchema = z.object({
  major: z.literal(1),
  minor: z.literal(0),
})

const targetSchema = z.enum(["codex", "claude"])
const claudeBlockingSelectorSchema = z.enum([
  "CLAUDE_CODE_USE_BEDROCK",
  "CLAUDE_CODE_USE_VERTEX",
  "CLAUDE_CODE_USE_FOUNDRY",
  "CLAUDE_CODE_USE_MANTLE",
  "CLAUDE_CODE_USE_ANTHROPIC_AWS",
  "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
])
const claudePreflightContextSchema = z.object({
  claudeConfigDir: z.string().nullable(),
  selectorState: z.enum(["unset", "disabled", "enabled", "unknown-nonempty"]),
  blockingSelector: claudeBlockingSelectorSchema.nullable().optional(),
  hostManagedState: z.enum(["unmanaged", "managed", "unknown"]),
  cwd: z.string(),
}).superRefine((context, validation) => {
  const environmentActive = context.selectorState === "enabled" || context.selectorState === "unknown-nonempty"
  const hostActive = context.hostManagedState === "managed" || context.hostManagedState === "unknown"
  const selector = context.blockingSelector ?? undefined
  const valid = environmentActive
    ? selector !== undefined && selector !== "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"
    : hostActive
      ? selector === "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST"
      : selector === undefined
  if (!valid) validation.addIssue({ code: "custom", message: "invalid-claude-blocking-selector", path: ["blockingSelector"] })
})

const controlProblemSchema = z.object({
  code: z.string(),
  message: z.string(),
  source: z.enum(["control-plane-context", "user-settings", "managed-settings", "shared-project-settings", "local-project-settings"]).optional(),
  selector: claudeBlockingSelectorSchema.optional(),
})

const providerViewSchema = z.object({
  id: z.string(),
  position: z.number().int().nonnegative(),
  providerRevision: z.number().int().positive(),
  name: z.string(),
  baseUrl: z.string(),
  model: z.string(),
  protocol: z.enum(["openai-responses", "anthropic-messages"]),
  authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer"]),
  routingRequirement: z.enum(["direct-compatible", "takeover-required"]),
  credential: z.enum(["present", "missing"]),
  completeness: z.enum(["complete", "incomplete"]),
  missingFields: z.array(z.enum(["base-url", "model", "credential"])),
  provenance: z.object({
    kind: z.string(),
    key: z.string(),
  }).nullable(),
  generated: z.boolean(),
  activeReferences: z.array(z.enum(["current", "activated-snapshot"])),
})

const providerPresetSchema = z.discriminatedUnion("key", [
  z.object({
    key: z.literal("openai-api-responses"),
    baseUrl: z.literal("https://api.openai.com/v1"),
    model: z.literal(""),
    protocol: z.literal("openai-responses"),
    authentication: z.literal("openai-bearer"),
  }),
  z.object({
    key: z.literal("anthropic-api-messages"),
    baseUrl: z.literal("https://api.anthropic.com/v1"),
    model: z.literal(""),
    protocol: z.literal("anthropic-messages"),
    authentication: z.literal("anthropic-api-key"),
  }),
])

const activatedSnapshotSchema = z.object({
  id: z.string().uuid(),
  providerId: z.string().uuid(),
  model: z.string(),
  protocol: z.enum(["openai-responses", "anthropic-messages"]),
  authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer"]),
  epoch: z.string().uuid(),
})

const targetViewSchema = z.object({
  target: targetSchema,
  managementRevision: z.number().int().nonnegative(),
  viewSequence: z.number().int().nonnegative(),
  service: z.object({
    epoch: z.string().uuid(),
    state: z.string(),
  }),
  mode: z.string(),
  takeover: z.object({
    state: z.string(),
    endpoint: z.string().nullable(),
  }),
  routeHealth: z.object({ state: z.literal("unobserved") }),
  providers: z.array(providerViewSchema),
  providerPresets: z.array(providerPresetSchema),
  currentProviderId: z.string().nullable(),
  servingProviderId: z.string().nullable(),
  managedConfiguration: z.object({
    state: z.string(),
    path: z.string().nullable(),
    restartRequired: z.boolean(),
  }),
  recovery: z.object({
    intentId: z.string().uuid().nullable(),
    state: z.string(),
  }),
  activatedSnapshot: activatedSnapshotSchema.nullable(),
  problems: z.array(controlProblemSchema),
})

const credentialEditSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("keep") }),
  z.object({ kind: z.literal("remove") }),
  z.object({ kind: z.literal("replace"), value: z.string() }),
])

const duplicateCredentialSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("without") }),
  z.object({ kind: z.literal("reuse-source") }),
  z.object({ kind: z.literal("replace"), value: z.string() }),
])

const draftCredentialSourceSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("missing") }),
  z.object({ kind: z.literal("ephemeral"), value: z.string() }),
  z.object({
    kind: z.literal("saved"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
  }),
])

const discoverySourceSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("saved"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
  }),
  z.object({
    kind: z.literal("draft"),
    baseUrl: z.string(),
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer"]),
    credentialSource: draftCredentialSourceSchema,
  }),
])

const reconciliationStrategySchema = z.enum(["adopt", "reapply", "restore"])
const compatibilityClassificationSchema = z.enum(["tested", "unknown-compatible", "incompatible"])
const reconciliationFieldStateSchema = z.enum(["present", "absent", "unchanged", "changed"])
const reconciliationFieldSchema = z.enum([
  "provider",
  "credential",
  "current-provider",
  "activated-snapshot",
  "takeover",
])
const shadowSourceSchema = z.union([
  z.enum([
    "codex-profile",
    "claude-managed",
    "claude-shared",
    "claude-project",
    "claude-local",
    "claude-host-managed",
  ]),
  z.object({ "claude-selector": claudeBlockingSelectorSchema }),
])
const reconciliationPreviewSchema = z.object({
  observationToken: z.string().uuid(),
  target: targetSchema,
  strategy: reconciliationStrategySchema,
  managementRevision: z.number().int().positive(),
  compatibility: z.object({
    version: z.string(),
    classification: compatibilityClassificationSchema,
    acknowledgementRequired: z.boolean(),
  }),
  shadowSources: z.array(shadowSourceSchema),
  changes: z.array(z.object({
    field: reconciliationFieldSchema,
    state: reconciliationFieldStateSchema,
  })),
  providerEffect: z.enum(["create-new", "keep-current", "exit-managed"]),
  restartRequired: z.boolean(),
  unobservableRuntimeBoundary: z.boolean(),
})

const targetActionSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("create-provider"),
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    credential: credentialEditSchema,
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer"]).optional(),
    presetKey: z.enum(["openai-api-responses", "anthropic-api-messages"]).nullable().optional(),
  }),
  z.object({
    kind: z.literal("update-provider"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    credential: credentialEditSchema,
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer"]).optional(),
  }),
  z.object({
    kind: z.literal("reorder-providers"),
    providerIds: z.array(z.string().uuid()),
  }),
  z.object({
    kind: z.literal("delete-provider"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
  }),
  z.object({
    kind: z.literal("duplicate-provider"),
    sourceProviderId: z.string().uuid(),
    sourceProviderRevision: z.number().int().positive(),
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    credential: duplicateCredentialSchema,
  }),
  z.object({
    kind: z.literal("activate-provider"),
    providerId: z.string(),
    mode: z.enum(["direct", "takeover"]),
  }),
  z.object({
    kind: z.literal("reconcile"),
    strategy: reconciliationStrategySchema,
    observationToken: z.string().uuid(),
    acknowledgeVersion: z.string().optional(),
  }),
])

const controlOperationSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("open-target"),
    target: targetSchema,
    claudeContext: claudePreflightContextSchema.optional(),
  }),
  z.object({
    kind: z.literal("act"),
    target: targetSchema,
    actionId: z.string().uuid(),
    expectedRevision: z.number().int().nonnegative(),
    action: z.unknown(),
  }),
  z.object({
    kind: z.literal("discover-models"),
    target: targetSchema,
    source: discoverySourceSchema,
  }),
  z.object({
    kind: z.literal("check-reachability"),
    target: targetSchema,
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
  }),
  z.object({
    kind: z.literal("preview-reconciliation"),
    target: targetSchema,
    strategy: reconciliationStrategySchema,
    claudeContext: claudePreflightContextSchema.optional(),
  }),
])

const actionOutcomeSchema = z.object({
  status: z.enum(["applied", "replayed"]),
  view: targetViewSchema,
})

const inspectionCategorySchema = z.enum([
  "invalid-endpoint",
  "missing-credential",
  "missing-provider",
  "stale-provider-revision",
  "authentication-rejected",
  "endpoint-unsupported",
  "rate-limited",
  "upstream-status",
  "timeout",
  "dns",
  "connect",
  "tls",
  "cancelled",
  "malformed-response",
  "response-too-large",
  "too-many-models",
])

const inspectionFailureSchema = z.object({
  category: inspectionCategorySchema,
  httpStatus: z.number().int().nullable(),
  attempts: z.number().int().nonnegative(),
  elapsedMs: z.number().int().nonnegative(),
  endpointOrigin: z.string().nullable(),
})

const modelDiscoveryResultSchema = z.discriminatedUnion("status", [
  z.object({
    status: z.literal("success"),
    models: z.array(z.object({
      id: z.string(),
      displayName: z.string().nullable(),
    })).max(2_048),
    attempts: z.number().int().positive(),
    elapsedMs: z.number().int().nonnegative(),
    endpointOrigin: z.string(),
  }),
  z.object({
    status: z.literal("failure"),
    failure: inspectionFailureSchema,
  }),
])

const reachabilityResultSchema = z.discriminatedUnion("status", [
  z.object({
    status: z.literal("reachable"),
    httpStatus: z.number().int().min(100).max(999),
    ttfbMs: z.number().int().nonnegative(),
    checkedAtUnixMs: z.number().int().nonnegative(),
    retryCount: z.number().int().min(0).max(1),
    slow: z.boolean(),
    endpointOrigin: z.string(),
  }),
  z.object({
    status: z.literal("unreachable"),
    failure: inspectionFailureSchema,
    checkedAtUnixMs: z.number().int().nonnegative(),
    retryCount: z.number().int().min(0).max(1),
  }),
])

const controlResultSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("target-view"), view: targetViewSchema }),
  z.object({ kind: z.literal("action-outcome"), outcome: actionOutcomeSchema }),
  z.object({ kind: z.literal("model-discovery"), result: modelDiscoveryResultSchema }),
  z.object({ kind: z.literal("reachability"), result: reachabilityResultSchema }),
  z.object({ kind: z.literal("reconciliation-preview"), preview: reconciliationPreviewSchema }),
])

const clientFrameSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("hello"), rpc: rpcSchema, release: z.string() }),
  z.object({ type: z.literal("request"), requestId: z.string(), operation: controlOperationSchema }),
  z.object({ type: z.literal("cancel"), requestId: z.string() }),
])

const serverFrameSchema = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("hello-ack"),
    rpc: rpcSchema,
    release: z.string(),
    serviceEpoch: z.string().uuid(),
    frameLimit: z.literal(FRAME_LIMIT),
  }),
  z.object({ type: z.literal("response"), requestId: z.string(), result: controlResultSchema }),
  z.object({
    type: z.literal("error"),
    requestId: z.string().nullable(),
    problem: controlProblemSchema,
    authoritativeView: targetViewSchema.optional(),
  }),
  z.object({ type: z.literal("target-view"), view: targetViewSchema }),
])

export type ClientFrame = z.infer<typeof clientFrameSchema>
export type ServerFrame = z.infer<typeof serverFrameSchema>
export type TargetView = z.infer<typeof targetViewSchema>
export type Target = z.infer<typeof targetSchema>
export type ClaudePreflightContext = z.infer<typeof claudePreflightContextSchema>
export type ClaudeBlockingSelector = z.infer<typeof claudeBlockingSelectorSchema>
export type TargetAction = z.infer<typeof targetActionSchema>
export type ActionOutcome = z.infer<typeof actionOutcomeSchema>
export type ControlProblem = z.infer<typeof controlProblemSchema>
export type ControlOperation = z.infer<typeof controlOperationSchema>
export type ControlResult = z.infer<typeof controlResultSchema>
export type DiscoverySource = z.infer<typeof discoverySourceSchema>
export type ModelDiscoveryResult = z.infer<typeof modelDiscoveryResultSchema>
export type ReachabilityResult = z.infer<typeof reachabilityResultSchema>
export type ReconciliationStrategy = z.infer<typeof reconciliationStrategySchema>
export type CompatibilityClassification = z.infer<typeof compatibilityClassificationSchema>
export type ReconciliationPreview = z.infer<typeof reconciliationPreviewSchema>

export const parseClientFrame = (value: unknown): ClientFrame => clientFrameSchema.parse(value)
export const parseServerFrame = (value: unknown): ServerFrame => serverFrameSchema.parse(value)
export const parseTargetView = (value: unknown): TargetView => targetViewSchema.parse(value)
export const parseTargetAction = (value: unknown): TargetAction => targetActionSchema.parse(value)
