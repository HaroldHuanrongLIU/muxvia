import type { InputRenderable } from "@opentui/core"
import { useKeyboard, usePaste } from "@opentui/solid"
import { createSignal, onCleanup, onMount, Show } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type { DiscoverySource, ModelDiscoveryResult, TargetAction } from "../control/types"
import { inspectionErrorKey, type Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"
import { ProviderModelPicker } from "./provider-model-picker"

export interface ProviderDraft {
  name: string
  baseUrl: string
  model: string
  providerId?: string
  providerRevision?: number
  presetKey?: "openai-api-responses" | null
}

export type ProviderFormResult = Extract<TargetAction,
  { kind: "create-provider" | "update-provider" | "duplicate-provider" }
>

export interface ProviderFormProps {
  mode: "create" | "edit" | "duplicate"
  initialDraft: ProviderDraft
  credentialPresence: "present" | "missing"
  duplicateCredentialChoice?: "without" | "reuse-source"
  discoverModels?: (source: DiscoverySource, signal?: AbortSignal) => Promise<ModelDiscoveryResult>
  pending: boolean
  t: Translator
  ref?: (value: ProviderFormRef | undefined) => void
  onDirtyChange: (dirty: boolean) => void
  onCancel: () => void
  onSave(result: ProviderFormResult): Promise<boolean>
}

export interface ProviderFormRef {
  isDirty(): boolean
  clearSensitive(): void
  focus(): void
}

type CredentialIntent = "keep" | "remove" | "replace"

export function ProviderForm(props: ProviderFormProps) {
  const overlay = useOverlay()
  const [focus, setFocus] = createSignal(0)
  const [name, setName] = createSignal(props.initialDraft.name)
  const [baseUrl, setBaseUrl] = createSignal(props.initialDraft.baseUrl)
  const [model, setModel] = createSignal(props.initialDraft.model)
  const [credential, setCredential] = createSignal("")
  const [credentialIntent, setCredentialIntent] = createSignal<CredentialIntent>(
    props.mode === "create" || props.duplicateCredentialChoice === "without" ? "remove" : "keep",
  )
  const [dirty, setDirtySignal] = createSignal(false)
  const [discovery, setDiscovery] = createSignal<
    | { status: "idle" | "pending" }
    | { status: "success"; models: Extract<ModelDiscoveryResult, { status: "success" }>["models"] }
    | { status: "failure"; category: Extract<ModelDiscoveryResult, { status: "failure" }>["failure"]["category"] }
  >({ status: "idle" })
  const inputs: Array<InputRenderable | undefined> = []
  let cancelScheduled = false
  let disposed = false
  let discoveryAbort: AbortController | undefined
  let discoveryGeneration = 0

  const setDirty = (next: boolean) => {
    if (dirty() === next) return
    setDirtySignal(next)
    props.onDirtyChange(next)
  }
  const update = (current: () => string, setter: (value: string) => void) => (value: string) => {
    if (value === current()) return
    setter(value)
    setDirty(true)
  }
  const clearSensitive = () => {
    if (credential()) setCredential("")
    if (credentialIntent() === "replace") {
      setCredentialIntent(props.mode === "create" || props.duplicateCredentialChoice === "without" ? "remove" : "keep")
    }
  }
  const formRef: ProviderFormRef = {
    isDirty: dirty,
    clearSensitive,
    focus: () => {
      setFocus(0)
      const input = inputs[0]
      if (input && !input.isDestroyed) input.focus()
    },
  }
  props.ref?.(formRef)
  onCleanup(() => {
    disposed = true
    discoveryGeneration++
    discoveryAbort?.abort()
    clearSensitive()
    props.ref?.(undefined)
  })

  const runDiscovery = async (source: DiscoverySource) => {
    if (!props.discoverModels) return
    discoveryAbort?.abort()
    const controller = new AbortController()
    discoveryAbort = controller
    const generation = ++discoveryGeneration
    setDiscovery({ status: "pending" })
    try {
      const result = await props.discoverModels(source, controller.signal)
      if (disposed || generation !== discoveryGeneration || controller.signal.aborted) return
      if (result.status === "success") {
        setDiscovery({ status: "success", models: result.models })
      } else if (result.failure.category !== "cancelled") {
        setDiscovery({ status: "failure", category: result.failure.category })
      }
    } catch (error) {
      if (disposed || generation !== discoveryGeneration || controller.signal.aborted) return
      const code = typeof error === "object" && error !== null && "code" in error ? String(error.code) : "connect"
      if (code !== "cancelled") setDiscovery({ status: "failure", category: "connect" })
    }
  }

  onMount(() => {
    if (props.mode === "edit" && props.discoverModels) {
      void runDiscovery({
        kind: "saved",
        providerId: props.initialDraft.providerId!,
        providerRevision: props.initialDraft.providerRevision!,
      })
    }
  })

  const credentialEdit = (): Extract<TargetAction, { kind: "create-provider" }> ["credential"] => {
    if (credentialIntent() === "replace") return { kind: "replace", value: credential() }
    if (credentialIntent() === "keep") return { kind: "keep" }
    return { kind: "remove" }
  }
  const submit = async () => {
    if (props.pending) return
    const fields = { name: name(), baseUrl: baseUrl(), model: model() }
    const result: ProviderFormResult = props.mode === "create"
      ? { kind: "create-provider", ...fields, credential: credentialEdit(), presetKey: props.initialDraft.presetKey ?? null }
      : props.mode === "edit"
        ? {
          kind: "update-provider",
          ...fields,
          providerId: props.initialDraft.providerId!,
          providerRevision: props.initialDraft.providerRevision!,
          credential: credentialEdit(),
        }
        : {
          kind: "duplicate-provider",
          ...fields,
          sourceProviderId: props.initialDraft.providerId!,
          sourceProviderRevision: props.initialDraft.providerRevision!,
          credential: credentialIntent() === "replace"
            ? { kind: "replace", value: credential() }
            : credentialIntent() === "keep" ? { kind: "reuse-source" } : { kind: "without" },
        }
    clearSensitive()
    const applied = await props.onSave(result)
    if (applied && !disposed) {
      setDirty(false)
      props.onCancel()
    }
  }

  const removeCredential = () => {
    clearSensitive()
    setCredentialIntent("remove")
    setDirty(true)
  }
  const refreshModels = () => {
    const credentialSource: Extract<DiscoverySource, { kind: "draft" }>["credentialSource"] = credentialIntent() === "replace"
      ? { kind: "ephemeral", value: credential() }
      : credentialIntent() === "keep" && props.credentialPresence === "present" && props.initialDraft.providerId && props.initialDraft.providerRevision
        ? {
          kind: "saved",
          providerId: props.initialDraft.providerId,
          providerRevision: props.initialDraft.providerRevision,
        }
        : { kind: "missing" }
    void runDiscovery({ kind: "draft", baseUrl: baseUrl(), credentialSource })
  }
  const openModelPicker = () => {
    const current = discovery()
    if (current.status !== "success" || current.models.length === 0) return
    overlay.push({
      id: "provider-model-picker",
      render: () => <ProviderModelPicker
        models={current.models}
        t={props.t}
        onSelect={(modelId) => {
          if (modelId !== model()) {
            setModel(modelId)
            setDirty(true)
          }
          overlay.closeTop()
        }}
      />,
    })
  }
  const cancel = () => {
    if (cancelScheduled) return
    cancelScheduled = true
    clearSensitive()
    queueMicrotask(() => {
      if (!disposed) props.onCancel()
    })
  }

  useCommandLayer({
    scope: "editor",
    priority: 200,
    enabled: () => overlay.depth === 0,
    handlers: {
      "provider.save": () => { void submit() },
      "provider.cancel": cancel,
      "provider.credential.remove": removeCredential,
      "provider.models.refresh": refreshModels,
      "provider.models.select": openModelPicker,
    },
  })

  useKeyboard((key) => {
    if (key.defaultPrevented || overlay.depth > 0) return
    if (key.name === "tab") {
      key.preventDefault()
      key.stopPropagation()
      setFocus((current) => (current + (key.shift ? 3 : 1)) % 4)
      return
    }
    if (focus() !== 3 || key.ctrl || key.meta || key.super || key.hyper) return
    if (key.name === "backspace") {
      key.preventDefault()
      key.stopPropagation()
      if (credential()) {
        setCredential((current) => current.slice(0, -1))
        setCredentialIntent("replace")
        setDirty(true)
      }
      return
    }
    if (key.sequence && key.sequence.charCodeAt(0) >= 32 && key.sequence.charCodeAt(0) !== 127) {
      key.preventDefault()
      key.stopPropagation()
      setCredential((current) => current + key.sequence)
      setCredentialIntent("replace")
      setDirty(true)
    }
  })

  usePaste((event) => {
    if (event.defaultPrevented || overlay.depth > 0 || focus() !== 3) return
    event.preventDefault()
    event.stopPropagation()
    const value = new TextDecoder().decode(event.bytes).replace(/[\r\n]/g, "")
    if (!value) return
    setCredential((current) => current + value)
    setCredentialIntent("replace")
    setDirty(true)
  })

  const inputStyle = {
    backgroundColor: theme.element,
    focusedBackgroundColor: theme.element,
    textColor: theme.text,
    focusedTextColor: theme.text,
    placeholderColor: theme.muted,
  }
  const title = props.mode === "edit" ? "provider.editor.edit-title" : "provider.editor.title"
  const credentialPlaceholder = () => credentialIntent() === "keep"
    ? props.t("provider.credential-reference.present")
    : props.t("provider.placeholder.credential")

  return (
    <box backgroundColor={theme.panel} flexDirection="column" padding={1} rowGap={1}>
      <text fg={theme.text}>{props.t(title)}</text>
      <box flexDirection="column">
        <text fg={focus() === 0 ? theme.primary : theme.muted}>{props.t("provider.field.name")}</text>
        <input ref={(input: InputRenderable) => { inputs[0] = input }} {...inputStyle} focused={focus() === 0} value={name()} onInput={update(name, setName)} placeholder={props.t("provider.placeholder.name")} />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 1 ? theme.primary : theme.muted}>{props.t("provider.field.base-url")}</text>
        <input ref={(input: InputRenderable) => { inputs[1] = input }} {...inputStyle} focused={focus() === 1} value={baseUrl()} onInput={update(baseUrl, setBaseUrl)} placeholder={props.t("provider.placeholder.base-url")} />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 2 ? theme.primary : theme.muted}>{props.t("provider.field.model")}</text>
        <input ref={(input: InputRenderable) => { inputs[2] = input }} {...inputStyle} focused={focus() === 2} value={model()} onInput={update(model, setModel)} placeholder={props.t("provider.placeholder.model")} />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 3 ? theme.primary : theme.muted}>{props.t("provider.field.credential")}</text>
        <text fg={theme.text} bg={theme.element}>{credential() ? "•".repeat(credential().length) : credentialPlaceholder()}</text>
      </box>
      <Show when={discovery().status !== "idle"}>
        <text fg={discovery().status === "failure" ? theme.error : theme.muted}>{(() => {
          const current = discovery()
          if (current.status === "pending") return props.t("provider.models.loading")
          if (current.status === "success") return props.t("provider.models.available", { count: current.models.length })
          if (current.status === "failure") {
            return props.t("provider.models.failure", { reason: props.t(inspectionErrorKey(current.category)) })
          }
          return ""
        })()}</text>
      </Show>
      <text fg={theme.muted}>{props.t(props.pending ? "provider.editor.saving" : "provider.editor.help")}</text>
    </box>
  )
}
