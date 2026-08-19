import { useKeyboard, usePaste } from "@opentui/solid"
import { createSignal, onCleanup } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type { Target, UniversalProviderAction, UniversalProviderCatalogView } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

type Provider = UniversalProviderCatalogView["providers"][number]
type Preset = UniversalProviderCatalogView["presets"][number]
type TargetDraft = {
  target: Target
  enabled: boolean
  model: string
  authentication: "openai-bearer" | "anthropic-api-key" | "anthropic-bearer"
  routingRequirement: "direct-compatible" | "takeover-required"
}

export interface UniversalProviderEditorProps {
  mode: "create" | "edit" | "duplicate"
  provider?: Provider
  preset?: Preset
  pending: boolean
  notice?: string
  t: Translator
  onCancel: () => void
  onSave: (action: UniversalProviderAction) => Promise<boolean>
}

function targetDraft(
  target: Target,
  provider: Provider | undefined,
  preset: Preset | undefined,
): TargetDraft {
  const source = provider?.targets.find((candidate) => candidate.target === target)
    ?? preset?.targets.find((candidate) => candidate.target === target)
  return source ? {
    target,
    enabled: source.enabled,
    model: source.model,
    authentication: source.authentication,
    routingRequirement: source.routingRequirement,
  } : {
    target,
    enabled: false,
    model: "",
    authentication: target === "codex" ? "openai-bearer" : "anthropic-api-key",
    routingRequirement: "direct-compatible",
  }
}

