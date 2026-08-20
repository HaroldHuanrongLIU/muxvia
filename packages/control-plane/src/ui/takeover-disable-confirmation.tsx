import { useCommandLayer } from "../commands/keymap"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export function TakeoverDisableConfirmation(props: {
  pending: boolean
  t: Translator
  onConfirm: () => void
  onCancel: () => void
}) {
  let scheduled = false
  const defer = (action: () => void) => {
    if (scheduled || props.pending) return
    scheduled = true
    queueMicrotask(() => {
      scheduled = false
      action()
    })
  }

  useCommandLayer({
    scope: "takeover-disable-confirm",
    priority: 400,
    handlers: {
      "target.takeover.disable-confirm": () => defer(props.onConfirm),
      "target.takeover.disable-cancel": () => defer(props.onCancel),
    },
  })

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("takeover-disable.title")}</text>
    <text fg={theme.muted}>{props.t("takeover-disable.message")}</text>
    <box flexDirection="row" columnGap={2}>
      <text fg={theme.warning}>{props.t("takeover-disable.confirm")}</text>
      <text fg={theme.muted}>{props.t("takeover-disable.cancel")}</text>
    </box>
    <text fg={theme.muted}>{props.pending ? props.t("takeover-disable.pending") : props.t("takeover-disable.help")}</text>
  </box>
}
