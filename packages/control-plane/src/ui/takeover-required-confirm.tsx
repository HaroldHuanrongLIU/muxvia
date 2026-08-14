import { useCommandLayer } from "../commands/keymap"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export interface TakeoverRequiredConfirmProps {
  providerName: string
  t: Translator
  onConfirm: () => void
  onCancel: () => void
}

export function TakeoverRequiredConfirm(props: TakeoverRequiredConfirmProps) {
  let scheduled = false
  const defer = (action: () => void) => {
    if (scheduled) return
    scheduled = true
    queueMicrotask(action)
  }

  useCommandLayer({
    scope: "takeover-required-confirm",
    priority: 400,
    handlers: {
      "provider.activate.takeover-confirm": () => defer(props.onConfirm),
      "provider.activate.takeover-cancel": () => defer(props.onCancel),
    },
  })

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("takeover-required.title")}</text>
    <text fg={theme.muted}>{props.t("takeover-required.message", { name: props.providerName })}</text>
    <box flexDirection="row" columnGap={2}>
      <text fg={theme.warning}>{props.t("takeover-required.confirm")}</text>
      <text fg={theme.muted}>{props.t("takeover-required.cancel")}</text>
    </box>
    <text fg={theme.muted}>{props.t("takeover-required.help")}</text>
  </box>
}