export function UniversalProviderEditor(props: UniversalProviderEditorProps) {
  const overlay = useOverlay()
  const sourceProviderId = props.provider?.id
  const sourceProviderRevision = props.provider?.providerRevision
  const [focus, setFocus] = createSignal(0)
  const [name, setName] = createSignal(props.mode === "duplicate"
    ? props.t("provider.duplicate.copy-name", { name: props.provider?.name ?? "" })
    : props.provider?.name ?? props.preset?.name ?? "")
  const [baseUrl, setBaseUrl] = createSignal(props.provider?.baseUrl ?? props.preset?.baseUrl ?? "")
  const [credential, setCredential] = createSignal("")
  const [credentialIntent, setCredentialIntent] = createSignal<"keep" | "remove" | "replace" | "reuse-source" | "without">(
    props.mode === "edit" ? "keep" : props.mode === "duplicate" ? "without" : "remove",
  )
  const [codex, setCodex] = createSignal(targetDraft("codex", props.provider, props.preset))
  const [claude, setClaude] = createSignal(targetDraft("claude", props.provider, props.preset))
  let disposed = false
  let cancelScheduled = false
  onCleanup(() => {
    disposed = true
    setCredential("")
  })

  const targets = () => [codex(), claude()]
  const credentialEdit = (): Extract<UniversalProviderAction, { kind: "create-universal-provider" }>["credential"] => {
    if (credentialIntent() === "replace") return { kind: "replace", value: credential() }
    if (credentialIntent() === "keep") return { kind: "keep" }
    return { kind: "remove" }
  }
  const duplicateCredential = (): Extract<UniversalProviderAction, { kind: "duplicate-universal-provider" }>["credential"] => {
    if (credentialIntent() === "replace") return { kind: "replace", value: credential() }
    if (credentialIntent() === "reuse-source") return { kind: "reuse-source" }
    return { kind: "without" }
  }
  const submit = async () => {
    if (props.pending) return
    const common = { name: name(), baseUrl: baseUrl(), targets: targets() }
    const action: UniversalProviderAction = props.mode === "create"
      ? {
        kind: "create-universal-provider",
        ...common,
        credential: credentialEdit(),
        presetKey: props.preset?.key ?? null,
      }
      : props.mode === "edit"
        ? {
          kind: "update-universal-provider",
          ...common,
          providerId: sourceProviderId!,
          providerRevision: sourceProviderRevision!,
          credential: credentialEdit(),
        }
        : {
          kind: "duplicate-universal-provider",
          ...common,
          sourceProviderId: sourceProviderId!,
          sourceProviderRevision: sourceProviderRevision!,
          credential: duplicateCredential(),
        }
    setCredential("")
    if (await props.onSave(action) && !disposed) props.onCancel()
  }
  const cancel = () => {
    if (cancelScheduled || props.pending) return
    cancelScheduled = true
    setCredential("")
    queueMicrotask(() => { if (!disposed) props.onCancel() })
  }
  useCommandLayer({
    scope: "universal-provider-editor",
    priority: 500,
    enabled: () => overlay.depth === 2,
    handlers: {
      "universal-provider.save": () => { void submit() },
      "universal-provider.cancel": cancel,
    },
  })

  const updateTarget = (target: Target, update: (current: TargetDraft) => TargetDraft) => {
    if (target === "codex") setCodex(update)
    else setClaude(update)
  }
  const updateText = (current: () => string, setter: (value: string) => void) => (value: string) => {
    if (value !== current()) setter(value)
  }
  useKeyboard((key) => {
    if (key.defaultPrevented || overlay.depth !== 2) return
    if (key.name === "tab") {
      key.preventDefault()
      key.stopPropagation()
      setFocus((current) => (current + (key.shift ? 9 : 1)) % 10)
      return
    }
    const currentFocus = focus()
    if ([3, 5, 6, 8, 9].includes(currentFocus) && (key.name === "space" || key.sequence === " ")) {
      key.preventDefault()
      key.stopPropagation()
      if (currentFocus === 3 || currentFocus === 6) {
        const target = currentFocus === 3 ? "codex" : "claude"
        updateTarget(target, (current) => ({ ...current, enabled: !current.enabled }))
      } else if (currentFocus === 5 || currentFocus === 9) {
        const target = currentFocus === 5 ? "codex" : "claude"
        updateTarget(target, (current) => ({
          ...current,
          routingRequirement: current.routingRequirement === "direct-compatible" ? "takeover-required" : "direct-compatible",
        }))
      } else {
        setClaude((current) => ({
          ...current,
          authentication: current.authentication === "anthropic-api-key" ? "anthropic-bearer" : "anthropic-api-key",
        }))
      }
      return
    }
    if (currentFocus !== 2 || key.ctrl || key.meta || key.super || key.hyper) return
    if (key.name === "backspace") {
      key.preventDefault()
      key.stopPropagation()
      if (credential()) {
        setCredential((current) => current.slice(0, -1))
        setCredentialIntent("replace")
      }
      return
    }
    if (props.mode === "duplicate" && credential().length === 0 && (key.name === "space" || key.sequence === " ")) {
      key.preventDefault()
      key.stopPropagation()
      setCredentialIntent((current) => current === "reuse-source" ? "without" : "reuse-source")
      return
    }
    if (key.sequence && key.sequence.charCodeAt(0) >= 32 && key.sequence.charCodeAt(0) !== 127) {
      key.preventDefault()
      key.stopPropagation()
      setCredential((current) => current + key.sequence)
      setCredentialIntent("replace")
    }
  })
  usePaste((event) => {
    if (event.defaultPrevented || overlay.depth !== 2 || focus() !== 2) return
    event.preventDefault()
    event.stopPropagation()
    const value = new TextDecoder().decode(event.bytes).replace(/[\r\n]/g, "")
    if (!value) return
    setCredential((current) => current + value)
    setCredentialIntent("replace")
  })

  const inputStyle = {
    backgroundColor: theme.element,
    focusedBackgroundColor: theme.element,
    textColor: theme.text,
    focusedTextColor: theme.text,
    placeholderColor: theme.muted,
  }
  const targetPanel = (target: Target, state: () => TargetDraft, modelFocus: number, enabledFocus: number, authFocus: number | undefined, routingFocus: number) =>
    <box flexDirection="column">
      <text fg={theme.secondary}>{props.t(`target.${target}`)}</text>
      <text fg={focus() === enabledFocus ? theme.primary : theme.muted} bg={theme.element}>
        {`${props.t("universal-provider.field.enabled")}: ${props.t(state().enabled ? "universal-provider.enabled" : "universal-provider.disabled")}`}
      </text>
      <text fg={focus() === modelFocus ? theme.primary : theme.muted}>{props.t("provider.field.model")}</text>
      <input
        {...inputStyle}
        focused={focus() === modelFocus}
        value={state().model}
        onInput={(value: string) => updateTarget(target, (current) => ({ ...current, model: value }))}
        placeholder={props.t("provider.placeholder.model")}
      />
      {authFocus === undefined ? <text fg={theme.muted}>{props.t("universal-provider.authentication.fixed")}</text> :
        <text fg={focus() === authFocus ? theme.primary : theme.muted} bg={theme.element}>
          {props.t(state().authentication === "anthropic-api-key" ? "provider.authentication.api-key" : "provider.authentication.bearer")}
        </text>}
      <text fg={focus() === routingFocus ? theme.primary : theme.muted} bg={theme.element}>
        {props.t(state().routingRequirement === "direct-compatible"
          ? "universal-provider.routing.direct"
          : "universal-provider.routing.takeover")}
      </text>
    </box>

  return <box backgroundColor={theme.panel} flexDirection="column" padding={1} rowGap={1}>
    <text fg={theme.primary}>{props.t(`universal-provider.editor.${props.mode}`)}</text>
    {props.notice ? <text fg={theme.error}>{props.notice}</text> : null}
    <box flexDirection="column">
      <text fg={focus() === 0 ? theme.primary : theme.muted}>{props.t("provider.field.name")}</text>
      <input {...inputStyle} focused={focus() === 0} value={name()} onInput={updateText(name, setName)} placeholder={props.t("provider.placeholder.name")} />
    </box>
    <box flexDirection="column">
      <text fg={focus() === 1 ? theme.primary : theme.muted}>{props.t("provider.field.base-url")}</text>
      <input {...inputStyle} focused={focus() === 1} value={baseUrl()} onInput={updateText(baseUrl, setBaseUrl)} placeholder={props.t("provider.placeholder.base-url")} />
    </box>
    <box flexDirection="column">
      <text fg={focus() === 2 ? theme.primary : theme.muted}>{props.t("provider.field.credential")}</text>
      <text fg={theme.text} bg={theme.element}>{credential()
        ? "•".repeat(credential().length)
        : props.t(`universal-provider.credential.${credentialIntent()}`)}</text>
    </box>
    {targetPanel("codex", codex, 4, 3, undefined, 5)}
    {targetPanel("claude", claude, 7, 6, 8, 9)}
    <text fg={props.pending ? theme.warning : theme.muted}>{props.t(props.pending
      ? "universal-provider.pending"
      : "universal-provider.editor.help")}</text>
  </box>
}
