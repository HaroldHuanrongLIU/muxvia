import { expect, test } from "bun:test"

import { commandCatalog } from "../src/commands/catalog"
import { en } from "../src/i18n/en"
import { zhCN } from "../src/i18n/zh-cn"
import {
  createCommandPresenter,
  createTranslator,
  labelTargetState,
  messageKeyForProblem,
  resolveLocale,
} from "../src/i18n"

test("catalogs have identical keys", () => {
  expect(Object.keys(zhCN).sort()).toEqual(Object.keys(en).sort())
})

test("catalogs include command titles and descriptions", () => {
  for (const command of commandCatalog) {
    expect(Object.hasOwn(en, command.titleKey)).toBe(true)
    expect(Object.hasOwn(en, command.descriptionKey)).toBe(true)
    expect(Object.hasOwn(zhCN, command.titleKey)).toBe(true)
    expect(Object.hasOwn(zhCN, command.descriptionKey)).toBe(true)
  }
})

test("locale precedence and Chinese variants are deterministic", () => {
  expect(resolveLocale({ LANG: "zh_CN.UTF-8" })).toBe("zh-CN")
  expect(resolveLocale({ LANG: "en_US.UTF-8", LC_MESSAGES: "zh-CN" })).toBe("zh-CN")
  expect(resolveLocale({ LANG: "zh_CN", LC_ALL: "C" })).toBe("en")
  expect(resolveLocale({ LC_ALL: "", LC_MESSAGES: "", LANG: " zh-TW.UTF-8 " })).toBe("zh-CN")
  expect(resolveLocale({})).toBe("en")
})

test("stable backend codes map to localized copy without backend messages", () => {
  const t = createTranslator("zh-CN")

  expect(t(messageKeyForProblem("stale-revision"))).toContain("状态")
  expect(t(messageKeyForProblem("takeover-required"))).toBe("此 Provider 需要 Target Takeover。")
  expect(t(messageKeyForProblem("takeover-active"))).toBe("使用直接激活前，请先停用 Target Takeover。")
  expect(t(messageKeyForProblem("provider-mode-active"), {
    selector: "CLAUDE_CODE_USE_VERTEX",
    source: "control-plane-context",
  })).toContain("CLAUDE_CODE_USE_VERTEX")
  expect(t(messageKeyForProblem("shadowing-configuration"), {
    source: "shared-project-settings",
  })).toContain("shared-project-settings")
  expect(t(messageKeyForProblem("configuration-drift"))).toContain("协调")
  expect(messageKeyForProblem("unrecognized-code")).toBe("error.generic")
})

test("Direct Activation and Takeover confirmation copy is complete in both locales", () => {
  const en = createTranslator("en")
  const zhCN = createTranslator("zh-CN")

  expect([
    en("command.direct.apply"),
    en("activity.direct.applied", { name: "Provider" }),
    en("takeover-required.title"),
    en("takeover-required.confirm"),
    en("takeover-required.cancel"),
  ]).toEqual([
    "Apply Direct Activation",
    "Direct Activation applied: Provider",
    "Enable Target Takeover?",
    "Enable Takeover",
    "Cancel",
  ])
  expect([
    zhCN("command.direct.apply"),
    zhCN("activity.direct.applied", { name: "Provider" }),
    zhCN("takeover-required.title"),
    zhCN("takeover-required.confirm"),
    zhCN("takeover-required.cancel"),
  ]).toEqual([
    "应用直接激活",
    "已直接激活：Provider",
    "启用 Target Takeover？",
    "启用 Takeover",
    "取消",
  ])
})

test("interpolation preserves operator values verbatim", () => {
  const operator = "模型-$A {unsafe} <b>markup</b>"

  expect(createTranslator("en")("activity.provider.saved", { name: operator })).toContain(operator)
  expect(createTranslator("zh-CN")("activity.provider.saved", { name: operator })).toContain(operator)
})

test("unknown placeholders retain stable text", () => {
  const t = createTranslator("en")

  expect(t("state.unknown", { other: "ignored" })).toBe("Unknown ({value})")
})

test("target states have localized known and unknown labels", () => {
  const en = createTranslator("en")
  const zhCN = createTranslator("zh-CN")

  expect(labelTargetState(en, "takeover")).toBe("Takeover")
  expect(labelTargetState(en, "managed")).toBe("Managed")
  expect(labelTargetState(zhCN, "managed")).toBe("受管理")
  expect(labelTargetState(en, "custom-state")).toBe("Unknown (custom-state)")
})

test("status labels use the canonical domain concepts in both locales", () => {
  const en = createTranslator("en")
  const zhCN = createTranslator("zh-CN")

  expect([
    en("status.current"),
    en("status.serving"),
    en("status.service"),
    en("status.config"),
    en("status.snapshot"),
  ]).toEqual([
    "Current Target Provider",
    "Serving Provider",
    "Routing Service",
    "Managed Configuration",
    "Activated Snapshot",
  ])
  expect([
    zhCN("status.current"),
    zhCN("status.serving"),
    zhCN("status.service"),
    zhCN("status.config"),
    zhCN("status.snapshot"),
  ]).toEqual([
    "当前 Target Provider",
    "服务中 Provider",
    "路由服务",
    "受管理配置",
    "已激活快照",
  ])
})

test("provenance labels distinguish presets from Universal Providers in both locales", () => {
  const en = createTranslator("en")
  const zhCN = createTranslator("zh-CN")

  expect([
    en("provider.provenance.preset"),
    en("provider.provenance.universal-provider"),
    en("provider.provenance.other"),
  ]).toEqual(["Preset", "Universal Provider", "Other provenance"])
  expect([
    zhCN("provider.provenance.preset"),
    zhCN("provider.provenance.universal-provider"),
    zhCN("provider.provenance.other"),
  ]).toEqual(["预设", "通用 Provider", "其他来源"])
})

test("command presenter translates a command title and description", () => {
  const present = createCommandPresenter(createTranslator("zh-CN"))

  expect(present({ titleKey: "command.target.codex", descriptionKey: "command.target.codex.description" })).toEqual({
    title: "打开 Codex CLI",
    description: "切换到 Codex CLI 控制台",
  })
})

test("command presenter retains nonexistent broad command keys", () => {
  const present = createCommandPresenter(createTranslator("en"))

  expect(present({ titleKey: "command.not-in-catalog", descriptionKey: "command.not-in-catalog.description" })).toEqual({
    title: "command.not-in-catalog",
    description: "command.not-in-catalog.description",
  })
})
