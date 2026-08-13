import { useTerminalDimensions } from "@opentui/solid"
import { createSignal, type Accessor } from "solid-js"

import { resolveSlash } from "../commands/catalog"
import { useMuxviaKeymap } from "../commands/keymap"
import type { CommandScope } from "../commands/types"
import { theme } from "../theme"
import { useOptionalOverlay } from "./overlay-stack"

export interface ActionPromptProps {
  scope: CommandScope
  placeholder: string
  metadata: string
  focusEnabled?: boolean | Accessor<boolean>
  onUnknown: (input: string) => void
}

export function ActionPrompt(props: ActionPromptProps) {
  const dimensions = useTerminalDimensions()
  const keymap = useMuxviaKeymap()
  const overlay = useOptionalOverlay()
  const [value, setValue] = createSignal("")
  const focusEnabled = () => typeof props.focusEnabled === "function"
    ? props.focusEnabled()
    : props.focusEnabled ?? (overlay ? overlay.depth === 0 : true)

  const submit = () => {
    const original = value()
    setValue("")
    const command = resolveSlash(original, props.scope) ?? resolveSlash(original, "global")
    if (command) keymap.dispatchCommand(command)
    else props.onUnknown(original)
  }

  const horizontalPadding = () => dimensions().width >= 7 ? 2 : 0

  return (
    <box
      height={Math.max(0, Math.min(3, dimensions().height))}
      border={["left"]}
      borderColor={theme.primary}
      backgroundColor={theme.element}
      paddingLeft={horizontalPadding()}
      paddingRight={horizontalPadding()}
      flexDirection="column"
      justifyContent="center"
    >
      <input
        focused={focusEnabled()}
        value={value()}
        onInput={setValue}
        onSubmit={submit}
        placeholder={props.placeholder}
        backgroundColor={theme.element}
        focusedBackgroundColor={theme.element}
        textColor={theme.text}
        focusedTextColor={theme.text}
        placeholderColor={theme.muted}
      />
      <text fg={theme.muted}>{props.metadata}</text>
    </box>
  )
}
