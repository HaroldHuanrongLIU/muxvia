import { z } from "zod"

export const FRAME_LIMIT = 1_048_576

const rpcSchema = z.object({
  major: z.literal(1),
  minor: z.literal(0),
})

const targetSchema = z.literal("codex")

const controlProblemSchema = z.object({
  code: z.string(),
  message: z.string(),
})

const providerViewSchema = z.object({
  id: z.string(),
  name: z.string(),
  baseUrl: z.string(),
  model: z.string(),
  credential: z.enum(["present", "missing"]),
})

const activatedSnapshotSchema = z.object({
  id: z.string().uuid(),
  providerId: z.string().uuid(),
  model: z.string(),
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
  providers: z.array(providerViewSchema),
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

const targetActionSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("save-provider"),
    name: z.string(),
    baseUrl: z.string(),
    model: z.string(),
    credential: z.string(),
  }),
  z.object({
    kind: z.literal("activate-provider"),
    providerId: z.string(),
    mode: z.literal("takeover"),
  }),
])

const controlOperationSchema = z.discriminatedUnion("kind", [
  z.object({
    kind: z.literal("open-target"),
    target: targetSchema,
  }),
  z.object({
    kind: z.literal("act"),
    target: targetSchema,
    actionId: z.string().uuid(),
    expectedRevision: z.number().int().nonnegative(),
    action: z.unknown(),
  }),
])

const actionOutcomeSchema = z.object({
  status: z.enum(["applied", "replayed"]),
  view: targetViewSchema,
})

const controlResultSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("target-view"), view: targetViewSchema }),
  z.object({ kind: z.literal("action-outcome"), outcome: actionOutcomeSchema }),
])

const clientFrameSchema = z.discriminatedUnion("type", [
  z.object({ type: z.literal("hello"), rpc: rpcSchema, release: z.string() }),
  z.object({ type: z.literal("request"), requestId: z.string(), operation: controlOperationSchema }),
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
export type TargetAction = z.infer<typeof targetActionSchema>
export type ActionOutcome = z.infer<typeof actionOutcomeSchema>
export type ControlProblem = z.infer<typeof controlProblemSchema>
export type ControlOperation = z.infer<typeof controlOperationSchema>
export type ControlResult = z.infer<typeof controlResultSchema>

export const parseClientFrame = (value: unknown): ClientFrame => clientFrameSchema.parse(value)
export const parseServerFrame = (value: unknown): ServerFrame => serverFrameSchema.parse(value)
export const parseTargetView = (value: unknown): TargetView => targetViewSchema.parse(value)
export const parseTargetAction = (value: unknown): TargetAction => targetActionSchema.parse(value)
