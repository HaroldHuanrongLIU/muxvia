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
  source: z.enum([
    "control-plane-context",
    "user-settings",
    "managed-settings",
    "shared-project-settings",
    "local-project-settings",
    "codex-profile",
    "claude-managed",
    "claude-shared",
    "claude-project",
    "claude-local",
    "claude-selector",
    "claude-host-managed",
  ]).optional(),
  selector: claudeBlockingSelectorSchema.optional(),
})

const routeHealthSchema = z.object({
  state: z.enum(["unobserved", "healthy", "degraded", "unavailable", "stale"]),
}).strict()

const importProvenanceSchema = z.object({
  sourceProduct: z.enum(["target-cli", "cc-switch", "muxvia"]),
  sourceTarget: z.enum(["codex", "claude", "universal"]),
  sourceIdentifier: z.string().min(1).max(256),
  configurationFingerprint: z.string().regex(/^[0-9a-f]{64}$/),
}).strict()

const providerViewSchema = z.object({
  id: z.string(),
  position: z.number().int().nonnegative(),
  providerRevision: z.number().int().positive(),
  name: z.string(),
  baseUrl: z.string(),
  model: z.string(),
  protocol: z.enum(["openai-responses", "anthropic-messages"]),
  authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]),
  routingRequirement: z.enum(["direct-compatible", "takeover-required"]),
  credential: z.enum(["present", "missing"]),
  completeness: z.enum(["complete", "incomplete"]),
  missingFields: z.array(z.enum(["base-url", "model", "credential", "subscription-account-binding"])),
  provenance: z.object({
    kind: z.string(),
    key: z.string(),
  }).nullable(),
  importProvenance: importProvenanceSchema.optional(),
  importedCurrent: z.boolean().optional(),
  generated: z.boolean(),
  universalProviderId: z.string().uuid().nullable(),
  synchronization: z.enum(["current", "pending"]).nullable(),
  ownership: z.object({
    name: z.enum(["target-provider", "universal-provider", "target-overlay", "target-fixed"]),
    baseUrl: z.enum(["target-provider", "universal-provider", "target-overlay", "target-fixed"]),
    model: z.enum(["target-provider", "universal-provider", "target-overlay", "target-fixed"]),
    protocol: z.enum(["target-provider", "universal-provider", "target-overlay", "target-fixed"]),
    authentication: z.enum(["target-provider", "universal-provider", "target-overlay", "target-fixed"]),
    routingRequirement: z.enum(["target-provider", "universal-provider", "target-overlay", "target-fixed"]),
    credential: z.enum(["target-provider", "universal-provider", "target-overlay", "target-fixed"]),
  }).strict(),
  routeHealth: routeHealthSchema,
  activeReferences: z.array(z.enum(["current", "activated-snapshot", "activated-route-plan"])),
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
  z.object({
    key: z.literal("codex-subscription-bridge"),
    baseUrl: z.literal("https://chatgpt.com/backend-api/codex"),
    model: z.literal(""),
    protocol: z.literal("anthropic-messages"),
    authentication: z.literal("codex-subscription"),
  }),
])

const activatedSnapshotSchema = z.object({
  id: z.string().uuid(),
  providerId: z.string().uuid(),
  model: z.string(),
  protocol: z.enum(["openai-responses", "anthropic-messages"]),
  authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]),
  epoch: z.string().uuid(),
})

const failoverDraftMemberSchema = z.object({
  providerId: z.string().uuid(),
  providerRevision: z.number().int().positive(),
}).strict()

const activatedRoutePlanMemberSchema = z.object({
  position: z.number().int().nonnegative(),
  providerId: z.string().uuid(),
  providerRevision: z.number().int().positive(),
  name: z.string(),
  model: z.string(),
  protocol: z.enum(["openai-responses", "anthropic-messages"]),
  authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]),
}).strict()

const failoverViewSchema = z.object({
  draftRevision: z.number().int().nonnegative(),
  draftMembers: z.array(failoverDraftMemberSchema),
  activePlan: z.object({
    id: z.string().uuid(),
    epoch: z.string().uuid(),
    members: z.array(activatedRoutePlanMemberSchema),
  }).strict().nullable(),
}).strict()

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
  routeHealth: routeHealthSchema,
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
  failover: failoverViewSchema,
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
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]),
    credentialSource: draftCredentialSourceSchema,
  }),
])

