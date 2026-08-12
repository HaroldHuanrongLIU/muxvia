import { randomUUID } from "node:crypto"
import { createConnection, type Socket } from "node:net"

import { encodeFrame, FrameDecoder } from "./framing"
import { createTargetSession, type MuxviaControl, type TargetSession } from "./target-session"
import {
  parseServerFrame,
  type ControlOperation,
  type ControlResult,
  type ServerFrame,
  type TargetView,
} from "./types"

type Pending = {
  resolve: (result: ControlResult) => void
  reject: (error: ControlError) => void
}

type SocketFactory = (socketPath: string) => Socket

export class ControlError extends Error {
  readonly code: string
  readonly retryable: boolean
  readonly authoritativeView?: TargetView

  constructor(code: string, message: string, authoritativeView?: TargetView) {
    super(message)
    this.name = "ControlError"
    this.code = code
    this.retryable = code === "stale-revision"
    this.authoritativeView = authoritativeView
  }
}

export interface RpcTransport {
  request(operation: ControlOperation): Promise<ControlResult>
  onTargetView(listener: (view: TargetView) => void): () => void
  whenClosed(): Promise<void>
  close(): Promise<void>
}

export class RpcClient implements RpcTransport, MuxviaControl {
  readonly #socket: Socket
  readonly #decoder = new FrameDecoder()
  readonly #pending = new Map<string, Pending>()
  readonly #viewListeners = new Set<(view: TargetView) => void>()
  #handshake?: {
    resolve: () => void
    reject: (error: ControlError) => void
  }
  #closed = false
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

  async openTarget(target: "codex"): Promise<TargetSession> {
    const result = await this.request({ kind: "open-target", target })
    if (result.kind !== "target-view") {
      throw new ControlError("invalid-response", "Expected a target view")
    }
    return createTargetSession(this, result.view)
  }

  request(operation: ControlOperation): Promise<ControlResult> {
    if (this.#closed) {
      return Promise.reject(new ControlError("connection-closed", "Control socket closed"))
    }
    const requestId = randomUUID()
    return new Promise<ControlResult>((resolve, reject) => {
      this.#pending.set(requestId, { resolve, reject })
      this.#socket.write(encodeFrame({ type: "request", requestId, operation }), (error) => {
        if (!error) return
        this.#pending.delete(requestId)
        reject(asControlError(error))
      })
    })
  }

  onTargetView(listener: (view: TargetView) => void): () => void {
    this.#viewListeners.add(listener)
    return () => this.#viewListeners.delete(listener)
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
        handshake.resolve()
      } else if (frame.type === "error") {
        handshake.reject(new ControlError(frame.problem.code, frame.problem.message, frame.authoritativeView))
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
    if (frame.type === "response") {
      const pending = this.#pending.get(frame.requestId)
      if (!pending) return
      this.#pending.delete(frame.requestId)
      pending.resolve(frame.result)
      return
    }
    if (frame.type === "error") {
      const failure = new ControlError(frame.problem.code, frame.problem.message, frame.authoritativeView)
      if (frame.requestId === null) {
        this.#socket.destroy()
        this.#fail(failure)
        return
      }
      const pending = this.#pending.get(frame.requestId)
      if (!pending) return
      this.#pending.delete(frame.requestId)
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
    for (const pending of this.#pending.values()) pending.reject(failure)
    this.#pending.clear()
    this.#viewListeners.clear()
    this.#resolveClosed()
  }
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
