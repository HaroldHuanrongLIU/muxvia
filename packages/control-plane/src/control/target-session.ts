import { randomUUID } from "node:crypto"

import { ControlError, type RpcTransport } from "./rpc-client"
import type {
  ActionOutcome,
  DiscoverySource,
  ModelDiscoveryResult,
  ReachabilityResult,
  TargetAction,
  TargetView,
} from "./types"

export interface MuxviaControl {
  openTarget(target: "codex"): Promise<TargetSession>
}

export interface TargetSession {
  get(): Readonly<TargetView>
  act(action: TargetAction): Promise<ActionOutcome>
  discoverModels(source: DiscoverySource, signal?: AbortSignal): Promise<ModelDiscoveryResult>
  checkReachability(
    providerId: string,
    providerRevision: number,
    signal?: AbortSignal,
  ): Promise<ReachabilityResult>
  subscribe(listener: (next: TargetView) => void): () => void
  whenClosed(): Promise<void>
  close(): Promise<void>
}

class TargetSessionImpl implements TargetSession {
  readonly #rpc: RpcTransport
  readonly #listeners = new Set<(next: TargetView) => void>()
  readonly #removePushListener: () => void
  #view: TargetView
  #actions: Promise<void> = Promise.resolve()
  #refresh?: Promise<void>
  #lastNotifiedSequence: number
  #closed = false

  constructor(rpc: RpcTransport, initialView: TargetView) {
    this.#rpc = rpc
    this.#view = initialView
    this.#lastNotifiedSequence = initialView.viewSequence
    this.#removePushListener = rpc.onTargetView((view) => this.#receivePush(view))
  }

  get(): Readonly<TargetView> {
    return this.#view
  }

  act(action: TargetAction): Promise<ActionOutcome> {
    if (this.#closed) {
      return Promise.reject(new ControlError("connection-closed", "Target session is closed"))
    }
    const result = this.#actions.then(async () => {
      const actionId = randomUUID()
      try {
        const response = await this.#rpc.request({
          kind: "act",
          target: "codex",
          actionId,
          expectedRevision: this.#view.managementRevision,
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
      target: "codex",
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
      target: "codex",
      providerId,
      providerRevision,
    }, { signal })
    if (response.kind !== "reachability") {
      throw new ControlError("invalid-response", "Expected a reachability result")
    }
    return response.result
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
    if (this.#closed || view.service.epoch !== this.#view.service.epoch) return
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
      const result = await this.#rpc.request({ kind: "open-target", target: "codex" })
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

export function createTargetSession(rpc: RpcTransport, initialView: TargetView): TargetSession {
  return new TargetSessionImpl(rpc, initialView)
}
