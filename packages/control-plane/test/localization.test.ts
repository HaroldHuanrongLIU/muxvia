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
    en("command.direct.apply.description"),
    en("command.provider.activate.direct.description"),
    en("activity.direct.applied", { name: "Provider" }),
    en("takeover-required.title"),
    en("takeover-required.confirm"),
    en("takeover-required.cancel"),
  ]).toEqual([
    "Apply Direct Activation",
    "Apply the Current Target Provider directly to Managed Configuration",
    "Apply the selected Provider directly to Managed Configuration",
    "Direct Activation applied: Provider",
    "Enable Target Takeover?",
    "Enable Takeover",
    "Cancel",
  ])
  expect([
    zhCN("command.direct.apply"),
    zhCN("command.direct.apply.description"),
    zhCN("command.provider.activate.direct.description"),
    zhCN("activity.direct.applied", { name: "Provider" }),
    zhCN("takeover-required.title"),
    zhCN("takeover-required.confirm"),
    zhCN("takeover-required.cancel"),
  ]).toEqual([
    "应用直接激活",
    "将当前 Target Provider 直接应用到受管理配置",
    "将选中的 Provider 直接应用到受管理配置",
    "已直接激活：Provider",
    "启用 Target Takeover？",
    "启用 Takeover",
    "取消",
  ])
})

test("Reconciliation compatibility, strategy, field, shadow, stale, busy, acknowledgement, and restart copy is exact in both locales", () => {
  const english = createTranslator("en")
  const chinese = createTranslator("zh-CN")

  expect([
    english("reconciliation.title"),
    english("reconciliation.compatibility.unknown-compatible", { version: "9.9.9" }),
    english("reconciliation.shadow.codex-profile"),
    english("reconciliation.shadow.claude-selector", { selector: "CLAUDE_CODE_USE_VERTEX" }),
    english("reconciliation.boundary"),
    english("reconciliation.field.credential"),
    english("reconciliation.state.changed"),
    english("reconciliation.strategy.adopt"),
    english("reconciliation.strategy.reapply"),
    english("reconciliation.strategy.restore"),
    english("reconciliation.acknowledgement", { version: "9.9.9" }),
    english("error.stale-reconciliation-preview"),
    english("error.target-busy"),
    english("reconciliation.restart.codex"),
    english("reconciliation.restart.claude"),
  ]).toEqual([
    "Reconcile Managed Configuration",
    "Untested but compatible · 9.9.9",
    "Codex profile",
    "Claude environment selector · CLAUDE_CODE_USE_VERTEX",
    "Command-line flags and resumed sessions may still override this configuration.",
    "Credential Reference",
    "Changed",
    "Adopt observed configuration",
    "Reapply committed configuration",
    "Restore pre-Muxvia configuration",
    "I acknowledge untested Target CLI version 9.9.9.",
    "Target state changed. Preview the reconciliation again.",
    "This Target has active model requests. Retry Restore when it is idle.",
    "Restart Codex after applying this reconciliation.",
    "Restart Claude Code after applying this reconciliation.",
  ])
  expect([
    chinese("reconciliation.title"),
    chinese("reconciliation.compatibility.unknown-compatible", { version: "9.9.9" }),
    chinese("reconciliation.shadow.codex-profile"),
    chinese("reconciliation.shadow.claude-selector", { selector: "CLAUDE_CODE_USE_VERTEX" }),
    chinese("reconciliation.boundary"),
    chinese("reconciliation.field.credential"),
    chinese("reconciliation.state.changed"),
    chinese("reconciliation.strategy.adopt"),
    chinese("reconciliation.strategy.reapply"),
    chinese("reconciliation.strategy.restore"),
    chinese("reconciliation.acknowledgement", { version: "9.9.9" }),
    chinese("error.stale-reconciliation-preview"),
    chinese("error.target-busy"),
    chinese("reconciliation.restart.codex"),
    chinese("reconciliation.restart.claude"),
  ]).toEqual([
    "协调受管理配置",
    "未经测试但兼容 · 9.9.9",
    "Codex 配置文件",
    "Claude 环境选择器 · CLAUDE_CODE_USE_VERTEX",
    "命令行标志和恢复的会话仍可能覆盖此配置。",
    "凭据引用",
    "已更改",
    "采用观测到的配置",
    "重新应用已提交配置",
    "恢复 Muxvia 之前的配置",
    "我确认使用未经测试的 Target CLI 版本 9.9.9。",
    "Target 状态已更改。请重新预览协调操作。",
    "此 Target 有活动的模型请求。请在空闲时重试恢复。",
    "应用此协调操作后重启 Codex。",
    "应用此协调操作后重启 Claude Code。",
  ])
})

test("Reconciliation stable problems map to fixed localized diagnostics", () => {
  expect(messageKeyForProblem("compatibility-acknowledgement-required")).toBe("error.compatibility-acknowledgement-required")
  expect(messageKeyForProblem("stale-reconciliation-preview")).toBe("error.stale-reconciliation-preview")
  expect(messageKeyForProblem("target-busy")).toBe("error.target-busy")
})

test("Reconciliation closed compatibility, shadow, field, and state labels have full locale parity", () => {
  const english = createTranslator("en")
  const chinese = createTranslator("zh-CN")
  expect([
    english("reconciliation.compatibility.tested", { version: "1" }),
    english("reconciliation.compatibility.incompatible", { version: "1" }),
    english("reconciliation.shadow.claude-managed"),
    english("reconciliation.shadow.claude-shared"),
    english("reconciliation.shadow.claude-project"),
    english("reconciliation.shadow.claude-local"),
    english("reconciliation.shadow.claude-host-managed"),
    english("reconciliation.field.provider"),
    english("reconciliation.field.current-provider"),
    english("reconciliation.field.activated-snapshot"),
    english("reconciliation.field.takeover"),
    english("reconciliation.state.present"),
    english("reconciliation.state.absent"),
    english("reconciliation.state.unchanged"),
  ]).toEqual([
    "Tested · 1",
    "Incompatible · 1",
    "Claude managed settings",
    "Claude shared settings",
    "Claude project settings",
    "Claude local settings",
    "Claude host-managed settings",
    "Target Provider",
    "Current Target Provider",
    "Activated Snapshot",
    "Target Takeover",
    "Present",
    "Absent",
    "Unchanged",
  ])
  expect([
    chinese("reconciliation.compatibility.tested", { version: "1" }),
    chinese("reconciliation.compatibility.incompatible", { version: "1" }),
    chinese("reconciliation.shadow.claude-managed"),
    chinese("reconciliation.shadow.claude-shared"),
    chinese("reconciliation.shadow.claude-project"),
    chinese("reconciliation.shadow.claude-local"),
    chinese("reconciliation.shadow.claude-host-managed"),
    chinese("reconciliation.field.provider"),
    chinese("reconciliation.field.current-provider"),
    chinese("reconciliation.field.activated-snapshot"),
    chinese("reconciliation.field.takeover"),
    chinese("reconciliation.state.present"),
    chinese("reconciliation.state.absent"),
    chinese("reconciliation.state.unchanged"),
  ]).toEqual([
    "已测试 · 1",
    "不兼容 · 1",
    "Claude 受管理设置",
    "Claude 共享设置",
    "Claude 项目设置",
    "Claude 本地设置",
    "Claude 主机管理设置",
    "Target Provider",
    "当前 Target Provider",
    "已激活快照",
    "Target Takeover",
    "存在",
    "不存在",
    "未更改",
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
