import { useCommandLayer } from "../commands/keymap"
import type { Translator } from "../i18n"
import { theme } from "../theme"

export function ProviderCredentialConfirmation(props: {
  sourceName: string
  t: Translator
  onReuse: () => void
  onWithout: () => void
  onCancel: () => void
}) {
  useCommandLayer({
    scope: "provider-credential-confirm",
    priority: 400,
    handlers: {
      "provider.credential.reuse": props.onReuse,
      "provider.credential.without": props.onWithout,
      "provider.credential.confirmation.cancel": props.onCancel,
    },
  })

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.text}>{props.t("provider.duplicate.credential.title")}</text>
    <text fg={theme.text}>{props.t("provider.duplicate.credential.message", { name: props.sourceName })}</text>
    <text fg={theme.muted}>{props.t("provider.credential-reference.present")}</text>
    <text fg={theme.muted}>{props.t("provider.duplicate.credential.help")}</text>
  </box>
}
