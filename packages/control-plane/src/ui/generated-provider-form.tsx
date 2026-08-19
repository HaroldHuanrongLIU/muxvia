import { useKeyboard } from "@opentui/solid"
import { createSignal } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type { Target, TargetAction, TargetView } from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

type Provider = TargetView["providers"][number]

export function GeneratedProviderForm(props: {
  provider: Provider
  providerRevision: number
  target: Target
  pending: boolean
  t: Translator
  onDirtyChange: (dirty: boolean) => void
  onCancel: () => void
  onSave: (action: Extract<TargetAction, { kind: "update-provider" }>) => Promise<boolean>
}) {
  const overlay = useOverlay()
  const [focus, setFocus] = createSignal(0)
  const [model, setModel] = createSignal(props.provider.model)
  const [authentication, setAuthentication] = createSignal(props.provider.authentication)
  const [routingRequirement, setRoutingRequirement] = createSignal(props.provider.routingRequirement)
  const markDirty = () => props.onDirtyChange(true)

  const submit = async () => {
    if (props.pending) return
    const applied = await props.onSave({
      kind: "update-provider",
      providerId: props.provider.id,
      providerRevision: props.providerRevision,
      name: props.provider.name,
      baseUrl: props.provider.baseUrl,
      model: model(),
      authentication: authentication(),
      routingRequirement: routingRequirement(),
      credential: { kind: "keep" },
    })
    if (applied) props.onCancel()
  }
  useCommandLayer({
    scope: "editor",
    priority: 250,
    enabled: () => overlay.depth === 0,
    handlers: {
      "provider.save": () => { void submit() },
      "provider.cancel": props.onCancel,
    },
  })
  useKeyboard((key) => {
    if (key.defaultPrevented || overlay.depth > 0) return
    const count = props.target === "claude" ? 3 : 2
    if (key.name === "tab") {
      key.preventDefault()
      key.stopPropagation()
      setFocus((current) => (current + (key.shift ? count - 1 : 1)) % count)
      return
    }
    if ((key.name !== "space" && key.sequence !== " ") || focus() === 0) return
    key.preventDefault()
    key.stopPropagation()
    if (props.target === "claude" && focus() === 1) {
      setAuthentication((current) => current === "anthropic-api-key" ? "anthropic-bearer" : "anthropic-api-key")
      markDirty()
    } else {
      setRoutingRequirement((current) => current === "direct-compatible" ? "takeover-required" : "direct-compatible")
      markDirty()
    }
  })
  const inputStyle = {
    backgroundColor: theme.element,
    focusedBackgroundColor: theme.element,
    textColor: theme.text,
    focusedTextColor: theme.text,
    placeholderColor: theme.muted,
  }
  const routingFocus = props.target === "claude" ? 2 : 1

  return <box backgroundColor={theme.panel} flexDirection="column" padding={1} rowGap={1}>
    <text fg={theme.primary}>{props.t("generated-provider.editor.title")}</text>
    <text fg={theme.warning}>{props.t("generated-provider.ownership")}</text>
    <text fg={theme.muted}>{`${props.t("provider.field.name")}: ${props.provider.name}`}</text>
    <text fg={theme.muted}>{`${props.t("provider.field.base-url")}: ${props.provider.baseUrl}`}</text>
    <text fg={theme.muted}>{props.t("generated-provider.protocol.fixed", { protocol: props.provider.protocol })}</text>
    <text fg={theme.muted}>{props.t("generated-provider.credential.read-only")}</text>
    <box flexDirection="column">
      <text fg={focus() === 0 ? theme.primary : theme.muted}>{props.t("provider.field.model")}</text>
      <input
        {...inputStyle}
        focused={focus() === 0}
        value={model()}
        onInput={(value: string) => {
          if (value !== model()) {
            setModel(value)
            markDirty()
          }
        }}
        placeholder={props.t("provider.placeholder.model")}
      />
    </box>
    {props.target === "claude" ? <text fg={focus() === 1 ? theme.primary : theme.muted} bg={theme.element}>
      {props.t(authentication() === "anthropic-api-key" ? "provider.authentication.api-key" : "provider.authentication.bearer")}
    </text> : null}
    <text fg={focus() === routingFocus ? theme.primary : theme.muted} bg={theme.element}>
      {props.t(routingRequirement() === "direct-compatible"
        ? "universal-provider.routing.direct"
        : "universal-provider.routing.takeover")}
    </text>
    <text fg={props.pending ? theme.warning : theme.muted}>{props.t(props.pending
      ? "provider.editor.saving"
      : "generated-provider.editor.help")}</text>
  </box>
}
