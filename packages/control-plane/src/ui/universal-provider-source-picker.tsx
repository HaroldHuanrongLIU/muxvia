import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createSignal, For, onMount } from "solid-js"

import type { UniversalProviderCatalogView } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"

type Preset = UniversalProviderCatalogView["presets"][number]

export function UniversalProviderSourcePicker(props: {
  presets: readonly Preset[]
  t: Translator
  onSelect: (preset: Preset | undefined) => void
}) {
  const [index, setIndex] = createSignal(0)
  const [capture, setCapture] = createSignal("")
  let input: InputRenderable | undefined
  const count = () => props.presets.length + 1
  const move = (delta: -1 | 1) => setIndex((current) => (current + delta + count()) % count())
  const select = () => props.onSelect(index() === 0 ? undefined : props.presets[index() - 1])
  onMount(() => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  }))
  const onKeyDown = (event: KeyEvent) => {
    if (event.name === "up" || event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      move(event.name === "up" ? -1 : 1)
    } else if (["return", "enter", "linefeed"].includes(event.name)) {
      event.preventDefault()
      event.stopPropagation()
      select()
    }
  }
  const onInput = (value: string) => {
    if (value === "up" || value === "down") {
      setCapture("")
      move(value === "up" ? -1 : 1)
    } else setCapture(value)
  }
  return <box backgroundColor={theme.panel} flexDirection="column" padding={1} rowGap={1}>
    <text fg={theme.primary}>{props.t("universal-provider.source.title")}</text>
    <box height={1}>
      <input
        ref={(value: InputRenderable) => { input = value }}
        value={capture()}
        focused
        onKeyDown={onKeyDown}
        onInput={onInput}
        backgroundColor={theme.panel}
        focusedBackgroundColor={theme.panel}
        textColor={theme.text}
        focusedTextColor={theme.text}
        placeholder={props.t("universal-provider.source.navigate")}
        placeholderColor={theme.muted}
        width="100%"
      />
    </box>
    <text fg={index() === 0 ? theme.background : theme.text} bg={index() === 0 ? theme.primary : theme.panel}>
      {props.t("universal-provider.source.blank")}
    </text>
    <For each={props.presets}>{(preset, presetIndex) => <text
      fg={index() === presetIndex() + 1 ? theme.background : theme.text}
      bg={index() === presetIndex() + 1 ? theme.primary : theme.panel}
    >{`${preset.name} · ${preset.key}`}</text>}</For>
    <text fg={theme.muted}>{props.t("universal-provider.source.help")}</text>
  </box>
}
