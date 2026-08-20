import type { InputRenderable, KeyEvent } from "@opentui/core"
import { createEffect, createSignal, For, onMount, Show, type Accessor } from "solid-js"

import { useCommandLayer } from "../commands/keymap"
import type {
  DeviceAuthorizationChallenge,
  SubscriptionAccountCatalogView,
  SubscriptionDefaultPreview,
  Target,
} from "../control/types"
import type { Translator } from "../i18n"
import { theme } from "../theme"
import { useOverlay } from "./overlay-stack"

export interface SubscriptionAuthorizationState {
  challenge: DeviceAuthorizationChallenge
  copySucceeded: boolean
  openSucceeded: boolean
}

export interface SubscriptionAccountPickerProps {
  view: Accessor<SubscriptionAccountCatalogView>
  selectedId: Accessor<string | undefined>
  pending: Accessor<boolean>
  notice: Accessor<string | undefined>
  authorization: Accessor<SubscriptionAuthorizationState | undefined>
  defaultPreview: Accessor<SubscriptionDefaultPreview | undefined>
  activeProvider: Accessor<{
    target: Target
    providerId: string
    providerRevision: number
    providerName: string
  } | undefined>
  t: Translator
  onSelectedIdChange: (accountId: string | undefined) => void
  onAuthorize: () => void
  onReauthorize: () => void
  onCancelAuthorization: () => void
  onPreviewDefault: () => void
  onConfirmDefault: () => void
  onBindFixed: () => void
  onBindFollowDefault: () => void
  onDelete: () => void
}

