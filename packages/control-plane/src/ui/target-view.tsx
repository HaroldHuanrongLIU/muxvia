import { For, Show } from "solid-js"

import type { TargetView as TargetViewProjection } from "../control/types"
import { theme } from "../theme"

export interface TargetViewProps {
  view: TargetViewProjection
  notice?: { kind: "error" | "success"; text: string }
}

function label(value: string): string {
  if (!value) return "—"
  return value.split("-").map((word) => word[0]?.toUpperCase() + word.slice(1)).join(" ")
}

function providerName(view: TargetViewProjection, id: string | null): string {
  if (!id) return "—"
  return view.providers.find((provider) => provider.id === id)?.name ?? "—"
}

export function TargetView(props: TargetViewProps) {
  const snapshot = () => {
    if (!props.view.activatedSnapshot) return "—"
    const provider = providerName(props.view, props.view.activatedSnapshot.providerId)
    return `${provider} · ${props.view.activatedSnapshot.model}`
  }

  return (
    <box flexDirection="column" rowGap={1}>
      <text fg={theme.primary}>MUXVIA</text>
      <text fg={theme.text}>Codex</text>
      <box flexDirection="column">
        <text fg={theme.text}>{`Mode       ${label(props.view.mode)}`}</text>
        <text fg={theme.text}>{`Current    ${providerName(props.view, props.view.currentProviderId)}`}</text>
        <text fg={theme.text}>{`Serving    ${providerName(props.view, props.view.servingProviderId)}`}</text>
        <text fg={theme.text}>{`Service    ${label(props.view.service.state)}`}</text>
        <text fg={theme.text}>{`Config     ${label(props.view.managedConfiguration.state)}`}</text>
        <text fg={theme.text}>{`Snapshot   ${snapshot()}`}</text>
      </box>

      <For each={props.view.providers}>{(provider) => (
        <box flexDirection="column">
          <text fg={theme.text}>{`Provider    ${provider.name}`}</text>
          <text fg={theme.muted}>{`Model       ${provider.model}`}</text>
          <text fg={theme.muted}>{`Credential  ${label(provider.credential)}`}</text>
        </box>
      )}</For>

      <Show when={props.view.managedConfiguration.restartRequired}>
        <text fg={theme.warning}>Restart Codex to use the managed configuration.</text>
      </Show>
      <For each={props.view.problems}>{(problem) => (
        <text fg={theme.error}>{`${problem.message} (${problem.code})`}</text>
      )}</For>
      <Show when={props.notice}>
        <text fg={props.notice?.kind === "error" ? theme.error : theme.success}>{props.notice?.text}</text>
      </Show>
    </box>
  )
}
