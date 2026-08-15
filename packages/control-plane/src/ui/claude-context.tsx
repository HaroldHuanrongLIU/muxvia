import { useTerminalDimensions } from "@opentui/solid"
import { Show } from "solid-js"

import type { Translator } from "../i18n"
import type { Target } from "../control/types"
import { theme } from "../theme"
import { ActionPrompt } from "./action-prompt"
import { Logo } from "./logo"

export interface UnavailableTargetProps {
  target: Target
  t: Translator
  notice?: string
  onUnknown: (input: string) => void
}

export function UnavailableTarget(props: UnavailableTargetProps) {
  const dimensions = useTerminalDimensions()

  return (
    <box width="100%" height="100%" flexDirection="column">
      <Show when={dimensions().width > 1 && dimensions().height > 1}>
        <scrollbox flexGrow={1} flexShrink={1} paddingTop={Math.max(0, Math.min(1, dimensions().height - 1))}>
          <box flexDirection="column" rowGap={1}>
            <Logo />
            <text fg={theme.text}>{props.t(props.target === "claude" ? "target.claude" : "target.codex")}</text>
            <text fg={theme.muted}>{props.t("target.unavailable.return")}</text>
            <Show when={props.notice}>
              <text fg={theme.error}>{props.notice}</text>
            </Show>
          </box>
        </scrollbox>
        <ActionPrompt
          scope={props.target}
          placeholder={props.t("prompt.target")}
          metadata={`${props.t(props.target === "claude" ? "prompt.meta.claude" : "prompt.meta.codex")} · ${props.t("prompt.hint.back")} · ${props.t("prompt.hint.exit")}`}
          onUnknown={props.onUnknown}
        />
      </Show>
    </box>
  )
}

export { UnavailableTarget as ClaudeContext }
