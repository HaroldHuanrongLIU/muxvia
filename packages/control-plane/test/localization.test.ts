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
  expect(messageKeyForProblem("unrecognized-code")).toBe("error.generic")
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
  const t = createTranslator("en")

  expect(labelTargetState(t, "takeover")).toBe("Takeover")
  expect(labelTargetState(t, "custom-state")).toBe("Unknown (custom-state)")
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
