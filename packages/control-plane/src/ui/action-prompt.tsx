import { useTerminalDimensions } from "@opentui/solid"
import { createSignal } from "solid-js"

import { resolveSlash } from "../commands/catalog"
import { useMuxviaKeymap } from "../commands/keymap"
import type { CommandScope } from "../commands/types"
import { theme } from "../theme"

export interface ActionPromptProps {
  scope: CommandScope
  placeholder: string
  metadata: string
  onUnknown: (input: string) => void
}

export function ActionPrompt(props: ActionPromptProps) {
  const dimensions = useTerminalDimensions()
  const keymap = useMuxviaKeymap()
  const [value, setValue] = createSignal("")

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
        focused
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
