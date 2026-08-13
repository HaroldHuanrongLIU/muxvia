/** @jsxImportSource @opentui/solid */
import { createDefaultOpenTuiKeymap } from "@opentui/keymap/opentui"
import {
  registerBackspacePopsPendingSequence,
  registerEscapeClearsPendingSequence,
  registerTimedLeader,
} from "@opentui/keymap/addons"
import { registerBaseLayoutFallback } from "@opentui/keymap/addons/opentui"
import { KeymapProvider, reactiveMatcherFromSignal, useBindings, useKeymap } from "@opentui/keymap/solid"
import { useRenderer } from "@opentui/solid"
import { createContext, createMemo, onCleanup, useContext, type Accessor, type JSX } from "solid-js"

import { commandsForScope } from "./catalog"
import type { CommandId, CommandScope, CommandTextKey } from "./types"

type OpenTuiKeymap = ReturnType<typeof createDefaultOpenTuiKeymap>
type Enabled = Accessor<boolean>
type Presenter = NonNullable<MuxviaKeymapProviderProps["presenter"]>
const PresenterContext = createContext<Presenter | undefined>()

export interface MuxviaKeymapProviderProps {
  children: JSX.Element
  presenter?: (textKeys: { titleKey: CommandTextKey; descriptionKey: CommandTextKey }) => {
    title: string
    description: string
  }
}

function registerMuxviaFields(keymap: OpenTuiKeymap): () => void {
  const enabled = keymap.registerBindingFields({
    enabled(value, context) {
      context.activeWhen(value as Enabled)
    },
  })
  const metadata = keymap.registerCommandFields({
    textKey(value, context) { context.attr("textKey", value) },
    slashName(value, context) { context.attr("slashName", value) },
    hidden(value, context) { context.attr("hidden", value) },
  })
  return () => {
    metadata()
    enabled()
  }
}

export function MuxviaKeymapProvider(props: MuxviaKeymapProviderProps): JSX.Element {
  const renderer = useRenderer()
  const keymap = createDefaultOpenTuiKeymap(renderer)
  const dispose = [
    registerMuxviaFields(keymap),
    registerBaseLayoutFallback(keymap),
    registerTimedLeader(keymap, { trigger: "ctrl+x", name: "leader", timeoutMs: 2_000 }),
    registerEscapeClearsPendingSequence(keymap),
    registerBackspacePopsPendingSequence(keymap),
  ]
  onCleanup(() => {
    for (const unregister of dispose.reverse()) unregister()
  })
  return <PresenterContext.Provider value={props.presenter}>
    <KeymapProvider keymap={keymap}>{props.children}</KeymapProvider>
  </PresenterContext.Provider>
}

export function useMuxviaKeymap(): OpenTuiKeymap {
  return useKeymap()
}

export interface UseCommandLayerOptions {
  scope: CommandScope
  priority: number
  enabled?: Enabled
  handlers: Partial<Record<CommandId, () => void>>
  onExecute?: (id: CommandId) => void
}

export function useCommandLayer(options: UseCommandLayerOptions): void {
  const presenter = useContext(PresenterContext)
  const enabled = options.enabled ?? (() => true)
  const matcher = reactiveMatcherFromSignal(enabled)
  const commands = createMemo(() => commandsForScope(options.scope).map((definition) => {
    const text = presenter?.({ titleKey: definition.titleKey, descriptionKey: definition.descriptionKey }) ?? {
      title: definition.titleKey,
      description: definition.descriptionKey,
    }
    return {
      name: definition.id,
      textKey: definition.titleKey,
      slashName: definition.slashName,
      namespace: "palette",
      hidden: !definition.palette,
      title: text.title,
      desc: text.description,
      enabled: matcher,
      run: () => {
        const handler = options.handlers[definition.id]
        if (!handler) return false
        handler()
        options.onExecute?.(definition.id)
        return true
      },
    }
  }))
  useBindings(() => ({
    priority: options.priority,
    enabled: matcher,
    commands: commands(),
    bindings: commandsForScope(options.scope).flatMap((definition) => definition.bindings.map((key) => ({
      key,
      cmd: definition.id,
      enabled: matcher,
    }))),
  }))
}
