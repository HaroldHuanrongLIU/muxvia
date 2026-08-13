import type { TargetView } from "../control/types"
import { labelTargetState, type Translator } from "../i18n"
import { theme } from "../theme"

export interface TargetSidebarProps {
  view: TargetView
  t: Translator
  width: number
}

export function TargetSidebar(props: TargetSidebarProps) {
  const none = () => props.t("value.none")
  const recovery = () => props.view.recovery.state === "clean"
    ? none()
    : labelTargetState(props.t, props.view.recovery.state)

  return (
    <box
      width={Math.max(1, props.width)}
      flexShrink={0}
      flexDirection="column"
      border={["left"]}
      borderColor={theme.borderSubtle}
      paddingLeft={Math.max(0, Math.min(2, props.width - 1))}
      rowGap={1}
    >
      <text fg={theme.secondary}>{props.t("sidebar.heading")}</text>
      <box flexDirection="column">
        <text fg={theme.muted}>{props.t("sidebar.revision")}</text>
        <text fg={theme.text}>{String(props.view.managementRevision)}</text>
      </box>
      <box flexDirection="column">
        <text fg={theme.muted}>{props.t("sidebar.sequence")}</text>
        <text fg={theme.text}>{String(props.view.viewSequence)}</text>
      </box>
      <box flexDirection="column">
        <text fg={theme.muted}>{props.t("sidebar.endpoint")}</text>
        <text fg={theme.text}>{props.view.takeover.endpoint ?? none()}</text>
      </box>
      <box flexDirection="column">
        <text fg={theme.muted}>{props.t("sidebar.recovery")}</text>
        <text fg={theme.text}>{recovery()}</text>
      </box>
    </box>
  )
}
