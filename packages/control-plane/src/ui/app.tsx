import { useRenderer, useTerminalDimensions } from "@opentui/solid"
import { createSignal, getOwner, Match, onCleanup, onMount, runWithOwner, Show, Switch } from "solid-js"

import { MuxviaKeymapProvider, useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { TargetSession } from "../control/target-session"
import type { TargetAction, TargetView as TargetViewProjection } from "../control/types"
import { createCommandPresenter, createTranslator, type Locale, type Translator } from "../i18n"
import { theme } from "../theme"
import { ActionPrompt } from "./action-prompt"
import { ClaudeContext } from "./claude-context"
import { CommandPalette } from "./command-palette"
import { Home } from "./home"
import { OverlayProvider, useOverlay } from "./overlay-stack"
import { ProviderForm } from "./provider-form"
import { TargetView } from "./target-view"

export type ShellRoute =
  | { kind: "home" }
  | { kind: "target"; target: "codex" | "claude" }

export interface AppProps {
  session: TargetSession
  locale?: Locale
}

type Notice = { kind: "error" | "success"; text: string }

function actionProblem(error: unknown): string {
  const code = typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : "internal-failure"
  if (code === "stale-revision") return "Target state changed. Retry the action."
  if (code === "invalid-provider" || code === "incomplete-provider") {
    return "Provider details are invalid. Check the Provider fields and try again."
  }
  return `Action failed (${code}). Review the Target state and try again.`
}

function Shell(props: { session: TargetSession; t: Translator }) {
  const renderer = useRenderer()
  const dimensions = useTerminalDimensions()
  const keymap = useMuxviaKeymap()
  const overlay = useOverlay()
  const owner = getOwner()
  const [route, setRoute] = createSignal<ShellRoute>({ kind: "home" })
  const [view, setView] = createSignal<TargetViewProjection>(props.session.get())
  const [providerForm, setProviderForm] = createSignal(false)
  const [saving, setSaving] = createSignal(false)
  const [applying, setApplying] = createSignal(false)
  const [notice, setNotice] = createSignal<Notice>()
  const sidebar = createSignal(true)

  onMount(() => {
    const unsubscribe = props.session.subscribe(setView)
    onCleanup(unsubscribe)
  })

  const isRoute = (target: "codex" | "claude") => {
    const current = route()
    return current.kind === "target" && current.target === target
  }
  const showHome = () => {
    setProviderForm(false)
    setNotice()
    setRoute({ kind: "home" })
  }
  const showTarget = (target: "codex" | "claude") => {
    setNotice()
    setRoute({ kind: "target", target })
  }
  const requestExit = () => {
    if (!renderer.isDestroyed) renderer.destroy()
  }
  const unknownCommand = (input: string) => {
    setNotice({ kind: "error", text: props.t("prompt.unknown", { command: input }) })
  }
  const showCommandPalette = () => {
    const entries = keymap.getCommandEntries({ visibility: "active", namespace: "palette" })
      .filter((entry) => entry.command.name !== "command.palette.show" && !entry.command.hidden)
    queueMicrotask(() => {
      const element = runWithOwner(owner!, () => (
        <CommandPalette
          entries={entries}
          title={props.t("command.palette.title")}
          searchPlaceholder={props.t("command.palette.search")}
        />
      ))
      overlay.replace({ id: "command-palette", element })
    })
  }

  const saveProvider = async (action: Extract<TargetAction, { kind: "save-provider" }>) => {
    if (saving()) return false
    setSaving(true)
    setNotice()
    try {
      const outcome = await props.session.act(action)
      setView(outcome.view)
      setProviderForm(false)
      setNotice({ kind: "success", text: "Provider saved." })
      return true
    } catch (error) {
      setView(props.session.get() as TargetViewProjection)
      setNotice({ kind: "error", text: actionProblem(error) })
      return false
    } finally {
      setSaving(false)
    }
  }

  const applyTakeover = async () => {
    if (applying()) return
    const current = view().providers.find((provider) => provider.id === view().currentProviderId)
    const visible = current ?? view().providers[0]
    if (!visible) {
      setNotice({ kind: "error", text: "Create a Provider before applying Target Takeover." })
      return
    }
    setApplying(true)
    setNotice()
    try {
      const outcome = await props.session.act({
        kind: "activate-provider",
        providerId: visible.id,
        mode: "takeover",
      })
      setView(outcome.view)
      setNotice({ kind: "success", text: "Target Takeover applied." })
    } catch (error) {
      setView(props.session.get() as TargetViewProjection)
      setNotice({ kind: "error", text: actionProblem(error) })
    } finally {
      setApplying(false)
    }
  }

  useCommandLayer({
    scope: "global",
    priority: 0,
    enabled: () => overlay.depth === 0,
    handlers: {
      "command.palette.show": showCommandPalette,
      "app.exit.request": requestExit,
    },
  })
  useCommandLayer({
    scope: "home",
    priority: 100,
    enabled: () => overlay.depth === 0 && route().kind === "home",
    handlers: {
      "target.codex.open": () => showTarget("codex"),
      "target.claude.open": () => showTarget("claude"),
    },
  })
  useCommandLayer({
    scope: "codex",
    priority: 100,
    enabled: () => overlay.depth === 0 && isRoute("codex") && !providerForm(),
    handlers: {
      "target.home": showHome,
      "target.sidebar.toggle": () => sidebar[1]((open) => !open),
      "provider.create": () => {
        if (saving() || applying()) return
        setNotice()
        setProviderForm(true)
      },
      "target.takeover.apply": () => { void applyTakeover() },
    },
  })
  useCommandLayer({
    scope: "claude",
    priority: 100,
    enabled: () => overlay.depth === 0 && isRoute("claude"),
    handlers: { "target.home": showHome },
  })
  useCommandLayer({
    scope: "editor",
    priority: 200,
    enabled: () => overlay.depth === 0 && providerForm(),
    handlers: {
      "provider.cancel": () => setProviderForm(false),
    },
  })

  const horizontalPadding = () => dimensions().width >= 5 ? 2 : 0

  return (
    <box
      width="100%"
      height="100%"
      backgroundColor={theme.background}
      flexDirection="column"
      paddingLeft={horizontalPadding()}
      paddingRight={horizontalPadding()}
    >
      <Switch>
        <Match when={route().kind === "home"}>
          <Home t={props.t} notice={notice()?.text} onUnknown={unknownCommand} />
        </Match>
        <Match when={isRoute("claude")}>
          <ClaudeContext t={props.t} notice={notice()?.text} onUnknown={unknownCommand} />
        </Match>
        <Match when={isRoute("codex")}>
          <Show
            when={providerForm()}
            fallback={(
              <>
                <scrollbox flexGrow={1} flexShrink={1} paddingTop={Math.max(0, Math.min(1, dimensions().height - 1))}>
                  <TargetView view={view()} notice={notice()} />
                </scrollbox>
                <ActionPrompt
                  scope="codex"
                  placeholder={props.t("prompt.target")}
                  metadata={applying()
                    ? props.t("activity.applying")
                    : `${props.t("prompt.meta.codex")} · ${props.t("prompt.hint.sidebar")} · ${props.t("prompt.hint.back")} · ${props.t("prompt.hint.exit")}`}
                  onUnknown={unknownCommand}
                />
              </>
            )}
          >
            <scrollbox flexGrow={1} flexShrink={1} paddingTop={Math.max(0, Math.min(1, dimensions().height - 1))}>
              <box flexDirection="column" rowGap={1}>
                <text fg={theme.primary}>MUXVIA</text>
                <Show when={notice()}>
                  <text fg={notice()?.kind === "error" ? theme.error : theme.success}>{notice()?.text}</text>
                </Show>
                <ProviderForm
                  pending={saving()}
                  onCancel={() => setProviderForm(false)}
                  onSave={saveProvider}
                />
              </box>
            </scrollbox>
          </Show>
        </Match>
      </Switch>
    </box>
  )
}

export function App(props: AppProps) {
  const t = createTranslator(props.locale ?? "en")
  return (
    <MuxviaKeymapProvider presenter={createCommandPresenter(t)}>
      <OverlayProvider>
        <Shell session={props.session} t={t} />
      </OverlayProvider>
    </MuxviaKeymapProvider>
  )
}
