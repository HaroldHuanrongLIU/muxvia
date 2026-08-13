/** @jsxImportSource @opentui/solid */
import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"
import { onCleanup, type JSX } from "solid-js"

import { resolveSlash } from "../src/commands/catalog"
import {
  MuxviaKeymapProvider,
  useCommandLayer,
  useMuxviaKeymap,
} from "../src/commands/keymap"
import type { CommandId } from "../src/commands/types"

type Handlers = Partial<Record<CommandId, () => void>>

function CommandHarness(props: {
  handlers: Handlers
  scope?: "home" | "codex" | "claude"
  Overlay?: () => JSX.Element
  onExecute?: (id: CommandId) => void
  onDispatch: (name: string) => void
  expose: (keymap: ReturnType<typeof useMuxviaKeymap>) => void
}) {
  const keymap = useMuxviaKeymap()
  useCommandLayer({
    scope: "global",
    priority: 0,
    handlers: props.handlers,
    onExecute: props.onExecute,
  })
  useCommandLayer({
    scope: props.scope ?? "home",
    priority: 100,
    handlers: props.handlers,
    onExecute: props.onExecute,
  })
  props.expose(keymap)
  onCleanup(keymap.on("dispatch", (event) => {
    if (typeof event.command === "string") props.onDispatch(event.command)
  }))
  const Overlay = props.Overlay
  return <>{Overlay && <Overlay />}</>
}

async function commandHarness(options: {
  handlers: Handlers
  scope?: "home" | "codex" | "claude"
  Overlay?: () => JSX.Element
  onExecute?: (id: CommandId) => void
  onDispatch: (name: string) => void
}) {
  let keymap: ReturnType<typeof useMuxviaKeymap> | undefined
  const setup = await testRender(() => (
    <MuxviaKeymapProvider>
      <CommandHarness {...options} expose={(next) => { keymap = next }} />
    </MuxviaKeymapProvider>
  ), { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  await setup.renderOnce()
  if (!keymap) throw new Error("keymap was not exposed")
  return { ...setup, keymap }
}

test("shortcut slash and palette dispatch one target command identity", async () => {
  const executed: string[] = []
  const reported: string[] = []
  const dispatched: string[] = []
  const setup = await commandHarness({
    handlers: { "target.codex.open": () => executed.push("target.codex.open") },
    onExecute: (id) => reported.push(id),
    onDispatch: (name) => dispatched.push(name),
  })
  try {
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    setup.keymap.dispatchCommand(resolveSlash("/codex", "home")!)
    setup.keymap.dispatchCommand("target.codex.open")
    expect(executed).toEqual([
      "target.codex.open",
      "target.codex.open",
      "target.codex.open",
    ])
    expect(reported).toEqual(executed)
    expect(dispatched).toEqual(["target.codex.open"])
  } finally {
    setup.renderer.destroy()
  }
})

test("ctrl+p resolves the global palette command", async () => {
  const executed: string[] = []
  const setup = await commandHarness({
    handlers: { "command.palette.show": () => executed.push("command.palette.show") },
    onDispatch: () => {},
  })
  try {
    setup.mockInput.pressKey("p", { ctrl: true })
    await setup.renderOnce()
    expect(executed).toEqual(["command.palette.show"])
  } finally {
    setup.renderer.destroy()
  }
})

test("leader sequence resolves the scoped sidebar command", async () => {
  const executed: string[] = []
  const setup = await commandHarness({
    handlers: { "target.sidebar.toggle": () => executed.push("target.sidebar.toggle") },
    onDispatch: () => {},
    scope: "codex",
  })
  try {
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("b")
    await setup.renderOnce()
    expect(executed).toEqual(["target.sidebar.toggle"])
  } finally {
    setup.renderer.destroy()
  }
})

test("slash commands reject names outside the active scope", () => {
  expect(resolveSlash("/provider", "claude")).toBeUndefined()
})

test("an overlay layer consumes escape before route and global layers", async () => {
  const executed: string[] = []
  let enabled = true
  function Overlay() {
    useCommandLayer({
      scope: "overlay",
      priority: 300,
      enabled: () => enabled,
      handlers: { "overlay.close": () => executed.push("overlay.close") },
    })
    return null
  }
  const setup = await commandHarness({
    handlers: { "target.home": () => executed.push("target.home") },
    onDispatch: () => {},
    scope: "codex",
    Overlay,
  })
  try {
    setup.mockInput.pressEscape()
    await setup.renderOnce()
    expect(executed).toEqual(["overlay.close"])
  } finally {
    setup.renderer.destroy()
  }
})
