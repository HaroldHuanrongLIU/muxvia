/** @jsxImportSource @opentui/solid */
import { expect, test } from "bun:test"
import type { InputRenderable } from "@opentui/core"
import { testRender } from "@opentui/solid"
import { createSignal, onMount, type JSX } from "solid-js"

import { MuxviaKeymapProvider, useCommandLayer, useMuxviaKeymap } from "../src/commands/keymap"
import { ActionPrompt } from "../src/ui/action-prompt"
import { OverlayProvider, useOverlay } from "../src/ui/overlay-stack"

function ConfirmationOverlay(props: { onCancel: () => void }) {
  useCommandLayer({
    scope: "confirm",
    priority: 300,
    handlers: { "app.exit.cancel": () => queueMicrotask(props.onCancel) },
  })
  return <text>Confirmation</text>
}

function StackHarness(props: {
  expose: (overlay: ReturnType<typeof useOverlay>) => void
  exposeEntries?: (entries: { a: JSX.Element; b: JSX.Element }) => void
  executed: string[]
}) {
  const overlay = useOverlay()
  const keymap = useMuxviaKeymap()
  const [route, setRoute] = createSignal<"home" | "codex">("codex")
  props.expose(overlay)
  props.exposeEntries?.({ a: <text>Overlay A</text>, b: <text>Overlay B</text> })
  useCommandLayer({
    scope: "global",
    priority: 0,
    enabled: () => overlay.depth === 0,
    handlers: { "command.palette.show": () => props.executed.push("palette") },
  })
  useCommandLayer({
    scope: "home",
    priority: 100,
    enabled: () => overlay.depth === 0,
    handlers: { "target.codex.open": () => props.executed.push("codex") },
  })
  useCommandLayer({
    scope: "codex",
    priority: 100,
    enabled: () => overlay.depth === 0 && route() === "codex",
    handlers: {
      "target.home": () => { props.executed.push("home"); setRoute("home") },
      "target.sidebar.toggle": () => props.executed.push("sidebar"),
    },
  })
  return <ActionPrompt
    scope="codex"
    placeholder="prompt"
    metadata="meta"
    focusEnabled={() => overlay.depth === 0}
    onUnknown={() => {}}
  />
}

test("renders only the top overlay and restores the original focus after two closes", async () => {
  let overlay: ReturnType<typeof useOverlay> | undefined
  let entries!: { a: JSX.Element; b: JSX.Element }
  const closed: string[] = []
  const setup = await testRender(() => (
    <MuxviaKeymapProvider>
      <OverlayProvider>
        <StackHarness
          executed={[]}
          expose={(value) => { overlay = value }}
          exposeEntries={(value) => { entries = value }}
        />
      </OverlayProvider>
    </MuxviaKeymapProvider>
  ), { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    const original = setup.renderer.currentFocusedRenderable as InputRenderable
    expect(original).toBeTruthy()

    overlay!.push({ id: "a", element: entries.a, onClose: () => closed.push("a") })
    overlay!.push({ id: "b", element: entries.b, onClose: () => closed.push("b") })
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Overlay B")
    expect(setup.captureCharFrame()).not.toContain("Overlay A")

    setup.mockInput.pressEscape()
    await setup.renderOnce()
    expect(closed).toEqual(["b"])
    expect(setup.captureCharFrame()).toContain("Overlay A")

    setup.mockInput.pressEscape()
    await setup.renderOnce()
    await Promise.resolve()
    expect(closed).toEqual(["b", "a"])
    expect(setup.renderer.currentFocusedRenderable).toBe(original)
  } finally {
    setup.renderer.destroy()
  }
})

test("nested overlays block unrelated route and global shortcuts and direct dispatch", async () => {
  let overlay: ReturnType<typeof useOverlay> | undefined
  let entries!: { a: JSX.Element; b: JSX.Element }
  const executed: string[] = []
  let keymap!: ReturnType<typeof useMuxviaKeymap>
  function ExposeKeymap() {
    keymap = useMuxviaKeymap()
    return null
  }
  const setup = await testRender(() => (
    <MuxviaKeymapProvider>
      <OverlayProvider>
        <ExposeKeymap />
        <StackHarness
          executed={executed}
          expose={(value) => { overlay = value }}
          exposeEntries={(value) => { entries = value }}
        />
      </OverlayProvider>
    </MuxviaKeymapProvider>
  ), { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    overlay!.push({ id: "a", element: entries.a })
    overlay!.push({ id: "b", element: entries.b })
    await setup.renderOnce()

    setup.mockInput.pressKey("1")
    setup.mockInput.pressKey("p", { ctrl: true })
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("b")
    keymap.dispatchCommand("target.home")
    await setup.renderOnce()
    expect(executed).toEqual([])

    setup.mockInput.pressEscape()
    setup.mockInput.pressEscape()
    await setup.renderOnce()
    expect(overlay!.depth).toBe(1)
    expect(setup.captureCharFrame()).toContain("Overlay A")

    setup.mockInput.pressEscape()
    await setup.renderOnce()
    expect(overlay!.depth).toBe(0)
  } finally {
    setup.renderer.destroy()
  }
})

test("clear closes every detached entry exactly once under reentrant callbacks", async () => {
  let overlay: ReturnType<typeof useOverlay> | undefined
  let entries!: { a: JSX.Element; b: JSX.Element }
  const closed: string[] = []
  function Expose() {
    const value = useOverlay()
    entries = { a: <text>A</text>, b: <text>B</text> }
    onMount(() => { overlay = value })
    return null
  }
  const setup = await testRender(() => (
    <MuxviaKeymapProvider><OverlayProvider><Expose /></OverlayProvider></MuxviaKeymapProvider>
  ), { width: 20, height: 5, useThread: false })
  try {
    await setup.renderOnce()
    overlay!.push({ id: "a", element: entries.a, onClose: () => { closed.push("a"); overlay!.clear() } })
    overlay!.push({ id: "b", element: entries.b, onClose: () => closed.push("b") })
    overlay!.clear()
    expect(closed).toEqual(["a", "b"])
    expect(overlay!.depth).toBe(0)
  } finally {
    setup.renderer.destroy()
  }
})

test("a nondismissible overlay leaves Escape exclusively to its modal command", async () => {
  let overlay: ReturnType<typeof useOverlay> | undefined
  let confirmation!: JSX.Element
  const closed: string[] = []
  function Expose() {
    overlay = useOverlay()
    confirmation = <ConfirmationOverlay onCancel={() => {
      closed.push("confirm")
      overlay!.closeTop()
    }} />
    return null
  }
  const setup = await testRender(() => (
    <MuxviaKeymapProvider><OverlayProvider><Expose /></OverlayProvider></MuxviaKeymapProvider>
  ), { width: 40, height: 10, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    overlay!.push({
      id: "confirmation",
      element: confirmation,
      dismissOnEscape: false,
      onClose: () => closed.push("entry"),
    })
    await setup.renderOnce()
    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    expect(overlay!.depth).toBe(0)
    expect(closed).toEqual(["confirm", "entry"])
  } finally {
    setup.renderer.destroy()
  }
})
