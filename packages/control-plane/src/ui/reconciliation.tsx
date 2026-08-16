import type { InputRenderable, KeyEvent } from "@opentui/core"
import { useTerminalDimensions } from "@opentui/solid"
import { For, onMount, Show, type Accessor } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type { ReconciliationPreview, ReconciliationStrategy, Target } from "../control/types"
import { messageKeyForProblem, type MessageKey, type Translator } from "../i18n"
import { theme } from "../theme"

export interface ReconciliationUiState {
  target: Target
  strategy?: ReconciliationStrategy
  preview?: ReconciliationPreview
  pending?: "preview" | "apply"
  errorCode?: string
  acknowledgedVersion?: string
}

export interface ReconciliationProps {
  state: Accessor<ReconciliationUiState>
  t: Translator
  onPreview: (strategy: ReconciliationStrategy) => void
  onAcknowledge: (version: string) => void
  onApply: () => void
  onCancel: () => void
}

function strategyKey(strategy: ReconciliationStrategy): MessageKey {
  switch (strategy) {
    case "adopt": return "reconciliation.strategy.adopt"
    case "reapply": return "reconciliation.strategy.reapply"
    case "restore": return "reconciliation.strategy.restore"
  }
}

function compatibilityKey(preview: ReconciliationPreview): MessageKey {
  switch (preview.compatibility.classification) {
    case "tested": return "reconciliation.compatibility.tested"
    case "unknown-compatible": return "reconciliation.compatibility.unknown-compatible"
    case "incompatible": return "reconciliation.compatibility.incompatible"
  }
}

function fieldKey(field: ReconciliationPreview["changes"][number]["field"]): MessageKey {
  switch (field) {
    case "provider": return "reconciliation.field.provider"
    case "credential": return "reconciliation.field.credential"
    case "current-provider": return "reconciliation.field.current-provider"
    case "activated-snapshot": return "reconciliation.field.activated-snapshot"
    case "takeover": return "reconciliation.field.takeover"
  }
}

function stateKey(state: ReconciliationPreview["changes"][number]["state"]): MessageKey {
  switch (state) {
    case "present": return "reconciliation.state.present"
    case "absent": return "reconciliation.state.absent"
    case "unchanged": return "reconciliation.state.unchanged"
    case "changed": return "reconciliation.state.changed"
  }
}

function effectKey(effect: ReconciliationPreview["providerEffect"]): MessageKey {
  switch (effect) {
    case "create-new": return "reconciliation.effect.create-new"
    case "keep-current": return "reconciliation.effect.keep-current"
    case "exit-managed": return "reconciliation.effect.exit-managed"
  }
}

function shadowKey(source: ReconciliationPreview["shadowSources"][number]): MessageKey {
  if (typeof source === "object") return "reconciliation.shadow.claude-selector"
  switch (source) {
    case "codex-profile": return "reconciliation.shadow.codex-profile"
    case "claude-managed": return "reconciliation.shadow.claude-managed"
    case "claude-shared": return "reconciliation.shadow.claude-shared"
    case "claude-project": return "reconciliation.shadow.claude-project"
    case "claude-local": return "reconciliation.shadow.claude-local"
    case "claude-host-managed": return "reconciliation.shadow.claude-host-managed"
  }
}

function shadowLabel(
  t: Translator,
  source: ReconciliationPreview["shadowSources"][number],
): string {
  if (typeof source === "object") {
    return t("reconciliation.shadow.claude-selector", { selector: source["claude-selector"] })
  }
  return t(shadowKey(source))
}

