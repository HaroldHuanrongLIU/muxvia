/** @jsxImportSource @opentui/solid */
import { RGBA, type Renderable } from "@opentui/core"
import { useRenderer, useTerminalDimensions } from "@opentui/solid"
import { createContext, createMemo, createSignal, onCleanup, Show, useContext, type JSX } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import { theme } from "../theme"

export interface OverlayEntry {
  id: string
  render: () => JSX.Element
  dismissOnEscape?: boolean
  onClose?: () => void
}

export interface OverlayController {
  readonly depth: number
  push(entry: OverlayEntry): void
  replace(entry: OverlayEntry): void
  closeTop(): void
  clear(): void
}

const OverlayContext = createContext<OverlayController>()

function reachesRoot(candidate: Renderable, root: Renderable): boolean {
  let current: Renderable | null = candidate
  while (current) {
    if (current === root) return true
    current = current.parent
  }
  return false
}

export function OverlayProvider(props: { children: JSX.Element }): JSX.Element {
  const renderer = useRenderer()
  const dimensions = useTerminalDimensions()
  const [stack, setStack] = createSignal<OverlayEntry[]>([])
  const closed = new WeakSet<OverlayEntry>()
  let priorFocus: Renderable | null = null
  let restoreGeneration = 0
  let closeScheduled = false
  let tearingDown = false
  const top = createMemo(() => stack().at(-1))

  const closeEntry = (entry: OverlayEntry) => {
    if (closed.has(entry)) return
    closed.add(entry)
    entry.onClose?.()
  }
  const captureFocus = () => {
    restoreGeneration++
    if (stack().length > 0) return
    if (priorFocus) return
    priorFocus = renderer.currentFocusedRenderable
    priorFocus?.blur()
  }
  const scheduleRestore = () => {
    if (tearingDown) return
    const generation = ++restoreGeneration
    queueMicrotask(() => {
      if (generation !== restoreGeneration || tearingDown || stack().length > 0 || renderer.isDestroyed) return
      const candidate = priorFocus
      priorFocus = null
      if (candidate && !candidate.isDestroyed && reachesRoot(candidate, renderer.root)) candidate.focus()
    })
  }

  const controller: OverlayController = {
    get depth() { return stack().length },
    push(entry) {
      if (tearingDown) return
      captureFocus()
      setStack((current) => [...current, entry])
    },
    replace(entry) {
      if (tearingDown) return
      captureFocus()
      const current = stack()
      const replaced = current.at(-1)
      setStack(replaced ? [...current.slice(0, -1), entry] : [entry])
      if (replaced) closeEntry(replaced)
    },
    closeTop() {
      if (tearingDown) return
      const current = stack()
      const top = current.at(-1)
      if (!top) return
      const next = current.slice(0, -1)
      setStack(next)
      closeEntry(top)
      if (next.length === 0) scheduleRestore()
    },
    clear() {
      if (tearingDown) return
      const current = stack()
      if (current.length === 0) return
      setStack([])
      for (const entry of current) closeEntry(entry)
      scheduleRestore()
    },
  }
  const requestCloseTop = () => {
    if (closeScheduled || tearingDown) return
    closeScheduled = true
    queueMicrotask(() => {
      closeScheduled = false
      controller.closeTop()
    })
  }

  useCommandLayer({
    scope: "overlay",
    priority: 300,
    enabled: () => controller.depth > 0 && top()?.dismissOnEscape !== false,
    handlers: { "overlay.close": requestCloseTop },
  })

  onCleanup(() => {
    tearingDown = true
    restoreGeneration++
    const current = stack()
    setStack([])
    for (const entry of current) closeEntry(entry)
    priorFocus = null
  })

  const panelWidth = () => Math.max(1, Math.min(60, dimensions().width - 2))
  const panelLeft = () => Math.max(0, Math.floor((dimensions().width - panelWidth()) / 2))
  const panelTop = () => Math.max(0, Math.floor(dimensions().height / 4))

  return <OverlayContext.Provider value={controller}>
    {props.children}
    <Show when={top()} keyed>{(entry: OverlayEntry) => (
      <box
        position="absolute"
        top={0}
        left={0}
        width="100%"
        height="100%"
        backgroundColor={RGBA.fromInts(0, 0, 0, 150)}
      >
        <box
          position="absolute"
          top={panelTop()}
          left={panelLeft()}
          width={panelWidth()}
          backgroundColor={theme.panel}
          flexDirection="column"
        >
          {entry.render()}
        </box>
      </box>
    )}</Show>
  </OverlayContext.Provider>
}

export function useOverlay(): OverlayController {
  const overlay = useContext(OverlayContext)
  if (!overlay) throw new Error("useOverlay must be used inside OverlayProvider")
  return overlay
}

export function useOptionalOverlay(): OverlayController | undefined {
  return useContext(OverlayContext)
}
