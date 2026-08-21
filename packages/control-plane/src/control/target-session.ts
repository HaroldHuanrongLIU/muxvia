import { randomUUID } from "node:crypto"

import { ControlError, type RpcTransport } from "./rpc-client"
import type {
  ActionOutcome,
  CompatibilityProbe,
  DiscoverySource,
  ClaudePreflightContext,
  ModelDiscoveryResult,
  OrdinaryTargetAction,
  ReachabilityResult,
  ReconciliationPreview,
  ReconciliationStrategy,
  RequestRecordDetail,
  RequestRecordPage,
  TargetAction,
  Target,
  TargetView,
} from "./types"
import type { UniversalProviderSession } from "./universal-provider-session"

export interface MuxviaControl {
  openTarget(target: Target): Promise<TargetSession>
  openUniversalProviders(): Promise<UniversalProviderSession>
}

export interface CompatibilityResolution {
  version: string
  managementRevision: number
}

export interface RequestRecordPageRequest {
  limit: number
  beforeCursor?: string
}

export interface TargetSession {
  get(): Readonly<TargetView>
  act(action: OrdinaryTargetAction): Promise<ActionOutcome>
  discoverModels(source: DiscoverySource, signal?: AbortSignal): Promise<ModelDiscoveryResult>
  checkReachability(
    providerId: string,
    providerRevision: number,
    signal?: AbortSignal,
  ): Promise<ReachabilityResult>
  previewReconciliation(
    strategy: ReconciliationStrategy,
    signal?: AbortSignal,
  ): Promise<ReconciliationPreview>
  probeCompatibility(signal?: AbortSignal): Promise<CompatibilityProbe>
  resolveCompatibility(input: CompatibilityResolution): Promise<ActionOutcome>
  listRequestRecords(
    input: RequestRecordPageRequest,
    signal?: AbortSignal,
  ): Promise<RequestRecordPage>
  inspectRequestRecord(recordId: string, signal?: AbortSignal): Promise<RequestRecordDetail>
  applyReconciliation(input: {
    strategy: ReconciliationStrategy
    observationToken: string
    acknowledgeVersion?: string
  }): Promise<ActionOutcome>
  subscribe(listener: (next: TargetView) => void): () => void
  whenClosed(): Promise<void>
  close(): Promise<void>
}

class TargetSessionImpl implements TargetSession {
  readonly #rpc: RpcTransport
  readonly #listeners = new Set<(next: TargetView) => void>()
  readonly #removePushListener: () => void
  readonly #target: Target
  readonly #claudeContext?: ClaudePreflightContext
  #view: TargetView
  #actions: Promise<void> = Promise.resolve()
  #refresh?: Promise<void>
  #lastNotifiedSequence: number
  #closed = false

  constructor(
    rpc: RpcTransport,
    initialView: TargetView,
    claudeContext?: ClaudePreflightContext,
  ) {
    this.#rpc = rpc
    this.#view = initialView
    this.#target = initialView.target
    this.#claudeContext = captureClaudeContext(claudeContext)
    this.#lastNotifiedSequence = initialView.viewSequence
    this.#removePushListener = rpc.onTargetView((view) => this.#receivePush(view))
  }

  get(): Readonly<TargetView> {
    return this.#view
  }

  act(action: OrdinaryTargetAction): Promise<ActionOutcome> {
    if ((action as TargetAction).kind === "resolve-compatibility") {
      return Promise.reject(compatibilityResolutionBypassError())
    }
    const capturedAction = captureOrdinaryAction(action)
    return this.#enqueueAction(capturedAction)
  }

