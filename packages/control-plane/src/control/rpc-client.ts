import { randomUUID } from "node:crypto"
import { createConnection, type Socket } from "node:net"

import { encodeFrame, FrameDecoder } from "./framing"
import { createTargetSession, type MuxviaControl, type TargetSession } from "./target-session"
import {
  createUniversalProviderSession,
  type UniversalProviderSession,
} from "./universal-provider-session"
import {
  createSubscriptionAccountSession,
  type SubscriptionAccountSession,
} from "./subscription-account-session"
import {
  parseServerFrame,
  type ControlOperation,
  type ClaudePreflightContext,
  type ControlResult,
  type ServerFrame,
  type TargetView,
  type Target,
  type UniversalProviderCatalogView,
  type SubscriptionAccountCatalogView,
} from "./types"

type Pending = {
  resolve: (result: ControlResult) => void
  reject: (error: ControlError) => void
  signal?: AbortSignal
  onAbort?: () => void
}

export type RequestOptions = { signal?: AbortSignal }
export type InspectionOperation = Extract<
  ControlOperation,
  { kind: "discover-models" | "check-reachability" | "preview-reconciliation" | "probe-compatibility" | "poll-device-authorization" | "list-request-records" | "inspect-request-record" | "list-usage-activity" | "refresh-native-usage" | "set-usage-retention" | "clear-usage" | "update-pricing-catalog" }
>
type NonInspectionOperation = Exclude<ControlOperation, InspectionOperation>

type SocketFactory = (socketPath: string) => Socket

export type ServiceMetadata = Readonly<{
  rpc: Readonly<{ major: 1; minor: 0 }>
  release: string
  serviceEpoch: string
}>

export type PreparedHandover = Readonly<{ release: string }>

export class ControlError extends Error {
  readonly code: string
  readonly retryable: boolean
  readonly authoritativeView?: TargetView
  readonly authoritativeUniversalProviderView?: UniversalProviderCatalogView
  readonly authoritativeSubscriptionAccountView?: SubscriptionAccountCatalogView
  readonly source?: string
  readonly selector?: string
  readonly recoveryBackupPath?: string

  constructor(
    code: string,
    message: string,
    authoritativeView?: TargetView,
    source?: string,
    selector?: string,
    authoritativeUniversalProviderView?: UniversalProviderCatalogView,
    authoritativeSubscriptionAccountView?: SubscriptionAccountCatalogView,
    recoveryBackupPath?: string,
  ) {
    super(message)
    this.name = "ControlError"
    this.code = code
    this.retryable = code === "stale-revision" || code === "stale-subscription-catalog-revision"
    this.authoritativeView = authoritativeView
    this.authoritativeUniversalProviderView = authoritativeUniversalProviderView
    this.authoritativeSubscriptionAccountView = authoritativeSubscriptionAccountView
    this.source = source
    this.selector = selector
    this.recoveryBackupPath = recoveryBackupPath
  }
}

export interface RpcTransport {
  request(operation: InspectionOperation, options?: RequestOptions): Promise<ControlResult>
  request(operation: NonInspectionOperation): Promise<ControlResult>
  onTargetView(listener: (view: TargetView) => void): () => void
  onUniversalProviderView(listener: (view: UniversalProviderCatalogView) => void): () => void
  onSubscriptionAccountView(listener: (view: SubscriptionAccountCatalogView) => void): () => void
  whenClosed(): Promise<void>
  close(): Promise<void>
}