const reconciliationStrategySchema = z.enum(["adopt", "reapply", "restore"])
const compatibilityClassificationSchema = z.enum(["tested", "unknown-compatible", "incompatible"])
const compatibilityViewSchema = z.object({
  version: z.string(),
  classification: compatibilityClassificationSchema,
  acknowledgementRequired: z.boolean(),
}).strict()
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
  compatibility: compatibilityViewSchema,
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
  z.object({ kind: z.literal("disable-takeover") }).strict(),
  z.object({
    kind: z.literal("create-provider"),
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    credential: credentialEditSchema,
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]).optional(),
    presetKey: z.enum(["openai-api-responses", "anthropic-api-messages", "codex-subscription-bridge"]).nullable().optional(),
  }),
  z.object({
    kind: z.literal("update-provider"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    credential: credentialEditSchema,
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]).optional(),
    routingRequirement: z.enum(["direct-compatible", "takeover-required"]).optional(),
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
  z.object({
    kind: z.literal("resolve-compatibility"),
    version: z.string(),
  }).strict(),
  z.object({
    kind: z.literal("save-failover-draft"),
    members: z.array(z.object({
      providerId: z.string().uuid(),
      providerRevision: z.number().int().positive(),
    }).strict()),
  }).strict(),
  z.object({
    kind: z.literal("apply-failover-chain"),
    draftRevision: z.number().int().positive(),
  }).strict(),
])

const providerImportSourceSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("live-target") }).strict(),
  z.object({ kind: z.literal("cc-switch"), payload: z.string().max(524_288) }).strict(),
  z.object({ kind: z.literal("cc-switch-sql"), path: z.string().max(4_096) }).strict(),
  z.object({ kind: z.literal("muxvia-export"), payload: z.string().max(524_288) }).strict(),
])

const providerImportResolutionSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("create") }).strict(),
  z.object({ kind: z.literal("use-existing"), providerId: z.string().uuid() }).strict(),
])

const providerImportChoiceSchema = z.object({
  candidateId: z.string().uuid(),
  resolution: providerImportResolutionSchema,
}).strict()

const providerImportTargetOverlaySchema = z.object({
  target: targetSchema,
  enabled: z.boolean(),
  model: z.string(),
  authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer"]),
  routingRequirement: z.enum(["direct-compatible", "takeover-required"]),
}).strict()

const providerImportMatchSchema = z.object({
  providerId: z.string().uuid(),
  name: z.string(),
}).strict()

const providerImportCandidateSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("target-provider"),
    candidateId: z.string().uuid(),
    target: targetSchema,
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    protocol: z.enum(["openai-responses", "anthropic-messages"]),
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]),
    routingRequirement: z.enum(["direct-compatible", "takeover-required"]),
    credential: z.enum(["present", "missing"]),
    importedCurrent: z.boolean(),
    exactMatches: z.array(providerImportMatchSchema),
  }).strict(),
  z.object({
    kind: z.literal("universal-provider"),
    candidateId: z.string().uuid(),
    name: z.string(),
    baseUrl: z.string(),
    credential: z.enum(["present", "missing"]),
    targets: z.array(providerImportTargetOverlaySchema),
    exactMatches: z.array(providerImportMatchSchema),
  }).strict(),
])

const providerImportPreviewSchema = z.object({
  previewToken: z.string().uuid(),
  source: z.object({
    product: z.enum(["target-cli", "cc-switch", "muxvia"]),
    target: z.enum(["codex", "claude", "universal"]),
  }).strict(),
  candidates: z.array(providerImportCandidateSchema).max(256),
  historicalUsage: z.object({
    recordCount: z.number().int().nonnegative(),
    startDate: z.string().regex(/^\d{4}-\d{2}-\d{2}$/).nullable(),
    endDate: z.string().regex(/^\d{4}-\d{2}-\d{2}$/).nullable(),
    estimatedStorageBytes: z.number().int().nonnegative(),
    selectedByDefault: z.literal(false),
  }).strict().optional(),
}).strict()

const providerImportRecordSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("target-provider"),
    candidateId: z.string().uuid(),
    resolution: z.enum(["created", "existing"]),
    target: targetSchema,
    providerId: z.string().uuid(),
  }).strict(),
  z.object({
    kind: z.literal("universal-provider"),
    candidateId: z.string().uuid(),
    resolution: z.enum(["created", "existing"]),
    providerId: z.string().uuid(),
  }).strict(),
])

