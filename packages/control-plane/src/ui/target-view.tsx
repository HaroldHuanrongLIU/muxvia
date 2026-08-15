import { For, Show } from "solid-js"

import type { TargetView as TargetViewProjection } from "../control/types"
import { labelTargetState, messageKeyForProblem, type MessageKey, type Translator } from "../i18n"
import { theme } from "../theme"

export interface ActivityEntry {
  id: number
  kind: "info" | "success" | "warning" | "error"
  messageKey: MessageKey
  values?: Record<string, string | number>
}

export interface TargetViewProps {
  view: TargetViewProjection
  activities: readonly ActivityEntry[]
  t: Translator
}

function providerName(view: TargetViewProjection, id: string | null, t: Translator): string {
  if (!id) return t("value.none")
  return view.providers.find((provider) => provider.id === id)?.name ?? t("value.none")
}

function activityColor(kind: ActivityEntry["kind"]): string {
  switch (kind) {
    case "success": return theme.success
    case "warning": return theme.warning
    case "error": return theme.error
    default: return theme.info
  }
}

export function TargetView(props: TargetViewProps) {
  const snapshot = () => {
    if (!props.view.activatedSnapshot) return props.t("value.none")
    const provider = providerName(props.view, props.view.activatedSnapshot.providerId, props.t)
    return `${provider} · ${props.view.activatedSnapshot.model}`
  }
  const status = (key: MessageKey, value: string) => {
    const label = props.t(key)
    return `${label}${" ".repeat(Math.max(2, 11 - label.length))}${value}`
  }
  const declaration = (key: MessageKey, value: string) => `${props.t(key).padEnd(12)}${value}`
  const managedConfiguration = () => {
    const state = labelTargetState(props.t, props.view.managedConfiguration.state)
    const path = props.view.managedConfiguration.path
    return path ? `${state} · ${path}` : state
  }
  const problemText = (code: string) => {
    const key = messageKeyForProblem(code)
    return props.t(key, key === "error.generic" ? { code } : undefined)
  }

  return (
    <box flexDirection="column" rowGap={1}>
      <text fg={theme.primary}>MUXVIA</text>
      <text fg={theme.text}>{props.t(props.view.target === "claude" ? "target.claude" : "target.codex")}</text>
      <box flexDirection="column">
        <text fg={theme.text}>{status("status.mode", labelTargetState(props.t, props.view.mode))}</text>
        <text fg={theme.text}>{status("status.current", providerName(props.view, props.view.currentProviderId, props.t))}</text>
        <text fg={theme.text}>{status("status.serving", providerName(props.view, props.view.servingProviderId, props.t))}</text>
        <text fg={theme.text}>{status("status.service", labelTargetState(props.t, props.view.service.state))}</text>
        <text fg={theme.text}>{status("status.config", managedConfiguration())}</text>
        <text fg={theme.text}>{status("status.snapshot", snapshot())}</text>
        <text fg={theme.text}>{status("status.health", labelTargetState(props.t, props.view.routeHealth.state))}</text>
      </box>

      <For each={props.view.providers}>{(provider) => (
        <box flexDirection="column">
          <text fg={theme.text}>{declaration("provider.heading", provider.name)}</text>
          <text fg={theme.muted}>{declaration("provider.model", provider.model)}</text>
          <text fg={theme.muted}>{declaration("provider.credential", props.t(provider.credential === "present" ? "provider.credential.present" : "provider.credential.absent"))}</text>
        </box>
      )}</For>

      <Show when={props.view.managedConfiguration.restartRequired}>
        <text fg={theme.warning}>{props.t(props.view.target === "claude" ? "activity.restart.claude" : "activity.restart")}</text>
      </Show>
      <For each={props.view.problems}>{(problem) => (
        <text fg={theme.error}>{problemText(problem.code)}</text>
      )}</For>
      <Show when={props.activities.length > 0}>
        <box flexDirection="column">
          <text fg={theme.muted}>{props.t("activity.heading")}</text>
          <For each={props.activities}>{(activity) => (
            <text fg={activityColor(activity.kind)}>{props.t(activity.messageKey, activity.values)}</text>
          )}</For>
        </box>
      </Show>
    </box>
  )
}