export function Reconciliation(props: ReconciliationProps) {
  const dimensions = useTerminalDimensions()
  let input: InputRenderable | undefined

  useCommandLayer({
    scope: "reconciliation",
    priority: 400,
    handlers: {
      "target.reconciliation.preview.adopt": () => props.onPreview("adopt"),
      "target.reconciliation.preview.reapply": () => props.onPreview("reapply"),
      "target.reconciliation.preview.restore": () => props.onPreview("restore"),
      "target.reconciliation.apply": props.onApply,
      "target.reconciliation.cancel": props.onCancel,
    },
  })

  onMount(() => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  }))

  const acknowledge = () => {
    const preview = props.state().preview
    if (
      props.state().pending
      || !preview?.compatibility.acknowledgementRequired
      || preview.compatibility.classification !== "unknown-compatible"
    ) return
    props.onAcknowledge(preview.compatibility.version)
  }
  const onKeyDown = (event: KeyEvent) => {
    if (event.name !== "y") return
    event.preventDefault()
    event.stopPropagation()
    acknowledge()
  }
  const capture = (value: string) => {
    if (value.toLowerCase().includes("y")) acknowledge()
  }
  const padding = () => dimensions().width > 3 ? 1 : 0

  return <box flexDirection="column" padding={padding()} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("reconciliation.title")}</text>
    <box height={1}>
      <input
        ref={(value: InputRenderable) => { input = value }}
        value=""
        focused
        onInput={capture}
        onKeyDown={onKeyDown}
        backgroundColor={theme.panel}
        focusedBackgroundColor={theme.panel}
        textColor={theme.panel}
        focusedTextColor={theme.panel}
        cursorColor={theme.panel}
        width="100%"
      />
    </box>
    <For each={["adopt", "reapply", "restore"] as const}>{(strategy) => (
      <text fg={props.state().strategy === strategy ? theme.primary : theme.muted}>
        {props.t(strategyKey(strategy))}
      </text>
    )}</For>
    <Show when={props.state().pending === "preview"}>
      <text fg={theme.warning}>{props.t("reconciliation.previewing")}</text>
    </Show>
    <Show when={props.state().errorCode}>
      <text fg={theme.error}>{props.t(props.state().errorCode === "shadowing-configuration"
        ? "reconciliation.error.shadowing-configuration"
        : messageKeyForProblem(props.state().errorCode!))}</text>
    </Show>
    <For each={props.state().preview ? [props.state().preview!] : []}>{(preview) => <box flexDirection="column">
      <text fg={preview.compatibility.classification === "incompatible" ? theme.error : theme.text}>
        {props.t(compatibilityKey(preview), { version: preview.compatibility.version })}
      </text>
      <text fg={theme.muted}>{props.t("reconciliation.shadow.heading")}</text>
      <Show when={preview.shadowSources.length === 0}>
        <text fg={theme.muted}>{props.t("reconciliation.shadow.none")}</text>
      </Show>
      <For each={preview.shadowSources}>{(source) => (
        <text fg={theme.warning}>{shadowLabel(props.t, source)}</text>
      )}</For>
      <For each={preview.changes}>{(change) => (
        <text fg={theme.text}>{`${props.t(fieldKey(change.field))}: ${props.t(stateKey(change.state))}`}</text>
      )}</For>
      <text fg={theme.text}>{props.t(effectKey(preview.providerEffect))}</text>
      <Show when={preview.unobservableRuntimeBoundary}>
        <text fg={theme.warning}>{props.t("reconciliation.boundary")}</text>
      </Show>
      <Show when={preview.restartRequired}>
        <text fg={theme.warning}>{props.t(preview.target === "claude" ? "reconciliation.restart.claude" : "reconciliation.restart.codex")}</text>
      </Show>
      <Show when={preview.compatibility.acknowledgementRequired}>
        <text fg={props.state().acknowledgedVersion === preview.compatibility.version ? theme.success : theme.warning}>
          {props.state().acknowledgedVersion === preview.compatibility.version
            ? props.t("reconciliation.acknowledged")
            : props.t("reconciliation.acknowledgement", { version: preview.compatibility.version })}
        </text>
      </Show>
    </box>}</For>
    <Show when={props.state().pending === "apply"}>
      <text fg={theme.warning}>{props.t("reconciliation.applying")}</text>
    </Show>
    <text fg={theme.muted}>{props.t("reconciliation.help")}</text>
  </box>
}