const providerImportOutcomeSchema = z.object({
  records: z.array(providerImportRecordSchema).max(256),
  historicalUsageImportedRecords: z.number().int().nonnegative().optional(),
}).strict()

const providerConfigurationExportSchema = z.object({
  format: z.literal("muxvia-provider-configuration"),
  version: z.literal(1),
  universalProviders: z.array(z.object({
    sourceId: z.string().uuid(),
    position: z.number().int().nonnegative(),
    name: z.string(),
    baseUrl: z.string(),
    credential: z.literal("missing"),
    targets: z.array(providerImportTargetOverlaySchema),
  }).strict()).max(256),
  targetProviders: z.array(z.object({
    sourceId: z.string().uuid(),
    target: targetSchema,
    position: z.number().int().nonnegative(),
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    credential: z.literal("missing"),
    protocol: z.enum(["openai-responses", "anthropic-messages"]),
    authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer", "codex-subscription"]),
    routingRequirement: z.enum(["direct-compatible", "takeover-required"]),
    universalProviderSourceId: z.string().uuid().nullable(),
  }).strict()).max(256),
  failoverDrafts: z.array(z.object({
    target: targetSchema,
    providerSourceIds: z.array(z.string().uuid()),
  }).strict()).max(2),
}).strict()

const recoveryBackupEntrySchema = z.object({
  kind: z.enum([
    "sqlite-state",
    "subscription-accounts",
    "codex-managed-configuration",
    "claude-managed-configuration",
  ]),
  present: z.boolean(),
  mode: z.number().int().min(0).max(0o777).optional(),
  byteLength: z.number().int().nonnegative(),
}).strict()

const recoveryBackupInspectionSchema = z.object({
  snapshotId: z.string().uuid(),
  createdAtUnixSeconds: z.number().int().positive(),
  createdByRelease: z.string().min(1).max(256),
  formatVersion: z.literal(1),
  databaseSchemaVersion: z.number().int().positive(),
  artifactSizeBytes: z.number().int().positive(),
  artifactSha256: z.string().regex(/^[0-9a-f]{64}$/),
  sensitive: z.literal(true),
  compatibility: z.enum([
    "compatible",
    "migration-required",
    "unsupported-database-schema",
  ]),
  entries: z.array(recoveryBackupEntrySchema).length(4),
}).strict()

const controlOperationSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("create-recovery-backup") }).strict(),
  z.object({ kind: z.literal("inspect-recovery-backup"), path: z.string() }).strict(),
  z.object({
    kind: z.literal("prepare-handover"),
    candidatePath: z.string(),
    expectedRelease: z.string(),
  }).strict(),
  z.object({
    kind: z.literal("force-stop"),
    acknowledgement: z.literal("managed-target-files-may-remain-pointed-at-dead-endpoint"),
  }).strict(),
  z.object({
    kind: z.literal("open-universal-providers"),
    claudeContext: claudePreflightContextSchema.optional(),
  }).strict(),
  z.object({ kind: z.literal("open-subscription-accounts") }).strict(),
  z.object({
    kind: z.literal("start-device-authorization"),
    reauthorizeAccountId: z.string().nullable(),
  }).strict(),
  z.object({
    kind: z.literal("poll-device-authorization"),
    flowId: z.string().uuid(),
  }).strict(),
  z.object({
    kind: z.literal("preview-default-subscription-account"),
    accountId: z.string(),
  }).strict(),
  z.object({
    kind: z.literal("subscription-account-act"),
    actionId: z.string().uuid(),
    expectedRevision: z.number().int().nonnegative(),
    action: z.unknown(),
  }).strict(),
  z.object({
    kind: z.literal("universal-provider-act"),
    actionId: z.string().uuid(),
    expectedRevision: z.number().int().nonnegative(),
    action: z.unknown(),
  }),
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
  z.object({
    kind: z.literal("probe-compatibility"),
    target: targetSchema,
  }).strict(),
  z.object({
    kind: z.literal("list-request-records"),
    target: targetSchema,
    beforeCursor: z.string().optional(),
    limit: z.number().int().min(1).max(100),
  }).strict(),
  z.object({
    kind: z.literal("inspect-request-record"),
    target: targetSchema,
    recordId: z.string().uuid(),
  }).strict(),
  z.object({
    kind: z.literal("list-usage-activity"),
    target: targetSchema,
    beforeCursor: z.string().optional(),
    limit: z.number().int().min(1).max(100),
  }).strict(),
  z.object({ kind: z.literal("refresh-native-usage"), target: targetSchema }).strict(),
  z.object({
    kind: z.literal("set-usage-retention"),
    target: targetSchema,
    detailedRetentionDays: z.number().int().min(1).max(3_650),
  }).strict(),
  z.object({ kind: z.literal("clear-usage"), target: targetSchema }).strict(),
  z.object({ kind: z.literal("update-pricing-catalog"), target: targetSchema }).strict(),
  z.object({
    kind: z.literal("preview-provider-import"),
    target: targetSchema,
    source: providerImportSourceSchema,
  }).strict(),
  z.object({
    kind: z.literal("confirm-provider-import"),
    target: targetSchema,
    previewToken: z.string().uuid(),
    choices: z.array(providerImportChoiceSchema).max(256),
    includeHistoricalUsage: z.boolean().optional(),
  }).strict(),
  z.object({
    kind: z.literal("export-provider-configuration"),
    target: targetSchema,
  }).strict(),
])

