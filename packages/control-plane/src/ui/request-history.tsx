import type { InputRenderable, KeyEvent } from "@opentui/core"
import { useTerminalDimensions } from "@opentui/solid"
import { createMemo, createSignal, For, onMount, Show, type Accessor } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type { RequestRecordDetail, RequestRecordSummary, Target } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export interface RequestHistoryUiState {
  target: Target
  records: readonly RequestRecordSummary[]
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
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined
  const selected = () => props.state().records[props.state().selectedIndex]
  const visibleRecords = createMemo(() => {
    const records = props.state().records
    const capacity = Math.max(1, Math.min(6, Math.floor(Math.max(1, dimensions().height - 8) / 2)))
    const start = Math.min(
      Math.max(0, records.length - capacity),
      Math.max(0, props.state().selectedIndex - capacity + 1),
    )
    return records.slice(start, start + capacity).map((record, offset) => ({ record, index: start + offset }))
  })

  useCommandLayer({
    scope: "activity",
    priority: 400,
    handlers: {
      "activity.select-previous": props.onPrevious,
      "activity.select-next": props.onNext,
      "activity.inspect": props.onInspect,
      "activity.more": props.onMore,
      "activity.cancel": props.onCancel,
    },
  })

  onMount(() => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  }))

  const onKeyDown = (event: KeyEvent) => {
    const handler = event.name === "up" || event.name === "k"
      ? props.onPrevious
      : event.name === "down" || event.name === "j"
        ? props.onNext
        : event.name === "return" || event.name === "enter" || event.name === "linefeed"
          ? props.onInspect
          : event.name === "m"
            ? props.onMore
            : event.name === "escape"
              ? props.onCancel
              : undefined
    if (!handler) return
    event.preventDefault()
    event.stopPropagation()
    handler()
  }
  const captureNavigation = (value: string) => {
    const handler = value === "up" || value === "k"
      ? props.onPrevious
      : value === "down" || value === "j"
        ? props.onNext
        : value === "m"
          ? props.onMore
          : undefined
    setKeyCapture("")
    handler?.()
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
    <Show when={props.state().pending === "list" && props.state().records.length === 0}>
      <text fg={theme.warning}>{props.t("request-history.loading")}</text>
    </Show>
    <Show when={props.state().errorCode}>
      <text fg={theme.error}>{props.t(errorKey(props.state().errorCode!))}</text>
    </Show>
    <Show when={!props.state().pending && props.state().records.length === 0 && !props.state().errorCode}>
      <text fg={theme.muted}>{props.t("request-history.empty")}</text>
    </Show>
    <For each={visibleRecords()}>{({ record, index }) => <box flexDirection="column">
      <text fg={index === props.state().selectedIndex ? theme.primary : theme.text}>
        {`${index === props.state().selectedIndex ? ">" : " "} ${formatCompletion(record.finishedAtUnixMs)} · ${record.providerName ?? props.t("request-history.provider.unknown")} · ${record.model}`}
      </text>
      <text fg={record.outcome === "success" ? theme.muted : theme.warning}>
        {`${record.usage
          ? props.t("request-history.usage", { input: record.usage.inputTokens, output: record.usage.outputTokens })
          : props.t("request-history.usage.unavailable")} · ${props.t("request-history.latency", { latency: record.latencyMs })} · ${props.t(outcomeKey(record.outcome))} · ${record.estimatedCostNanoUsd === null
          ? props.t("request-history.cost.unpriced")
          : props.t("request-history.cost.estimated", { cost: formatNanoUsd(record.estimatedCostNanoUsd) })}`}
      </text>
    </box>}</For>
    <Show when={props.state().pending === "detail"}>
      <text fg={theme.warning}>{props.t("request-history.detail.loading")}</text>
    </Show>
    <Show when={props.state().detail}>{(detail: Accessor<RequestRecordDetail>) => <box flexDirection="column">
      <text fg={theme.warning}>{props.t("request-history.detail.sensitive")}</text>
      <Show when={detail().record.errorPayloadTruncated}>
        <text fg={theme.warning}>{props.t("request-history.detail.truncated")}</text>
      </Show>
      <text fg={theme.text}>{detail().errorPayload ?? props.t("request-history.detail.empty")}</text>
    </box>}</Show>
    <Show when={selected() && !selected()!.hasErrorPayload && !props.state().detail}>
      <text fg={theme.muted}>{props.t("request-history.detail.success")}</text>
    </Show>
    <text fg={theme.muted}>{props.t("request-history.help", {
      more: props.state().nextCursor ? props.t("request-history.help.more") : props.t("request-history.help.end"),
    })}</text>
  </box>
}
