import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createEffect, createSignal, For, onMount, Show, type Accessor } from "solid-js"

import { useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { UniversalProviderCatalogView } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

type UniversalProvider = UniversalProviderCatalogView["providers"][number]

export interface UniversalProviderPickerProps {
  providers: Accessor<readonly UniversalProvider[]>
  selectedId: Accessor<string | undefined>
  pending: Accessor<boolean>
  notice: Accessor<string | undefined>
  noticeKind: Accessor<"error" | "success" | undefined>
  t: Translator
  onSelectedIdChange: (id: string | undefined) => void
  onCreate: () => void
  onEdit: () => void
  onDuplicate: () => void
  onDelete: () => void
  onSynchronize: () => void
}

export function UniversalProviderPicker(props: UniversalProviderPickerProps) {
  const overlay = useOverlay()
  const keymap = useMuxviaKeymap()
  const [selectedId, setSelectedId] = createSignal(props.selectedId() ?? props.providers()[0]?.id)
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined
  const selected = () => props.providers().find((provider) => provider.id === selectedId()) ?? props.providers()[0]

  createEffect(() => {
    const providers = props.providers()
    const current = selectedId()
    const next = current && providers.some((provider) => provider.id === current) ? current : providers[0]?.id
    if (current !== next) setSelectedId(next)
    if (props.selectedId() !== next) props.onSelectedIdChange(next)
  })
  onMount(() => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  }))

  useCommandLayer({
    scope: "universal-provider-picker",
    priority: 350,
    enabled: () => overlay.depth === 1 && !props.pending(),
    handlers: {
      "universal-provider.create": props.onCreate,
      "universal-provider.edit": props.onEdit,
      "universal-provider.duplicate": props.onDuplicate,
      "universal-provider.delete": props.onDelete,
      "universal-provider.synchronize": props.onSynchronize,
    },
  })
  useCommandLayer({
    scope: "overlay",
    priority: 450,
    enabled: () => overlay.depth === 1 && props.pending(),
    handlers: { "overlay.close": () => {} },
  })

  const move = (delta: -1 | 1) => {
    const providers = props.providers()
    if (providers.length < 2) return
    const index = Math.max(0, providers.findIndex((provider) => provider.id === selected()?.id))
    const next = providers[(index + delta + providers.length) % providers.length]!.id
    setSelectedId(next)
    props.onSelectedIdChange(next)
  }
  const onKeyDown = (event: KeyEvent) => {
    if (props.pending()) {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (event.name === "up" || event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      move(event.name === "up" ? -1 : 1)
      return
    }
    if (["return", "enter", "linefeed"].includes(event.name)) {
      event.preventDefault()
      event.stopPropagation()
      keymap.dispatchCommand("universal-provider.edit")
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
  const targetState = (provider: UniversalProvider, target: "codex" | "claude") =>
    provider.targets.find((candidate) => candidate.target === target)

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.primary}>{props.t("universal-provider.list.title")}</text>
    <Show when={props.notice()}><text fg={props.noticeKind() === "success" ? theme.success : theme.error}>{props.notice()}</text></Show>
    <Show when={props.pending()}><text fg={theme.warning}>{props.t("universal-provider.pending")}</text></Show>
    <box height={1}>
      <input
        ref={(value: InputRenderable) => { input = value }}
        value={keyCapture()}
        focused
        onInput={captureNavigation}
        onKeyDown={onKeyDown}
        backgroundColor={theme.panel}
        focusedBackgroundColor={theme.panel}
        textColor={theme.text}
        focusedTextColor={theme.text}
        placeholder={props.t("universal-provider.navigate")}
        placeholderColor={theme.muted}
        cursorColor={theme.primary}
        width="100%"
      />
    </box>
    <Show when={props.providers().length === 0}>
      <text fg={theme.muted}>{props.t("universal-provider.empty")}</text>
    </Show>
    <box flexDirection="column">
      <For each={props.providers()}>{(provider) => {
        const active = () => provider.id === selected()?.id
        return <text fg={active() ? theme.background : theme.text} bg={active() ? theme.primary : theme.panel}>
          {`${provider.name} · ${props.t(provider.credential === "present" ? "provider.credential.present" : "provider.credential.absent")}`}
        </text>
      }}</For>
    </box>
    <For each={selected() ? [selected()!] : []}>{(provider) => <box flexDirection="column" rowGap={1}>
      <text fg={theme.text}>{provider.name}</text>
      <text fg={theme.muted}>{provider.baseUrl}</text>
      <Show when={provider.provenance?.kind === "preset"}>
        <text fg={theme.muted}>{props.t("universal-provider.provenance.preset", { key: provider.provenance?.key ?? "" })}</text>
      </Show>
      <box flexDirection="column">
        <For each={["codex", "claude"] as const}>{(target) => {
          const state = () => targetState(provider, target)
          return <text fg={state()?.synchronization === "pending" ? theme.warning : theme.success}>
            {props.t("universal-provider.target.rail", {
              target: props.t(`target.${target}`),
              state: props.t(state()?.synchronization === "pending" ? "universal-provider.sync.pending" : "universal-provider.sync.current"),
              enabled: props.t(state()?.enabled ? "universal-provider.enabled" : "universal-provider.disabled"),
            })}
          </text>
        }}</For>
      </box>
      <For each={provider.targets}>{(target) => <Show when={target.activeReferences.length > 0}>
        <text fg={theme.warning}>{props.t("universal-provider.references", {
          target: props.t(`target.${target.target}`),
          references: target.activeReferences.join(", "),
        })}</text>
      </Show>}</For>
    </box>}</For>
    <text fg={theme.muted}>{props.t("universal-provider.list.help")}</text>
  </box>
}