const compatibilityProbeSchema = z.object({
  target: targetSchema,
  managementRevision: z.number().int().nonnegative(),
  compatibility: compatibilityViewSchema,
}).strict()

const actionOutcomeSchema = z.object({
  status: z.enum(["applied", "replayed"]),
  view: targetViewSchema,
})

const universalProviderPresetTargetSchema = z.object({
  target: targetSchema,
  enabled: z.boolean(),
  model: z.string(),
  authentication: z.enum(["openai-bearer", "anthropic-api-key", "anthropic-bearer"]),
  routingRequirement: z.enum(["direct-compatible", "takeover-required"]),
}).strict()

const universalProviderPresetKeySchema = z.enum([
  "openai-api-responses",
  "anthropic-api-messages",
])

const universalProviderPresetSchema = z.object({
  key: universalProviderPresetKeySchema,
  name: z.string(),
  baseUrl: z.string(),
  targets: z.array(universalProviderPresetTargetSchema),
}).strict()

const universalProviderTargetSchema = universalProviderPresetTargetSchema.extend({
  overlayRevision: z.number().int().positive(),
  generatedProviderId: z.string().uuid().nullable(),
  synchronization: z.enum(["current", "pending"]),
  activeReferences: z.array(z.enum(["current", "activated-snapshot", "activated-route-plan"])),
}).strict()

const universalProviderSchema = z.object({
  id: z.string().uuid(),
  position: z.number().int().nonnegative(),
  providerRevision: z.number().int().positive(),
  name: z.string(),
  baseUrl: z.string(),
  credential: z.enum(["present", "missing"]),
  provenance: z.object({ kind: z.string(), key: z.string() }).nullable(),
  importProvenance: importProvenanceSchema.optional(),
  targets: z.array(universalProviderTargetSchema),
}).strict()

const universalProviderCatalogSchema = z.object({
  revision: z.number().int().nonnegative(),
  viewSequence: z.number().int().nonnegative(),
  providers: z.array(universalProviderSchema),
  presets: z.array(universalProviderPresetSchema),
}).strict()

const universalProviderActionSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("create-universal-provider"),
    name: z.string(),
    baseUrl: z.string(),
    credential: credentialEditSchema,
    presetKey: universalProviderPresetKeySchema.nullable(),
    targets: z.array(universalProviderPresetTargetSchema),
  }).strict(),
  z.object({
    kind: z.literal("update-universal-provider"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
    name: z.string(),
    baseUrl: z.string(),
    credential: credentialEditSchema,
    targets: z.array(universalProviderPresetTargetSchema),
  }).strict(),
  z.object({
    kind: z.literal("duplicate-universal-provider"),
    sourceProviderId: z.string().uuid(),
    sourceProviderRevision: z.number().int().positive(),
    name: z.string(),
    baseUrl: z.string(),
    credential: duplicateCredentialSchema,
    targets: z.array(universalProviderPresetTargetSchema),
  }).strict(),
  z.object({
    kind: z.literal("delete-universal-provider"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
  }).strict(),
  z.object({
    kind: z.literal("synchronize-universal-provider"),
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
  }).strict(),
])

const subscriptionAccountActionSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("set-default-account"),
    accountId: z.string(),
    previewToken: z.string().uuid(),
  }).strict(),
  z.object({
    kind: z.literal("bind-provider-fixed"),
    target: targetSchema,
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
    accountId: z.string(),
  }).strict(),
  z.object({
    kind: z.literal("bind-provider-follow-default"),
    target: targetSchema,
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
  }).strict(),
  z.object({
    kind: z.literal("delete-account"),
    accountId: z.string(),
  }).strict(),
])

const subscriptionProviderBindingSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("fixed"), accountId: z.string() }).strict(),
  z.object({ kind: z.literal("follow-default") }).strict(),
])

const subscriptionAccountCatalogSchema = z.object({
  revision: z.number().int().nonnegative(),
  viewSequence: z.number().int().nonnegative(),
  defaultAccountId: z.string().nullable(),
  accounts: z.array(z.object({
    accountId: z.string(),
    email: z.string().nullable(),
    authenticatedAt: z.number().int(),
    state: z.enum(["authorized", "needs-reauthorization"]),
    default: z.boolean(),
  }).strict()),
  bindings: z.array(z.object({
    target: targetSchema,
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
    providerName: z.string(),
    binding: subscriptionProviderBindingSchema,
    resolution: z.object({
      state: z.enum(["available", "needs-reauthorization", "missing", "no-default"]),
      accountId: z.string().nullable(),
    }).strict(),
  }).strict()),
  recovery: z.object({
    state: z.enum(["clean", "recovery-required"]),
  }).strict(),
}).strict()

const deviceAuthorizationChallengeSchema = z.object({
  flowId: z.string().uuid(),
  userCode: z.string(),
  verificationUrl: z.string(),
  expiresInSeconds: z.number().int().nonnegative(),
  pollIntervalSeconds: z.number().int().positive(),
}).strict()

const deviceAuthorizationPollSchema = z.discriminatedUnion("status", [
  z.object({ status: z.literal("pending") }).strict(),
  z.object({ status: z.literal("expired") }).strict(),
  z.object({ status: z.literal("authorized"), accountId: z.string() }).strict(),
])

const subscriptionDefaultPreviewSchema = z.object({
  previewToken: z.string().uuid(),
  accountId: z.string(),
  effects: z.array(z.object({
    target: targetSchema,
    providerId: z.string().uuid(),
    providerRevision: z.number().int().positive(),
    providerName: z.string(),
    currentAccountId: z.string().nullable(),
    nextAccountId: z.string().nullable(),
    nextResolution: z.enum(["available", "needs-reauthorization", "missing", "no-default"]),
  }).strict()),
}).strict()

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

const requestUsageSchema = z.object({
  inputTokens: z.number().int().nonnegative(),
  cachedInputTokens: z.number().int().nonnegative(),
  cacheCreationInputTokens: z.number().int().nonnegative(),
  outputTokens: z.number().int().nonnegative(),
}).strict()

const requestRecordSummarySchema = z.object({
  id: z.string().uuid(),
  planId: z.string().uuid(),
  planEpoch: z.string().uuid(),
  providerId: z.string().uuid().nullable(),
  providerName: z.string().nullable(),
  model: z.string(),
  protocol: z.enum(["openai-responses", "anthropic-messages"]),
  startedAtUnixMs: z.number().int().nonnegative(),
  finishedAtUnixMs: z.number().int().nonnegative(),
  latencyMs: z.number().int().nonnegative(),
  outcome: z.enum([
    "success",
    "upstream-error",
    "semantic-error",
    "transport-error",
    "route-unavailable",
    "cancelled",
    "stream-error",
  ]),
  httpStatus: z.number().int().min(100).max(999).nullable(),
  usage: requestUsageSchema.nullable(),
  estimatedCostNanoUsd: z.number().int().nonnegative().nullable(),
  hasErrorPayload: z.boolean(),
  errorPayloadTruncated: z.boolean(),
}).strict()

const pricingSnapshotSchema = z.object({
  catalogVersion: z.string(),
  source: z.string(),
  sourceModel: z.string(),
  inputNanoUsdPerMillion: z.number().int().nonnegative(),
  outputNanoUsdPerMillion: z.number().int().nonnegative(),
  cacheReadMultiplierPpm: z.number().int().nonnegative(),
  cacheCreationMultiplierPpm: z.number().int().nonnegative(),
  pricedAtUnixMs: z.number().int().nonnegative(),
  estimatedCostNanoUsd: z.number().int().nonnegative(),
}).strict()

