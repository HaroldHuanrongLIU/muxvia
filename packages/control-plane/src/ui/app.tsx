import { useRenderer, useTerminalDimensions } from "@opentui/solid"
import { createSignal, Match, onCleanup, onMount, Show, Switch } from "solid-js"

import { MuxviaKeymapProvider, useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { TargetSession } from "../control/target-session"
import type { ReachabilityResult, Target, TargetAction, TargetView as TargetViewProjection } from "../control/types"
import { createCommandPresenter, createTranslator, messageKeyForProblem, type Locale, type Translator } from "../i18n"
import { theme } from "../theme"
import { ActionPrompt } from "./action-prompt"
import { ClaudeContext } from "./claude-context"
import { CommandPalette } from "./command-palette"
import { ExitConfirmation } from "./exit-confirmation"
import { Home } from "./home"
import { OverlayProvider, useOverlay, type OverlayToken } from "./overlay-stack"
import { ProviderDeleteConfirmation } from "./provider-delete-confirmation"
import { ProviderCredentialConfirmation } from "./provider-credential-confirmation"
import { ProviderForm, type ProviderDraft, type ProviderFormRef, type ProviderFormResult } from "./provider-form"
import { ProviderPicker } from "./provider-picker"
import { ProviderSourcePicker, type ProviderSource } from "./provider-source-picker"
import { TargetSidebar } from "./target-sidebar"
import { TargetView, type ActivityEntry } from "./target-view"
import { TakeoverRequiredConfirm } from "./takeover-required-confirm"

export type ShellRoute =
  | { kind: "home" }
  | { kind: "target"; target: "codex" | "claude" }

export interface AppProps {
  session?: TargetSession
  sessions?: Partial<Record<Target, TargetSession>>
  unavailable?: Partial<Record<Target, string>>
  locale?: Locale
}

type Notice = { kind: "error" | "success"; text: string }
type ActivityDraft = Omit<ActivityEntry, "id">
type InspectionCategory = Extract<ReachabilityResult, { status: "unreachable" }>["failure"]["category"]
type Editor = {
  mode: "create" | "edit" | "duplicate"
  draft: ProviderDraft
  credentialPresence: "present" | "missing"
  duplicateCredentialChoice?: "without" | "reuse-source"
}

function safeInspectionCategory(error: unknown): InspectionCategory {
  const code = typeof error === "object" && error !== null && "code" in error ? String(error.code) : "connect"
  switch (code) {
    case "invalid-endpoint":
    case "missing-credential":
    case "missing-provider":
    case "stale-provider-revision":
    case "authentication-rejected":
    case "endpoint-unsupported":
    case "rate-limited":
    case "upstream-status":
    case "timeout":
    case "dns":
    case "connect":
    case "tls":
    case "cancelled":
    case "malformed-response":
    case "response-too-large":
    case "too-many-models":
      return code
    default:
      return "connect"
  }
}

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

export function useCommandPaletteOpener(t: Translator, canOpen: () => boolean = () => true): () => void {
  const keymap = useMuxviaKeymap()
  const overlay = useOverlay()
  let openScheduled = false
  let ownerActive = true

  onCleanup(() => { ownerActive = false })

  return () => {
    if (openScheduled || !canOpen()) return
    openScheduled = true
    const entries = keymap.getCommandEntries({ visibility: "active", namespace: "palette" })
      .filter((entry) => entry.command.name !== "command.palette.show" && !entry.command.hidden)
    queueMicrotask(() => {
      if (!ownerActive || !canOpen()) {
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

function Shell(props: {
  sessions: Partial<Record<Target, TargetSession>>
  unavailable: Partial<Record<Target, string>>
  t: Translator
}) {
  const renderer = useRenderer()
  const dimensions = useTerminalDimensions()
  const overlay = useOverlay()
  const [route, setRoute] = createSignal<ShellRoute>({ kind: "home" })
  const initialViews = Object.fromEntries(
    Object.entries(props.sessions).map(([target, session]) => [target, session!.get()]),
  ) as Partial<Record<Target, TargetViewProjection>>
  const [views, setViews] = createSignal(initialViews)
  const [editor, setEditor] = createSignal<Editor>()
  const [selectedProviderId, setSelectedProviderId] = createSignal<string>()
  const [savingByTarget, setSavingByTarget] = createSignal<Record<Target, boolean>>({ codex: false, claude: false })
  const [mutationByTarget, setMutationByTarget] = createSignal<Record<Target, boolean>>({ codex: false, claude: false })
  const [reachability, setReachability] = createSignal<{
    providerId: string
    providerRevision: number
    pending: boolean
    result?: ReachabilityResult
    errorCategory?: InspectionCategory
  }>()
  const [applyingByTarget, setApplyingByTarget] = createSignal<Record<Target, "direct" | "takeover" | undefined>>({ codex: undefined, claude: undefined })
  const showCommandPalette = useCommandPaletteOpener(props.t, () => applying() === undefined)
  const [notice, setNotice] = createSignal<Notice>()
  const [activities, setActivities] = createSignal<ActivityEntry[]>([])
  const [sidebarOpen, setSidebarOpen] = createSignal(true)
  let providerFormRef: ProviderFormRef | undefined
  let nextActivityId = 1
  const lastViewSequence: Partial<Record<Target, number>> = Object.fromEntries(
    Object.entries(initialViews).map(([target, initial]) => [target, initial!.viewSequence]),
  )
  let editorGeneration = 0
  let exitScheduled = false
  let providerPickerScheduled = false
  let providerSourcePickerScheduled = false
  let reachabilityAbort: AbortController | undefined
  let reachabilityGeneration = 0
  let exiting = false
  let disposed = false

  onCleanup(() => { disposed = true })

  const appendActivity = (activity: ActivityDraft) => {
    if (disposed || exiting) return
    setActivities((current) => [...current, { id: nextActivityId++, ...activity }].slice(-50))
  }
  const activeTarget = (): Target | undefined => {
    const current = route()
    return current.kind === "target" ? current.target : undefined
  }
  const saving = () => activeTarget() ? savingByTarget()[activeTarget()!] : false
  const setTargetSaving = (target: Target, value: boolean) =>
    setSavingByTarget((current) => ({ ...current, [target]: value }))
  const providerMutationPending = () => activeTarget() ? mutationByTarget()[activeTarget()!] : false
  const setTargetMutationPending = (target: Target, value: boolean) =>
    setMutationByTarget((current) => ({ ...current, [target]: value }))
  const applying = () => activeTarget() ? applyingByTarget()[activeTarget()!] : undefined
  const setTargetApplying = (target: Target, value: "direct" | "takeover" | undefined) =>
    setApplyingByTarget((current) => ({ ...current, [target]: value }))
  const session = (target = activeTarget()) => target ? props.sessions[target] : undefined
  const view = () => {
    const target = activeTarget()
    return target ? views()[target] : undefined
  }
  const installView = (next: TargetViewProjection, source: "action" | "subscription") => {
    const last = lastViewSequence[next.target] ?? -1
    if (disposed || exiting || next.viewSequence < last) return
    const inspection = reachability()
    if (inspection && activeTarget() === next.target) {
      const nextProvider = next.providers.find((provider) => provider.id === inspection.providerId)
      if (!nextProvider || nextProvider.providerRevision !== inspection.providerRevision) {
        reachabilityGeneration++
        reachabilityAbort?.abort()
        setReachability()
      }
    }
    const increased = next.viewSequence > last
    if (increased) lastViewSequence[next.target] = next.viewSequence
    setViews((current) => ({ ...current, [next.target]: next }))
    if (source === "subscription" && increased && activeTarget() === next.target) {
      appendActivity({ kind: "info", messageKey: "activity.state.updated" })
    }
  }

  onMount(() => {
    const unsubscribes = Object.values(props.sessions).map((targetSession) =>
      targetSession.subscribe((next) => installView(next, "subscription")))
    onCleanup(() => { for (const unsubscribe of unsubscribes) unsubscribe() })
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
    providerFormRef?.clearSensitive()
    editorGeneration++
    setEditor()
    overlay.clear()
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
    const targetSession = session()
    const target = activeTarget()
    if (!targetSession || !target) return false
    const generation = editorGeneration
    const providerName = action.name
    setTargetSaving(target, true)
    setNotice()
    try {
      const outcome = await targetSession.act(action)
      if (disposed || exiting) return false
      installView(outcome.view, "action")
      if (generation !== editorGeneration || activeTarget() !== target) return false
      const activity: ActivityDraft = {
        kind: "success",
        messageKey: "activity.provider.saved",
        values: { name: providerName },
      }
      appendActivity(activity)
      setNotice({ kind: "success", text: props.t(activity.messageKey, activity.values) })
      return true
    } catch (error) {
      const authoritative = targetSession.get() as TargetViewProjection
      if (disposed || exiting) return false
      installView(authoritative, "action")
      if (generation !== editorGeneration || activeTarget() !== target) return false
      const code = typeof error === "object" && error !== null && "code" in error
        ? String(error.code)
        : "internal-failure"
      if (action.kind === "update-provider" && code === "stale-provider-revision") {
        const latest = authoritative.providers.find((provider) => provider.id === action.providerId)
        if (latest) {
          setEditor((current) => current && current.mode === "edit" && current.draft.providerId === latest.id
            ? { ...current, draft: { ...current.draft, providerRevision: latest.providerRevision } }
            : current)
        }
      }
      const activity = actionProblem(error)
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
      return false
    } finally {
      if (!disposed && !exiting) setTargetSaving(target, false)
    }
  }

  const selectedProvider = () => view()?.providers.find((provider) => provider.id === selectedProviderId())
  const openEditor = (
    mode: Editor["mode"],
    provider?: TargetViewProjection["providers"][number],
    options: Pick<Editor, "duplicateCredentialChoice"> & { source?: ProviderSource } = {},
  ) => {
    if (saving() || providerMutationPending()) return
    setNotice()
    editorGeneration++
    const source = options.source
    setEditor({
      mode,
      draft: source?.kind === "preset"
        ? {
          name: "",
          baseUrl: source.preset.baseUrl,
          model: source.preset.model,
          presetKey: source.preset.key,
          authentication: source.preset.authentication,
        }
        : provider
        ? {
          name: mode === "duplicate" ? props.t("provider.duplicate.copy-name", { name: provider.name }) : provider.name,
          baseUrl: provider.baseUrl,
          model: provider.model,
          authentication: provider.authentication,
          providerId: provider.id,
          providerRevision: provider.providerRevision,
        }
        : { name: "", baseUrl: "", model: "" },
      credentialPresence: provider?.credential ?? "missing",
      duplicateCredentialChoice: options.duplicateCredentialChoice,
    })
  }
  const openProviderSourcePicker = () => {
    if (!session() || saving() || applying() || providerSourcePickerScheduled) return
    providerSourcePickerScheduled = true
    queueMicrotask(() => {
      providerSourcePickerScheduled = false
      if (disposed || saving() || applying()) return
      overlay.replace({
        id: "provider-source-picker",
        render: () => <ProviderSourcePicker
          presets={view()?.providerPresets ?? []}
          t={props.t}
          onSelect={(source) => {
            overlay.closeTop()
            openEditor("create", undefined, { source })
          }}
        />,
      })
    })
  }
  const openProviderPicker = () => {
    if (saving() || providerMutationPending() || applying() || providerPickerScheduled) return
    providerPickerScheduled = true
    queueMicrotask(() => {
      providerPickerScheduled = false
      if (disposed || saving() || providerMutationPending() || applying()) return
      setNotice()
      const currentView = view()
      if (!currentView) return
      const preferred = currentView.providers.find((provider) => provider.id === currentView.currentProviderId) ?? currentView.providers[0]
      setSelectedProviderId((selected) => currentView.providers.some((provider) => provider.id === selected)
        ? selected
        : preferred?.id)
      const pickerToken = Symbol("provider-picker")
      overlay.replace({
        id: "provider-picker",
        token: pickerToken,
        dismissOnEscape: () => applying() === undefined,
        render: () => <ProviderPicker
          target={activeTarget()!}
          providers={() => view()?.providers ?? []}
          selectedId={selectedProviderId}
          t={props.t}
          pending={() => providerMutationPending() || applying() !== undefined}
          activationMode={applying}
          allowDirect={() => activeTarget() === "codex"}
          onSelectedIdChange={setSelectedProviderId}
          onEdit={() => {
            const provider = selectedProvider()
            if (!provider || providerMutationPending()) return
            overlay.close(pickerToken)
            openEditor("edit", provider)
          }}
          onActivateDirect={() => {
            const provider = selectedProvider()
            if (!provider || applying()) return
            void activateProvider(provider.id, "direct", pickerToken)
          }}
          onActivateTakeover={() => {
            const provider = selectedProvider()
            if (!provider || applying()) return
            void activateProvider(provider.id, "takeover", pickerToken)
          }}
          onDuplicate={() => requestDuplicate()}
          reachability={() => {
            const current = reachability()
            const selected = selectedProvider()
            if (!current || !selected) return undefined
            return current.providerId === selected.id && current.providerRevision === selected.providerRevision
              ? current
              : undefined
          }}
          onCheckReachability={() => { void checkSelectedReachability() }}
          onMove={(delta) => moveSelected(delta)}
          onDelete={() => requestDelete()}
        />,
        onClose: () => {
          reachabilityGeneration++
          reachabilityAbort?.abort()
          reachabilityAbort = undefined
          setReachability()
        },
      })
    })
  }

  const openDuplicateEditor = (provider: TargetViewProjection["providers"][number], choice: "without" | "reuse-source") => {
    overlay.clear()
    openEditor("duplicate", provider, { duplicateCredentialChoice: choice })
  }
  const requestDuplicate = () => {
    const provider = selectedProvider()
    if (!provider || providerMutationPending()) return
    if (provider.credential === "missing") {
      openDuplicateEditor(provider, "without")
      return
    }
    overlay.push({
      id: "provider-credential-confirmation",
      dismissOnEscape: false,
      render: () => <ProviderCredentialConfirmation
        sourceName={provider.name}
        t={props.t}
        onReuse={() => openDuplicateEditor(provider, "reuse-source")}
        onWithout={() => openDuplicateEditor(provider, "without")}
        onCancel={() => overlay.closeTop()}
      />,
    })
  }

  const checkSelectedReachability = async () => {
    const provider = selectedProvider()
    const targetSession = session()
    const target = activeTarget()
    if (!provider || !targetSession || !target) return
    reachabilityAbort?.abort()
    const controller = new AbortController()
    reachabilityAbort = controller
    const generation = ++reachabilityGeneration
    setReachability({ providerId: provider.id, providerRevision: provider.providerRevision, pending: true })
    try {
      const result = await targetSession.checkReachability(provider.id, provider.providerRevision, controller.signal)
      if (disposed || controller.signal.aborted || generation !== reachabilityGeneration || activeTarget() !== target) return
      if (result.status === "unreachable" && result.failure.category === "cancelled") {
        setReachability()
        return
      }
      setReachability({ providerId: provider.id, providerRevision: provider.providerRevision, pending: false, result })
    } catch (error) {
      if (disposed || controller.signal.aborted || generation !== reachabilityGeneration) return
      const category = safeInspectionCategory(error)
      if (category === "cancelled") {
        setReachability()
        return
      }
      setReachability({
        providerId: provider.id,
        providerRevision: provider.providerRevision,
        pending: false,
        errorCategory: category,
      })
    }
  }

  const runProviderMutation = async (action: Extract<TargetAction, { kind: "reorder-providers" | "delete-provider" }>) => {
    if (providerMutationPending()) return false
    const targetSession = session()
    const target = activeTarget()
    if (!targetSession || !target) return false
    setTargetMutationPending(target, true)
    setNotice()
    try {
      const outcome = await targetSession.act(action)
      if (disposed || exiting) return false
      installView(outcome.view, "action")
      if (activeTarget() !== target) return false
      if (action.kind === "delete-provider" && selectedProviderId() === action.providerId) {
        setSelectedProviderId(outcome.view.providers[0]?.id)
      }
      return true
    } catch (error) {
      if (disposed || exiting) return false
      installView(targetSession.get() as TargetViewProjection, "action")
      if (activeTarget() !== target) return false
      const activity = actionProblem(error)
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
      return false
    } finally {
      if (!disposed && !exiting) setTargetMutationPending(target, false)
    }
  }
  const moveSelected = (delta: -1 | 1) => {
    const nextIds = moveIdentity((view()?.providers ?? []).map(({ id }) => id), selectedProviderId(), delta)
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

  const openTakeoverConfirmation = (providerId: string, providerName: string, pickerToken?: OverlayToken) => {
    overlay.push({
      id: "takeover-required-confirm",
      dismissOnEscape: false,
      render: () => <TakeoverRequiredConfirm
        providerName={providerName}
        t={props.t}
        onConfirm={() => {
          overlay.closeTop()
          void activateProvider(providerId, "takeover", pickerToken)
        }}
        onCancel={() => overlay.closeTop()}
      />,
    })
  }

  const showActivationProblem = (code: string) => {
    const activity = actionProblem({ code })
    appendActivity(activity)
    setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
  }

  const closeOriginPicker = (pickerToken?: OverlayToken) => {
    if (pickerToken) overlay.close(pickerToken)
  }

  const activateProvider = async (
    providerId: string,
    mode: "direct" | "takeover",
    pickerToken?: OverlayToken,
  ) => {
    if (applying()) return
    const targetSession = session()
    const target = activeTarget()
    if (!targetSession || !target || (target === "claude" && mode === "direct")) return
    const provider = view()?.providers.find((candidate) => candidate.id === providerId)
    if (!provider) {
      closeOriginPicker(pickerToken)
      showActivationProblem("missing-provider")
      return
    }
    if (mode === "direct" && provider.completeness === "incomplete") {
      closeOriginPicker(pickerToken)
      showActivationProblem("incomplete-provider")
      return
    }
    if (mode === "direct" && provider.routingRequirement === "takeover-required") {
      openTakeoverConfirmation(provider.id, provider.name, pickerToken)
      return
    }
    const providerName = provider.name
    setTargetApplying(target, mode)
    setNotice()
    try {
      const outcome = await targetSession.act({
        kind: "activate-provider",
        providerId,
        mode,
      })
      if (disposed || exiting) return
      installView(outcome.view, "action")
      if (activeTarget() !== target) return
      closeOriginPicker(pickerToken)
      const activity: ActivityDraft = {
        kind: "success",
        messageKey: mode === "direct" ? "activity.direct.applied" : "activity.takeover.applied",
        values: { name: providerName },
      }
      appendActivity(activity)
      setNotice({ kind: "success", text: props.t(activity.messageKey, activity.values) })
    } catch (error) {
      const authoritative = targetSession.get() as TargetViewProjection
      if (disposed || exiting) return
      installView(authoritative, "action")
      if (activeTarget() !== target) return
      const code = typeof error === "object" && error !== null && "code" in error
        ? String(error.code)
        : "internal-failure"
      if (mode === "direct" && code === "takeover-required") {
        const authoritativeProvider = authoritative.providers.find((candidate) => candidate.id === providerId)
        openTakeoverConfirmation(providerId, authoritativeProvider?.name ?? providerName, pickerToken)
        return
      }
      closeOriginPicker(pickerToken)
      const activity = actionProblem(error)
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) })
    } finally {
      if (!disposed && !exiting) setTargetApplying(target, undefined)
    }
  }

  const applyDefaultProvider = (mode: "direct" | "takeover") => {
    const currentView = view()
    const current = currentView?.providers.find((provider) => provider.id === currentView.currentProviderId)
    const provider = current ?? currentView?.providers[0]
    if (!provider) {
      const activity: ActivityDraft = { kind: "warning", messageKey: "activity.provider.required" }
      appendActivity(activity)
      setNotice({ kind: "error", text: props.t(activity.messageKey) })
      return
    }
    void activateProvider(provider.id, mode)
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
        openProviderSourcePicker()
      },
      "provider.list": openProviderPicker,
      "target.direct.apply": () => applyDefaultProvider("direct"),
      "target.takeover.apply": () => applyDefaultProvider("takeover"),
    },
  })
  useCommandLayer({
    scope: "claude",
    priority: 100,
    enabled: () => overlay.depth === 0 && isRoute("claude") && !editor(),
    handlers: {
      "target.home": showHome,
      "target.sidebar.toggle": () => setSidebarOpen((open) => !open),
      "provider.create": openProviderSourcePicker,
      "provider.list": openProviderPicker,
      "target.takeover.apply": () => applyDefaultProvider("takeover"),
    },
  })
  const horizontalPadding = () => dimensions().width >= 5 ? 2 : 0
  const innerWidth = () => Math.max(0, dimensions().width - horizontalPadding() * 2)
  const showSidebar = () => route().kind === "target" && !!session() && dimensions().width > 120 && sidebarOpen()
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
        <Match when={route().kind === "target" && !session()}>
          <ClaudeContext
            target={activeTarget() ?? "claude"}
            t={props.t}
            notice={props.t(messageKeyForProblem(props.unavailable[activeTarget() ?? "claude"] ?? "service-unavailable"))}
            onUnknown={unknownCommand}
          />
        </Match>
        <Match when={route().kind === "target" && !!session() && !!view()}>
          <box flexGrow={1} flexShrink={1} flexDirection="row" columnGap={sidebarGap()}>
            <Show
              when={editor()}
              fallback={(
                <scrollbox minWidth={1} flexGrow={1} flexShrink={1} paddingTop={contentPaddingTop()}>
                  <TargetView view={view()!} activities={activities()} t={props.t} />
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
                    mode={editor()?.mode ?? "create"}
                    initialDraft={editor()?.draft ?? { name: "", baseUrl: "", model: "" }}
                    credentialPresence={editor()?.credentialPresence ?? "missing"}
                    duplicateCredentialChoice={editor()?.duplicateCredentialChoice}
                    visibleProviderRevision={editor()?.draft.providerId
                      ? view()?.providers.find((provider) => provider.id === editor()?.draft.providerId)?.providerRevision
                      : undefined}
                    target={activeTarget()!}
                    discoverModels={(source, signal) => session()!.discoverModels(source, signal)}
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
              <TargetSidebar view={view()!} t={props.t} width={sidebarWidth()} />
            </Show>
          </box>
          <Show when={!editor() && overlay.depth === 0}>
            <ActionPrompt
              scope={activeTarget()!}
              placeholder={props.t("prompt.target")}
              focusEnabled={() => overlay.depth === 0}
              metadata={applying()
                ? props.t(applying() === "direct" ? "activity.direct.applying" : "activity.applying")
                : `${props.t(activeTarget() === "claude" ? "prompt.meta.claude" : "prompt.meta.codex")} · ${props.t("prompt.hint.sidebar")} · ${props.t("prompt.hint.back")} · ${props.t("prompt.hint.exit")}`}
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
  const sessions = props.sessions ?? (props.session ? { [props.session.get().target]: props.session } : {})
  return (
    <MuxviaKeymapProvider presenter={createCommandPresenter(t)}>
      <OverlayProvider>
        <Shell sessions={sessions} unavailable={props.unavailable ?? {}} t={t} />
      </OverlayProvider>
    </MuxviaKeymapProvider>
  )
}
