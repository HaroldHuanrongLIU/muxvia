import { useKeyboard, useRenderer } from "@opentui/solid"
import { createSignal, onCleanup, onMount, Show } from "solid-js"

import type { TargetSession } from "../control/target-session"
import type { TargetAction, TargetView as TargetViewProjection } from "../control/types"
import { theme } from "../theme"
import { ProviderForm } from "./provider-form"
import { TargetView } from "./target-view"

export interface AppProps {
  session: TargetSession
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

export function App(props: AppProps) {
  const renderer = useRenderer()
  const [view, setView] = createSignal<TargetViewProjection>(props.session.get())
  const [providerForm, setProviderForm] = createSignal(false)
  const [saving, setSaving] = createSignal(false)
  const [applying, setApplying] = createSignal(false)
  const [notice, setNotice] = createSignal<Notice>()

  onMount(() => {
    const unsubscribe = props.session.subscribe(setView)
    onCleanup(unsubscribe)
  })

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

  useKeyboard((key) => {
    if (key.ctrl && key.name === "c") {
      key.preventDefault()
      key.stopPropagation()
      renderer.destroy()
      return
    }
    if (providerForm()) return
    if (key.name === "q") {
      key.preventDefault()
      key.stopPropagation()
      renderer.destroy()
      return
    }
    if (key.name === "p" && !saving() && !applying()) {
      key.preventDefault()
      key.stopPropagation()
      setNotice()
      setProviderForm(true)
      return
    }
    if (key.name === "a" && !saving() && !applying()) {
      key.preventDefault()
      key.stopPropagation()
      void applyTakeover()
    }
  })

  return (
    <box width="100%" height="100%" backgroundColor={theme.background} flexDirection="column" paddingX={2}>
      <scrollbox flexGrow={1} flexShrink={1} paddingTop={1}>
        <Show
          when={providerForm()}
          fallback={<TargetView view={view()} notice={notice()} />}
        >
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
        </Show>
      </scrollbox>
      <box
        height={3}
        border={["left"]}
        borderColor={theme.primary}
        backgroundColor={theme.panel}
        paddingLeft={1}
        flexDirection="column"
        justifyContent="center"
      >
        <text fg={theme.text}>{applying() ? "Applying Target Takeover…" : "[p] provider   [a] apply takeover   [q] quit"}</text>
      </box>
    </box>
  )
}