const requestRecordPageSchema = z.object({
  target: targetSchema,
  records: z.array(requestRecordSummarySchema),
  nextCursor: z.string().nullable(),
}).strict()

const requestRecordDetailSchema = z.object({
  target: targetSchema,
  record: requestRecordSummarySchema,
  pricingSnapshot: pricingSnapshotSchema.nullable(),
  errorPayload: z.string().nullable(),
  errorPayloadSensitive: z.boolean(),
}).strict()

const nativeUsageRecordSummarySchema = z.object({
  id: z.string().uuid(),
  model: z.string(),
  observedAtUnixMs: z.number().int().nonnegative(),
  usage: requestUsageSchema,
  estimatedCostNanoUsd: z.number().int().nonnegative().nullable(),
}).strict()

const dailyUsageRollupSchema = z.object({
  localDate: z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
  requestRecordCount: z.number().int().nonnegative(),
  nativeUsageRecordCount: z.number().int().nonnegative(),
  successfulRequestCount: z.number().int().nonnegative(),
  failedRequestCount: z.number().int().nonnegative(),
  usage: requestUsageSchema,
  pricedRecordCount: z.number().int().nonnegative(),
  unpricedRecordCount: z.number().int().nonnegative(),
  estimatedCostNanoUsd: z.number().int().nonnegative(),
  latencyObservationCount: z.number().int().nonnegative(),
  totalLatencyMs: z.number().int().nonnegative(),
}).strict()

const migratedUsageRollupSchema = z.object({
  id: z.string().uuid(),
  sourceProduct: z.literal("cc-switch"),
  localDate: z.string().regex(/^\d{4}-\d{2}-\d{2}$/),
  sourceRecordCount: z.number().int().positive(),
  successfulRequestCount: z.number().int().nonnegative(),
  failedRequestCount: z.number().int().nonnegative(),
  usage: requestUsageSchema,
  latencyObservationCount: z.number().int().nonnegative(),
  totalLatencyMs: z.number().int().nonnegative(),
}).strict()

const usageActivityEntrySchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("request-record"), record: requestRecordSummarySchema }).strict(),
  z.object({ kind: z.literal("native-usage-record"), record: nativeUsageRecordSummarySchema }).strict(),
  z.object({ kind: z.literal("daily-usage-rollup"), rollup: dailyUsageRollupSchema }).strict(),
  z.object({ kind: z.literal("migrated-usage-rollup"), rollup: migratedUsageRollupSchema }).strict(),
])

const usageActivityPageSchema = z.object({
  target: targetSchema,
  entries: z.array(usageActivityEntrySchema),
  nextCursor: z.string().nullable(),
  detailedRetentionDays: z.number().int().min(1).max(3_650),
  catalogVersion: z.string(),
}).strict()

const nativeUsageRefreshSchema = z.object({
  target: targetSchema,
  importedRecords: z.number().int().nonnegative(),
  scannedFiles: z.number().int().nonnegative(),
}).strict()

const usageRetentionOutcomeSchema = z.object({
  target: targetSchema,
  detailedRetentionDays: z.number().int().min(1).max(3_650),
  rolledUpDays: z.number().int().nonnegative(),
  prunedRequestRecords: z.number().int().nonnegative(),
  prunedNativeUsageRecords: z.number().int().nonnegative(),
}).strict()

const usageClearOutcomeSchema = z.object({
  target: targetSchema,
  clearedRequestRecords: z.number().int().nonnegative(),
  clearedNativeUsageRecords: z.number().int().nonnegative(),
  clearedDailyRollups: z.number().int().nonnegative(),
  clearedMigratedUsageRollups: z.number().int().nonnegative().optional(),
  clearedImportCursors: z.number().int().nonnegative(),
}).strict()

const pricingCatalogUpdateOutcomeSchema = z.object({
  target: targetSchema,
  catalogVersion: z.string(),
  source: z.string(),
  backfilledRequestRecords: z.number().int().nonnegative(),
  backfilledNativeUsageRecords: z.number().int().nonnegative(),
}).strict()

const controlResultSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("handover-prepared"), release: z.string() }).strict(),
  z.object({
    kind: z.literal("force-stop-accepted"),
    warning: z.literal("managed-target-files-may-remain-pointed-at-dead-endpoint"),
  }).strict(),
  z.object({ kind: z.literal("target-view"), view: targetViewSchema }),
  z.object({ kind: z.literal("universal-provider-catalog"), view: universalProviderCatalogSchema }).strict(),
  z.object({ kind: z.literal("subscription-account-catalog"), view: subscriptionAccountCatalogSchema }).strict(),
  z.object({
    kind: z.literal("device-authorization-challenge"),
    challenge: deviceAuthorizationChallengeSchema,
  }).strict(),
  z.object({
    kind: z.literal("device-authorization-poll"),
    poll: deviceAuthorizationPollSchema,
  }).strict(),
  z.object({
    kind: z.literal("subscription-default-preview"),
    preview: subscriptionDefaultPreviewSchema,
  }).strict(),
  z.object({
    kind: z.literal("subscription-account-outcome"),
    outcome: z.object({
      status: z.enum(["applied", "replayed"]),
      view: subscriptionAccountCatalogSchema,
    }).strict(),
  }).strict(),
  z.object({
    kind: z.literal("universal-provider-outcome"),
    outcome: z.object({
      status: z.enum(["applied", "replayed"]),
      view: universalProviderCatalogSchema,
    }).strict(),
  }).strict(),
  z.object({ kind: z.literal("action-outcome"), outcome: actionOutcomeSchema }),
  z.object({ kind: z.literal("model-discovery"), result: modelDiscoveryResultSchema }),
  z.object({ kind: z.literal("reachability"), result: reachabilityResultSchema }),
  z.object({ kind: z.literal("reconciliation-preview"), preview: reconciliationPreviewSchema }),
  z.object({ kind: z.literal("compatibility-probe"), probe: compatibilityProbeSchema }).strict(),
  z.object({ kind: z.literal("request-record-page"), page: requestRecordPageSchema }).strict(),
  z.object({ kind: z.literal("request-record-detail"), detail: requestRecordDetailSchema }).strict(),
  z.object({ kind: z.literal("usage-activity-page"), page: usageActivityPageSchema }).strict(),
  z.object({ kind: z.literal("native-usage-refresh"), refresh: nativeUsageRefreshSchema }).strict(),
  z.object({ kind: z.literal("usage-retention-outcome"), outcome: usageRetentionOutcomeSchema }).strict(),
  z.object({ kind: z.literal("usage-clear-outcome"), outcome: usageClearOutcomeSchema }).strict(),
  z.object({ kind: z.literal("pricing-catalog-update-outcome"), outcome: pricingCatalogUpdateOutcomeSchema }).strict(),
  z.object({ kind: z.literal("provider-import-preview"), preview: providerImportPreviewSchema }).strict(),
  z.object({ kind: z.literal("provider-import-outcome"), outcome: providerImportOutcomeSchema }).strict(),
  z.object({ kind: z.literal("provider-configuration-export"), export: providerConfigurationExportSchema }).strict(),
  z.object({
    kind: z.literal("recovery-backup-created"),
    path: z.string(),
    inspection: recoveryBackupInspectionSchema,
  }).strict(),
  z.object({
    kind: z.literal("recovery-backup-inspection"),
    inspection: recoveryBackupInspectionSchema,
  }).strict(),
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
    authoritativeUniversalProviderView: universalProviderCatalogSchema.optional(),
    authoritativeSubscriptionAccountView: subscriptionAccountCatalogSchema.optional(),
  }),
  z.object({ type: z.literal("target-view"), view: targetViewSchema }),
  z.object({ type: z.literal("universal-provider-view"), view: universalProviderCatalogSchema }).strict(),
  z.object({ type: z.literal("subscription-account-view"), view: subscriptionAccountCatalogSchema }).strict(),
])

