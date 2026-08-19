import { useCommandLayer } from "../commands/keymap"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export function UniversalProviderConfirmation(props: {
  title: string
  message: string
  pending: boolean
  notice?: string
  t: Translator
  onConfirm: () => void
  onCancel: () => void
}) {
  let scheduled = false
  const defer = (action: () => void) => {
    if (scheduled || props.pending) return
    scheduled = true
    queueMicrotask(action)
  }
  useCommandLayer({
    scope: "universal-provider-confirm",
    priority: 500,
    handlers: {
      "universal-provider.confirm": () => defer(props.onConfirm),
      "universal-provider.confirm.cancel": () => defer(props.onCancel),
    },
  })
  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.title}</text>
    <text fg={theme.muted}>{props.message}</text>
    {props.notice ? <text fg={theme.error}>{props.notice}</text> : null}
    <text fg={theme.warning}>{props.pending ? props.t("universal-provider.pending") : props.t("universal-provider.confirm.help")}</text>
  </box>
}
