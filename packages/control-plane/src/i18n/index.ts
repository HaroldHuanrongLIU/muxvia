import type { CommandTextKey } from "../commands/types"
import { en, type MessageKey } from "./en"
import { zhCN } from "./zh-cn"

export { en, zhCN }
export type { MessageKey }

export type Locale = "en" | "zh-CN"
export type Translator = (key: MessageKey, values?: Record<string, string | number>) => string

export function resolveLocale(env: Readonly<Record<string, string | undefined>>): Locale {
  for (const value of [env.LC_ALL, env.LC_MESSAGES, env.LANG]) {
    const normalized = value?.trim().toLowerCase()
    if (!normalized) continue
    return /^zh(?:[-_]|$)/.test(normalized) ? "zh-CN" : "en"
  }
  return "en"
}

export function isMessageKey(value: string): value is MessageKey {
  return Object.hasOwn(en, value)
}

export function createTranslator(locale: Locale): Translator {
  const messages: Readonly<Record<MessageKey, string>> = locale === "zh-CN" ? zhCN : en
  return (key, values) => {
    const message = messages[key]
    return message.replace(/\{([^{}]+)\}/g, (placeholder, name) => (
      values && Object.hasOwn(values, name) ? String(values[name]) : placeholder
    ))
  }
}

const problemMessageKeys = new Map<string, MessageKey>([
  ["stale-revision", "error.stale-revision"],
  ["invalid-provider", "error.invalid-provider"],
  ["incomplete-provider", "error.incomplete-provider"],
  ["recovery-required", "error.recovery-required"],
  ["service-unavailable", "error.service-unavailable"],
  ["incompatible-target-cli", "error.incompatible-target-cli"],
  ["untested-target-cli", "error.untested-target-cli"],
  ["unsupported-configuration-home", "error.unsupported-configuration-home"],
  ["configuration-collision", "error.configuration-collision"],
  ["configuration-write-failed", "error.configuration-write-failed"],
  ["internal-failure", "error.internal-failure"],
])

export function messageKeyForProblem(code: string): MessageKey {
  return problemMessageKeys.get(code) ?? "error.generic"
}

export function labelTargetState(t: Translator, value: string): string {
  switch (value) {
    case "unmanaged": return t("state.unmanaged")
    case "managed": return t("state.managed")
    case "takeover": return t("state.takeover")
    case "direct": return t("state.direct")
    case "running": return t("state.running")
    case "ready": return t("state.ready")
    case "inactive": return t("state.inactive")
    case "active": return t("state.active")
    case "applied": return t("state.applied")
    case "recovery-required": return t("state.recovery-required")
    default: return t("state.unknown", { value })
  }
}

export function createCommandPresenter(t: Translator) {
  return (textKeys: { titleKey: CommandTextKey; descriptionKey: CommandTextKey }) => ({
    title: isMessageKey(textKeys.titleKey) ? t(textKeys.titleKey) : textKeys.titleKey,
    description: isMessageKey(textKeys.descriptionKey) ? t(textKeys.descriptionKey) : textKeys.descriptionKey,
  })
}
