import { useKeyboard, usePaste } from "@opentui/solid"
import { createSignal, onCleanup } from "solid-js"

import type { TargetAction } from "../control/types"
import { theme } from "../theme"

type ProviderDraft = Extract<TargetAction, { kind: "save-provider" }>

export interface ProviderFormProps {
  pending: boolean
  onCancel: () => void
  onSave: (draft: ProviderDraft) => Promise<boolean>
}

export function ProviderForm(props: ProviderFormProps) {
  const [focus, setFocus] = createSignal(0)
  const [name, setName] = createSignal("")
  const [baseUrl, setBaseUrl] = createSignal("")
  const [model, setModel] = createSignal("")
  const [credential, setCredential] = createSignal("")

  const clearCredential = () => setCredential("")
  onCleanup(clearCredential)

  const submit = async () => {
    if (props.pending) return
    const draft: ProviderDraft = {
      kind: "save-provider",
      name: name(),
      baseUrl: baseUrl(),
      model: model(),
      credential: credential(),
    }
    clearCredential()
    await props.onSave(draft)
  }

  useKeyboard((key) => {
    if (key.name === "escape") {
      key.preventDefault()
      key.stopPropagation()
      clearCredential()
      props.onCancel()
      return
    }
    if (key.name === "tab") {
      key.preventDefault()
      key.stopPropagation()
      setFocus((current) => (current + (key.shift ? 3 : 1)) % 4)
      return
    }
    if ((key.name === "return" || key.name === "enter" || key.name === "linefeed") && focus() === 3) {
      key.preventDefault()
      key.stopPropagation()
      void submit()
      return
    }
    if (focus() !== 3 || key.ctrl || key.meta || key.super || key.hyper) return
    if (key.name === "backspace") {
      key.preventDefault()
      key.stopPropagation()
      setCredential((current) => current.slice(0, -1))
      return
    }
    if (key.sequence && key.sequence.charCodeAt(0) >= 32 && key.sequence.charCodeAt(0) !== 127) {
      key.preventDefault()
      key.stopPropagation()
      setCredential((current) => current + key.sequence)
    }
  })

  usePaste((event) => {
    if (focus() !== 3) return
    event.preventDefault()
    event.stopPropagation()
    setCredential((current) => current + new TextDecoder().decode(event.bytes).replace(/[\r\n]/g, ""))
  })

  const inputStyle = {
    backgroundColor: theme.element,
    focusedBackgroundColor: theme.element,
    textColor: theme.text,
    focusedTextColor: theme.text,
    placeholderColor: theme.muted,
  }

  return (
    <box backgroundColor={theme.panel} flexDirection="column" padding={1} rowGap={1}>
      <text fg={theme.text}>Provider</text>
      <box flexDirection="column">
        <text fg={focus() === 0 ? theme.primary : theme.muted}>Name</text>
        <input {...inputStyle} focused={focus() === 0} value={name()} onInput={setName} placeholder="Fixture Provider" />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 1 ? theme.primary : theme.muted}>Base URL</text>
        <input {...inputStyle} focused={focus() === 1} value={baseUrl()} onInput={setBaseUrl} placeholder="https://provider.example/v1" />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 2 ? theme.primary : theme.muted}>Model</text>
        <input {...inputStyle} focused={focus() === 2} value={model()} onInput={setModel} placeholder="gpt-model" />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 3 ? theme.primary : theme.muted}>Credential</text>
        <text fg={theme.text} bg={theme.element}>{credential() ? "•".repeat(credential().length) : "API credential"}</text>
      </box>
      <text fg={theme.muted}>{props.pending ? "Saving…" : "[Enter] save   [Esc] cancel"}</text>
    </box>
  )
}
