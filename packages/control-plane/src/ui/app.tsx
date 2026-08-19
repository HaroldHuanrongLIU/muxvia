import { useRenderer, useTerminalDimensions } from "@opentui/solid"
import { createSignal, Match, onCleanup, onMount, Show, Switch, type Accessor } from "solid-js"

import { MuxviaKeymapProvider, useCommandLayer, useMuxviaKeymap } from "../commands/keymap"
import type { TargetSession } from "../control/target-session"
import type { UniversalProviderSession } from "../control/universal-provider-session"
import type { ReachabilityResult, ReconciliationStrategy, Target, TargetAction, TargetView as TargetViewProjection, UniversalProviderAction, UniversalProviderCatalogView } from "../control/types"
import { createCommandPresenter, createTranslator, messageKeyForProblem, type Locale, type Translator } from "../i18n"
import { theme } from "../theme"
import { ActionPrompt } from "./action-prompt"
import { ClaudeContext } from "./claude-context"
import { CommandPalette } from "./command-palette"
import { ExitConfirmation } from "./exit-confirmation"
import { Home } from "./home"
import { GeneratedProviderForm } from "./generated-provider-form"
import { OverlayProvider, useOverlay, type OverlayToken } from "./overlay-stack"
import { ProviderDeleteConfirmation } from "./provider-delete-confirmation"
import { ProviderCredentialConfirmation } from "./provider-credential-confirmation"
import { ProviderForm, type ProviderDraft, type ProviderFormRef, type ProviderFormResult } from "./provider-form"
import { ProviderPicker } from "./provider-picker"
import { ProviderSourcePicker, type ProviderSource } from "./provider-source-picker"
import { Reconciliation, type ReconciliationUiState } from "./reconciliation"
import { TargetSidebar } from "./target-sidebar"
import { TargetView, type ActivityEntry } from "./target-view"
import { TakeoverRequiredConfirm } from "./takeover-required-confirm"
import { UniversalProviderConfirmation } from "./universal-provider-confirmation"
import { UniversalProviderEditor } from "./universal-provider-editor"
import { UniversalProviderPicker } from "./universal-provider-picker"
import { UniversalProviderSourcePicker } from "./universal-provider-source-picker"

export type ShellRoute =
  | { kind: "home" }
  | { kind: "target"; target: "codex" | "claude" }

export interface AppProps {
  session?: TargetSession
  sessions?: Partial<Record<Target, TargetSession>> | Accessor<Partial<Record<Target, TargetSession>>>
  unavailable?: Partial<Record<Target, string>> | Accessor<Partial<Record<Target, string>>>
  universalSession?: UniversalProviderSession | Accessor<UniversalProviderSession | undefined>
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
  dirty?: boolean
}
type ReconciliationWorkflowState = ReconciliationUiState & {
  overlayToken: OverlayToken
  originSession: TargetSession
  generation: number
}

const reconciliationProblemCodes = new Set([
  "compatibility-acknowledgement-required",
  "configuration-drift",
  "shadowing-configuration",
  "untested-target-cli",
  "incompatible-target-cli",
])

function managedWriteProblem(view: TargetViewProjection | undefined): string | undefined {
  return view?.problems.find((problem) => reconciliationProblemCodes.has(problem.code))?.code
}

