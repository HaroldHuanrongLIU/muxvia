/** @jsxImportSource @opentui/solid */
import { RGBA, type Renderable } from "@opentui/core"
import { useRenderer, useTerminalDimensions } from "@opentui/solid"
import { createContext, createMemo, createSignal, For, onCleanup, useContext, type JSX } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import { theme } from "../theme"

export interface OverlayEntry {
  id: string
  token?: OverlayToken
  render: () => JSX.Element
  dismissOnEscape?: boolean | (() => boolean)
  onClose?: () => void
}

export type OverlayToken = symbol

export interface OverlayController {
  readonly depth: number
  push(entry: OverlayEntry): void
  replace(entry: OverlayEntry): void
  close(token: OverlayToken): void
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

function canDismiss(entry: OverlayEntry | undefined): boolean {
  if (!entry) return false
  const dismissible = entry.dismissOnEscape
  return (typeof dismissible === "function" ? dismissible() : dismissible) !== false
}

export function OverlayProvider(props: { children: JSX.Element }): JSX.Element {
  const renderer = useRenderer()
  const dimensions = useTerminalDimensions()
  const [stack, setStack] = createSignal<OverlayEntry[]>([])
  const closed = new WeakSet<OverlayEntry>()
  const returnFocus = new WeakMap<OverlayEntry, Renderable | null>()
  let restoreGeneration = 0
  let closeScheduled = false
  let tearingDown = false
  const top = createMemo(() => stack().at(-1))

  const closeEntry = (entry: OverlayEntry) => {
    if (closed.has(entry)) return
    closed.add(entry)
    entry.onClose?.()
  }
  const captureFocus = (entry: OverlayEntry, current = stack()) => {
    restoreGeneration++
    const candidate = renderer.currentFocusedRenderable
      ?? (current.length > 0 ? returnFocus.get(current.at(-1)!) : null)
      ?? null
    returnFocus.set(entry, candidate)
    renderer.currentFocusedRenderable?.blur()
  }
  const scheduleRestore = (candidate: Renderable | null | undefined) => {
    if (tearingDown) return
    const generation = ++restoreGeneration
    queueMicrotask(() => {
      if (generation !== restoreGeneration || tearingDown || renderer.isDestroyed) return
      if (candidate && !candidate.isDestroyed && reachesRoot(candidate, renderer.root)) candidate.focus()
    })
  }

  const controller: OverlayController = {
    get depth() { return stack().length },
    push(entry) {
      if (tearingDown) return
      captureFocus(entry)
      setStack((current) => [...current, entry])
    },
    replace(entry) {
      if (tearingDown) return
      const current = stack()
      const replaced = current.at(-1)
      if (replaced) {
        returnFocus.set(entry, returnFocus.get(replaced) ?? null)
        restoreGeneration++
      } else {
        captureFocus(entry, current)
      }
      setStack(replaced ? [...current.slice(0, -1), entry] : [entry])
      if (replaced) closeEntry(replaced)
    },
    close(token) {
      if (tearingDown) return
      const current = stack()
      const index = current.findIndex((entry) => entry.token === token)
      if (index < 0) return
      const entry = current[index]!
      const wasTop = index === current.length - 1
      const next = [...current.slice(0, index), ...current.slice(index + 1)]
      setStack(next)
      closeEntry(entry)
      if (wasTop) scheduleRestore(returnFocus.get(entry))
    },
    closeTop() {
      if (tearingDown) return
      const current = stack()
      const top = current.at(-1)
      if (!top) return
      const next = current.slice(0, -1)
      setStack(next)
      closeEntry(top)
      scheduleRestore(returnFocus.get(top))
    },
    clear() {
      if (tearingDown) return
      const current = stack()
      if (current.length === 0) return
      const candidate = returnFocus.get(current[0]!)
      setStack([])
      for (const entry of current) closeEntry(entry)
      scheduleRestore(candidate)
    },
  }
  const requestCloseTop = () => {
    if (closeScheduled || tearingDown) return
    const requested = top()
    if (!canDismiss(requested)) return
    closeScheduled = true
    queueMicrotask(() => {
      closeScheduled = false
      if (top() !== requested || !canDismiss(requested)) return
      controller.closeTop()
    })
  }

  useCommandLayer({
    scope: "overlay",
    priority: 300,
    enabled: () => controller.depth > 0 && canDismiss(top()),
    handlers: { "overlay.close": requestCloseTop },
  })

  onCleanup(() => {
    tearingDown = true
    restoreGeneration++
    const current = stack()
    setStack([])
    for (const entry of current) closeEntry(entry)
  })

  const panelWidth = () => Math.max(1, Math.min(60, dimensions().width - 2))
  const panelLeft = () => Math.max(0, Math.floor((dimensions().width - panelWidth()) / 2))
  const panelTop = () => Math.max(0, Math.floor(dimensions().height / 4))

  return <OverlayContext.Provider value={controller}>
    {props.children}
    <For each={stack()}>{(entry: OverlayEntry) => (
      <box
        visible={top() === entry}
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
    )}</For>
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
