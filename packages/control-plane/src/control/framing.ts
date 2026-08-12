import { FRAME_LIMIT } from "./types"

const encoder = new TextEncoder()
const decoder = new TextDecoder("utf-8", { fatal: true })

export function encodeFrame(value: unknown): Uint8Array {
  const body = encoder.encode(JSON.stringify(value))
  if (body.byteLength > FRAME_LIMIT) throw new Error("frame-too-large")

  const frame = new Uint8Array(4 + body.byteLength)
  new DataView(frame.buffer).setUint32(0, body.byteLength, false)
  frame.set(body, 4)
  return frame
}

export class FrameDecoder {
  #buffer = new Uint8Array()

  push(chunk: Uint8Array): unknown[] {
    const joined = new Uint8Array(this.#buffer.byteLength + chunk.byteLength)
    joined.set(this.#buffer)
    joined.set(chunk, this.#buffer.byteLength)
    this.#buffer = joined

    const values: unknown[] = []
    let offset = 0
    while (this.#buffer.byteLength - offset >= 4) {
      const length = new DataView(this.#buffer.buffer, this.#buffer.byteOffset + offset, 4).getUint32(0, false)
      if (length > FRAME_LIMIT) throw new Error("frame-too-large")
      if (this.#buffer.byteLength - offset - 4 < length) break

      const body = this.#buffer.subarray(offset + 4, offset + 4 + length)
      let text: string
      try {
        text = decoder.decode(body)
      } catch {
        throw new Error("invalid-utf8")
      }
      try {
        values.push(JSON.parse(text))
      } catch {
        throw new Error("invalid-json")
      }
      offset += 4 + length
    }
    this.#buffer = this.#buffer.slice(offset)
    return values
  }

  finish(): void {
    if (this.#buffer.byteLength > 0) throw new Error("unexpected-eof")
  }
}