function safeReconciliationProblem(error: unknown): string {
  let code = "internal-failure"
  try {
    if (typeof error === "object" && error !== null && "code" in error) code = String(error.code)
  } catch {
    return "internal-failure"
  }
  switch (code) {
    case "compatibility-acknowledgement-required":
    case "configuration-write-failed":
    case "incompatible-target-cli":
    case "recovery-required":
    case "shadowing-configuration":
    case "stale-compatibility-probe":
    case "stale-reconciliation-preview":
    case "stale-revision":
    case "target-busy":
    case "untested-target-cli":
      return code
    default:
      return "internal-failure"
  }
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

function safeUniversalProblem(error: unknown): string {
  const code = typeof error === "object" && error !== null && "code" in error ? String(error.code) : "internal-failure"
  switch (code) {
    case "generated-provider-referenced":
    case "generated-provider-delete-forbidden":
    case "generated-provider-read-only":
    case "invalid-universal-provider":
    case "no-universal-provider-change":
    case "provider-synchronization-blocked":
    case "recovery-required":
    case "shadowing-configuration":
    case "stale-universal-catalog-revision":
    case "stale-universal-provider-revision":
    case "state-store-error":
    case "untested-target-cli":
    case "incompatible-target-cli":
    case "configuration-drift":
    case "compatibility-acknowledgement-required":
      return code
    default:
      return "internal-failure"
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
  const source = typeof error === "object" && error !== null && "source" in error
    ? String(error.source)
    : undefined
  const selector = typeof error === "object" && error !== null && "selector" in error
    ? String(error.selector)
    : undefined
  return {
    kind: "error",
    messageKey,
    values: messageKey === "error.generic" ? { code } : { source: source ?? "unknown", selector: selector ?? "unknown" },
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
  sessions: Accessor<Partial<Record<Target, TargetSession>>>
  unavailable: Accessor<Partial<Record<Target, string>>>
  universalSession: Accessor<UniversalProviderSession | undefined>
  t: Translator
}) {
  const renderer = useRenderer()
  const dimensions = useTerminalDimensions()
  const overlay = useOverlay()
  const [route, setRoute] = createSignal<ShellRoute>({ kind: "home" })
  const initialViews = Object.fromEntries(
    Object.entries(props.sessions()).map(([target, session]) => [target, session!.get()]),
  ) as Partial<Record<Target, TargetViewProjection>>
  const [views, setViews] = createSignal(initialViews)
  const [editors, setEditors] = createSignal<Partial<Record<Target, Editor>>>({})
  const [selectedProviderIds, setSelectedProviderIds] = createSignal<Partial<Record<Target, string>>>({})
  const [savingByTarget, setSavingByTarget] = createSignal<Record<Target, boolean>>({ codex: false, claude: false })
  const [mutationByTarget, setMutationByTarget] = createSignal<Record<Target, boolean>>({ codex: false, claude: false })
  type ReachabilityState = {
    providerId: string
    providerRevision: number
    pending: boolean
    result?: ReachabilityResult
    errorCategory?: InspectionCategory
  }
  const [reachabilityByTarget, setReachabilityByTarget] = createSignal<Partial<Record<Target, ReachabilityState>>>({})
  const [applyingByTarget, setApplyingByTarget] = createSignal<Record<Target, "direct" | "takeover" | undefined>>({ codex: undefined, claude: undefined })
  const [reconciliationByTarget, setReconciliationByTarget] = createSignal<Partial<Record<Target, ReconciliationWorkflowState>>>({})
  const showCommandPalette = useCommandPaletteOpener(props.t, () => applying() === undefined)
  const [notices, setNotices] = createSignal<Partial<Record<Target | "home", Notice>>>({})
  const [activitiesByTarget, setActivitiesByTarget] = createSignal<Record<Target, ActivityEntry[]>>({ codex: [], claude: [] })
  const [sidebarOpenByTarget, setSidebarOpenByTarget] = createSignal<Record<Target, boolean>>({ codex: true, claude: true })
  const [universalView, setUniversalView] = createSignal<UniversalProviderCatalogView | undefined>(props.universalSession()?.get() as UniversalProviderCatalogView | undefined)
  const [universalPending, setUniversalPending] = createSignal(false)
  const [universalNotice, setUniversalNotice] = createSignal<string>()
  const [universalNoticeKind, setUniversalNoticeKind] = createSignal<"error" | "success">()
  const [selectedUniversalProviderId, setSelectedUniversalProviderId] = createSignal<string>()
  const providerFormRefs: Partial<Record<Target, ProviderFormRef>> = {}
  const providerFormRefCallbacks = Object.fromEntries(
    (["codex", "claude"] as const).map((target) => [target, (value: ProviderFormRef | undefined) => {
      if (value) providerFormRefs[target] = value
      else delete providerFormRefs[target]
    }]),
  ) as Record<Target, (value: ProviderFormRef | undefined) => void>
  let nextActivityId = 1
  const lastViewSequence: Partial<Record<Target, number>> = Object.fromEntries(
    Object.entries(initialViews).map(([target, initial]) => [target, initial!.viewSequence]),
  )
  const editorGenerations: Record<Target, number> = { codex: 0, claude: 0 }
  let exitScheduled = false
  const providerPickerScheduled: Record<Target, boolean> = { codex: false, claude: false }
  const providerSourcePickerScheduled: Record<Target, boolean> = { codex: false, claude: false }
  const reconciliationScheduled: Record<Target, boolean> = { codex: false, claude: false }
  const reconciliationGenerations: Record<Target, number> = { codex: 0, claude: 0 }
  const reconciliationAborts: Partial<Record<Target, AbortController>> = {}
  const reachabilityAborts: Partial<Record<Target, AbortController>> = {}
  const reachabilityGenerations: Record<Target, number> = { codex: 0, claude: 0 }
  let exiting = false
  let disposed = false

  onCleanup(() => {
    disposed = true
    reconciliationAborts.codex?.abort()
    reconciliationAborts.claude?.abort()
  })

  const activeTarget = (): Target | undefined => {
    const current = route()
    return current.kind === "target" ? current.target : undefined
  }
  const editor = () => {
    const target = activeTarget()
    return target ? editors()[target] : undefined
  }
  const setEditor = (next?: Editor | ((current?: Editor) => Editor | undefined), target = activeTarget()) => {
    if (!target) return
    setEditors((current) => {
      const value = typeof next === "function" ? next(current[target]) : next
      return { ...current, [target]: value }
    })
  }
  const selectedProviderId = () => {
    const target = activeTarget()
    return target ? selectedProviderIds()[target] : undefined
  }
  const setSelectedProviderId = (next: string | undefined | ((current?: string) => string | undefined), target = activeTarget()) => {
    if (!target) return
    setSelectedProviderIds((current) => ({
      ...current,
      [target]: typeof next === "function" ? next(current[target]) : next,
    }))
  }
  const reachability = () => {
    const target = activeTarget()
    return target ? reachabilityByTarget()[target] : undefined
  }
  const setReachability = (next?: ReachabilityState, target = activeTarget()) => {
    if (!target) return
    setReachabilityByTarget((current) => ({ ...current, [target]: next }))
  }
  const routeStateKey = (): Target | "home" => activeTarget() ?? "home"
  const notice = () => notices()[routeStateKey()]
  const setNotice = (next?: Notice, key: Target | "home" = routeStateKey()) =>
    setNotices((current) => ({ ...current, [key]: next }))
  const activities = () => activitiesByTarget()[activeTarget() ?? "codex"]
  const sidebarOpen = () => sidebarOpenByTarget()[activeTarget() ?? "codex"]
  const setSidebarOpen = (next: boolean | ((current: boolean) => boolean), target = activeTarget()) => {
    if (!target) return
    setSidebarOpenByTarget((current) => ({
      ...current,
      [target]: typeof next === "function" ? next(current[target]) : next,
    }))
  }
  const hasDirtyEditor = () => Object.values(editors()).some((current) => current?.dirty)
  const bumpEditorGeneration = (target = activeTarget()) => target === undefined ? -1 : ++editorGenerations[target]
  const appendActivity = (activity: ActivityDraft, target = activeTarget()) => {
    if (!target) return
    if (disposed || exiting) return
    setActivitiesByTarget((current) => ({
      ...current,
      [target]: [...current[target], { id: nextActivityId++, ...activity }].slice(-50),
    }))
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
  const session = (target = activeTarget()) => target ? props.sessions()[target] : undefined
  const view = () => {
    const target = activeTarget()
    return target ? views()[target] : undefined
  }
  const installView = (next: TargetViewProjection, source: "action" | "subscription") => {
    const last = lastViewSequence[next.target] ?? -1
    if (disposed || exiting || next.viewSequence < last) return
    const inspection = reachabilityByTarget()[next.target]
    if (inspection) {
      const nextProvider = next.providers.find((provider) => provider.id === inspection.providerId)
      if (!nextProvider || nextProvider.providerRevision !== inspection.providerRevision) {
        reachabilityGenerations[next.target]++
        reachabilityAborts[next.target]?.abort()
        setReachability(undefined, next.target)
      }
    }
    const increased = next.viewSequence > last
    if (increased) lastViewSequence[next.target] = next.viewSequence
    setViews((current) => ({ ...current, [next.target]: next }))
    if (source === "subscription" && increased && activeTarget() === next.target) {
      appendActivity({ kind: "info", messageKey: "activity.state.updated" }, next.target)
    }
  }

  const updateReconciliation = (
    target: Target,
    token: OverlayToken,
    update: (current: ReconciliationWorkflowState) => ReconciliationWorkflowState,
  ) => {
    setReconciliationByTarget((states) => {
      const current = states[target]
      if (!current || current.overlayToken !== token) return states
      return { ...states, [target]: update(current) }
    })
  }

  const closeReconciliation = (target: Target, token: OverlayToken) => {
    const current = reconciliationByTarget()[target]
    if (!current || current.overlayToken !== token || current.pending === "apply") return
    reconciliationAborts[target]?.abort()
    overlay.close(token)
  }

  const previewReconciliation = async (
    target: Target,
    token: OverlayToken,
    strategy: ReconciliationStrategy,
  ) => {
    const current = reconciliationByTarget()[target]
    if (!current || current.overlayToken !== token || current.pending) return
    reconciliationAborts[target]?.abort()
    const controller = new AbortController()
    reconciliationAborts[target] = controller
    const generation = ++reconciliationGenerations[target]
    const originSession = current.originSession
    updateReconciliation(target, token, (state) => ({
      ...state,
      strategy,
      generation,
      preview: undefined,
      pending: "preview",
      errorCode: undefined,
      acknowledgedVersion: undefined,
    }))
    try {
      const preview = await originSession.previewReconciliation(strategy, controller.signal)
      const latest = reconciliationByTarget()[target]
      if (
        disposed
        || exiting
        || controller.signal.aborted
        || !latest
        || latest.overlayToken !== token
        || latest.originSession !== originSession
        || latest.generation !== generation
      ) return
      updateReconciliation(target, token, (state) => ({ ...state, preview, pending: undefined }))
    } catch (error) {
      const latest = reconciliationByTarget()[target]
      if (controller.signal.aborted || !latest || latest.overlayToken !== token || latest.generation !== generation) return
      updateReconciliation(target, token, (state) => ({
        ...state,
        pending: undefined,
        errorCode: safeReconciliationProblem(error),
      }))
    } finally {
      if (reconciliationAborts[target] === controller) delete reconciliationAborts[target]
    }
  }

  const probeCompatibility = async (
    target: Target,
    token: OverlayToken,
  ) => {
    const current = reconciliationByTarget()[target]
    if (!current || current.overlayToken !== token || current.pending) return
    reconciliationAborts[target]?.abort()
    const controller = new AbortController()
    reconciliationAborts[target] = controller
    const generation = ++reconciliationGenerations[target]
    const originSession = current.originSession
    updateReconciliation(target, token, (state) => ({
      ...state,
      generation,
      compatibilityProbe: undefined,
      pending: "preview",
      errorCode: undefined,
    }))
    try {
      const probe = await originSession.probeCompatibility(controller.signal)
      const latest = reconciliationByTarget()[target]
      if (
        disposed
        || exiting
        || controller.signal.aborted
        || !latest
        || latest.overlayToken !== token
        || latest.originSession !== originSession
        || latest.generation !== generation
      ) return
      updateReconciliation(target, token, (state) => ({
        ...state,
        compatibilityProbe: probe,
        pending: undefined,
      }))
    } catch (error) {
      const latest = reconciliationByTarget()[target]
      if (controller.signal.aborted || !latest || latest.overlayToken !== token || latest.generation !== generation) return
      updateReconciliation(target, token, (state) => ({
        ...state,
        pending: undefined,
        errorCode: safeReconciliationProblem(error),
      }))
    } finally {
      if (reconciliationAborts[target] === controller) delete reconciliationAborts[target]
    }
  }

  const resolveReconciliationCompatibility = async (target: Target, token: OverlayToken, version: string) => {
    const current = reconciliationByTarget()[target]
    if (!current || current.overlayToken !== token || current.pending) return
    if (!current.compatibilityOnly) {
      updateReconciliation(target, token, (state) => state.preview?.compatibility.version === version
        ? { ...state, acknowledgedVersion: version, errorCode: undefined }
        : state)
      return
    }
    const classification = current.compatibilityProbe?.compatibility.classification
    if (
      (classification !== "unknown-compatible" && classification !== "tested")
      || current.compatibilityProbe?.compatibility.version !== version
      || (classification === "unknown-compatible"
        && !current.compatibilityProbe.compatibility.acknowledgementRequired)
    ) return
    const originSession = current.originSession
    const generation = current.generation
    updateReconciliation(target, token, (state) => ({ ...state, pending: "apply", errorCode: undefined }))
    try {
      const outcome = await originSession.resolveCompatibility({
        version,
        managementRevision: current.compatibilityProbe.managementRevision,
      })
      const latest = reconciliationByTarget()[target]
      if (
        disposed
        || exiting
        || !latest
        || latest.overlayToken !== token
        || latest.originSession !== originSession
        || latest.generation !== generation
      ) return
      installView(outcome.view, "action")
      if (outcome.status === "applied") {
        const activity: ActivityDraft = {
          kind: "success",
          messageKey: classification === "unknown-compatible"
            ? "activity.compatibility.acknowledged"
            : "activity.compatibility.resolved",
          values: { version },
        }
        appendActivity(activity, target)
        if (activeTarget() === target) {
          setNotice({ kind: "success", text: props.t(activity.messageKey, activity.values) }, target)
        }
      }
      overlay.close(token)
    } catch (error) {
      if (disposed || exiting) return
      installView(originSession.get() as TargetViewProjection, "action")
      const latest = reconciliationByTarget()[target]
      if (!latest || latest.overlayToken !== token || latest.generation !== generation) return
      const code = safeReconciliationProblem(error)
      updateReconciliation(target, token, (state) => ({
        ...state,
        pending: undefined,
        errorCode: code,
        ...(code === "stale-revision" ? { compatibilityProbe: undefined } : {}),
      }))
    }
  }

  const applyReconciliation = async (target: Target, token: OverlayToken) => {
    const current = reconciliationByTarget()[target]
    const preview = current?.preview
    if (!current || current.overlayToken !== token || current.pending || !preview) return
    if (preview.compatibility.classification === "incompatible") {
      updateReconciliation(target, token, (state) => ({ ...state, errorCode: "incompatible-target-cli" }))
      return
    }
    if (preview.shadowSources.length > 0) {
      updateReconciliation(target, token, (state) => ({ ...state, errorCode: "shadowing-configuration" }))
      return
    }
    if (
      preview.compatibility.acknowledgementRequired
      && current.acknowledgedVersion !== preview.compatibility.version
    ) {
      updateReconciliation(target, token, (state) => ({
        ...state,
        errorCode: "compatibility-acknowledgement-required",
      }))
      return
    }
    const originSession = current.originSession
    const generation = current.generation
    updateReconciliation(target, token, (state) => ({ ...state, pending: "apply", errorCode: undefined }))
    try {
      const outcome = await originSession.applyReconciliation({
        strategy: preview.strategy,
        observationToken: preview.observationToken,
        ...(preview.compatibility.acknowledgementRequired
          ? { acknowledgeVersion: preview.compatibility.version }
          : {}),
      })
      const latest = reconciliationByTarget()[target]
      if (
        disposed
        || exiting
        || !latest
        || latest.overlayToken !== token
        || latest.originSession !== originSession
        || latest.generation !== generation
      ) return
      installView(outcome.view, "action")
      const activity: ActivityDraft = {
        kind: "success",
        messageKey: "activity.reconciliation.applied",
        values: { strategy: props.t(`reconciliation.strategy.short.${preview.strategy}`) },
      }
      appendActivity(activity, target)
      if (activeTarget() === target) setNotice({ kind: "success", text: props.t(activity.messageKey, activity.values) }, target)
      overlay.close(token)
    } catch (error) {
      if (disposed || exiting) return
      installView(originSession.get() as TargetViewProjection, "action")
      const latest = reconciliationByTarget()[target]
      if (!latest || latest.overlayToken !== token || latest.generation !== generation) return
      const code = safeReconciliationProblem(error)
      updateReconciliation(target, token, (state) => ({
        ...state,
        pending: undefined,
        errorCode: code,
        ...(code === "stale-reconciliation-preview"
          ? { preview: undefined, acknowledgedVersion: undefined }
          : {}),
      }))
    }
  }

  const openReconciliation = () => {
    const target = activeTarget()
    const originSession = session(target)
    if (!target || !originSession) return
    const blocked = managedWriteProblem(views()[target])
    if (
      !blocked
      || reconciliationScheduled[target]
      || overlay.depth > 0
    ) return
    reconciliationScheduled[target] = true
    queueMicrotask(() => {
      reconciliationScheduled[target] = false
      if (disposed || exiting || activeTarget() !== target || overlay.depth > 0) return
      const token = Symbol(`reconciliation-${target}`)
      const generation = ++reconciliationGenerations[target]
      const compatibilityOnly = blocked === "compatibility-acknowledgement-required"
        || blocked === "incompatible-target-cli"
      const initial: ReconciliationWorkflowState = {
        target,
        originSession,
        overlayToken: token,
        generation,
        compatibilityOnly,
      }
      setReconciliationByTarget((states) => ({ ...states, [target]: initial }))
      overlay.replace({
        id: "target-reconciliation",
        token,
        dismissOnEscape: () => reconciliationByTarget()[target]?.pending !== "apply",
        render: () => <Reconciliation
          state={() => reconciliationByTarget()[target] ?? initial}
          t={props.t}
          onPreview={(strategy) => { void previewReconciliation(target, token, strategy) }}
          onProbe={() => { void probeCompatibility(target, token) }}
          onResolve={(version) => { void resolveReconciliationCompatibility(target, token, version) }}
          onApply={() => { void applyReconciliation(target, token) }}
          onCancel={() => closeReconciliation(target, token)}
        />,
        onClose: () => {
          reconciliationAborts[target]?.abort()
          delete reconciliationAborts[target]
          reconciliationGenerations[target]++
          setReconciliationByTarget((states) => states[target]?.overlayToken === token
            ? { ...states, [target]: undefined }
            : states)
        },
      })
      if (compatibilityOnly) void probeCompatibility(target, token)
    })
  }

  onMount(() => {
    const unsubscribes = Object.values(props.sessions()).map((targetSession) =>
      targetSession.subscribe((next) => installView(next, "subscription")))
    const catalog = props.universalSession()
    if (catalog) unsubscribes.push(catalog.subscribe((next) => setUniversalView(next)))
    onCleanup(() => { for (const unsubscribe of unsubscribes) unsubscribe() })
  })

  const isRoute = (target: "codex" | "claude") => {
    const current = route()
    return current.kind === "target" && current.target === target
  }
  const showHome = () => {
    setRoute({ kind: "home" })
  }
  const showTarget = (target: "codex" | "claude") => {
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
    const mountedDirtyForm = editor()?.dirty ? providerFormRefs[activeTarget()!] : undefined
    overlay.closeTop()
    if (mountedDirtyForm) queueMicrotask(() => mountedDirtyForm.focus())
  }
  const confirmExit = () => {
    if (exiting) return
    exiting = true
    for (const ref of Object.values(providerFormRefs)) ref?.clearSensitive()
    for (const target of ["codex", "claude"] as const) {
      bumpEditorGeneration(target)
      setEditor(undefined, target)
    }
    overlay.clear()
    destroyRenderer()
  }
  const requestExit = () => {
    if (exitScheduled || exiting) return
    if (!hasDirtyEditor()) {
      destroyRenderer()
      return
    }
    exitScheduled = true
    queueMicrotask(() => {
      exitScheduled = false
      if (disposed || exiting || renderer.isDestroyed || !hasDirtyEditor()) return
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
    if (activeTarget()) appendActivity(activity)
  }
  const saveProvider = async (action: ProviderFormResult) => {
    if (saving()) return false
    const targetSession = session()
    const target = activeTarget()
    if (!targetSession || !target) return false
    const blocked = managedWriteProblem(views()[target])
    if (blocked) {
      const activity = actionProblem({ code: blocked })
      appendActivity(activity, target)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) }, target)
      return false
    }
    const generation = editorGenerations[target]
    const providerName = action.name
    setTargetSaving(target, true)
    setNotice()
    try {
      const outcome = await targetSession.act(action)
      if (disposed || exiting) return false
      installView(outcome.view, "action")
      if (generation !== editorGenerations[target] || activeTarget() !== target) return false
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
      if (generation !== editorGenerations[target] || activeTarget() !== target) return false
      const code = typeof error === "object" && error !== null && "code" in error
        ? String(error.code)
        : "internal-failure"
      if (action.kind === "update-provider" && code === "stale-provider-revision") {
        const latest = authoritative.providers.find((provider) => provider.id === action.providerId)
        if (latest) {
          setEditor((current) => current && current.mode === "edit" && current.draft.providerId === latest.id
            ? { ...current, draft: { ...current.draft, providerRevision: latest.providerRevision } }
            : current, target)
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
    bumpEditorGeneration()
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
    const target = activeTarget()
    if (!target || !session(target) || saving() || applying() || providerSourcePickerScheduled[target]) return
    providerSourcePickerScheduled[target] = true
    queueMicrotask(() => {
      providerSourcePickerScheduled[target] = false
      if (disposed || activeTarget() !== target || saving() || applying()) return
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
    const target = activeTarget()
    if (!target || saving() || providerMutationPending() || applying() || providerPickerScheduled[target]) return
    providerPickerScheduled[target] = true
    queueMicrotask(() => {
      providerPickerScheduled[target] = false
      if (disposed || activeTarget() !== target || saving() || providerMutationPending() || applying()) return
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
          target={target}
          providers={() => view()?.providers ?? []}
          selectedId={selectedProviderId}
          t={props.t}
          pending={() => providerMutationPending() || applying() !== undefined}
          activationMode={applying}
          allowDirect={() => managedWriteProblem(views()[target]) === undefined}
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
          reachabilityGenerations[target]++
          reachabilityAborts[target]?.abort()
          reachabilityAborts[target] = undefined
          setReachability(undefined, target)
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
    reachabilityAborts[target]?.abort()
    const controller = new AbortController()
    reachabilityAborts[target] = controller
    const generation = ++reachabilityGenerations[target]
    setReachability({ providerId: provider.id, providerRevision: provider.providerRevision, pending: true }, target)
    try {
      const result = await targetSession.checkReachability(provider.id, provider.providerRevision, controller.signal)
      if (disposed || controller.signal.aborted || generation !== reachabilityGenerations[target] || activeTarget() !== target) return
      if (result.status === "unreachable" && result.failure.category === "cancelled") {
        setReachability(undefined, target)
        return
      }
      setReachability({ providerId: provider.id, providerRevision: provider.providerRevision, pending: false, result }, target)
    } catch (error) {
      if (disposed || controller.signal.aborted || generation !== reachabilityGenerations[target]) return
      const category = safeInspectionCategory(error)
      if (category === "cancelled") {
        setReachability(undefined, target)
        return
      }
      setReachability({
        providerId: provider.id,
        providerRevision: provider.providerRevision,
        pending: false,
        errorCategory: category,
      }, target)
    }
  }

  const runProviderMutation = async (action: Extract<TargetAction, { kind: "reorder-providers" | "delete-provider" }>) => {
    if (providerMutationPending()) return false
    const targetSession = session()
    const target = activeTarget()
    if (!targetSession || !target) return false
    const blocked = managedWriteProblem(views()[target])
    if (blocked) {
      const activity = actionProblem({ code: blocked })
      appendActivity(activity, target)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) }, target)
      return false
    }
    setTargetMutationPending(target, true)
    setNotice()
    try {
      const outcome = await targetSession.act(action)
      if (disposed || exiting) return false
      installView(outcome.view, "action")
      if (activeTarget() !== target) return false
      if (action.kind === "delete-provider" && selectedProviderId() === action.providerId) {
        setSelectedProviderId(outcome.view.providers[0]?.id, target)
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
    if (provider.generated) {
      setNotice({ kind: "error", text: props.t("generated-provider.delete.blocked") })
      return
    }
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
    if (!targetSession || !target) return
    const blocked = managedWriteProblem(views()[target])
    if (blocked) {
      const activity = actionProblem({ code: blocked })
      appendActivity(activity, target)
      setNotice({ kind: "error", text: props.t(activity.messageKey, activity.values) }, target)
      return
    }
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

  const selectedUniversalProvider = () => universalView()?.providers.find((provider) =>
    provider.id === selectedUniversalProviderId()) ?? universalView()?.providers[0]

  const runUniversalAction = async (
    originSession: UniversalProviderSession,
    action: UniversalProviderAction,
  ): Promise<boolean> => {
    if (universalPending()) return false
    setUniversalPending(true)
    setUniversalNotice()
    setUniversalNoticeKind()
    try {
      const outcome = await originSession.act(action)
      if (disposed || exiting || props.universalSession() !== originSession) return false
      setUniversalView(outcome.view)
      const selected = selectedUniversalProviderId()
      if (selected && !outcome.view.providers.some((provider) => provider.id === selected)) {
        setSelectedUniversalProviderId(outcome.view.providers[0]?.id)
      }
      setUniversalNotice(props.t("universal-provider.applied"))
      setUniversalNoticeKind("success")
      return true
    } catch (error) {
      if (disposed || exiting || props.universalSession() !== originSession) return false
      setUniversalView(originSession.get() as UniversalProviderCatalogView)
      setUniversalNotice(props.t("universal-provider.error", { code: safeUniversalProblem(error) }))
      setUniversalNoticeKind("error")
      return false
    } finally {
      if (!disposed && !exiting) setUniversalPending(false)
    }
  }

  const openUniversalEditor = (
    originSession: UniversalProviderSession,
    mode: "create" | "edit" | "duplicate",
    provider = selectedUniversalProvider(),
    preset?: UniversalProviderCatalogView["presets"][number],
  ) => {
    if (universalPending() || (mode !== "create" && !provider)) return
    setUniversalNotice()
    setUniversalNoticeKind()
    overlay.push({
      id: "universal-provider-editor",
      dismissOnEscape: false,
      render: () => <UniversalProviderEditor
        mode={mode}
        provider={provider}
        preset={preset}
        pending={universalPending()}
        notice={universalNotice()}
        t={props.t}
        onCancel={() => overlay.closeTop()}
        onSave={(action) => runUniversalAction(originSession, action)}
      />,
    })
  }

  const openUniversalSourcePicker = (originSession: UniversalProviderSession) => {
    if (universalPending()) return
    setUniversalNotice()
    setUniversalNoticeKind()
    overlay.push({
      id: "universal-provider-source-picker",
      render: () => <UniversalProviderSourcePicker
        presets={universalView()?.presets ?? []}
        t={props.t}
        onSelect={(preset) => {
          overlay.closeTop()
          queueMicrotask(() => {
            if (!disposed && !universalPending()) openUniversalEditor(originSession, "create", undefined, preset)
          })
        }}
      />,
    })
  }

  const confirmUniversalAction = (
    originSession: UniversalProviderSession,
    kind: "delete" | "synchronize",
  ) => {
    const provider = selectedUniversalProvider()
    if (!provider || universalPending()) return
    setUniversalNotice()
    setUniversalNoticeKind()
    const removals = provider.targets.filter((target) => !target.enabled && target.generatedProviderId !== null)
    const blockers = provider.targets.flatMap((target) => target.activeReferences.map((reference) =>
      `${props.t(`target.${target.target}`)}: ${reference}`))
    overlay.push({
      id: `universal-provider-${kind}-confirmation`,
      dismissOnEscape: false,
      render: () => <UniversalProviderConfirmation
        title={props.t(`universal-provider.${kind}.title`)}
        message={props.t(`universal-provider.${kind}.message`, {
          name: provider.name,
          targets: removals.length
            ? removals.map((target) => props.t(`target.${target.target}`)).join(", ")
            : props.t("universal-provider.none"),
          blockers: blockers.length ? blockers.join(", ") : props.t("universal-provider.none"),
        })}
        pending={universalPending()}
        notice={universalNotice()}
        t={props.t}
        onCancel={() => overlay.closeTop()}
        onConfirm={() => { void (async () => {
          const applied = await runUniversalAction(originSession, kind === "delete"
            ? {
              kind: "delete-universal-provider",
              providerId: provider.id,
              providerRevision: provider.providerRevision,
            }
            : {
              kind: "synchronize-universal-provider",
              providerId: provider.id,
              providerRevision: provider.providerRevision,
            })
          if (applied) overlay.closeTop()
        })() }}
      />,
    })
  }

  const openUniversalProviders = () => {
    const originSession = props.universalSession()
    if (!originSession || overlay.depth > 0 || universalPending()) return
    setUniversalView(originSession.get() as UniversalProviderCatalogView)
    setUniversalNotice()
    setUniversalNoticeKind()
    const catalog = originSession.get()
    const preferred = catalog.providers.find((provider) => provider.id === selectedUniversalProviderId())
      ?? catalog.providers[0]
    setSelectedUniversalProviderId(preferred?.id)
    overlay.replace({
      id: "universal-provider-picker",
      dismissOnEscape: () => !universalPending(),
      render: () => <UniversalProviderPicker
        providers={() => universalView()?.providers ?? []}
        selectedId={selectedUniversalProviderId}
        pending={universalPending}
        notice={universalNotice}
        noticeKind={universalNoticeKind}
        t={props.t}
        onSelectedIdChange={setSelectedUniversalProviderId}
        onCreate={() => openUniversalSourcePicker(originSession)}
        onEdit={() => openUniversalEditor(originSession, "edit")}
        onDuplicate={() => openUniversalEditor(originSession, "duplicate")}
        onDelete={() => confirmUniversalAction(originSession, "delete")}
        onSynchronize={() => confirmUniversalAction(originSession, "synchronize")}
      />,
    })
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
  for (const target of ["codex", "claude"] as const) {
    useCommandLayer({
      scope: target,
      priority: 50,
      enabled: () => overlay.depth === 0 && isRoute(target) && !!editor(),
      handlers: { "target.home": showHome },
    })
  }
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
      "universal-provider.list": openUniversalProviders,
      "target.direct.apply": () => applyDefaultProvider("direct"),
      "target.takeover.apply": () => applyDefaultProvider("takeover"),
      "target.reconciliation.open": openReconciliation,
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
      "universal-provider.list": openUniversalProviders,
      "target.direct.apply": () => applyDefaultProvider("direct"),
      "target.takeover.apply": () => applyDefaultProvider("takeover"),
      "target.reconciliation.open": openReconciliation,
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
            notice={props.t(messageKeyForProblem(props.unavailable()[activeTarget() ?? "claude"] ?? "service-unavailable"))}
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
                  <Show when={editor()?.mode === "edit" && view()?.providers.find((provider) => provider.id === editor()?.draft.providerId)?.generated}
                    fallback={<ProviderForm
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
                    ref={providerFormRefCallbacks[activeTarget()!]}
                    pending={saving()}
                    initialDirty={editor()?.dirty}
                    onDirtyChange={(dirty) => setEditor((current) => current ? { ...current, dirty } : current)}
                    onDraftChange={(draft) => setEditor((current) => current
                      ? { ...current, draft: { ...current.draft, ...draft } }
                      : current)}
                    onCancel={() => {
                      bumpEditorGeneration()
                      setEditor()
                    }}
                    onSave={saveProvider}
                  />}>
                    <GeneratedProviderForm
                      provider={view()!.providers.find((provider) => provider.id === editor()!.draft.providerId)!}
                      providerRevision={editor()!.draft.providerRevision!}
                      target={activeTarget()!}
                      pending={saving()}
                      t={props.t}
                      onDirtyChange={(dirty) => setEditor((current) => current ? { ...current, dirty } : current)}
                      onCancel={() => {
                        bumpEditorGeneration()
                        setEditor()
                      }}
                      onSave={saveProvider}
                    />
                  </Show>
                </box>
              </scrollbox>
            </Show>
            <Show when={showSidebar()}>
              <TargetSidebar view={view()!} t={props.t} width={sidebarWidth()} />
            </Show>
          </box>
          <Show when={!editor()}>
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
  const sessionSource = props.sessions
  const unavailableSource = props.unavailable
  const universalSessionSource = props.universalSession
  const sessions: Accessor<Partial<Record<Target, TargetSession>>> = typeof sessionSource === "function"
    ? sessionSource
    : () => sessionSource ?? (props.session ? { [props.session.get().target]: props.session } : {})
  const unavailable: Accessor<Partial<Record<Target, string>>> = typeof unavailableSource === "function"
    ? unavailableSource
    : () => unavailableSource ?? {}
  const universalSession: Accessor<UniversalProviderSession | undefined> = typeof universalSessionSource === "function"
    ? universalSessionSource
    : () => universalSessionSource
  return (
    <MuxviaKeymapProvider presenter={createCommandPresenter(t)}>
      <OverlayProvider>
        <Shell sessions={sessions} unavailable={unavailable} universalSession={universalSession} t={t} />
      </OverlayProvider>
    </MuxviaKeymapProvider>
  )
}
