import { useCommandLayer } from "../commands/keymap"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export interface ExitConfirmationProps {
  t: Translator
  onConfirm: () => void
  onCancel: () => void
}

export function ExitConfirmation(props: ExitConfirmationProps) {
  let scheduled = false
  const defer = (action: () => void) => {
    if (scheduled) return
    scheduled = true
    queueMicrotask(action)
  }

  useCommandLayer({
    scope: "confirm",
    priority: 300,
    handlers: {
      "app.exit.confirm": () => defer(props.onConfirm),
      "app.exit.cancel": () => defer(props.onCancel),
    },
  })

  return (
    <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
      <text fg={theme.text}>{props.t("exit.title")}</text>
      <text fg={theme.muted}>{props.t("exit.message")}</text>
      <box flexDirection="row" columnGap={2}>
        <text fg={theme.primary}>{props.t("exit.confirm")}</text>
        <text fg={theme.muted}>{props.t("exit.cancel")}</text>
      </box>
    </box>
  )
}