  #enqueueAction(
    action: TargetAction,
    expectedRevision?: number,
    compatibilityResolution = false,
  ): Promise<ActionOutcome> {
    if (this.#closed) {
      return Promise.reject(new ControlError("connection-closed", "Target session is closed"))
    }
    const result = this.#actions.then(async () => {
      if (action.kind === "resolve-compatibility" && !compatibilityResolution) {
        throw compatibilityResolutionBypassError()
      }
      const actionId = randomUUID()
      try {
        const response = await this.#rpc.request({
          kind: "act",
          target: this.#target,
          actionId,
          expectedRevision: expectedRevision ?? this.#view.managementRevision,
          action,
        })
        if (response.kind !== "action-outcome") {
          throw new ControlError("invalid-response", "Expected an action outcome")
        }
        this.#view = response.outcome.view
        return response.outcome
      } catch (error) {
        if (error instanceof ControlError && error.authoritativeView) {
          this.#replace(error.authoritativeView)
        }
        throw error
      }
    })
    this.#actions = result.then(() => undefined, () => undefined)
    return result
  }

  async discoverModels(
    source: DiscoverySource,
    signal?: AbortSignal,
  ): Promise<ModelDiscoveryResult> {
    if (this.#closed) {
      throw new ControlError("connection-closed", "Target session is closed")
    }
    const response = await this.#rpc.request({
      kind: "discover-models",
      target: this.#target,
      source,
    }, { signal })
    if (response.kind !== "model-discovery") {
      throw new ControlError("invalid-response", "Expected a model discovery result")
    }
    return response.result
  }

  async checkReachability(
    providerId: string,
    providerRevision: number,
    signal?: AbortSignal,
  ): Promise<ReachabilityResult> {
    if (this.#closed) {
      throw new ControlError("connection-closed", "Target session is closed")
    }
    const response = await this.#rpc.request({
      kind: "check-reachability",
      target: this.#target,
      providerId,
      providerRevision,
    }, { signal })
    if (response.kind !== "reachability") {
      throw new ControlError("invalid-response", "Expected a reachability result")
    }
    return response.result
  }

  async previewReconciliation(
    strategy: ReconciliationStrategy,
    signal?: AbortSignal,
  ): Promise<ReconciliationPreview> {
    if (this.#closed) {
      throw new ControlError("connection-closed", "Target session is closed")
    }
    const response = await this.#rpc.request({
      kind: "preview-reconciliation",
      target: this.#target,
      strategy,
      claudeContext: this.#claudeContext,
    }, { signal })
    if (
      response.kind !== "reconciliation-preview"
      || response.preview.target !== this.#target
      || response.preview.strategy !== strategy
    ) {
      throw new ControlError("invalid-response", "Reconciliation preview response did not match request")
    }
    return freezeReconciliationPreview(response.preview)
  }

  async probeCompatibility(signal?: AbortSignal): Promise<CompatibilityProbe> {
    if (this.#closed) {
      throw new ControlError("connection-closed", "Target session is closed")
    }
    const response = await this.#rpc.request({
      kind: "probe-compatibility",
      target: this.#target,
    }, { signal })
    if (
      response.kind !== "compatibility-probe"
      || response.probe.target !== this.#target
    ) {
      throw new ControlError("invalid-response", "Compatibility probe response did not match request")
    }
    Object.freeze(response.probe.compatibility)
    return Object.freeze(response.probe)
  }

  resolveCompatibility(input: CompatibilityResolution): Promise<ActionOutcome> {
    return this.#enqueueAction(
      { kind: "resolve-compatibility", version: input.version },
      input.managementRevision,
      true,
    )
  }

  async listRequestRecords(
    input: RequestRecordPageRequest,
    signal?: AbortSignal,
  ): Promise<RequestRecordPage> {
    if (this.#closed) {
      throw new ControlError("connection-closed", "Target session is closed")
    }
    const limit = input.limit
    const beforeCursor = input.beforeCursor
    const response = await this.#rpc.request({
      kind: "list-request-records",
      target: this.#target,
      limit,
      ...(beforeCursor === undefined ? {} : { beforeCursor }),
    }, { signal })
    if (
      response.kind !== "request-record-page"
      || response.page.target !== this.#target
    ) {
      throw new ControlError("invalid-response", "Request record page did not match Target")
    }
    return freezeOwnedValue(structuredClone(response.page))
  }

  async inspectRequestRecord(
    recordId: string,
    signal?: AbortSignal,
  ): Promise<RequestRecordDetail> {
    if (this.#closed) {
      throw new ControlError("connection-closed", "Target session is closed")
    }
    const capturedRecordId = recordId
    const response = await this.#rpc.request({
      kind: "inspect-request-record",
      target: this.#target,
      recordId: capturedRecordId,
    }, { signal })
    if (
      response.kind !== "request-record-detail"
      || response.detail.target !== this.#target
      || response.detail.record.id !== capturedRecordId
    ) {
      throw new ControlError("invalid-response", "Request record detail did not match request")
    }
    return freezeOwnedValue(structuredClone(response.detail))
  }

  applyReconciliation(input: {
    strategy: ReconciliationStrategy
    observationToken: string
    acknowledgeVersion?: string
  }): Promise<ActionOutcome> {
    return this.act({
      kind: "reconcile",
      strategy: input.strategy,
      observationToken: input.observationToken,
      acknowledgeVersion: input.acknowledgeVersion,
    })
  }

  subscribe(listener: (next: TargetView) => void): () => void {
    if (this.#closed) return () => {}
    this.#listeners.add(listener)
    return () => this.#listeners.delete(listener)
  }

  whenClosed(): Promise<void> {
    return this.#rpc.whenClosed()
  }

  async close(): Promise<void> {
    if (this.#closed) return
    this.#closed = true
    this.#listeners.clear()
    this.#removePushListener()
    await this.#rpc.close()
  }

  #receivePush(view: TargetView): void {
    if (this.#closed || view.target !== this.#target || view.service.epoch !== this.#view.service.epoch) return
    if (view.viewSequence === this.#view.viewSequence) {
      if (view.viewSequence > this.#lastNotifiedSequence) this.#replace(view)
      return
    }
    if (view.viewSequence < this.#view.viewSequence) return
    if (view.viewSequence === this.#view.viewSequence + 1) {
      this.#replace(view)
      return
    }
    if (!this.#refresh) {
      this.#refresh = this.#refreshTarget().finally(() => {
        this.#refresh = undefined
      })
    }
  }

  async #refreshTarget(): Promise<void> {
    try {
      const result = await this.#rpc.request({
        kind: "open-target",
        target: this.#target,
        claudeContext: this.#claudeContext,
      })
      if (result.kind === "target-view" && result.view.viewSequence > this.#view.viewSequence) {
        this.#replace(result.view)
      }
    } catch {
      // The transport reports connection failure to pending callers; a push refresh is best effort.
    }
  }

  #replace(view: TargetView): void {
    this.#view = view
    this.#lastNotifiedSequence = view.viewSequence
    for (const listener of this.#listeners) listener(view)
  }
}

