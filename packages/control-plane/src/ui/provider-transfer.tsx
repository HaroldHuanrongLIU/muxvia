import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createEffect, createSignal, For, onMount, Show, type Accessor } from "solid-js"

import type {
  ProviderConfigurationExport,
  ProviderImportCandidateView,
  ProviderImportChoice,
  ProviderImportOutcome,
  ProviderImportPreview,
  ProviderImportSource,
  Target,
} from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"

type ImportSourceKind = "live-target" | "cc-switch" | "cc-switch-sql" | "muxvia-export"
type Stage = "source" | "payload" | "preview" | "complete"

const importSourceKinds: readonly ImportSourceKind[] = ["live-target", "cc-switch", "cc-switch-sql", "muxvia-export"]

function safeProblemCode(error: unknown): string {
  return typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : "internal-failure"
}

export function ProviderImportWizard(props: {
  target: Target
  t: Translator
  onPreview: (source: ProviderImportSource) => Promise<ProviderImportPreview>
  onConfirm: (
    previewToken: string,
    choices: ProviderImportChoice[],
    includeHistoricalUsage: boolean,
  ) => Promise<ProviderImportOutcome>
  onClose: () => void
}) {
  const [stage, setStage] = createSignal<Stage>("source")
  const [sourceIndex, setSourceIndex] = createSignal(0)
  const [payload, setPayload] = createSignal("")
  const [preview, setPreview] = createSignal<ProviderImportPreview>()
  const [outcome, setOutcome] = createSignal<ProviderImportOutcome>()
  const [selectedIndex, setSelectedIndex] = createSignal(0)
  const [selected, setSelected] = createSignal<Set<string>>(new Set())
  const [resolutions, setResolutions] = createSignal<Record<string, ProviderImportChoice["resolution"]>>({})
  const [includeHistoricalUsage, setIncludeHistoricalUsage] = createSignal(false)
  const [pending, setPending] = createSignal(false)
  const [errorCode, setErrorCode] = createSignal<string>()
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined

  const focus = () => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  })
  onMount(focus)
  createEffect(() => {
    stage()
    focus()
  })

  const sourceKind = () => importSourceKinds[sourceIndex()]!
  const candidates = () => preview()?.candidates ?? []
  const move = (delta: -1 | 1) => {
    if (stage() === "source") {
      setSourceIndex((current) => (current + delta + importSourceKinds.length) % importSourceKinds.length)
      return
    }
    if (stage() === "preview" && candidates().length > 0) {
      setSelectedIndex((current) => (current + delta + candidates().length) % candidates().length)
    }
  }
  const beginPreview = async () => {
    if (pending()) return
    const kind = sourceKind()
    if (stage() === "source" && kind !== "live-target") {
      setStage("payload")
      setPayload("")
      return
    }
    const source: ProviderImportSource = kind === "live-target"
      ? { kind: "live-target" }
      : kind === "cc-switch"
      ? { kind: "cc-switch", payload: payload() }
      : kind === "cc-switch-sql"
      ? { kind: "cc-switch-sql", path: payload() }
      : { kind: "muxvia-export", payload: payload() }
    if (kind !== "live-target" && payload().trim().length === 0) return
    setPending(true)
    setErrorCode()
    try {
      const next = await props.onPreview(source)
      const selectedIds = new Set<string>()
      const nextResolutions: Record<string, ProviderImportChoice["resolution"]> = {}
      for (const candidate of next.candidates) {
        if (candidate.kind !== "target-provider" || !candidate.importedCurrent) {
          selectedIds.add(candidate.candidateId)
        }
        nextResolutions[candidate.candidateId] = { kind: "create" }
      }
      setPreview(next)
      setSelected(selectedIds)
      setResolutions(nextResolutions)
      setIncludeHistoricalUsage(next.historicalUsage?.selectedByDefault ?? false)
      setSelectedIndex(0)
      setStage("preview")
    } catch (error) {
      setErrorCode(safeProblemCode(error))
    } finally {
      if (kind !== "live-target") setPayload("")
      setPending(false)
    }
  }
  const toggleSelected = () => {
    const candidate = candidates()[selectedIndex()]
    if (!candidate || pending()) return
    setSelected((current) => {
      const next = new Set(current)
      if (next.has(candidate.candidateId)) next.delete(candidate.candidateId)
      else next.add(candidate.candidateId)
      return next
    })
  }
  const cycleResolution = () => {
    const candidate = candidates()[selectedIndex()]
    if (!candidate || candidate.exactMatches.length === 0 || pending()) return
    const options: ProviderImportChoice["resolution"][] = [
      { kind: "create" },
      ...candidate.exactMatches.map((match) => ({ kind: "use-existing" as const, providerId: match.providerId })),
    ]
    const current = resolutions()[candidate.candidateId]
    const index = options.findIndex((option) =>
      option.kind === current?.kind
      && (option.kind === "create" || option.providerId === (current as { providerId?: string }).providerId))
    setResolutions((values) => ({
      ...values,
      [candidate.candidateId]: options[(index + 1) % options.length]!,
    }))
  }
  const confirm = async () => {
    const current = preview()
    if (!current || pending()) return
    const choices = current.candidates
      .filter((candidate) => selected().has(candidate.candidateId))
      .map((candidate): ProviderImportChoice => ({
        candidateId: candidate.candidateId,
        resolution: resolutions()[candidate.candidateId] ?? { kind: "create" },
      }))
    if (choices.length === 0 && !includeHistoricalUsage()) return
    setPending(true)
    setErrorCode()
    try {
      setOutcome(await props.onConfirm(current.previewToken, choices, includeHistoricalUsage()))
      setStage("complete")
    } catch (error) {
      setErrorCode(safeProblemCode(error))
    } finally {
      setPending(false)
    }
  }
  const submit = () => {
    if (stage() === "complete") props.onClose()
    else if (stage() === "preview") void confirm()
    else void beginPreview()
  }
  const onKeyDown = (event: KeyEvent) => {
    if (pending()) return
    if (event.name === "up" || event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      move(event.name === "up" ? -1 : 1)
    } else if (stage() === "preview" && (event.name === "space" || event.name === " ")) {
      event.preventDefault()
      event.stopPropagation()
      toggleSelected()
    } else if (stage() === "preview" && event.name === "m") {
      event.preventDefault()
      event.stopPropagation()
      cycleResolution()
    } else if (stage() === "preview" && event.name === "u" && preview()?.historicalUsage) {
      event.preventDefault()
      event.stopPropagation()
      setIncludeHistoricalUsage((value) => !value)
    } else if (event.name === "return" || event.name === "enter" || event.name === "linefeed") {
      event.preventDefault()
      event.stopPropagation()
      submit()
    }
  }
  const capture = (value: string) => {
    if (stage() === "payload") {
      setPayload(value)
      return
    }
    if (value === "up" || value === "down") move(value === "up" ? -1 : 1)
    else if (stage() === "preview" && value === " ") toggleSelected()
    else if (stage() === "preview" && value.toLowerCase() === "m") cycleResolution()
    else if (stage() === "preview" && value.toLowerCase() === "u" && preview()?.historicalUsage) {
      setIncludeHistoricalUsage((current) => !current)
    }
    setKeyCapture("")
  }
  const sourceLabel = (kind: ImportSourceKind): string => {
    switch (kind) {
      case "live-target": return props.t("provider-transfer.source.live-target")
      case "cc-switch": return props.t("provider-transfer.source.cc-switch")
      case "cc-switch-sql": return props.t("provider-transfer.source.cc-switch-sql")
      case "muxvia-export": return props.t("provider-transfer.source.muxvia-export")
    }
  }
  const resolutionLabel = (candidate: ProviderImportCandidateView): string => {
    const resolution = resolutions()[candidate.candidateId]
    if (resolution?.kind === "use-existing") {
      return props.t("provider-transfer.preview.exact", {
        name: candidate.exactMatches.find((match) => match.providerId === resolution.providerId)?.name ?? resolution.providerId,
      })
    }
    return props.t("provider-transfer.preview.create")
  }

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.primary}>{props.t("provider-transfer.import.title")}</text>
    <Show when={stage() !== "complete"} fallback={<text fg={theme.success}>{props.t("provider-transfer.complete", { count: outcome()?.records.length ?? 0 })}</text>}>
      <Show when={stage() === "payload"} fallback={<input
        ref={(value: InputRenderable) => { input = value }}
        value={keyCapture()}
        focused
        onInput={capture}
        onKeyDown={onKeyDown}
        backgroundColor={theme.panel}
        focusedBackgroundColor={theme.panel}
        textColor={theme.panel}
        focusedTextColor={theme.panel}
        cursorColor={theme.primary}
        width="100%"
      />}>
        <input
          ref={(value: InputRenderable) => { input = value; focus() }}
          value={payload()}
          focused
          onInput={capture}
          onKeyDown={onKeyDown}
          backgroundColor={theme.panel}
          focusedBackgroundColor={theme.panel}
          textColor={theme.text}
          focusedTextColor={theme.text}
          placeholder={props.t("provider-transfer.payload.placeholder")}
          placeholderColor={theme.muted}
          cursorColor={theme.primary}
          width="100%"
        />
      </Show>
      <Show when={stage() === "source"}>
        <For each={importSourceKinds}>{(kind, index) => <text
          fg={sourceIndex() === index() ? theme.background : theme.text}
          bg={sourceIndex() === index() ? theme.primary : theme.panel}
        >{sourceLabel(kind)}</text>}</For>
        <text fg={theme.muted}>{props.t("provider-transfer.source.help")}</text>
      </Show>
      <Show when={stage() === "payload"}>
        <text fg={theme.muted}>{props.t("provider-transfer.payload.help")}</text>
      </Show>
      <Show when={stage() === "preview"}>
        <text fg={theme.info}>{props.t("provider-transfer.preview.source", {
          product: preview()?.source.product ?? "",
          target: preview()?.source.target ?? "",
        })}</text>
        <For each={candidates()}>{(candidate, index) => <box flexDirection="column">
          <text fg={selectedIndex() === index() ? theme.primary : theme.text}>
            {`${selected().has(candidate.candidateId) ? "[x]" : "[ ]"} ${candidate.kind === "target-provider" && candidate.importedCurrent ? props.t("provider-transfer.preview.imported-current") + " · " : ""}${candidate.name} · ${candidate.baseUrl || props.t("value.none")}`}
          </text>
          <text fg={theme.muted}>{`${candidate.kind === "target-provider" ? candidate.model : candidate.targets.filter((target) => target.enabled).map((target) => target.model).join(", ")} · ${resolutionLabel(candidate)} · ${props.t("provider-transfer.preview.credential-redacted", { state: candidate.credential })}`}</text>
        </box>}</For>
        <Show when={preview()?.historicalUsage}>{(usage: Accessor<NonNullable<ProviderImportPreview["historicalUsage"]>>) => <box flexDirection="column">
          <text fg={includeHistoricalUsage() ? theme.primary : theme.text}>
            {`${includeHistoricalUsage() ? "[x]" : "[ ]"} ${props.t("provider-transfer.preview.historical-usage")}`}
          </text>
          <text fg={theme.muted}>{props.t("provider-transfer.preview.historical-usage-summary", {
            count: usage().recordCount,
            start: usage().startDate ?? props.t("value.none"),
            end: usage().endDate ?? props.t("value.none"),
            bytes: usage().estimatedStorageBytes,
          })}</text>
        </box>}</Show>
        <text fg={theme.warning}>{props.t("provider-transfer.preview.help")}</text>
      </Show>
    </Show>
    <Show when={stage() === "complete"}><input
      ref={(value: InputRenderable) => { input = value; focus() }}
      value=""
      focused
      onKeyDown={onKeyDown}
      backgroundColor={theme.panel}
      focusedBackgroundColor={theme.panel}
      textColor={theme.panel}
      focusedTextColor={theme.panel}
      cursorColor={theme.panel}
      width="100%"
    /></Show>
    <Show when={errorCode()}><text fg={theme.error}>{props.t("provider-transfer.error", { code: errorCode()! })}</text></Show>
    <Show when={pending()}><text fg={theme.warning}>{props.t("provider-transfer.pending")}</text></Show>
    <Show when={stage() === "complete" && outcome()?.historicalUsageImportedRecords !== undefined}>
      <text fg={theme.success}>{props.t("provider-transfer.complete.historical-usage", {
        count: outcome()?.historicalUsageImportedRecords ?? 0,
      })}</text>
    </Show>
    <Show when={stage() === "complete"}><text fg={theme.muted}>{props.t("provider-transfer.complete.help")}</text></Show>
  </box>
}

export function ProviderExportView(props: {
  t: Translator
  load: () => Promise<ProviderConfigurationExport>
}) {
  const [exportValue, setExportValue] = createSignal<ProviderConfigurationExport>()
  const [errorCode, setErrorCode] = createSignal<string>()

  onMount(() => {
    void props.load().then(setExportValue, (error) => setErrorCode(safeProblemCode(error)))
  })

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.primary}>{props.t("provider-transfer.export.title")}</text>
    <text fg={theme.success}>{props.t("provider-transfer.export.redacted")}</text>
    <Show when={exportValue()} fallback={errorCode()
      ? <text fg={theme.error}>{props.t("provider-transfer.error", { code: errorCode()! })}</text>
      : <text fg={theme.warning}>{props.t("provider-transfer.pending")}</text>}>
      <scrollbox height={16} width="100%">
        <text fg={theme.text}>{JSON.stringify(exportValue(), null, 2)}</text>
      </scrollbox>
      <text fg={theme.muted}>{props.t("provider-transfer.export.help")}</text>
    </Show>
  </box>
}
