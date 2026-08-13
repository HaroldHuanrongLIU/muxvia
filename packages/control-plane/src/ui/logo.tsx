import { useTerminalDimensions } from "@opentui/solid"
import { For, Show } from "solid-js"

import { theme } from "../theme"

const blockWordmark = [
  "┏━┳━┓╻ ╻╻ ╻╻  ╻╻┏━┓",
  "┃ ┃ ┃┃ ┃┏╋┛┃  ┃┃┣━┫",
  "╹ ╹ ╹┗━┛╹ ╹┗━━┛╹╹ ╹",
  "        MUXVIA",
] as const

export function Logo() {
  const dimensions = useTerminalDimensions()

  return (
    <Show when={dimensions().width > 1 && dimensions().height > 1}>
      <Show
        when={dimensions().width >= 60}
        fallback={<text fg={theme.primary}><b>MUXVIA</b></text>}
      >
        <box flexDirection="column" alignItems="center">
          <For each={blockWordmark}>{(line, index) => (
            <text fg={index() === blockWordmark.length - 1 ? theme.text : theme.primary}>{line}</text>
          )}</For>
        </box>
      </Show>
    </Show>
  )
}