export class RpcClient implements RpcTransport, MuxviaControl {
  readonly #socket: Socket
  readonly #decoder = new FrameDecoder()
  readonly #pending = new Map<string, Pending>()
  readonly #viewListeners = new Set<(view: TargetView) => void>()
  readonly #universalProviderViewListeners = new Set<
    (view: UniversalProviderCatalogView) => void
  >()
  readonly #subscriptionAccountViewListeners = new Set<
    (view: SubscriptionAccountCatalogView) => void
  >()
  #handshake?: {
    resolve: () => void
    reject: (error: ControlError) => void
  }
  #closed = false
  #serviceMetadata?: ServiceMetadata
  readonly #closedPromise: Promise<void>
  readonly #resolveClosed: () => void

  private constructor(socket: Socket) {
    this.#socket = socket
    let resolveClosed!: () => void
    this.#closedPromise = new Promise<void>((resolve) => {
      resolveClosed = resolve
    })
    this.#resolveClosed = resolveClosed
    socket.on("data", (chunk) => {
      if (typeof chunk === "string") {
        this.#fail(new ControlError("invalid-response", "Control socket returned text data"))
        return
      }
      this.#receive(chunk)
    })
    socket.on("error", (error) => this.#fail(error))
    socket.on("close", () => this.#fail(new ControlError("connection-closed", "Control socket closed")))
  }

  get serviceMetadata(): ServiceMetadata {
    if (!this.#serviceMetadata) {
      throw new ControlError("invalid-response", "Service metadata is unavailable")
    }
    return this.#serviceMetadata
  }

  static async connect(
    socketPath: string,
    release: string,
    signal?: AbortSignal,
    createSocket: SocketFactory = (path) => createConnection({ path }),
  ): Promise<RpcClient> {
    if (signal?.aborted) {
      throw new ControlError("service-unavailable", "Control connection was cancelled")
    }
    const socket = createSocket(socketPath)
    const client = new RpcClient(socket)
    const cancelled = new ControlError("service-unavailable", "Control connection was cancelled")
    const onAbort = () => {
      socket.destroy()
      client.#fail(cancelled)
    }
    signal?.addEventListener("abort", onAbort, { once: true })
    try {
      await new Promise<void>((resolve, reject) => {
        client.#handshake = { resolve, reject }
        socket.once("connect", () => {
          socket.write(encodeFrame({
            type: "hello",
            rpc: { major: 1, minor: 0 },
            release,
          }))
        })
      })
      if (signal?.aborted) throw cancelled
      return client
    } catch (error) {
      const failure = asControlError(error)
      if (!client.#closed) {
        socket.destroy()
        client.#fail(failure)
      }
      throw failure
    } finally {
      signal?.removeEventListener("abort", onAbort)
    }
  }

  async openTarget(target: Target, claudeContext?: ClaudePreflightContext): Promise<TargetSession> {
    const result = await this.request({ kind: "open-target", target, claudeContext })
    if (result.kind !== "target-view") {
      throw new ControlError("invalid-response", "Expected a target view")
    }
    return createTargetSession(this, result.view, claudeContext)
  }

  async openUniversalProviders(claudeContext?: ClaudePreflightContext): Promise<UniversalProviderSession> {
    const result = await this.request({
      kind: "open-universal-providers",
      ...(claudeContext ? { claudeContext } : {}),
    })
    if (result.kind !== "universal-provider-catalog") {
      throw new ControlError("invalid-response", "Expected a Universal Provider catalog")
    }
    return createUniversalProviderSession(this, result.view, claudeContext)
  }

  async openSubscriptionAccounts(): Promise<SubscriptionAccountSession> {
    const result = await this.request({ kind: "open-subscription-accounts" })
    if (result.kind !== "subscription-account-catalog") {
      throw new ControlError("invalid-response", "Expected a Subscription Account catalog")
    }
    return createSubscriptionAccountSession(this, result.view)
  }

  async prepareHandover(
    candidatePath: string,
    expectedRelease: string,
  ): Promise<PreparedHandover> {
    const result = await this.request({
      kind: "prepare-handover",
      candidatePath,
      expectedRelease,
    })
    if (result.kind !== "handover-prepared" || result.release !== expectedRelease) {
      throw new ControlError("invalid-response", "Handover response did not match request")
    }
    return Object.freeze({ release: result.release })
  }

  request(operation: InspectionOperation, options?: RequestOptions): Promise<ControlResult>
  request(operation: NonInspectionOperation): Promise<ControlResult>
  request(operation: ControlOperation, options: RequestOptions = {}): Promise<ControlResult> {
    if (this.#closed) {
      return Promise.reject(new ControlError("connection-closed", "Control socket closed"))
    }
    if (options.signal && !isInspectionOperation(operation)) {
      return Promise.reject(new ControlError(
        "invalid-request",
        "AbortSignal is only supported for inspection operations",
      ))
    }
    if (options.signal?.aborted) {
      return Promise.reject(new ControlError("cancelled", "Control request was cancelled"))
    }
    const requestId = randomUUID()
    return new Promise<ControlResult>((resolve, reject) => {
      const onAbort = () => {
        const pending = this.#takePending(requestId)
        if (!pending) return
        this.#socket.write(encodeFrame({ type: "cancel", requestId }))
        pending.reject(new ControlError("cancelled", "Control request was cancelled"))
      }
      const pending: Pending = { resolve, reject, signal: options.signal, onAbort }
      this.#pending.set(requestId, pending)
      options.signal?.addEventListener("abort", onAbort, { once: true })
      if (options.signal?.aborted) {
        onAbort()
        return
      }
      this.#socket.write(encodeFrame({ type: "request", requestId, operation }), (error) => {
        if (!error) return
        this.#takePending(requestId)?.reject(asControlError(error))
      })
    })
  }

  onTargetView(listener: (view: TargetView) => void): () => void {
    this.#viewListeners.add(listener)
    return () => this.#viewListeners.delete(listener)
  }

  onUniversalProviderView(
    listener: (view: UniversalProviderCatalogView) => void,
  ): () => void {
    this.#universalProviderViewListeners.add(listener)
    return () => this.#universalProviderViewListeners.delete(listener)
  }

  onSubscriptionAccountView(
    listener: (view: SubscriptionAccountCatalogView) => void,
  ): () => void {
    this.#subscriptionAccountViewListeners.add(listener)
    return () => this.#subscriptionAccountViewListeners.delete(listener)
  }

  whenClosed(): Promise<void> {
    return this.#closedPromise
  }

  async close(): Promise<void> {
    if (this.#closed) return
    this.#socket.destroy()
    this.#fail(new ControlError("connection-closed", "Control socket closed"))
    await this.#closedPromise
  }

  #receive(chunk: Uint8Array): void {
    try {
      for (const value of this.#decoder.push(chunk)) {
        this.#handleFrame(parseServerFrame(value))
      }
    } catch (error) {
      this.#socket.destroy()
      this.#fail(asControlError(error))
    }
  }

  #handleFrame(frame: ServerFrame): void {
    if (this.#handshake) {
      const handshake = this.#handshake
      this.#handshake = undefined
      if (frame.type === "hello-ack") {
        this.#serviceMetadata = Object.freeze({
          rpc: Object.freeze({ major: frame.rpc.major, minor: frame.rpc.minor }),
          release: frame.release,
          serviceEpoch: frame.serviceEpoch,
        })
        handshake.resolve()
      } else if (frame.type === "error") {
        handshake.reject(new ControlError(
          frame.problem.code, frame.problem.message, frame.authoritativeView,
          frame.problem.source, frame.problem.selector,
          frame.authoritativeUniversalProviderView,
          frame.authoritativeSubscriptionAccountView,
          frame.recoveryBackupPath,
        ))
      } else {
        handshake.reject(new ControlError("invalid-response", "Expected hello acknowledgement"))
      }
      return
    }

    if (frame.type === "target-view") {
      setTimeout(() => {
        if (this.#closed) return
        for (const listener of this.#viewListeners) listener(frame.view)
      }, 0)
      return
    }
    if (frame.type === "universal-provider-view") {
      setTimeout(() => {
        if (this.#closed) return
        for (const listener of this.#universalProviderViewListeners) listener(frame.view)
      }, 0)
      return
    }
    if (frame.type === "subscription-account-view") {
      setTimeout(() => {
        if (this.#closed) return
        for (const listener of this.#subscriptionAccountViewListeners) listener(frame.view)
      }, 0)
      return
    }
    if (frame.type === "response") {
      const pending = this.#takePending(frame.requestId)
      if (!pending) return
      pending.resolve(frame.result)
      return
    }
    if (frame.type === "error") {
      const failure = new ControlError(
        frame.problem.code, frame.problem.message, frame.authoritativeView,
        frame.problem.source, frame.problem.selector,
        frame.authoritativeUniversalProviderView,
        frame.authoritativeSubscriptionAccountView,
        frame.recoveryBackupPath,
      )
      if (frame.requestId === null) {
        this.#socket.destroy()
        this.#fail(failure)
        return
      }
      const pending = this.#takePending(frame.requestId)
      if (!pending) return
      pending.reject(failure)
    }
  }

  #fail(error: unknown): void {
    if (this.#closed) return
    this.#closed = true
    const failure = asControlError(error)
    const handshake = this.#handshake
    this.#handshake = undefined
    handshake?.reject(failure)
    for (const requestId of [...this.#pending.keys()]) this.#takePending(requestId)?.reject(failure)
    this.#pending.clear()
    this.#viewListeners.clear()
    this.#universalProviderViewListeners.clear()
    this.#subscriptionAccountViewListeners.clear()
    this.#resolveClosed()
  }

  #takePending(requestId: string): Pending | undefined {
    const pending = this.#pending.get(requestId)
    if (!pending) return undefined
    this.#pending.delete(requestId)
    if (pending.signal && pending.onAbort) {
      pending.signal.removeEventListener("abort", pending.onAbort)
    }
    return pending
  }
}

function isInspectionOperation(operation: ControlOperation): operation is InspectionOperation {
  return operation.kind === "discover-models"
    || operation.kind === "check-reachability"
    || operation.kind === "preview-reconciliation"
    || operation.kind === "probe-compatibility"
    || operation.kind === "poll-device-authorization"
    || operation.kind === "list-request-records"
    || operation.kind === "inspect-request-record"
    || operation.kind === "list-usage-activity"
    || operation.kind === "refresh-native-usage"
    || operation.kind === "set-usage-retention"
    || operation.kind === "clear-usage"
    || operation.kind === "update-pricing-catalog"
}

function asControlError(error: unknown): ControlError {
  if (error instanceof ControlError) return error
  const message = error instanceof Error ? error.message : "Control socket failed"
  const code = typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : undefined
  if (code === "ENOENT" || code === "ECONNREFUSED") {
    return new ControlError("service-unavailable", message)
  }
  if (message === "frame-too-large" || message === "invalid-json" || message === "invalid-utf8") {
    return new ControlError(message, message)
  }
  return new ControlError("connection-closed", message)
}
