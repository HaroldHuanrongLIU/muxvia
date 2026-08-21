import type { InputRenderable, KeyEvent } from "@opentui/core"
import { useTerminalDimensions } from "@opentui/solid"
import { createMemo, createSignal, For, onMount, Show, type Accessor } from "solid-js"

import { resolveBinding } from "../commands/catalog"
import { useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { RequestRecordDetail, RequestRecordSummary, Target, UsageActivityEntry } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export interface RequestHistoryUiState {
  target: Target
  entries: readonly UsageActivityEntry[]
  nextCursor: string | null
  selectedIndex: number
  detail?: RequestRecordDetail
  pending?: "list" | "detail"
  errorCode?: string
}

export interface RequestHistoryProps {
  state: Accessor<RequestHistoryUiState>
  t: Translator
  onPrevious: () => void
  onNext: () => void
  onInspect: () => void
  onMore: () => void
  onRefresh: () => void
  onCancel: () => void
}

function formatCompletion(unixMilliseconds: number): string {
  const date = new Date(unixMilliseconds)
  if (Number.isNaN(date.valueOf())) return String(unixMilliseconds)
  const rendered = date.toISOString()
  return rendered.replace("T", " ").replace(".000Z", "Z")
}

function formatNanoUsd(nanoUsd: number): string {
  const whole = Math.floor(nanoUsd / 1_000_000_000)
  const fractional = String(nanoUsd % 1_000_000_000).padStart(9, "0").replace(/0+$/, "")
  return `$${whole}${fractional ? `.${fractional}` : ""}`
}

function outcomeKey(outcome: RequestRecordSummary["outcome"]) {
  return `request-history.outcome.${outcome}` as const
}

function errorKey(code: string) {
  switch (code) {
    case "invalid-request-history-cursor": return "request-history.error.invalid-cursor" as const
    case "request-record-not-found": return "request-history.error.not-found" as const
    case "request-history-unavailable": return "request-history.error.unavailable" as const
    default: return "request-history.error.generic" as const
  }
}

export function RequestHistory(props: RequestHistoryProps) {
  const dimensions = useTerminalDimensions()
  const keymap = useMuxviaKeymap()
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined
  const selected = () => props.state().entries[props.state().selectedIndex]
  const selectedRequest = () => {
    const entry = selected()
    return entry?.kind === "request-record" ? entry.record : undefined
  }
  const visibleRecords = createMemo(() => {
    const records = props.state().entries
    const capacity = props.state().detail
      ? 1
      : Math.max(1, Math.min(6, Math.floor(Math.max(1, dimensions().height - 8) / 2)))
    const start = Math.min(
      Math.max(0, records.length - capacity),
      Math.max(0, props.state().selectedIndex - capacity + 1),
    )
    return records.slice(start, start + capacity).map((record, offset) => ({ record, index: start + offset }))
  })
  const visibleDetail = createMemo(() => {
    const payload = props.state().detail?.errorPayload
    if (payload === null || payload === undefined) return { text: undefined, clipped: false }
    const capacity = Math.max(
      1,
      Math.min(4_096, dimensions().width * Math.max(1, dimensions().height - 12)),
    )
    return {
      text: payload.slice(0, capacity),
      clipped: payload.length > capacity,
    }
  })

  useCommandLayer({
    scope: "activity",
    priority: 400,
    handlers: {
      "activity.select-previous": props.onPrevious,
      "activity.select-next": props.onNext,
      "activity.inspect": props.onInspect,
      "activity.more": props.onMore,
      "activity.refresh": props.onRefresh,
      "activity.cancel": props.onCancel,
    },
  })

  onMount(() => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  }))

  const dispatchBinding = (binding: string) => {
    const command = resolveBinding("activity", binding)
    if (!command) return false
    keymap.dispatchCommand(command)
    return true
  }
  const onKeyDown = (event: KeyEvent) => {
    const binding = event.name === "enter" || event.name === "linefeed" ? "return" : event.name
    if (!dispatchBinding(binding)) return
    event.preventDefault()
    event.stopPropagation()
  }
  const captureNavigation = (value: string) => {
    if (dispatchBinding(value)) setKeyCapture("")
    else setKeyCapture(value)
  }

  return <box flexDirection="column" padding={dimensions().width > 3 ? 1 : 0} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("request-history.title", {
      target: props.t(props.state().target === "codex" ? "target.codex" : "target.claude"),
    })}</text>
    <box height={1}>
      <input
        ref={(value: InputRenderable) => { input = value }}
        value={keyCapture()}
        focused
        onInput={captureNavigation}
        onKeyDown={onKeyDown}
        backgroundColor={theme.panel}
        focusedBackgroundColor={theme.panel}
        textColor={theme.panel}
        focusedTextColor={theme.panel}
        cursorColor={theme.panel}
        width="100%"
      />
    </box>
    <Show when={props.state().pending === "list" && props.state().entries.length === 0}>
      <text fg={theme.warning}>{props.t("request-history.loading")}</text>
    </Show>
    <Show when={props.state().errorCode}>
      <text fg={theme.error}>{props.t(errorKey(props.state().errorCode!))}</text>
    </Show>
    <Show when={!props.state().pending && props.state().entries.length === 0 && !props.state().errorCode}>
      <text fg={theme.muted}>{props.t("request-history.empty")}</text>
    </Show>
    <For each={visibleRecords()}>{({ record: entry, index }) => <box flexDirection="column">
      <Show when={entry.kind === "request-record"}>{() => {
        const record = (entry as Extract<UsageActivityEntry, { kind: "request-record" }>).record
        return <>
          <text fg={index === props.state().selectedIndex ? theme.primary : theme.text}>
            {`${index === props.state().selectedIndex ? ">" : " "} Request · ${record.providerName ?? props.t("request-history.provider.unknown")} · ${record.model}`}
          </text>
          <text fg={record.outcome === "success" ? theme.muted : theme.warning}>
            {`${formatCompletion(record.finishedAtUnixMs)} · ${record.usage
              ? props.t("request-history.usage", { input: record.usage.inputTokens, output: record.usage.outputTokens })
              : props.t("request-history.usage.unavailable")} · ${props.t("request-history.latency", { latency: record.latencyMs })} · ${props.t(outcomeKey(record.outcome))} · ${record.estimatedCostNanoUsd === null
              ? props.t("request-history.cost.unpriced")
              : props.t("request-history.cost.estimated", { cost: formatNanoUsd(record.estimatedCostNanoUsd) })}`}
          </text>
        </>
      }}</Show>
      <Show when={entry.kind === "native-usage-record"}>{() => {
        const record = (entry as Extract<UsageActivityEntry, { kind: "native-usage-record" }>).record
        return <>
          <text fg={index === props.state().selectedIndex ? theme.primary : theme.text}>
            {`${index === props.state().selectedIndex ? ">" : " "} Native · ${formatCompletion(record.observedAtUnixMs)} · ${record.model}`}
          </text>
          <text fg={theme.muted}>{`${props.t("request-history.usage", { input: record.usage.inputTokens, output: record.usage.outputTokens })} · ${record.estimatedCostNanoUsd === null
            ? props.t("request-history.cost.unpriced")
            : props.t("request-history.cost.estimated", { cost: formatNanoUsd(record.estimatedCostNanoUsd) })}`}</text>
        </>
      }}</Show>
      <Show when={entry.kind === "daily-usage-rollup"}>{() => {
        const rollup = (entry as Extract<UsageActivityEntry, { kind: "daily-usage-rollup" }>).rollup
        return <>
          <text fg={index === props.state().selectedIndex ? theme.primary : theme.text}>
            {`${index === props.state().selectedIndex ? ">" : " "} Daily rollup · ${rollup.localDate} · ${rollup.requestRecordCount} Request / ${rollup.nativeUsageRecordCount} Native`}
          </text>
          <text fg={theme.muted}>{`${props.t("request-history.usage", { input: rollup.usage.inputTokens, output: rollup.usage.outputTokens })} · ${props.t("request-history.cost.estimated", { cost: formatNanoUsd(rollup.estimatedCostNanoUsd) })}`}</text>
        </>
      }}</Show>
      <Show when={entry.kind === "migrated-usage-rollup"}>{() => {
        const rollup = (entry as Extract<UsageActivityEntry, { kind: "migrated-usage-rollup" }>).rollup
        return <>
          <text fg={index === props.state().selectedIndex ? theme.primary : theme.text}>
            {`${index === props.state().selectedIndex ? ">" : " "} ${props.t("request-history.migrated")} · ${rollup.localDate} · ${rollup.sourceRecordCount}`}
          </text>
          <text fg={theme.muted}>{`${props.t("request-history.usage", { input: rollup.usage.inputTokens, output: rollup.usage.outputTokens })} · ${rollup.sourceProduct} · ${props.t("request-history.cost.unpriced")}`}</text>
        </>
      }}</Show>
    </box>}</For>
    <Show when={props.state().pending === "detail"}>
      <text fg={theme.warning}>{props.t("request-history.detail.loading")}</text>
    </Show>
    <Show when={props.state().detail}>{(detail: Accessor<RequestRecordDetail>) => <box flexDirection="column">
      <Show when={detail().errorPayloadSensitive}>
        <text fg={theme.warning}>{props.t("request-history.detail.sensitive")}</text>
      </Show>
      <Show when={detail().record.errorPayloadTruncated}>
        <text fg={theme.warning}>{props.t("request-history.detail.truncated")}</text>
      </Show>
      <Show when={visibleDetail().clipped}>
        <text fg={theme.warning}>{props.t("request-history.detail.display-clipped")}</text>
      </Show>
      <text fg={theme.text}>{visibleDetail().text ?? props.t("request-history.detail.empty")}</text>
    </box>}</Show>
    <Show when={selectedRequest()?.outcome === "success" && !props.state().detail}>
      <text fg={theme.muted}>{props.t("request-history.detail.success")}</text>
    </Show>
    <text fg={theme.muted}>{props.t("request-history.help", {
      more: props.state().nextCursor ? props.t("request-history.help.more") : props.t("request-history.help.end"),
    })}</text>
  </box>
}
