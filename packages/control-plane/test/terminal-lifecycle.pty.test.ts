import { expect, test } from "bun:test"
import { resolve } from "node:path"

const fixture = resolve(import.meta.dir, "fixtures/pty-control-plane.tsx")
const exitDeadlineMs = 8_000

const cases = [
  { name: "default-normal", screen: undefined, exit: "command", alternate: true },
  { name: "explicit-alternate", screen: "true", exit: "command", alternate: true },
  { name: "explicit-main", screen: "false", exit: "command", alternate: false },
  { name: "sigint", screen: undefined, exit: "SIGINT", alternate: true },
  { name: "sigterm", screen: undefined, exit: "SIGTERM", alternate: true },
  { name: "exception", screen: undefined, exit: "exception", alternate: true },
] as const

function deferred<T>() {
  let resolvePromise!: (value: T | PromiseLike<T>) => void
  const promise = new Promise<T>((resolve) => { resolvePromise = resolve })
  return { promise, resolve: resolvePromise }
}

test("real PTYs restore terminal flags and screen mode for every exit path", async () => {
  for (const scenario of cases) {
    const output: Uint8Array[] = []
    const ready = deferred<{ screenMode: string; runningFlags: number }>()
    let sessionClosed = 0
    const terminal = new Bun.Terminal({
      cols: 80,
      rows: 24,
      name: "xterm-256color",
      data: (_terminal, data) => { output.push(Uint8Array.from(data)) },
    })
    const baselineFlags = terminal.localFlags
    const env: Record<string, string | undefined> = {
      ...process.env,
      TERM: "xterm-256color",
    }
    delete env.OTUI_NO_NATIVE_RENDER
    if (scenario.screen === undefined) delete env.OTUI_USE_ALTERNATE_SCREEN
    else env.OTUI_USE_ALTERNATE_SCREEN = scenario.screen

    const proc = Bun.spawn([process.execPath, fixture], {
      terminal,
      env,
      ipc: (message) => {
        if (typeof message !== "object" || message === null || !("type" in message)) return
        if (message.type === "ready") {
          ready.resolve({ screenMode: String(message.screenMode), runningFlags: terminal.localFlags })
        }
        if (message.type === "session-closed") sessionClosed++
      },
    })

    let exitedInTime = true
    try {
      const mounted = await Promise.race([
        ready.promise,
        Bun.sleep(exitDeadlineMs).then(() => { throw new Error(`${scenario.name}: ready timeout`) }),
      ])
      expect(mounted.runningFlags).not.toBe(baselineFlags)
      expect(mounted.screenMode).not.toBe("split-footer")

      if (scenario.exit === "command") terminal.write("/quit\r")
      else if (scenario.exit === "exception") proc.send({ type: "crash" })
      else proc.kill(scenario.exit)

      const exitCode = await Promise.race([
        proc.exited,
        Bun.sleep(exitDeadlineMs).then(() => {
          exitedInTime = false
          throw new Error(`${scenario.name}: exit timeout`)
        }),
      ])
      const finalFlags = terminal.localFlags
      const ansi = new TextDecoder().decode(Buffer.concat(output.map((chunk) => Buffer.from(chunk))))

      expect(exitedInTime).toBeTrue()
      expect(finalFlags).toBe(baselineFlags)
      expect(sessionClosed).toBe(1)
      if (scenario.exit === "exception") expect(exitCode).toBe(70)
      if (scenario.exit === "command") expect(exitCode).toBe(0)
      if (scenario.alternate) {
        expect(ansi).toContain("\x1b[?1049h")
        expect(ansi).toContain("\x1b[?1049l")
      } else {
        expect(ansi).not.toContain("\x1b[?1049h")
        expect(ansi).not.toContain("\x1b[?1049l")
      }
    } finally {
      if (proc.exitCode === null) proc.kill("SIGKILL")
      await proc.exited.catch(() => {})
      terminal.close()
    }
  }
}, 60_000)