export function SubscriptionAccountPicker(props: SubscriptionAccountPickerProps) {
  const overlay = useOverlay()
  const [selectedId, setSelectedId] = createSignal(props.selectedId() ?? props.view().accounts[0]?.accountId)
  const [keyCapture, setKeyCapture] = createSignal("")
  let input: InputRenderable | undefined
  const selected = () => props.view().accounts.find((account) => account.accountId === selectedId())
    ?? props.view().accounts[0]

  createEffect(() => {
    const accounts = props.view().accounts
    const current = selectedId()
    const next = current && accounts.some((account) => account.accountId === current)
      ? current
      : accounts[0]?.accountId
    if (next !== current) setSelectedId(next)
    if (next !== props.selectedId()) props.onSelectedIdChange(next)
  })
  onMount(() => queueMicrotask(() => {
    if (input && !input.isDestroyed) input.focus()
  }))

  useCommandLayer({
    scope: "subscription-account-picker",
    priority: 350,
    enabled: () => overlay.depth === 1,
    handlers: {
      "subscription-account.authorize": props.onAuthorize,
      "subscription-account.reauthorize": props.onReauthorize,
      "subscription-account.default": props.onPreviewDefault,
      "subscription-account.bind.fixed": props.onBindFixed,
      "subscription-account.bind.follow-default": props.onBindFollowDefault,
      "subscription-account.delete": props.onDelete,
      "subscription-account.confirm": props.onConfirmDefault,
      "subscription-account.cancel": () => {
        if (props.authorization()) props.onCancelAuthorization()
        else overlay.closeTop()
      },
    },
  })

  const move = (delta: -1 | 1) => {
    if (props.pending() || props.authorization() || props.defaultPreview()) return
    const accounts = props.view().accounts
    if (accounts.length < 2) return
    const index = Math.max(0, accounts.findIndex((account) => account.accountId === selected()?.accountId))
    const next = accounts[(index + delta + accounts.length) % accounts.length]!.accountId
    setSelectedId(next)
    props.onSelectedIdChange(next)
  }
  const onKeyDown = (event: KeyEvent) => {
    if (event.name === "up" || event.name === "down") {
      event.preventDefault()
      event.stopPropagation()
      move(event.name === "up" ? -1 : 1)
    }
  }
  const captureNavigation = (value: string) => {
    if (value === "up" || value === "down") {
      setKeyCapture("")
      move(value === "up" ? -1 : 1)
      return
    }
    setKeyCapture(value)
  }

  return <box flexDirection="column" padding={1} rowGap={1} backgroundColor={theme.panel}>
    <text fg={theme.primary}>{props.t("subscription-account.title")}</text>
    <Show when={props.notice()}><text fg={theme.warning}>{props.notice()}</text></Show>
    <Show when={props.authorization()} fallback={<>
      <Show when={props.defaultPreview()} fallback={<>
        <box height={1}>
          <input
            ref={(value: InputRenderable) => { input = value }}
            value={keyCapture()}
            focused
            onInput={captureNavigation}
            onKeyDown={onKeyDown}
            backgroundColor={theme.panel}
            focusedBackgroundColor={theme.panel}
            textColor={theme.text}
            focusedTextColor={theme.text}
            placeholder={props.t("subscription-account.navigate")}
            placeholderColor={theme.muted}
            cursorColor={theme.primary}
            width="100%"
          />
        </box>
        <Show when={props.view().accounts.length === 0}>
          <text fg={theme.muted}>{props.t("subscription-account.empty")}</text>
        </Show>
        <For each={props.view().accounts}>{(account) => {
          const active = () => account.accountId === selected()?.accountId
          return <text fg={active() ? theme.background : theme.text} bg={active() ? theme.primary : theme.panel}>
            {props.t("subscription-account.row", {
              identity: account.email ?? account.accountId,
              state: props.t(`subscription-account.state.${account.state}`),
              default: account.default ? props.t("subscription-account.default.marker") : "",
            })}
          </text>
        }}</For>
        <For each={props.activeProvider() ? [props.activeProvider()!] : []}>{(provider) => <text fg={theme.muted}>
          {props.t("subscription-account.active-provider", {
            target: props.t(`target.${provider.target}`),
            provider: provider.providerName,
          })}
        </text>}</For>
        <For each={props.view().bindings}>{(binding) => <text fg={binding.resolution.state === "available" ? theme.success : theme.warning}>
          {props.t("subscription-account.binding", {
            target: props.t(`target.${binding.target}`),
            provider: binding.providerName,
            kind: props.t(`subscription-account.binding.${binding.binding.kind}`),
            state: props.t(`subscription-account.resolution.${binding.resolution.state}`),
          })}
        </text>}</For>
        <text fg={theme.muted}>{props.t("subscription-account.help")}</text>
      </>}>
        <For each={props.defaultPreview() ? [props.defaultPreview()!] : []}>{(preview) => <box flexDirection="column" rowGap={1}>
          <text fg={theme.warning}>{props.t("subscription-account.default.preview", { count: preview.effects.length })}</text>
          <For each={preview.effects}>{(effect) => <text fg={theme.text}>
            {props.t("subscription-account.default.effect", {
              target: props.t(`target.${effect.target}`),
              provider: effect.providerName,
              state: props.t(`subscription-account.resolution.${effect.nextResolution}`),
            })}
          </text>}</For>
          <text fg={theme.muted}>{props.t("subscription-account.default.confirm")}</text>
        </box>}</For>
      </Show>
    </>}>
      <For each={props.authorization() ? [props.authorization()!] : []}>{(authorization) => <box flexDirection="column" rowGap={1}>
      <text fg={theme.text}>{props.t("subscription-account.device.code", { code: authorization.challenge.userCode })}</text>
      <text fg={theme.muted}>{authorization.challenge.verificationUrl}</text>
      <Show when={!authorization.copySucceeded}>
        <text fg={theme.warning}>{props.t("subscription-account.device.copy-failed")}</text>
      </Show>
      <Show when={!authorization.openSucceeded}>
        <text fg={theme.warning}>{props.t("subscription-account.device.open-failed")}</text>
      </Show>
      <text fg={theme.warning}>{props.t("subscription-account.device.pending")}</text>
      <text fg={theme.muted}>{props.t("subscription-account.device.cancel")}</text>
    </box>}</For></Show>
  </box>
}
