import { useCommandLayer } from "../commands/keymap"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export interface ProviderDeleteConfirmationProps {
  name: string
  t: Translator
  pending: boolean
  onConfirm: () => void
  onCancel: () => void
}

export function ProviderDeleteConfirmation(props: ProviderDeleteConfirmationProps) {
  let scheduled = false
  const defer = (action: () => void) => {
    if (scheduled || props.pending) return
    scheduled = true
    queueMicrotask(action)
  }
  useCommandLayer({
    scope: "provider-delete-confirm",
    priority: 400,
    handlers: {
      "provider.delete.confirm": () => defer(props.onConfirm),
      "provider.delete.cancel": () => defer(props.onCancel),
    },
  })
  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("provider.delete.title")}</text>
    <text fg={theme.muted}>{props.t("provider.delete.message", { name: props.name })}</text>
    <box flexDirection="row" columnGap={2}>
      <text fg={theme.error}>{props.t("provider.delete.confirm")}</text>
      <text fg={theme.muted}>{props.t("provider.delete.cancel")}</text>
    </box>
  </box>
}
