/** @jsxImportSource @opentui/solid */
import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"
import { createSignal, onCleanup, type JSX } from "solid-js"

import { resolveSlash } from "../src/commands/catalog"
import {
  MuxviaKeymapProvider,
  useCommandLayer,
  useMuxviaKeymap,
} from "../src/commands/keymap"
import type { CommandId } from "../src/commands/types"
import { createTranslator } from "../src/i18n"
import { ActionPrompt } from "../src/ui/action-prompt"
import { useCommandPaletteOpener } from "../src/ui/app"
import { OverlayProvider, useOverlay } from "../src/ui/overlay-stack"

type Handlers = Partial<Record<CommandId, () => void>>

function CommandHarness(props: {
  handlers: Handlers
  scope?: "home" | "codex" | "claude" | "reconciliation"
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
  scope?: "home" | "codex" | "claude" | "reconciliation"
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
  paletteReplacements: string[]
  paletteCloses: string[]
  expose: (keymap: ReturnType<typeof useMuxviaKeymap>) => void
  exposeSetHomeEnabled: (setEnabled: (enabled: boolean) => void) => void
}) {
  const overlay = useOverlay()
  const keymap = useMuxviaKeymap()
  const [homeEnabled, setHomeEnabled] = createSignal(true)
  const originalReplace = overlay.replace
  overlay.replace = (entry) => {
    props.paletteReplacements.push(entry.id)
    originalReplace({
      ...entry,
      onClose: () => {
        entry.onClose?.()
        props.paletteCloses.push(entry.id)
      },
    })
  }
  onCleanup(() => { overlay.replace = originalReplace })
  const showCommandPalette = useCommandPaletteOpener(createTranslator("en"))
  props.exposeSetHomeEnabled(setHomeEnabled)
  const reportTarget = (id: CommandId) => {
    if (id === "target.codex.open") props.reported.push(id)
  }
  useCommandLayer({
    scope: "global",
    priority: 0,
    enabled: () => overlay.depth === 0,
    handlers: {
      "command.palette.show": showCommandPalette,
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
  const paletteReplacements: string[] = []
  const paletteCloses: string[] = []
  let keymap!: ReturnType<typeof useMuxviaKeymap>
  let setHomeEnabled!: (enabled: boolean) => void
  const setup = await testRender(() => (
    <MuxviaKeymapProvider>
      <OverlayProvider>
        <PaletteIdentityHarness
          executed={executed}
          reported={reported}
          dispatched={dispatched}
          paletteReplacements={paletteReplacements}
          paletteCloses={paletteCloses}
          expose={(value) => { keymap = value }}
          exposeSetHomeEnabled={(value) => { setHomeEnabled = value }}
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
    expect(paletteReplacements).toEqual(["command-palette"])
    expect(paletteCloses).toEqual(["command-palette"])

    setHomeEnabled(false)
    keymap.dispatchCommand("command.palette.show")
    keymap.dispatchCommand("command.palette.show")
    await setup.renderOnce()
    const globalOnly = setup.captureCharFrame()
    expect(paletteReplacements).toEqual(["command-palette", "command-palette"])
    expect(paletteCloses).toEqual(["command-palette"])
    expect(globalOnly).toContain("command.app.exit")
    expect(globalOnly).not.toContain("command.target.codex")
    expect(globalOnly).not.toContain("command.target.claude")
    expect(globalOnly).not.toContain("command.palette")
    expect(globalOnly).not.toContain("command.provider.save")
    expect(globalOnly).not.toContain("command.target.sidebar")

    keymap.dispatchCommand("overlay.close")
    await setup.renderOnce()
    setHomeEnabled(true)
    keymap.dispatchCommand("command.palette.show")
    await setup.renderOnce()
    expect(paletteReplacements).toEqual([
      "command-palette",
      "command-palette",
      "command-palette",
    ])
    const freshHome = setup.captureCharFrame()
    expect(freshHome).toContain("command.target.codex")
    expect(freshHome).toContain("command.target.claude")
    expect(freshHome).toContain("command.app.exit")
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

test("slash and leader resolve one exact Codex Direct Activation command", async () => {
  const executed: string[] = []
  const setup = await commandHarness({
    handlers: { "target.direct.apply": () => executed.push("target.direct.apply") } as Handlers,
    onDispatch: () => {},
    scope: "codex",
  })
  try {
    expect(resolveSlash("/direct", "codex")).toBe("target.direct.apply")

    setup.keymap.dispatchCommand(resolveSlash("/direct", "codex")!)
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.renderOnce()

    expect(executed).toEqual(["target.direct.apply", "target.direct.apply"])
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude slash and leader resolve the existing Direct Activation command exactly once", async () => {
  const executed: string[] = []
  const dispatched: string[] = []
  const setup = await commandHarness({
    handlers: { "target.direct.apply": () => executed.push("target.direct.apply") } as Handlers,
    onDispatch: (id) => dispatched.push(id),
    scope: "claude",
  })
  try {
    expect(resolveSlash("/direct", "claude")).toBe("target.direct.apply")

    setup.keymap.dispatchCommand(resolveSlash("/direct", "claude")!)
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.renderOnce()

    expect(executed).toEqual(["target.direct.apply", "target.direct.apply"])
    expect(dispatched.filter((id) => id === "target.direct.apply")).toHaveLength(1)
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude accepts Provider Direct and Takeover route commands", () => {
  expect(resolveSlash("/provider", "claude")).toBe("provider.create")
  expect(resolveSlash("/takeover", "claude")).toBe("target.takeover.apply")
  expect(resolveSlash("/direct", "claude")).toBe("target.direct.apply")
})

test.each(["codex", "claude"] as const)(
  "Reconciliation uses the exact shared command family through the real %s keymap",
  async (scope) => {
    const executed: string[] = []
    const handlers = Object.fromEntries([
      "target.reconciliation.open",
      "target.reconciliation.preview.adopt",
      "target.reconciliation.preview.reapply",
      "target.reconciliation.preview.restore",
      "target.reconciliation.apply",
      "target.reconciliation.cancel",
    ].map((id) => [id, () => executed.push(id)])) as Handlers
    const setup = await commandHarness({ handlers, onDispatch: () => {}, scope })
    try {
      expect(resolveSlash("/reconcile", scope)).toBe("target.reconciliation.open")
      setup.keymap.dispatchCommand(resolveSlash("/reconcile", scope)!)
      await setup.renderOnce()
      expect(executed).toEqual(["target.reconciliation.open"])
    } finally {
      setup.renderer.destroy()
    }

    const modal = await commandHarness({ handlers, onDispatch: () => {}, scope: "reconciliation" })
    try {
      modal.keymap.dispatchCommand("target.reconciliation.preview.adopt")
      modal.keymap.dispatchCommand("target.reconciliation.preview.reapply")
      modal.keymap.dispatchCommand("target.reconciliation.preview.restore")
      modal.keymap.dispatchCommand("target.reconciliation.apply")
      modal.keymap.dispatchCommand("target.reconciliation.cancel")
      await modal.renderOnce()
      expect(executed).toEqual([
        "target.reconciliation.open",
        "target.reconciliation.preview.adopt",
        "target.reconciliation.preview.reapply",
        "target.reconciliation.preview.restore",
        "target.reconciliation.apply",
        "target.reconciliation.cancel",
      ])
    } finally {
      modal.renderer.destroy()
    }
  },
)

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