export type ClientFrame = z.infer<typeof clientFrameSchema>
export type ServerFrame = z.infer<typeof serverFrameSchema>
export type TargetView = z.infer<typeof targetViewSchema>
export type Target = z.infer<typeof targetSchema>
export type ClaudePreflightContext = z.infer<typeof claudePreflightContextSchema>
export type ClaudeBlockingSelector = z.infer<typeof claudeBlockingSelectorSchema>
export type TargetAction = z.infer<typeof targetActionSchema>
export type UniversalProviderAction = z.infer<typeof universalProviderActionSchema>
export type SubscriptionAccountAction = z.infer<typeof subscriptionAccountActionSchema>
export type SubscriptionAccountCatalogView = z.infer<typeof subscriptionAccountCatalogSchema>
export type DeviceAuthorizationChallenge = z.infer<typeof deviceAuthorizationChallengeSchema>
export type DeviceAuthorizationPoll = z.infer<typeof deviceAuthorizationPollSchema>
export type SubscriptionDefaultPreview = z.infer<typeof subscriptionDefaultPreviewSchema>
export type SubscriptionAccountOutcome = Extract<
  ControlResult,
  { kind: "subscription-account-outcome" }
>["outcome"]
export type UniversalProviderCatalogView = z.infer<typeof universalProviderCatalogSchema>
export type OrdinaryTargetAction = Exclude<TargetAction, { kind: "resolve-compatibility" }>
export type ActionOutcome = z.infer<typeof actionOutcomeSchema>
export type ControlProblem = z.infer<typeof controlProblemSchema>
export type ControlOperation = z.infer<typeof controlOperationSchema>
export type ControlResult = z.infer<typeof controlResultSchema>
export type DiscoverySource = z.infer<typeof discoverySourceSchema>
export type ModelDiscoveryResult = z.infer<typeof modelDiscoveryResultSchema>
export type ReachabilityResult = z.infer<typeof reachabilityResultSchema>
export type ReconciliationStrategy = z.infer<typeof reconciliationStrategySchema>
export type CompatibilityClassification = z.infer<typeof compatibilityClassificationSchema>
export type CompatibilityProbe = z.infer<typeof compatibilityProbeSchema>
export type ReconciliationPreview = z.infer<typeof reconciliationPreviewSchema>
export type RequestRecordSummary = z.infer<typeof requestRecordSummarySchema>
export type RequestRecordPage = z.infer<typeof requestRecordPageSchema>
export type RequestRecordDetail = z.infer<typeof requestRecordDetailSchema>
export type PricingSnapshot = z.infer<typeof pricingSnapshotSchema>
export type NativeUsageRecordSummary = z.infer<typeof nativeUsageRecordSummarySchema>
export type DailyUsageRollup = z.infer<typeof dailyUsageRollupSchema>
export type UsageActivityEntry = z.infer<typeof usageActivityEntrySchema>
export type UsageActivityPage = z.infer<typeof usageActivityPageSchema>
export type NativeUsageRefresh = z.infer<typeof nativeUsageRefreshSchema>
export type UsageRetentionOutcome = z.infer<typeof usageRetentionOutcomeSchema>
export type UsageClearOutcome = z.infer<typeof usageClearOutcomeSchema>
export type PricingCatalogUpdateOutcome = z.infer<typeof pricingCatalogUpdateOutcomeSchema>
export type ProviderImportSource = z.infer<typeof providerImportSourceSchema>
export type ProviderImportChoice = z.infer<typeof providerImportChoiceSchema>
export type ProviderImportPreview = z.infer<typeof providerImportPreviewSchema>
export type ProviderImportCandidateView = ProviderImportPreview["candidates"][number]
export type ProviderImportOutcome = z.infer<typeof providerImportOutcomeSchema>
export type ProviderConfigurationExport = z.infer<typeof providerConfigurationExportSchema>
export type RecoveryBackupInspection = z.infer<typeof recoveryBackupInspectionSchema>
export type UniversalProviderOutcome = Extract<
  ControlResult,
  { kind: "universal-provider-outcome" }
>["outcome"]

export const parseClientFrame = (value: unknown): ClientFrame => clientFrameSchema.parse(value)
export const parseServerFrame = (value: unknown): ServerFrame => serverFrameSchema.parse(value)
export const parseTargetView = (value: unknown): TargetView => targetViewSchema.parse(value)
export const parseTargetAction = (value: unknown): TargetAction => targetActionSchema.parse(value)
export const parseUniversalProviderAction = (value: unknown): UniversalProviderAction =>
  universalProviderActionSchema.parse(value)
export const parseSubscriptionAccountAction = (value: unknown): SubscriptionAccountAction =>
  subscriptionAccountActionSchema.parse(value)
