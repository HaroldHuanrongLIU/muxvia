import type { InputRenderable } from "@opentui/core"
import { useKeyboard, usePaste } from "@opentui/solid"
import { createSignal, onCleanup } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type { TargetAction } from "../control/types"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

type ProviderDraft = Extract<TargetAction, { kind: "save-provider" }>

export interface ProviderFormProps {
  pending: boolean
  ref?: (value: ProviderFormRef | undefined) => void
  onDirtyChange: (dirty: boolean) => void
  onCancel: () => void
  onSave: (draft: ProviderDraft) => Promise<boolean>
}

export interface ProviderFormRef {
  isDirty(): boolean
  clearSensitive(): void
  focus(): void
}

export function ProviderForm(props: ProviderFormProps) {
  const overlay = useOverlay()
  const [focus, setFocus] = createSignal(0)
  const [name, setName] = createSignal("")
  const [baseUrl, setBaseUrl] = createSignal("")
  const [model, setModel] = createSignal("")
  const [credential, setCredential] = createSignal("")
  const [dirty, setDirtySignal] = createSignal(false)
  const inputs: Array<InputRenderable | undefined> = []
  let cancelScheduled = false
  let disposed = false

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
    clearSensitive()
    props.ref?.(undefined)
  })

  const submit = async () => {
    if (props.pending) return
    const draft: ProviderDraft = {
      kind: "save-provider",
      name: name(),
      baseUrl: baseUrl(),
      model: model(),
      credential: credential(),
    }
    clearSensitive()
    const applied = await props.onSave(draft)
    if (applied && !disposed) {
      setDirty(false)
      props.onCancel()
    }
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
    },
  })

  useKeyboard((key) => {
    if (key.defaultPrevented) return
    if (overlay.depth > 0) return
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
      const current = credential()
      if (current) {
        setCredential(current.slice(0, -1))
        setDirty(true)
      }
      return
    }
    if (key.sequence && key.sequence.charCodeAt(0) >= 32 && key.sequence.charCodeAt(0) !== 127) {
      key.preventDefault()
      key.stopPropagation()
      setCredential((current) => current + key.sequence)
      setDirty(true)
    }
  })

  usePaste((event) => {
    if (event.defaultPrevented) return
    if (overlay.depth > 0) return
    if (focus() !== 3) return
    event.preventDefault()
    event.stopPropagation()
    const value = new TextDecoder().decode(event.bytes).replace(/[\r\n]/g, "")
    if (!value) return
    setCredential((current) => current + value)
    setDirty(true)
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
        <input ref={(input: InputRenderable) => { inputs[0] = input }} {...inputStyle} focused={focus() === 0} value={name()} onInput={update(name, setName)} placeholder="Fixture Provider" />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 1 ? theme.primary : theme.muted}>Base URL</text>
        <input ref={(input: InputRenderable) => { inputs[1] = input }} {...inputStyle} focused={focus() === 1} value={baseUrl()} onInput={update(baseUrl, setBaseUrl)} placeholder="https://provider.example/v1" />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 2 ? theme.primary : theme.muted}>Model</text>
        <input ref={(input: InputRenderable) => { inputs[2] = input }} {...inputStyle} focused={focus() === 2} value={model()} onInput={update(model, setModel)} placeholder="gpt-model" />
      </box>
      <box flexDirection="column">
        <text fg={focus() === 3 ? theme.primary : theme.muted}>Credential</text>
        <text fg={theme.text} bg={theme.element}>{credential() ? "•".repeat(credential().length) : "API credential"}</text>
      </box>
      <text fg={theme.muted}>{props.pending ? "Saving…" : "[Enter] save   [Esc] cancel"}</text>
    </box>
  )
}
