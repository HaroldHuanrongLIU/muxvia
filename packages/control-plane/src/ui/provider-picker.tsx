import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createSignal, For, onMount, type Accessor } from "solid-js"

import { useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { ReachabilityResult, TargetView } from "../control/types"
import { inspectionErrorKey, type Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

type Provider = TargetView["providers"][number]

export interface ProviderPickerProps {
  providers: Accessor<readonly Provider[]>
  selectedId: Accessor<string | undefined>
  t: Translator
  pending: Accessor<boolean>
  onSelectedIdChange: (id: string) => void
  onEdit: () => void
  onDuplicate: () => void
  reachability: Accessor<{ pending: boolean; result?: ReachabilityResult } | undefined>
  onCheckReachability: () => void
  onMove: (delta: -1 | 1) => void
  onDelete: () => void
}

export function ProviderPicker(props: ProviderPickerProps) {
  const overlay = useOverlay()
  const keymap = useMuxviaKeymap()
  const [selectedId, setSelectedId] = createSignal(props.selectedId() ?? props.providers()[0]?.id)
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined
  const selected = () => props.providers().find((provider) => provider.id === selectedId()) ?? props.providers()[0]

  onMount(() => {
    if (input && !input.isDestroyed) input.focus()
  })

  useCommandLayer({
    scope: "provider-picker",
    priority: 300,
    enabled: () => overlay.depth === 1,
    handlers: {
      "provider.edit": props.onEdit,
      "provider.duplicate": props.onDuplicate,
      "provider.reachability.check": props.onCheckReachability,
      "provider.move-up": () => props.onMove(-1),
      "provider.move-down": () => props.onMove(1),
      "provider.delete": props.onDelete,
    },
  })

  const moveSelection = (delta: -1 | 1) => {
    const current = selected()
    const providers = props.providers()
    if (!current || providers.length < 2) return
    const index = providers.findIndex((provider) => provider.id === current.id)
    const nextId = providers[(index + delta + providers.length) % providers.length]!.id
    setSelectedId(nextId)
    props.onSelectedIdChange(nextId)
  }
  const onKeyDown = (event: KeyEvent) => {
    if (event.name === "up") {
      event.preventDefault()
      event.stopPropagation()
      moveSelection(-1)
      return
    }
    if (event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      moveSelection(1)
      return
    }
    if (event.name === "return" || event.name === "enter" || event.name === "linefeed") {
      event.preventDefault()
      event.stopPropagation()
      keymap.dispatchCommand("provider.edit")
    }
  }
  const captureNavigation = (value: string) => {
    if (value === "up") {
      setKeyCapture("")
      moveSelection(-1)
      return
    }
    if (value === "down") {
      setKeyCapture("")
      moveSelection(1)
      return
    }
    setKeyCapture(value)
  }
  const provenance = (provider: Provider) => provider.provenance
    ? props.t("provider.provenance.preset")
    : props.t("provider.provenance.ordinary")

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("provider.list.title")}</text>
    <box height={1}>
      <input
        ref={(value: InputRenderable) => {
          input = value
          const focus = () => {
          const focused = input
          if (focused && !focused.isDestroyed) focused.focus()
        }
        queueMicrotask(focus)
      }}
        value={keyCapture()}
        focused
        onInput={captureNavigation}
        onKeyDown={onKeyDown}
        backgroundColor={theme.panel}
        focusedBackgroundColor={theme.panel}
        textColor={theme.text}
        focusedTextColor={theme.text}
        placeholder="Navigate Providers"
        placeholderColor={theme.muted}
        cursorColor={theme.primary}
        width="100%"
      />
    </box>
    <box flexDirection="column">
      <For each={props.providers()}>{(provider) => {
        const active = () => provider.id === selected()?.id
        return <text fg={active() ? theme.background : theme.text} bg={active() ? theme.primary : theme.panel}>
          {`${provider.name} · ${props.t(provider.completeness === "complete" ? "provider.complete" : "provider.incomplete")} · ${provenance(provider)}`}
        </text>
      }}</For>
    </box>
    <For each={selected() ? [selected()!] : []}>{(provider) => <box flexDirection="column">
      <text fg={theme.text}>{provider.name}</text>
      <text fg={theme.muted}>{provider.baseUrl}</text>
      <text fg={theme.muted}>{provider.model}</text>
      <text fg={theme.muted}>{props.t(provider.credential === "present" ? "provider.credential-reference.present" : "provider.credential-reference.missing")}</text>
      <text fg={theme.muted}>{provider.generated ? props.t("provider.generated") : props.t("provider.not-generated")}</text>
      <text fg={theme.muted}>{provider.activeReferences.length
        ? provider.activeReferences.map((reference) => props.t(`provider.reference.${reference}`)).join(" · ")
        : props.t("provider.reference.none")}</text>
      <For each={props.reachability() ? [props.reachability()!] : []}>{(state) => <text fg={theme.muted}>{
        state.pending
          ? props.t("provider.reachability.checking")
          : state.result?.status === "reachable"
            ? props.t("provider.reachability.reachable", {
              status: state.result.httpStatus,
              ttfb: state.result.ttfbMs,
              retries: state.result.retryCount,
              slow: state.result.slow ? props.t("provider.reachability.slow") : "",
            })
            : state.result?.status === "unreachable"
              ? props.t("provider.reachability.unreachable", {
                reason: props.t(inspectionErrorKey(state.result.failure.category)),
                status: state.result.failure.httpStatus === null ? "" : ` · HTTP ${state.result.failure.httpStatus}`,
                retries: state.result.retryCount,
              })
              : ""
      }</text>}</For>
    </box>}</For>
    <text fg={theme.muted}>{props.t("provider.list.help")}</text>
  </box>
}
