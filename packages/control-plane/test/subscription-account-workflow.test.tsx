import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"

import type {
  SubscriptionAccountSession,
  SubscriptionPlatformEffects,
} from "../src/control/subscription-account-session"
import { ControlError } from "../src/control/rpc-client"
import type { CompatibilityResolution, TargetSession } from "../src/control/target-session"
import type {
  ActionOutcome,
  CompatibilityProbe,
  DeviceAuthorizationPoll,
  OrdinaryTargetAction,
  SubscriptionAccountAction,
  SubscriptionAccountCatalogView,
  SubscriptionAccountOutcome,
  SubscriptionDefaultPreview,
  TargetView,
} from "../src/control/types"
import { App } from "../src/ui/app"
import {
  assertControlledSecretSource,
  waitForSecretFreeFrame,
} from "./secret-audit"

const providerId = "00000000-0000-4000-8000-000000001191"

function targetView(target: "codex" | "claude" = "codex"): TargetView {
  return {
    target,
    managementRevision: 1,
    viewSequence: 1,
    service: { epoch: "00000000-0000-4000-8000-000000001192", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: [{
      id: providerId,
      position: 0,
      providerRevision: 1,
      name: "Account-backed Provider",
      baseUrl: "https://example.test/v1",
      model: "model",
      protocol: "openai-responses",
      authentication: "openai-bearer",
      routingRequirement: "direct-compatible",
      credential: "present",
      completeness: "complete",
      missingFields: [],
      provenance: null,
      generated: false,
      universalProviderId: null,
      synchronization: null,
      ownership: {
        name: "target-provider", baseUrl: "target-provider", model: "target-provider",
        protocol: "target-fixed", authentication: "target-provider",
        routingRequirement: "target-provider", credential: "target-provider",
      },
      routeHealth: { state: "unobserved" },
      activeReferences: [],
    }],
    providerPresets: [],
    currentProviderId: providerId,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    failover: { draftRevision: 1, draftMembers: [], activePlan: null },
    problems: [],
  }
}

function accountCatalog(revision = 1): SubscriptionAccountCatalogView {
  return {
    revision,
    viewSequence: revision,
    defaultAccountId: "account-primary",
    accounts: [{
      accountId: "account-primary",
      email: "operator@example.test",
      authenticatedAt: 1,
      state: "authorized",
      default: true,
    }],
    bindings: [],
    recovery: { state: "clean" },
  }
}

class MemoryTargetSession implements TargetSession {
  readonly actions: OrdinaryTargetAction[] = []
  constructor(readonly view = targetView()) {}
  get(): Readonly<TargetView> { return this.view }
  async act(action: OrdinaryTargetAction): Promise<ActionOutcome> {
    this.actions.push(structuredClone(action))
    return { status: "applied", view: this.view }
  }
  async discoverModels(): Promise<never> { throw new Error("unused") }
  async checkReachability(): Promise<never> { throw new Error("unused") }
  async previewReconciliation(): Promise<never> { throw new Error("unused") }
  async probeCompatibility(): Promise<CompatibilityProbe> { throw new Error("unused") }
  async resolveCompatibility(_input: CompatibilityResolution): Promise<ActionOutcome> { throw new Error("unused") }
  async applyReconciliation(): Promise<never> { throw new Error("unused") }
  async listRequestRecords(): Promise<never> { throw new Error("unused") }
  async listUsageActivity(): Promise<never> { throw new Error("unused") }
  async refreshNativeUsage(): Promise<never> { throw new Error("unused") }
  async setUsageRetention(): Promise<never> { throw new Error("unused") }
  async clearUsage(): Promise<never> { throw new Error("unused") }
  async updatePricingCatalog(): Promise<never> { throw new Error("unused") }
  async inspectRequestRecord(): Promise<never> { throw new Error("unused") }
  subscribe(): () => void { return () => {} }
  async whenClosed(): Promise<void> { return await new Promise(() => {}) }
  async close(): Promise<void> {}
}

function bridgeTargetView(missingBinding = false): TargetView {
  const initial = targetView("claude")
  return {
    ...initial,
    providers: [{
      ...initial.providers[0]!,
      name: "Codex Subscription Bridge",
      baseUrl: "https://chatgpt.com/backend-api/codex",
      model: "gpt-5.6",
      protocol: "anthropic-messages",
      authentication: "codex-subscription",
      routingRequirement: "takeover-required",
      credential: "missing",
      completeness: missingBinding ? "incomplete" : "complete",
      missingFields: missingBinding ? ["subscription-account-binding"] : [],
      provenance: { kind: "preset", key: "codex-subscription-bridge" },
    }],
  }
}

class MemorySubscriptionSession implements SubscriptionAccountSession {
  readonly actions: SubscriptionAccountAction[] = []
  startCalls: Array<string | undefined> = []
  polls: DeviceAuthorizationPoll[] = [{ status: "pending" }, { status: "authorized", accountId: "account-primary" }]
  actionFailure?: unknown
  #view: SubscriptionAccountCatalogView

  constructor(initial = accountCatalog()) { this.#view = initial }

  get(): Readonly<SubscriptionAccountCatalogView> { return this.#view }
  async startDeviceAuthorization(accountId?: string) {
    this.startCalls.push(accountId)
    return {
      flowId: "00000000-0000-4000-8000-000000001193",
      userCode: "ABCD-EFGH",
      verificationUrl: "https://auth.openai.com/codex/device",
      expiresInSeconds: 900,
      pollIntervalSeconds: 11,
    }
  }
  async pollDeviceAuthorization(): Promise<DeviceAuthorizationPoll> {
    return this.polls.shift() ?? { status: "expired" }
  }
  async previewDefault(accountId: string): Promise<SubscriptionDefaultPreview> {
    return {
      previewToken: "00000000-0000-4000-8000-000000001194",
      accountId,
      effects: [{
        target: "codex",
        providerId,
        providerRevision: 1,
        providerName: "Account-backed Provider",
        currentAccountId: null,
        nextAccountId: accountId,
        nextResolution: "available",
      }],
    }
  }
  async act(action: SubscriptionAccountAction): Promise<SubscriptionAccountOutcome> {
    this.actions.push(structuredClone(action))
    if (this.actionFailure) throw this.actionFailure
    this.#view = { ...this.#view, revision: this.#view.revision + 1, viewSequence: this.#view.viewSequence + 1 }
    return { status: "applied", view: this.#view }
  }
  subscribe(): () => void { return () => {} }
  async whenClosed(): Promise<void> { return await new Promise(() => {}) }
  async close(): Promise<void> {}
}

test("either Target opens one account workflow and browser failure does not stop polling", async () => {
  const accounts = new MemorySubscriptionSession()
  const copied: string[] = []
  const opened: string[] = []
  let releaseFirstWait!: () => void
  const firstWait = new Promise<void>((resolve) => { releaseFirstWait = resolve })
  let waitCount = 0
  const effects: SubscriptionPlatformEffects = {
    copyUserCode: async (code) => { copied.push(code); return false },
    openVerificationUrl: async (url) => { opened.push(url); return false },
    wait: async () => {
      waitCount++
      if (waitCount === 1) await firstWait
    },
  }
  const setup = await testRender(() => <App
    session={new MemoryTargetSession(targetView("claude"))}
    subscriptionAccountSession={accounts}
    subscriptionEffects={effects}
  />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/accounts")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Subscription Accounts"))
    setup.mockInput.pressKey("a")
    const challenge = await setup.waitForFrame((frame) => frame.includes("ABCD-EFGH"))
    expect(challenge).toContain("auth.openai.com/codex/device")
    releaseFirstWait()
    await setup.waitFor(() => accounts.polls.length === 0)
    const authorized = await setup.waitForFrame((frame) => frame.includes("Authorization complete"))
    expect(authorized).toContain("operator@example.test")
    expect(copied).toEqual(["ABCD-EFGH"])
    expect(opened).toEqual(["https://auth.openai.com/codex/device"])

    setup.mockInput.pressKey("s")
    const preview = await setup.waitForFrame((frame) => frame.includes("Default preview"))
    expect(preview).toContain("Follow Default")
    expect(preview).toContain("— → account-primary")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => accounts.actions.length === 1)
    expect(accounts.actions[0]).toEqual({
      kind: "set-default-account",
      accountId: "account-primary",
      previewToken: "00000000-0000-4000-8000-000000001194",
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("Bridge picker blocks Direct, presents every binding failure, and hands off to Subscription Accounts", async () => {
  const cases = [
    { label: "Not bound", target: bridgeTargetView(true), bindings: [] },
    {
      label: "Missing Account",
      target: bridgeTargetView(),
      bindings: [{
        target: "claude" as const,
        providerId,
        providerRevision: 1,
        providerName: "Codex Subscription Bridge",
        binding: { kind: "fixed" as const, accountId: "missing-account" },
        resolution: { state: "missing" as const, accountId: "missing-account" },
      }],
    },
    {
      label: "Needs Reauthorization",
      target: bridgeTargetView(),
      bindings: [{
        target: "claude" as const,
        providerId,
        providerRevision: 1,
        providerName: "Codex Subscription Bridge",
        binding: { kind: "fixed" as const, accountId: "account-primary" },
        resolution: { state: "needs-reauthorization" as const, accountId: "account-primary" },
      }],
    },
  ]

  for (const [index, fixture] of cases.entries()) {
    const target = new MemoryTargetSession(fixture.target)
    const accounts = new MemorySubscriptionSession({ ...accountCatalog(), bindings: fixture.bindings })
    const setup = await testRender(() => <App
      session={target}
      subscriptionAccountSession={accounts}
    />, { width: 120, height: 30, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("2")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      const picker = await setup.waitForFrame((frame) => frame.includes(`Subscription Account · ${fixture.label}`))
      expect(picker).toContain("Takeover required")
      expect(picker.replace(/\s+/g, " ")).toContain("Subscription Account authentication · no Provider credential")
      expect(picker).not.toContain("Credential Reference missing")
      expect(picker.replace(/\s+/g, " ")).toContain("ctrl+x i Subscription Accounts")

      setup.mockInput.pressKey("x", { ctrl: true })
      setup.mockInput.pressKey("a")
      await Promise.resolve()
      expect(target.actions).toHaveLength(0)

      if (index === 0) {
        setup.mockInput.pressKey("x", { ctrl: true })
        setup.mockInput.pressKey("i")
        const overlay = await setup.waitForFrame((frame) => frame.includes("Subscription Accounts"))
        expect(overlay.replace(/\s+/g, " ")).toContain("Active binding target · Claude Code")
        setup.mockInput.pressKey("f")
        await setup.waitFor(() => accounts.actions.length === 1)
        expect(accounts.actions[0]).toEqual({
          kind: "bind-provider-fixed",
          target: "claude",
          providerId,
          providerRevision: 1,
          accountId: "account-primary",
        })
      }
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("Subscription Account diagnostics scan controlled private surfaces before fixed rendering", async () => {
  const secrets = [
    "ACCOUNT_CREDENTIAL_SECRET_11951",
    "ACCOUNT_IDENTITY_SECRET_11952",
    "ACCOUNT_CONFIG_SECRET_11953",
    "ACCOUNT_BACKEND_SECRET_11954",
    "ACCOUNT_SETTINGS_SECRET_11955",
  ] as const
  const accounts = new MemorySubscriptionSession()
  const failure = Object.assign(
    new ControlError("internal-failure", secrets[3]),
    {
      credential: secrets[0],
      account: secrets[1],
      config: { raw: secrets[2] },
      settings: { raw: secrets[4] },
    },
  )
  accounts.actionFailure = failure
  assertControlledSecretSource(failure, secrets, "subscription-account-failure-source")
  const setup = await testRender(() => <App
    session={new MemoryTargetSession()}
    subscriptionAccountSession={accounts}
  />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/accounts")
    setup.mockInput.pressEnter()
    setup.mockInput.pressKey("f")
    const frame = await waitForSecretFreeFrame(
      setup,
      (next) => next.includes("Subscription Account action failed: internal-failure"),
      secrets,
      "subscription-account-action-error",
    )
    expect(frame).not.toContain(secrets[3])
  } finally {
    setup.renderer.destroy()
  }
})

test("account workflow binds Fixed or Follow Default metadata and cancels reauthorization locally", async () => {
  const accounts = new MemorySubscriptionSession({
    ...accountCatalog(),
    accounts: [
      ...accountCatalog().accounts,
      {
        accountId: "account-secondary",
        email: null,
        authenticatedAt: 2,
        state: "needs-reauthorization",
        default: false,
      },
    ],
  })
  let waitAborted = false
  const effects: SubscriptionPlatformEffects = {
    copyUserCode: async () => true,
    openVerificationUrl: async () => true,
    wait: async (_milliseconds, signal) => await new Promise<void>((_resolve, reject) => {
      signal.addEventListener("abort", () => {
        waitAborted = true
        reject(new Error("cancelled"))
      }, { once: true })
    }),
  }
  const setup = await testRender(() => <App
    session={new MemoryTargetSession()}
    subscriptionAccountSession={accounts}
    subscriptionEffects={effects}
  />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/accounts")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("account-secondary"))

    setup.mockInput.pressKey("f")
    await setup.waitFor(() => accounts.actions.length === 1)
    expect(accounts.actions[0]).toEqual({
      kind: "bind-provider-fixed",
      target: "codex",
      providerId,
      providerRevision: 1,
      accountId: "account-primary",
    })
    setup.mockInput.pressKey("l")
    await setup.waitFor(() => accounts.actions.length === 2)
    expect(accounts.actions[1]).toEqual({
      kind: "bind-provider-follow-default",
      target: "codex",
      providerId,
      providerRevision: 1,
    })

    setup.mockInput.pressKey("down")
    setup.mockInput.pressKey("r")
    await setup.waitForFrame((frame) => frame.includes("ABCD-EFGH"))
    expect(accounts.startCalls).toEqual(["account-secondary"])
    setup.mockInput.pressEscape()
    const cancelled = await setup.waitForFrame((frame) => frame.includes("cancelled locally"))
    expect(cancelled).toContain("account-secondary")
    expect(waitAborted).toBeTrue()

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitFor(() => accounts.actions.length === 3)
    expect(accounts.actions[2]).toEqual({
      kind: "delete-account",
      accountId: "account-secondary",
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("Subscription Account catalog has locale parity and renders from 1x1 through 121x30", async () => {
  for (const [width, height] of [[1, 1], [20, 8], [80, 24], [121, 30]] as const) {
    const setup = await testRender(() => <App
      session={new MemoryTargetSession()}
      subscriptionAccountSession={new MemorySubscriptionSession()}
      locale="zh-CN"
    />, { width, height, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/accounts")
      setup.mockInput.pressEnter()
      await setup.renderOnce()
      expect(() => setup.captureCharFrame()).not.toThrow()
      if (width >= 80) {
        const frame = await setup.waitForFrame((next) => next.includes("订阅账户"))
        expect(frame).toContain("已授权")
        expect(frame).toContain("跟随默认")
      }
    } finally {
      setup.renderer.destroy()
    }
  }
})
