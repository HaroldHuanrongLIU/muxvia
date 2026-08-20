import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createSignal, For, onMount } from "solid-js"

import { useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { TargetView } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

export type ProviderSource =
  | { kind: "blank" }
  | { kind: "preset"; preset: TargetView["providerPresets"][number] }

export function ProviderSourcePicker(props: {
  presets: readonly TargetView["providerPresets"][number][]
  t: Translator
  onSelect: (source: ProviderSource) => void
}) {
  const overlay = useOverlay()
  const keymap = useMuxviaKeymap()
  const sources = (): ProviderSource[] => [{ kind: "blank" }, ...props.presets.map((preset) => ({ kind: "preset" as const, preset }))]
  const [selected, setSelected] = createSignal(0)
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined

  onMount(() => input?.focus())
  useCommandLayer({
    scope: "provider-source-picker",
    priority: 300,
    enabled: () => overlay.depth === 1,
    handlers: { "provider.create": () => props.onSelect(sources()[selected()]!) },
  })

  const move = (delta: -1 | 1) => {
    const choices = sources()
    setSelected((current) => (current + delta + choices.length) % choices.length)
  }
  const onKeyDown = (event: KeyEvent) => {
    if (event.name === "up" || event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      move(event.name === "up" ? -1 : 1)
      return
    }
    if (event.name === "return" || event.name === "enter" || event.name === "linefeed") {
      event.preventDefault()
      event.stopPropagation()
      keymap.dispatchCommand("provider.create")
    }
  }
  const captureNavigation = (value: string) => {
    if (value === "up" || value === "down") {
      setKeyCapture("")
      move(value === "up" ? -1 : 1)
      return
    }
    setKeyCapture(value)
  }
  const label = (source: ProviderSource) => {
    if (source.kind === "blank") return props.t("provider.source.blank")
    switch (source.preset.key) {
      case "anthropic-api-messages": return props.t("provider.preset.anthropic-api-messages")
      case "codex-subscription-bridge": return props.t("provider.preset.codex-subscription-bridge")
      default: return props.t("provider.preset.openai-api-responses")
    }
  }

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("provider.source.title")}</text>
    <input
      ref={(value: InputRenderable) => { input = value; queueMicrotask(() => input?.focus()) }}
      value={keyCapture()}
      focused
      onInput={captureNavigation}
      onKeyDown={onKeyDown}
      backgroundColor={theme.panel}
      focusedBackgroundColor={theme.panel}
      textColor={theme.text}
      focusedTextColor={theme.text}
      placeholder={props.t("provider.source.help")}
      placeholderColor={theme.muted}
      cursorColor={theme.primary}
      width="100%"
    />
    <For each={sources()}>{(source, index) => (
      <text fg={selected() === index() ? theme.background : theme.text} bg={selected() === index() ? theme.primary : theme.panel}>
        {label(source)}
      </text>
    )}</For>
    <text fg={theme.muted}>{props.t("provider.source.help")}</text>
  </box>
}
