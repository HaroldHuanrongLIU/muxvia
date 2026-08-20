import { randomUUID } from "node:crypto"

import { ControlError, type RpcTransport } from "./rpc-client"
import type {
  UniversalProviderAction,
  UniversalProviderCatalogView,
  UniversalProviderOutcome,
  ClaudePreflightContext,
} from "./types"

export interface UniversalProviderSession {
  get(): Readonly<UniversalProviderCatalogView>
  act(action: UniversalProviderAction): Promise<UniversalProviderOutcome>
  subscribe(listener: (next: UniversalProviderCatalogView) => void): () => void
  whenClosed(): Promise<void>
  close(): Promise<void>
}

class UniversalProviderSessionImpl implements UniversalProviderSession {
  readonly #rpc: RpcTransport
  readonly #listeners = new Set<(next: UniversalProviderCatalogView) => void>()
  readonly #removePushListener: () => void
  #view: UniversalProviderCatalogView
  #actions: Promise<void> = Promise.resolve()
  #refresh?: Promise<void>
  #lastNotifiedSequence: number
  readonly #claudeContext?: ClaudePreflightContext
  #closed = false

  constructor(
    rpc: RpcTransport,
    initialView: UniversalProviderCatalogView,
    claudeContext?: ClaudePreflightContext,
  ) {
    this.#rpc = rpc
    this.#view = initialView
    this.#lastNotifiedSequence = initialView.viewSequence
    this.#claudeContext = claudeContext === undefined
      ? undefined
      : freezeOwnedValue(structuredClone(claudeContext))
    this.#removePushListener = rpc.onUniversalProviderView((view) => this.#receivePush(view))
  }

  get(): Readonly<UniversalProviderCatalogView> {
    return this.#view
  }

  act(action: UniversalProviderAction): Promise<UniversalProviderOutcome> {
    if (this.#closed) {
      return Promise.reject(new ControlError("connection-closed", "Universal Provider session is closed"))
    }
    const captured = freezeOwnedValue(structuredClone(action))
    const result = this.#actions.then(async () => {
      try {
        const response = await this.#rpc.request({
          kind: "universal-provider-act",
          actionId: randomUUID(),
          expectedRevision: this.#view.revision,
          action: captured,
        })
        if (response.kind !== "universal-provider-outcome") {
          throw new ControlError("invalid-response", "Expected a Universal Provider outcome")
        }
        this.#view = response.outcome.view
        return response.outcome
      } catch (error) {
        if (
          error instanceof ControlError
          && error.authoritativeUniversalProviderView
          && error.authoritativeUniversalProviderView.viewSequence >= this.#view.viewSequence
        ) {
          if (error.authoritativeUniversalProviderView.viewSequence === this.#view.viewSequence) {
            this.#view = error.authoritativeUniversalProviderView
          } else {
            this.#replace(error.authoritativeUniversalProviderView)
          }
        }
        if (
          error instanceof ControlError
          && (error.code === "stale-universal-catalog-revision"
            || error.code === "stale-universal-provider-revision")
        ) {
          await this.#refreshCatalog()
        }
        throw error
      }
    })
    this.#actions = result.then(() => undefined, () => undefined)
    return result
  }

  subscribe(listener: (next: UniversalProviderCatalogView) => void): () => void {
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

  #receivePush(view: UniversalProviderCatalogView): void {
    if (this.#closed) return
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
      this.#refresh = this.#refreshCatalog().finally(() => {
        this.#refresh = undefined
      })
    }
  }

  async #refreshCatalog(): Promise<void> {
    try {
      const result = await this.#rpc.request({
        kind: "open-universal-providers",
        ...(this.#claudeContext ? { claudeContext: this.#claudeContext } : {}),
      })
      if (
        result.kind === "universal-provider-catalog"
        && result.view.viewSequence > this.#view.viewSequence
      ) {
        this.#replace(result.view)
      }
    } catch {
      // The transport reports connection failures; catalog refresh is best effort.
    }
  }

  #replace(view: UniversalProviderCatalogView): void {
    this.#view = view
    this.#lastNotifiedSequence = view.viewSequence
    for (const listener of this.#listeners) listener(view)
  }
}

function freezeOwnedValue<T>(value: T): T {
  if (typeof value !== "object" || value === null || Object.isFrozen(value)) return value
  for (const nested of Object.values(value)) freezeOwnedValue(nested)
  return Object.freeze(value)
}

export function createUniversalProviderSession(
  rpc: RpcTransport,
  initialView: UniversalProviderCatalogView,
  claudeContext?: ClaudePreflightContext,
): UniversalProviderSession {
  return new UniversalProviderSessionImpl(rpc, initialView, claudeContext)
}
