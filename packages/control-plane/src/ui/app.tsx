import { useRenderer, useTerminalDimensions } from "@opentui/solid"
import { createSignal, Match, onCleanup, onMount, Show, Switch } from "solid-js"

import { MuxviaKeymapProvider, useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { TargetSession } from "../control/target-session"
import type { TargetAction, TargetView as TargetViewProjection } from "../control/types"
import { createCommandPresenter, createTranslator, messageKeyForProblem, type Locale, type Translator } from "../i18n"
import { theme } from "../theme"
import { ActionPrompt } from "./action-prompt"
import { ClaudeContext } from "./claude-context"
import { CommandPalette } from "./command-palette"
import { ExitConfirmation } from "./exit-confirmation"
import { Home } from "./home"
import { OverlayProvider, useOverlay } from "./overlay-stack"
import { ProviderDeleteConfirmation } from "./provider-delete-confirmation"
import { ProviderForm, type ProviderDraft, type ProviderFormRef, type ProviderFormResult } from "./provider-form"
import { ProviderPicker } from "./provider-picker"
import { TargetSidebar } from "./target-sidebar"
import { TargetView, type ActivityEntry } from "./target-view"

export type ShellRoute =
  | { kind: "home" }
  | { kind: "target"; target: "codex" | "claude" }

export interface AppProps {
  session: TargetSession
  locale?: Locale
}

type Notice = { kind: "error" | "success"; text: string }
type ActivityDraft = Omit<ActivityEntry, "id">
type Editor = { mode: "create" | "edit"; draft: ProviderDraft; credentialPresence: "present" | "missing" }

function moveIdentity(ids: readonly string[], id: string | undefined, delta: -1 | 1): string[] | undefined {
  if (!id) return undefined
  const index = ids.indexOf(id)
  const target = index + delta
  if (index < 0 || target < 0 || target >= ids.length) return undefined
  const next = [...ids]
  const [moved] = next.splice(index, 1)
  next.splice(target, 0, moved!)
  return next
}

function actionProblem(error: unknown): ActivityDraft {
  const code = typeof error === "object" && error !== null && "code" in error
    ? String(error.code)
    : "internal-failure"
  const messageKey = messageKeyForProblem(code)
  return {
    kind: "error",
    messageKey,
    values: messageKey === "error.generic" ? { code } : undefined,
  }
}

export function useCommandPaletteOpener(t: Translator): () => void {
  const keymap = useMuxviaKeymap()
  const overlay = useOverlay()
  let openScheduled = false
  let ownerActive = true

  onCleanup(() => { ownerActive = false })

  return () => {
    if (openScheduled) return
    openScheduled = true
    const entries = keymap.getCommandEntries({ visibility: "active", namespace: "palette" })
      .filter((entry) => entry.command.name !== "command.palette.show" && !entry.command.hidden)
    queueMicrotask(() => {
      if (!ownerActive) {
        openScheduled = false
        return
      }
      try {
        overlay.replace({
          id: "command-palette",
          render: () => <CommandPalette
            entries={entries}
            title={t("command.palette.title")}
            searchPlaceholder={t("command.palette.search")}
          />,
        })
      } finally {
        openScheduled = false
      }
    })
  }
}

function Shell(props: { session: TargetSession; t: Translator }) {
  const renderer = useRenderer()
  const dimensions = useTerminalDimensions()
  const overlay = useOverlay()
  const showCommandPalette = useCommandPaletteOpener(props.t)
  const [route, setRoute] = createSignal<ShellRoute>({ kind: "home" })
  const [view, setView] = createSignal<TargetViewProjection>(props.session.get())
  const [editor, setEditor] = createSignal<Editor>()
  const [selectedProviderId, setSelectedProviderId] = createSignal<string>()
  const [saving, setSaving] = createSignal(false)
  const [providerMutationPending, setProviderMutationPending] = createSignal(false)
  const [applying, setApplying] = createSignal(false)
  const [notice, setNotice] = createSignal<Notice>()
  const [activities, setActivities] = createSignal<ActivityEntry[]>([])
  const [sidebarOpen, setSidebarOpen] = createSignal(true)
  let providerFormRef: ProviderFormRef | undefined
  let nextActivityId = 1
  let lastViewSequence = props.session.get().viewSequence
  let editorGeneration = 0
  let exitScheduled = false
  let providerPickerScheduled = false
  let exiting = false
  let disposed = false

  onCleanup(() => { disposed = true })

  const appendActivity = (activity: ActivityDraft) => {
    if (disposed || exiting) return
    setActivities((current) => [...current, { id: nextActivityId++, ...activity }].slice(-50))
  }
  const installView = (next: TargetViewProjection, source: "action" | "subscription") => {
    if (disposed || exiting || next.viewSequence < lastViewSequence) return
    const increased = next.viewSequence > lastViewSequence
    if (increased) lastViewSequence = next.viewSequence
    setView(next)
    if (source === "subscription" && increased) {
      appendActivity({ kind: "info", messageKey: "activity.state.updated" })
    }
  }

  onMount(() => {
    const unsubscribe = props.session.subscribe((next) => installView(next, "subscription"))
    onCleanup(unsubscribe)
  })

  const isRoute = (target: "codex" | "claude") => {
    const current = route()
    return current.kind === "target" && current.target === target
  }
  const showHome = () => {
    providerFormRef?.clearSensitive()
    editorGeneration++
    setEditor()
    setNotice()
    setRoute({ kind: "home" })
  }
  const showTarget = (target: "codex" | "claude") => {
    setNotice()
    setRoute({ kind: "target", target })
  }
  const destroyRenderer = () => {
    if (exitScheduled || renderer.isDestroyed) return
    exitScheduled = true
    queueMicrotask(() => {
      exitScheduled = false
      if (!renderer.isDestroyed) renderer.destroy()
    })
  }
  const cancelExit = () => {
    overlay.closeTop()
    queueMicrotask(() => providerFormRef?.focus())
  }
  const confirmExit = () => {
    if (exiting) return
    exiting = true
    providerFormRef?.clearSensitive()
    editorGeneration++
    setEditor()
    overlay.clear()
    destroyRenderer()
  }
  const requestExit = () => {
    if (exitScheduled || exiting) return
    if (!providerFormRef?.isDirty()) {
      destroyRenderer()
      return
    }
    exitScheduled = true
    queueMicrotask(() => {
      exitScheduled = false
      if (disposed || exiting || renderer.isDestroyed || !providerFormRef?.isDirty()) return
      overlay.replace({
        id: "exit-confirmation",
        render: () => <ExitConfirmation t={props.t} onConfirm={confirmExit} onCancel={cancelExit} />,
        dismissOnEscape: false,
      })
    })
  }
  const unknownCommand = (input: string) => {
    const activity: ActivityDraft = {
      kind: "error",
      messageKey: "prompt.unknown",
      values: { command: input },
    }
    setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
    if (isRoute("codex")) appendActivity(activity)
  }
  const saveProvider = async (action: ProviderFormResult) => {
    if (saving()) return false
    const generation = editorGeneration
    const providerName = action.name
    setSaving(true)
    setNotice()
    try {
      const outcome = await props.session.act(action)
      if (disposed || exiting || generation !== editorGeneration) return false
      installView(outcome.view, "action")
      const activity: ActivityDraft = {
        kind: "success",
        messageKey: "activity.provider.saved",
        values: { name: providerName },
      }
      appendActivity(activity)
      setNotice({ kind: "success", text: props.t(activity.messageKey, activity.values) })
      return true
    } catch (error) {
      if (disposed || exiting || generation !== editorGeneration) return false
      installView(props.session.get() as TargetViewProjection, "action")
      const activity = actionProblem(error)
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
      return false
    } finally {
      if (!disposed && !exiting) setSaving(false)
    }
  }

  const selectedProvider = () => view().providers.find((provider) => provider.id === selectedProviderId())
  const openEditor = (mode: Editor["mode"], provider?: TargetViewProjection["providers"][number]) => {
    if (saving() || providerMutationPending()) return
    setNotice()
    editorGeneration++
    setEditor({
      mode,
      draft: provider
        ? {
          name: provider.name,
          baseUrl: provider.baseUrl,
          model: provider.model,
          providerId: provider.id,
          providerRevision: provider.providerRevision,
        }
        : { name: "", baseUrl: "", model: "" },
      credentialPresence: provider?.credential ?? "missing",
    })
  }
  const openProviderPicker = () => {
    if (saving() || providerMutationPending() || providerPickerScheduled) return
    providerPickerScheduled = true
    queueMicrotask(() => {
      providerPickerScheduled = false
      if (disposed || saving() || providerMutationPending()) return
      setNotice()
      const first = view().providers[0]
      setSelectedProviderId((current) => view().providers.some((provider) => provider.id === current) ? current : first?.id)
      overlay.replace({
        id: "provider-picker",
        render: () => <ProviderPicker
          providers={() => view().providers}
          selectedId={selectedProviderId}
          t={props.t}
          pending={providerMutationPending}
          onSelectedIdChange={setSelectedProviderId}
          onEdit={() => {
            const provider = selectedProvider()
            if (!provider || providerMutationPending()) return
            overlay.closeTop()
            openEditor("edit", provider)
          }}
          onMove={(delta) => moveSelected(delta)}
          onDelete={() => requestDelete()}
        />,
      })
    })
  }

  const runProviderMutation = async (action: Extract<TargetAction, { kind: "reorder-providers" | "delete-provider" }>) => {
    if (providerMutationPending()) return false
    setProviderMutationPending(true)
    setNotice()
    try {
      const outcome = await props.session.act(action)
      if (disposed || exiting) return false
      installView(outcome.view, "action")
      return true
    } catch (error) {
      if (disposed || exiting) return false
      installView(props.session.get() as TargetViewProjection, "action")
      const activity = actionProblem(error)
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
      return false
    } finally {
      if (!disposed && !exiting) setProviderMutationPending(false)
    }
  }
  const moveSelected = (delta: -1 | 1) => {
    const nextIds = moveIdentity(view().providers.map(({ id }) => id), selectedProviderId(), delta)
    if (nextIds) void runProviderMutation({ kind: "reorder-providers", providerIds: nextIds })
  }
  const requestDelete = () => {
    const provider = selectedProvider()
    if (!provider || providerMutationPending()) return
    overlay.push({
      id: "provider-delete-confirmation",
      dismissOnEscape: false,
      render: () => <ProviderDeleteConfirmation
        name={provider.name}
        t={props.t}
        pending={providerMutationPending()}
        onCancel={() => overlay.closeTop()}
        onConfirm={() => {
          overlay.closeTop()
          void runProviderMutation({
            kind: "delete-provider",
            providerId: provider.id,
            providerRevision: provider.providerRevision,
          })
        }}
      />,
    })
  }

  const applyTakeover = async () => {
    if (applying()) return
    const current = view().providers.find((provider) => provider.id === view().currentProviderId)
    const visible = current ?? view().providers[0]
    if (!visible) {
      const activity: ActivityDraft = { kind: "warning", messageKey: "activity.provider.required" }
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey) })
      return
    }
    const providerName = visible.name
    setApplying(true)
    setNotice()
    try {
      const outcome = await props.session.act({
        kind: "activate-provider",
        providerId: visible.id,
        mode: "takeover",
      })
      if (disposed || exiting) return
      installView(outcome.view, "action")
      const activity: ActivityDraft = {
        kind: "success",
        messageKey: "activity.takeover.applied",
        values: { name: providerName },
      }
      appendActivity(activity)
      setNotice({ kind: "success", text: props.t(activity.messageKey, activity.values) })
    } catch (error) {
      if (disposed || exiting) return
      installView(props.session.get() as TargetViewProjection, "action")
      const activity = actionProblem(error)
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
    } finally {
      if (!disposed && !exiting) setApplying(false)
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
    enabled: () => overlay.depth === 0 && isRoute("codex") && !editor(),
    handlers: {
      "target.home": showHome,
      "target.sidebar.toggle": () => setSidebarOpen((open) => !open),
      "provider.create": () => {
        if (saving() || applying()) return
        openEditor("create")
      },
      "provider.list": openProviderPicker,
      "target.takeover.apply": () => { void applyTakeover() },
    },
  })
  useCommandLayer({
    scope: "claude",
    priority: 100,
    enabled: () => overlay.depth === 0 && isRoute("claude"),
    handlers: { "target.home": showHome },
  })
  const horizontalPadding = () => dimensions().width >= 5 ? 2 : 0
  const innerWidth = () => Math.max(0, dimensions().width - horizontalPadding() * 2)
  const showSidebar = () => isRoute("codex") && dimensions().width > 120 && sidebarOpen()
  const sidebarGap = () => showSidebar() ? 2 : 0
  const sidebarWidth = () => Math.max(1, Math.min(42, Math.max(0, innerWidth() - sidebarGap() - 1)))
  const contentPaddingTop = () => Math.max(0, Math.min(1, dimensions().height - 1))

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
          <box flexGrow={1} flexShrink={1} flexDirection="row" columnGap={sidebarGap()}>
            <Show
              when={editor()}
              fallback={(
                <scrollbox minWidth={1} flexGrow={1} flexShrink={1} paddingTop={contentPaddingTop()}>
                  <TargetView view={view()} activities={activities()} t={props.t} />
                </scrollbox>
              )}
            >
              <scrollbox minWidth={1} flexGrow={1} flexShrink={1} paddingTop={contentPaddingTop()}>
                <box flexDirection="column" rowGap={1}>
                  <text fg={theme.primary}>MUXVIA</text>
                  <Show when={notice()}>
                    <text fg={notice()?.kind === "error" ? theme.error : theme.success}>{notice()?.text}</text>
                  </Show>
                  <ProviderForm
                    mode={editor()!.mode}
                    initialDraft={editor()!.draft}
                    credentialPresence={editor()!.credentialPresence}
                    t={props.t}
                    ref={(value) => { providerFormRef = value }}
                    pending={saving()}
                    onDirtyChange={() => {}}
                    onCancel={() => {
                      editorGeneration++
                      setEditor()
                    }}
                    onSave={saveProvider}
                  />
                </box>
              </scrollbox>
            </Show>
            <Show when={showSidebar()}>
              <TargetSidebar view={view()} t={props.t} width={sidebarWidth()} />
            </Show>
          </box>
          <Show when={!editor() && overlay.depth === 0}>
            <ActionPrompt
              scope="codex"
              placeholder={props.t("prompt.target")}
              focusEnabled={() => overlay.depth === 0}
              metadata={applying()
                ? props.t("activity.applying")
                : `${props.t("prompt.meta.codex")} · ${props.t("prompt.hint.sidebar")} · ${props.t("prompt.hint.back")} · ${props.t("prompt.hint.exit")}`}
              onUnknown={unknownCommand}
            />
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
