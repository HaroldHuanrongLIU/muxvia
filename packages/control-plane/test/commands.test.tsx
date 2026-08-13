/** @jsxImportSource @opentui/solid */
import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"
import { createSignal, getOwner, onCleanup, runWithOwner, type JSX } from "solid-js"

import { resolveSlash } from "../src/commands/catalog"
import {
  MuxviaKeymapProvider,
  useCommandLayer,
  useMuxviaKeymap,
} from "../src/commands/keymap"
import type { CommandId } from "../src/commands/types"
import { ActionPrompt } from "../src/ui/action-prompt"
import { CommandPalette } from "../src/ui/command-palette"
import { OverlayProvider, useOverlay } from "../src/ui/overlay-stack"

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

function PaletteIdentityHarness(props: {
  executed: string[]
  reported: string[]
  dispatched: string[]
  snapshots: string[][]
  expose: (keymap: ReturnType<typeof useMuxviaKeymap>) => void
  exposeDisableHome: (disable: () => void) => void
}) {
  const owner = getOwner()
  const overlay = useOverlay()
  const keymap = useMuxviaKeymap()
  const [homeEnabled, setHomeEnabled] = createSignal(true)
  props.exposeDisableHome(() => setHomeEnabled(false))
  const reportTarget = (id: CommandId) => {
    if (id === "target.codex.open") props.reported.push(id)
  }
  useCommandLayer({
    scope: "global",
    priority: 0,
    enabled: () => overlay.depth === 0,
    handlers: {
      "command.palette.show": () => {
        const entries = keymap.getCommandEntries({ visibility: "active", namespace: "palette" })
          .filter((entry) => entry.command.name !== "command.palette.show" && !entry.command.hidden)
        props.snapshots.push(entries.map((entry) => entry.command.name))
        queueMicrotask(() => {
          const element = runWithOwner(owner!, () => (
            <CommandPalette entries={entries} title="Commands" searchPlaceholder="Search commands" />
          ))
          overlay.replace({ id: "palette", element })
        })
      },
    },
  })
  useCommandLayer({
    scope: "home",
    priority: 100,
    enabled: () => overlay.depth === 0 && homeEnabled(),
    handlers: { "target.codex.open": () => props.executed.push("target.codex.open") },
    onExecute: reportTarget,
  })
  useCommandLayer({
    scope: "codex",
    priority: 100,
    enabled: () => false,
    handlers: { "target.sidebar.toggle": () => props.executed.push("target.sidebar.toggle") },
  })
  useCommandLayer({
    scope: "editor",
    priority: 200,
    enabled: () => overlay.depth === 0,
    handlers: { "provider.save": () => props.executed.push("provider.save") },
  })
  props.expose(keymap)
  onCleanup(keymap.on("dispatch", (event) => {
    if (typeof event.command === "string") props.dispatched.push(event.command)
  }))
  return <ActionPrompt
    scope="home"
    placeholder="prompt"
    metadata="meta"
    focusEnabled={() => overlay.depth === 0}
    onUnknown={() => {}}
  />
}

test("shortcut slash and palette selection execute one exact target command identity", async () => {
  const executed: string[] = []
  const reported: string[] = []
  const dispatched: string[] = []
  const snapshots: string[][] = []
  let keymap!: ReturnType<typeof useMuxviaKeymap>
  let disableHome!: () => void
  const setup = await testRender(() => (
    <MuxviaKeymapProvider>
      <OverlayProvider>
        <PaletteIdentityHarness
          executed={executed}
          reported={reported}
          dispatched={dispatched}
          snapshots={snapshots}
          expose={(value) => { keymap = value }}
          exposeDisableHome={(value) => { disableHome = value }}
        />
      </OverlayProvider>
    </MuxviaKeymapProvider>
  ), { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    expect(dispatched).toEqual(["target.codex.open"])
    keymap.dispatchCommand(resolveSlash("/codex", "home")!)
    keymap.dispatchCommand("command.palette.show")
    await setup.renderOnce()
    await setup.mockInput.typeText("codex")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(executed).toEqual([
      "target.codex.open",
      "target.codex.open",
      "target.codex.open",
    ])
    expect(reported).toEqual(executed)
    expect(snapshots).toHaveLength(1)
    expect(snapshots[0]).toEqual([
      "target.codex.open",
      "target.claude.open",
      "app.exit.request",
    ])
    expect(snapshots[0]).not.toContain("command.palette.show")
    expect(snapshots[0]).not.toContain("provider.save")
    expect(snapshots[0]).not.toContain("target.sidebar.toggle")

    disableHome()
    keymap.dispatchCommand("command.palette.show")
    await setup.renderOnce()
    expect(snapshots[1]).toEqual(["app.exit.request"])
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
