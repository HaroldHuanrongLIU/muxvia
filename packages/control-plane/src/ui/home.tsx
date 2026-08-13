import { useTerminalDimensions } from "@opentui/solid"
import { Show } from "solid-js"

import type { Translator } from "../i18n"
import { theme } from "../theme"
import { ActionPrompt } from "./action-prompt"
import { Logo } from "./logo"

export interface HomeProps {
  t: Translator
  notice?: string
  onUnknown: (input: string) => void
}

export function Home(props: HomeProps) {
  const dimensions = useTerminalDimensions()

  return (
    <box width="100%" height="100%" flexDirection="column">
      <Show when={dimensions().width > 1 && dimensions().height > 1}>
        <box flexGrow={1} flexShrink={1} />
        <box flexDirection="column" alignItems="center" rowGap={1}>
          <Logo />
          <box flexDirection="column">
            <text fg={theme.text}>{`[1] ${props.t("home.target.codex")}`}</text>
            <Show when={dimensions().height >= 16}>
              <text fg={theme.muted}>{props.t("home.target.codex.detail")}</text>
            </Show>
          </box>
          <box flexDirection="column">
            <text fg={theme.text}>{`[2] ${props.t("home.target.claude")}`}</text>
            <Show when={dimensions().height >= 16}>
              <text fg={theme.muted}>{props.t("home.target.claude.detail")}</text>
            </Show>
          </box>
        </box>
        <box flexGrow={1} flexShrink={1} />
        <Show when={props.notice}>
          <text fg={theme.error}>{props.notice}</text>
        </Show>
        <ActionPrompt
          scope="home"
          placeholder={props.t("prompt.home")}
          metadata={`${props.t("prompt.meta.home")} · ${props.t("prompt.hint.commands")} · ${props.t("prompt.hint.exit")}`}
          onUnknown={props.onUnknown}
        />
        <text fg={theme.muted}>{props.t("home.tip")}</text>
        <text fg={theme.muted}>{props.t("home.footer.targets")}</text>
      </Show>
    </box>
  )
}
