import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createSignal, For, onMount } from "solid-js"

import { useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { ModelDiscoveryResult } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

type Model = Extract<ModelDiscoveryResult, { status: "success" }>["models"][number]

export function ProviderModelPicker(props: {
  models: readonly Model[]
  t: Translator
  onSelect: (modelId: string) => void
}) {
  const overlay = useOverlay()
  const keymap = useMuxviaKeymap()
  const [selected, setSelected] = createSignal(0)
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined

  onMount(() => input?.focus())
  useCommandLayer({
    scope: "provider-model-picker",
    priority: 400,
    handlers: {
      "provider.models.select": () => {
        const model = props.models[selected()]
        if (model) props.onSelect(model.id)
      },
    },
  })

  const move = (delta: -1 | 1) => {
    if (props.models.length < 2) return
    setSelected((current) => (current + delta + props.models.length) % props.models.length)
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
      keymap.dispatchCommand("provider.models.select")
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

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("provider.models.title")}</text>
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
      placeholder={props.t("provider.models.picker-help")}
      placeholderColor={theme.muted}
      cursorColor={theme.primary}
      width="100%"
    />
    <For each={props.models}>{(model, index) => (
      <text fg={selected() === index() ? theme.background : theme.text} bg={selected() === index() ? theme.primary : theme.panel}>
        {model.displayName ? `${model.id} · ${model.displayName}` : model.id}
      </text>
    )}</For>
    <text fg={theme.muted}>{props.t("provider.models.picker-help")}</text>
  </box>
}