function captureClaudeContext(
  context: ClaudePreflightContext | undefined,
): ClaudePreflightContext | undefined {
  if (!context) return undefined
  return Object.freeze({
    claudeConfigDir: context.claudeConfigDir,
    selectorState: context.selectorState,
    ...(context.blockingSelector === undefined ? {} : { blockingSelector: context.blockingSelector }),
    hostManagedState: context.hostManagedState,
    cwd: context.cwd,
  })
}

function captureOrdinaryAction(action: OrdinaryTargetAction): OrdinaryTargetAction {
  return freezeOwnedValue(structuredClone(action))
}

function freezeOwnedValue<T>(value: T): T {
  if (typeof value !== "object" || value === null || Object.isFrozen(value)) return value
  for (const nested of Object.values(value)) freezeOwnedValue(nested)
  return Object.freeze(value)
}

function compatibilityResolutionBypassError(): ControlError {
  return new ControlError(
    "unsupported-operation",
    "Compatibility resolution requires resolveCompatibility",
  )
}

function freezeReconciliationPreview(preview: ReconciliationPreview): ReconciliationPreview {
  Object.freeze(preview.compatibility)
  for (const source of preview.shadowSources) {
    if (typeof source === "object") Object.freeze(source)
  }
  for (const change of preview.changes) Object.freeze(change)
  Object.freeze(preview.shadowSources)
  Object.freeze(preview.changes)
  return Object.freeze(preview)
}

export function createTargetSession(
  rpc: RpcTransport,
  initialView: TargetView,
  claudeContext?: ClaudePreflightContext,
): TargetSession {
  return new TargetSessionImpl(rpc, initialView, claudeContext)
}
