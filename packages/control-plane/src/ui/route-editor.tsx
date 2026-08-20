import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createEffect, createSignal, For, onMount, Show, type Accessor } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type { TargetView } from "../control/types"
import { labelTargetState, messageKeyForProblem, type Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

type DraftMember = NonNullable<TargetView["failover"]>["draftMembers"][number]

export interface RouteEditorProps {
  view: Accessor<TargetView>
  t: Translator
  pending: Accessor<"save" | "apply" | undefined>
  errorCode: Accessor<string | undefined>
  onSave: (members: readonly DraftMember[]) => void
  onApply: () => void
}

export function RouteEditor(props: RouteEditorProps) {
  const overlay = useOverlay()
  const [selectedId, setSelectedId] = createSignal<string>()
  let input: InputRenderable | undefined

  const draft = () => props.view().failover?.draftMembers ?? []
  const selectedIndex = () => Math.max(0, draft().findIndex((member) => member.providerId === selectedId()))
  const provider = (id: string) => props.view().providers.find((candidate) => candidate.id === id)
  const available = () => props.view().providers.find((candidate) => !draft().some((member) => member.providerId === candidate.id))
  const active = () => props.view().failover?.activePlan
  const activeMatchesDraft = () => {
    const plan = active()
    const members = draft()
    return !!plan
      && plan.members.length === members.length
      && plan.members.every((member, index) => (
        member.providerId === members[index]?.providerId
        && member.providerRevision === members[index]?.providerRevision
      ))
  }

  createEffect(() => {
    const members = draft()
    const selected = selectedId()
    if (!selected || !members.some((member) => member.providerId === selected)) setSelectedId(members[0]?.providerId)
  })
  onMount(() => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  }))

  const navigate = (delta: -1 | 1) => {
    const members = draft()
    if (members.length < 2) return
    setSelectedId(members[(selectedIndex() + delta + members.length) % members.length]?.providerId)
  }
  const move = (delta: -1 | 1) => {
    if (props.pending()) return
    const members = [...draft()]
    const index = selectedIndex()
    const next = index + delta
    if (next < 1 || next >= members.length) return
    ;[members[index], members[next]] = [members[next]!, members[index]!]
    props.onSave(members)
  }
  const add = () => {
    if (props.pending()) return
    const next = available()
    if (!next) return
    props.onSave([...draft(), { providerId: next.id, providerRevision: next.providerRevision }])
    setSelectedId(next.id)
  }
  const remove = () => {
    if (props.pending()) return
    const index = selectedIndex()
    if (index < 1) return
    props.onSave(draft().filter((_, memberIndex) => memberIndex !== index))
  }

  useCommandLayer({
    scope: "route-editor",
    priority: 400,
    enabled: () => overlay.depth === 1 && !props.pending(),
    handlers: {
      "route.move-up": () => move(-1),
      "route.move-down": () => move(1),
      "route.add-provider": add,
      "route.remove-provider": remove,
      "route.apply": props.onApply,
    },
  })
  useCommandLayer({
    scope: "overlay",
    priority: 500,
    enabled: () => overlay.depth === 1 && props.pending() === "apply",
    handlers: { "overlay.close": () => {} },
  })

  const onKeyDown = (event: KeyEvent) => {
    if (props.pending()) {
      event.preventDefault()
      event.stopPropagation()
      return
    }
    if (event.name === "up" || event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      navigate(event.name === "up" ? -1 : 1)
    }
  }

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("route.title")}</text>
    <box height={1}>
      <input
        ref={(value: InputRenderable) => { input = value }}
        value=""
        focused
        onKeyDown={onKeyDown}
        backgroundColor={theme.panel}
        focusedBackgroundColor={theme.panel}
        textColor={theme.panel}
        focusedTextColor={theme.panel}
        cursorColor={theme.panel}
        width="100%"
      />
    </box>
    <text fg={theme.muted}>{props.t("route.current-serving", {
      current: provider(props.view().currentProviderId ?? "")?.name ?? props.t("value.none"),
      serving: provider(props.view().servingProviderId ?? "")?.name ?? props.t("value.none"),
    })}</text>
    <text fg={activeMatchesDraft() ? theme.success : theme.warning}>
      {props.t(activeMatchesDraft() ? "route.matches" : "route.diverged")}
    </text>
    <text fg={theme.muted}>{props.t("route.epoch", { epoch: active()?.epoch ?? props.t("value.none") })}</text>
    <For each={draft()}>{(member, index) => {
      const declaration = () => provider(member.providerId)
      const selected = () => member.providerId === selectedId()
      const health = () => declaration()?.routeHealth?.state ?? "unobserved"
      const complete = () => declaration()?.completeness === "complete"
      const synchronized = () => declaration()?.synchronization !== "pending"
      return <text fg={selected() ? theme.background : complete() && synchronized() ? theme.text : theme.warning} bg={selected() ? theme.primary : theme.panel}>
        {`${String(index() + 1).padStart(2, "0")} · ${index() === 0 ? props.t("route.current") : props.t("route.fallback")} · ${declaration()?.name ?? member.providerId} · ${props.t(complete() ? "provider.complete" : "provider.incomplete")} · ${props.t(synchronized() ? "route.synchronized" : "route.unsynchronized")} · ${labelTargetState(props.t, health())}`}
      </text>
    }}</For>
    <For each={available() ? [available()!] : []}>{(next) => <text fg={theme.muted}>{props.t("route.available", { name: next.name })}</text>}</For>
    <Show when={props.pending()}><text fg={theme.warning}>{props.t(props.pending() === "apply" ? "route.applying" : "route.saving")}</text></Show>
    <Show when={props.errorCode()}><text fg={theme.error}>{props.t(messageKeyForProblem(props.errorCode()!))}</text></Show>
    <text fg={theme.muted}>{props.t("route.help")}</text>
  </box>
}
