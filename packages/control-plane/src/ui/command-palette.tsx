/** @jsxImportSource @opentui/solid */
import { TextAttributes, type InputRenderable, type KeyEvent, type Renderable } from "@opentui/core"
import type { CommandEntry } from "@opentui/keymap"
import { useTerminalDimensions } from "@opentui/solid"
import { createEffect, createMemo, createSignal, For } from "solid-js"

import { useMuxviaKeymap } from "../commands/keymap"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

export interface CommandPaletteProps {
  entries: readonly CommandEntry<Renderable, KeyEvent>[]
  title: string
  searchPlaceholder: string
}

type PaletteEntry = CommandEntry<Renderable, KeyEvent>

function commandText(command: PaletteEntry["command"], field: "title" | "desc"): string {
  const value = command[field]
  return typeof value === "string" ? value : ""
}

export function CommandPalette(props: CommandPaletteProps) {
  const dimensions = useTerminalDimensions()
  const keymap = useMuxviaKeymap()
  const overlay = useOverlay()
  const [query, setQuery] = createSignal("")
  const [selected, setSelected] = createSignal(0)
  const filtered = createMemo(() => {
    const needle = query().trim().toLocaleLowerCase()
    if (!needle) return props.entries
    return props.entries.filter((entry) => (
      commandText(entry.command, "title").toLocaleLowerCase().includes(needle)
      || commandText(entry.command, "desc").toLocaleLowerCase().includes(needle)
    ))
  })
  const rowCount = () => Math.max(1, Math.floor(dimensions().height / 2) - 6)
  const windowStart = createMemo(() => Math.max(0, selected() - rowCount() + 1))
  const visible = createMemo(() => filtered().slice(windowStart(), windowStart() + rowCount()))

  createEffect(() => {
    const last = filtered().length - 1
    setSelected((current) => Math.max(0, Math.min(current, last)))
  })

  const onKeyDown = (event: KeyEvent) => {
    const entries = filtered()
    if (event.name === "up") {
      event.preventDefault()
      event.stopPropagation()
      if (entries.length) setSelected((current) => (current + entries.length - 1) % entries.length)
      return
    }
    if (event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      if (entries.length) setSelected((current) => (current + 1) % entries.length)
      return
    }
    if (event.name !== "return" && event.name !== "enter" && event.name !== "linefeed") return
    event.preventDefault()
    event.stopPropagation()
    const entry = entries[selected()]
    if (!entry) return
    overlay.clear()
    keymap.dispatchCommand(entry.command.name)
  }

  const binding = (entry: PaletteEntry) => entry.bindings[0]?.sequence
    .map((part) => part.display)
    .join(" ") ?? ""

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.title}</text>
    <input
      ref={(input: InputRenderable) => queueMicrotask(() => {
        if (!input.isDestroyed) input.focus()
      })}
      value={query()}
      onInput={setQuery}
      onKeyDown={onKeyDown}
      placeholder={props.searchPlaceholder}
      backgroundColor={theme.element}
      focusedBackgroundColor={theme.element}
      textColor={theme.text}
      focusedTextColor={theme.text}
      placeholderColor={theme.muted}
    />
    <box flexDirection="column">
      <For each={visible()}>{(entry, index) => {
        const active = () => windowStart() + index() === selected()
        const title = () => commandText(entry.command, "title") || entry.command.name
        const suffix = () => binding(entry)
        return <text
          width="100%"
          fg={active() ? theme.background : theme.text}
          bg={active() ? theme.primary : theme.panel}
          attributes={active() ? TextAttributes.BOLD : TextAttributes.NONE}
        >{suffix() ? `${title()}  ${suffix()}` : title()}</text>
      }}</For>
    </box>
  </box>
}
