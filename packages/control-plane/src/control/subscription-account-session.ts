import { randomUUID } from "node:crypto"

import { ControlError, type RpcTransport } from "./rpc-client"
import type {
  DeviceAuthorizationChallenge,
  DeviceAuthorizationPoll,
  SubscriptionAccountAction,
  SubscriptionAccountCatalogView,
  SubscriptionAccountOutcome,
  SubscriptionDefaultPreview,
} from "./types"

export interface SubscriptionAccountSession {
  get(): Readonly<SubscriptionAccountCatalogView>
  startDeviceAuthorization(reauthorizeAccountId?: string): Promise<DeviceAuthorizationChallenge>
  pollDeviceAuthorization(flowId: string, signal?: AbortSignal): Promise<DeviceAuthorizationPoll>
  previewDefault(accountId: string): Promise<SubscriptionDefaultPreview>
  act(action: SubscriptionAccountAction): Promise<SubscriptionAccountOutcome>
  subscribe(listener: (next: SubscriptionAccountCatalogView) => void): () => void
  whenClosed(): Promise<void>
  close(): Promise<void>
}

export interface SubscriptionPlatformEffects {
  copyUserCode(userCode: string): Promise<boolean>
  openVerificationUrl(url: string): Promise<boolean>
  wait(milliseconds: number, signal: AbortSignal): Promise<void>
}

class SubscriptionAccountSessionImpl implements SubscriptionAccountSession {
  readonly #rpc: RpcTransport
  readonly #listeners = new Set<(next: SubscriptionAccountCatalogView) => void>()
  readonly #removePushListener: () => void
  #view: SubscriptionAccountCatalogView
  #actions: Promise<void> = Promise.resolve()
  #refresh?: Promise<void>
  #lastNotifiedSequence: number
  #closed = false

  constructor(rpc: RpcTransport, initialView: SubscriptionAccountCatalogView) {
    this.#rpc = rpc
    this.#view = initialView
    this.#lastNotifiedSequence = initialView.viewSequence
    this.#removePushListener = rpc.onSubscriptionAccountView((view) => this.#receivePush(view))
  }

  get(): Readonly<SubscriptionAccountCatalogView> {
    return this.#view
  }

  async startDeviceAuthorization(
    reauthorizeAccountId?: string,
  ): Promise<DeviceAuthorizationChallenge> {
    this.#ensureOpen()
    const result = await this.#rpc.request({
      kind: "start-device-authorization",
      reauthorizeAccountId: reauthorizeAccountId ?? null,
    })
    if (result.kind !== "device-authorization-challenge") {
      throw new ControlError("invalid-response", "Expected a device authorization challenge")
    }
    if (result.challenge.verificationUrl !== "https://auth.openai.com/codex/device") {
      throw new ControlError("invalid-response", "Device authorization verification URL was invalid")
    }
    return freezeOwnedValue(structuredClone(result.challenge))
  }

  async pollDeviceAuthorization(
    flowId: string,
    signal?: AbortSignal,
  ): Promise<DeviceAuthorizationPoll> {
    this.#ensureOpen()
    const result = await this.#rpc.request(
      { kind: "poll-device-authorization", flowId },
      { signal },
    )
    if (result.kind !== "device-authorization-poll") {
      throw new ControlError("invalid-response", "Expected a device authorization poll result")
    }
    return freezeOwnedValue(structuredClone(result.poll))
  }

  async previewDefault(accountId: string): Promise<SubscriptionDefaultPreview> {
    this.#ensureOpen()
    const result = await this.#rpc.request({
      kind: "preview-default-subscription-account",
      accountId,
    })
    if (result.kind !== "subscription-default-preview") {
      throw new ControlError("invalid-response", "Expected a Subscription Account default preview")
    }
    return freezeOwnedValue(structuredClone(result.preview))
  }

  act(action: SubscriptionAccountAction): Promise<SubscriptionAccountOutcome> {
    if (this.#closed) {
      return Promise.reject(new ControlError("connection-closed", "Subscription Account session is closed"))
    }
    const captured = freezeOwnedValue(structuredClone(action))
    const result = this.#actions.then(async () => {
      try {
        const response = await this.#rpc.request({
          kind: "subscription-account-act",
          actionId: randomUUID(),
          expectedRevision: this.#view.revision,
          action: captured,
        })
        if (response.kind !== "subscription-account-outcome") {
          throw new ControlError("invalid-response", "Expected a Subscription Account outcome")
        }
        this.#view = response.outcome.view
        return response.outcome
      } catch (error) {
        if (
          error instanceof ControlError
          && error.authoritativeSubscriptionAccountView
          && error.authoritativeSubscriptionAccountView.viewSequence >= this.#view.viewSequence
        ) {
          if (error.authoritativeSubscriptionAccountView.viewSequence === this.#view.viewSequence) {
            this.#view = error.authoritativeSubscriptionAccountView
          } else {
            this.#replace(error.authoritativeSubscriptionAccountView)
          }
        }
        if (error instanceof ControlError && error.code === "stale-revision") {
          await this.#refreshCatalog()
        }
        throw error
      }
    })
    this.#actions = result.then(() => undefined, () => undefined)
    return result
  }

  subscribe(listener: (next: SubscriptionAccountCatalogView) => void): () => void {
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

  #receivePush(view: SubscriptionAccountCatalogView): void {
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
      const result = await this.#rpc.request({ kind: "open-subscription-accounts" })
      if (
        result.kind === "subscription-account-catalog"
        && result.view.viewSequence > this.#view.viewSequence
      ) {
        this.#replace(result.view)
      }
    } catch {
      // The transport owns connection failure reporting; refresh is best effort.
    }
  }

  #replace(view: SubscriptionAccountCatalogView): void {
    this.#view = view
    this.#lastNotifiedSequence = view.viewSequence
    for (const listener of this.#listeners) listener(view)
  }

  #ensureOpen(): void {
    if (this.#closed) {
      throw new ControlError("connection-closed", "Subscription Account session is closed")
    }
  }
}

function freezeOwnedValue<T>(value: T): T {
  if (typeof value !== "object" || value === null || Object.isFrozen(value)) return value
  for (const nested of Object.values(value)) freezeOwnedValue(nested)
  return Object.freeze(value)
}

export function createSubscriptionAccountSession(
  rpc: RpcTransport,
  initialView: SubscriptionAccountCatalogView,
): SubscriptionAccountSession {
  return new SubscriptionAccountSessionImpl(rpc, initialView)
}
