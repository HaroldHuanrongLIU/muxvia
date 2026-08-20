import { expect, test } from "bun:test"
import type { InputRenderable } from "@opentui/core"
import { testRender } from "@opentui/solid"

import { MuxviaKeymapProvider, useMuxviaKeymap } from "../src/commands/keymap"
import type { CompatibilityResolution, TargetSession } from "../src/control/target-session"
import type { UniversalProviderSession } from "../src/control/universal-provider-session"
import type {
  ActionOutcome,
  CompatibilityProbe,
  DiscoverySource,
  ModelDiscoveryResult,
  OrdinaryTargetAction,
  ReachabilityResult,
  ReconciliationPreview,
  ReconciliationStrategy,
  TargetAction,
  TargetView,
  UniversalProviderAction,
  UniversalProviderCatalogView,
  UniversalProviderOutcome,
} from "../src/control/types"
import { createTranslator } from "../src/i18n"
import { App } from "../src/ui/app"
import { OverlayProvider } from "../src/ui/overlay-stack"
import { ProviderForm, type ProviderFormResult } from "../src/ui/provider-form"
import {
  assertControlledSecretSource,
  assertSecretFreeStructured,
  auditSecretFreeActions,
  auditSecretFreeFrame,
  auditSecretFreePreview,
  auditSecretFreeView,
  waitForSecretFreeCondition,
  waitForSecretFreeFrame,
} from "./secret-audit"

const credentialSecret = "provider-secret-must-not-render"
const credentialUuid = "00000000-0000-4000-8000-000000000099"
const configSecret = "config-compatibility-secret-must-not-render"
const backendSecret = "backend-claude-direct-secret-must-not-render"
const settingsSecret = "settings-claude-direct-secret-must-not-render"
const claudeDirectSecrets = [credentialSecret, backendSecret, settingsSecret] as const
const compatibilitySecrets = [credentialSecret, configSecret, backendSecret, settingsSecret] as const
const bridgeAccountSecret = "subscription-bridge-account-secret-must-not-render"
const bridgeSecrets = [credentialSecret, bridgeAccountSecret, configSecret, backendSecret, settingsSecret] as const

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void
  let reject!: (reason?: unknown) => void
  const promise = new Promise<T>((next, fail) => {
    resolve = next
    reject = fail
  })
  return { promise, resolve, reject }
}

function provider(overrides: Partial<TargetView["providers"][number]>): TargetView["providers"][number] {
  return {
    id: "00000000-0000-4000-8000-000000000011",
    position: 0,
    providerRevision: 1,
    name: "First Provider",
    baseUrl: "https://first.example/v1",
    model: "first-model",
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
    ...overrides,
  }
}

function view(overrides: Partial<TargetView> = {}): TargetView {
  return {
    target: "codex",
    managementRevision: 1,
    viewSequence: 1,
    service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: [],
    providerPresets: [],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    failover: { draftRevision: 1, draftMembers: [], activePlan: null },
    problems: [],
    ...overrides,
  }
}

type RecordedTargetAction<T extends TargetAction = TargetAction> = T extends { credential: infer Credential }
  ? Omit<T, "credential"> & {
    credential: Credential extends { kind: "replace" }
      ? { kind: "replace"; valuePresent: boolean }
      : Credential
  }
  : T

function projectAction(action: TargetAction): RecordedTargetAction {
  if ("credential" in action && action.credential.kind === "replace") {
    return {
      ...action,
      credential: { kind: "replace", valuePresent: action.credential.value.length > 0 },
    } as RecordedTargetAction
  }
  return action as RecordedTargetAction
}

class MemoryTargetSession implements TargetSession {
  readonly actions: RecordedTargetAction[] = []
  readonly reconciliationPreviews: ReconciliationStrategy[] = []
  readonly reconciliationPreviewResults: ReconciliationPreview[] = []
  readonly reconciliationApplies: Array<{
    strategy: ReconciliationStrategy
    observationToken: string
    acknowledgeVersion?: string
  }> = []
  readonly compatibilityProbes: CompatibilityProbe[] = []
  readonly compatibilityResolutions: CompatibilityResolution[] = []
  readonly reachabilityChecks: string[] = []
  readonly discoveryRequests: DiscoverySource[] = []
  lastError: unknown
  readonly #listeners = new Set<(next: TargetView) => void>()
  #view: TargetView
  #handler: (action: TargetAction) => Promise<ActionOutcome>
  previewHandler?: (strategy: ReconciliationStrategy, signal?: AbortSignal) => Promise<ReconciliationPreview>
  compatibilityProbeHandler?: (signal?: AbortSignal) => Promise<CompatibilityProbe>
  compatibilityResolutionHandler?: (input: CompatibilityResolution) => Promise<ActionOutcome>
  reconciliationHandler?: (input: {
    strategy: ReconciliationStrategy
    observationToken: string
    acknowledgeVersion?: string
  }) => Promise<ActionOutcome>
  reachabilityHandler?: (providerId: string, providerRevision: number, signal?: AbortSignal) => Promise<ReachabilityResult>
  discoveryHandler?: (source: DiscoverySource, signal?: AbortSignal) => Promise<ModelDiscoveryResult>

  constructor(
    initial: TargetView,
    handler: (action: TargetAction) => Promise<ActionOutcome> = async () => ({ status: "applied", view: initial }),
  ) {
    this.#view = initial
    this.#handler = handler
  }

  get(): Readonly<TargetView> { return this.#view }
  async act(action: OrdinaryTargetAction): Promise<ActionOutcome> {
    return await this.#applyAction(action)
  }
  async #applyAction(action: TargetAction): Promise<ActionOutcome> {
    this.actions.push(projectAction(action))
    try {
      const outcome = await this.#handler(action)
      this.#view = outcome.view
      return outcome
    } catch (error) {
      this.lastError = error
      throw error
    }
  }
  setView(next: TargetView): void { this.#view = next }
  pushView(next: TargetView): void {
    this.#view = next
    for (const listener of this.#listeners) listener(next)
  }
  async discoverModels(source: DiscoverySource, signal?: AbortSignal): Promise<ModelDiscoveryResult> {
    this.discoveryRequests.push(source)
    if (!this.discoveryHandler) throw new Error("discovery not configured in this fixture")
    return await this.discoveryHandler(source, signal)
  }
  async checkReachability(providerId: string, providerRevision: number, signal?: AbortSignal): Promise<ReachabilityResult> {
    this.reachabilityChecks.push(`${providerId}:${providerRevision}`)
    if (!this.reachabilityHandler) throw new Error("reachability not configured in this fixture")
    return await this.reachabilityHandler(providerId, providerRevision, signal)
  }
  async previewReconciliation(strategy: ReconciliationStrategy, signal?: AbortSignal): Promise<ReconciliationPreview> {
    this.reconciliationPreviews.push(strategy)
    if (!this.previewHandler) throw new Error("reconciliation not configured in this fixture")
    const preview = await this.previewHandler(strategy, signal)
    this.reconciliationPreviewResults.push(preview)
    return preview
  }
  async probeCompatibility(signal?: AbortSignal): Promise<CompatibilityProbe> {
    if (!this.compatibilityProbeHandler) throw new Error("compatibility probe not configured in this fixture")
    const probe = await this.compatibilityProbeHandler(signal)
    this.compatibilityProbes.push(probe)
    return probe
  }
  async resolveCompatibility(input: CompatibilityResolution): Promise<ActionOutcome> {
    this.compatibilityResolutions.push(structuredClone(input))
    if (this.compatibilityResolutionHandler) {
      const outcome = await this.compatibilityResolutionHandler(input)
      this.#view = outcome.view
      return outcome
    }
    return await this.#applyAction({ kind: "resolve-compatibility", version: input.version })
  }
  async applyReconciliation(input: {
    strategy: ReconciliationStrategy
    observationToken: string
    acknowledgeVersion?: string
  }): Promise<ActionOutcome> {
    this.reconciliationApplies.push(input)
    if (!this.reconciliationHandler) throw new Error("reconciliation not configured in this fixture")
    const outcome = await this.reconciliationHandler(input)
    this.#view = outcome.view
    return outcome
  }
  subscribe(listener: (next: TargetView) => void): () => void {
    this.#listeners.add(listener)
    return () => this.#listeners.delete(listener)
  }
  async whenClosed(): Promise<void> { return await new Promise(() => {}) }
  async close(): Promise<void> {}
}

class MemoryUniversalProviderSession implements UniversalProviderSession {
  readonly actions: unknown[] = []
  readonly #listeners = new Set<(next: UniversalProviderCatalogView) => void>()
  #view: UniversalProviderCatalogView
  readonly #handler: (action: UniversalProviderAction) => Promise<UniversalProviderOutcome>

  constructor(
    initial: UniversalProviderCatalogView,
    handler: (action: UniversalProviderAction) => Promise<UniversalProviderOutcome> = async () => ({ status: "applied", view: initial }),
  ) {
    this.#view = initial
    this.#handler = handler
  }
  get(): Readonly<UniversalProviderCatalogView> { return this.#view }
  async act(action: UniversalProviderAction): Promise<UniversalProviderOutcome> {
    this.actions.push("credential" in action && action.credential.kind === "replace"
      ? { ...structuredClone(action), credential: { kind: "replace", valuePresent: action.credential.value.length > 0 } }
      : structuredClone(action))
    const outcome = await this.#handler(action)
    this.#view = outcome.view
    return outcome
  }
  subscribe(listener: (next: UniversalProviderCatalogView) => void): () => void {
    this.#listeners.add(listener)
    return () => this.#listeners.delete(listener)
  }
  push(next: UniversalProviderCatalogView): void {
    this.#view = next
    for (const listener of this.#listeners) listener(next)
  }
  async whenClosed(): Promise<void> { return await new Promise(() => {}) }
  async close(): Promise<void> {}
}

function universalCatalog(): UniversalProviderCatalogView {
  return {
    revision: 1,
    viewSequence: 1,
    providers: [{
      id: "00000000-0000-4000-8000-000000000070",
      position: 0,
      providerRevision: 1,
      name: "Shared Frontier",
      baseUrl: "https://shared.example/v1",
      credential: "present",
      provenance: { kind: "preset", key: "openai-api-responses" },
      targets: [
        {
          target: "codex", enabled: true, model: "gpt-shared", authentication: "openai-bearer",
          routingRequirement: "direct-compatible", overlayRevision: 1,
          generatedProviderId: "00000000-0000-4000-8000-000000000071",
          synchronization: "current", activeReferences: [],
        },
        {
          target: "claude", enabled: true, model: "claude-shared", authentication: "anthropic-api-key",
          routingRequirement: "takeover-required", overlayRevision: 1,
          generatedProviderId: "00000000-0000-4000-8000-000000000072",
          synchronization: "pending", activeReferences: [],
        },
      ],
    }],
    presets: [],
  }
}

function reconciliationPreview(
  target: "codex" | "claude",
  strategy: ReconciliationStrategy,
  overrides: Partial<ReconciliationPreview> = {},
): ReconciliationPreview {
  return {
    observationToken: "00000000-0000-4000-8000-000000000090",
    target,
    strategy,
    managementRevision: 1,
    compatibility: { version: "9.9.9", classification: "tested", acknowledgementRequired: false },
    shadowSources: [],
    changes: [
      { field: "provider", state: "changed" },
      { field: "credential", state: "unchanged" },
      { field: "takeover", state: "absent" },
    ],
    providerEffect: "keep-current",
    restartRequired: true,
    unobservableRuntimeBoundary: true,
    ...overrides,
  }
}

function controlledClaudeDirectSources(): unknown {
  return {
    credential: credentialSecret,
    backend: new Error(backendSecret),
    settings: { raw: settingsSecret },
  }
}

function controlledCompatibilityProblem(code: string): TargetView["problems"][number] {
  return {
    code,
    message: backendSecret,
    credentialDiagnostic: credentialSecret,
    configDiagnostic: configSecret,
    settingsDiagnostic: settingsSecret,
  } as TargetView["problems"][number]
}

async function waitForClaudeDirectFrame(
  setup: Awaited<ReturnType<typeof testRender>>,
  predicate: (frame: string) => boolean,
  label: string,
): Promise<string> {
  return await waitForSecretFreeFrame(setup, predicate, claudeDirectSecrets, label)
}

async function waitForClaudeDirectActions(
  setup: Awaited<ReturnType<typeof testRender>>,
  session: MemoryTargetSession,
  count: number,
  label: string,
): Promise<void> {
  await waitForSecretFreeCondition(
    setup,
    () => session.actions.length === count,
    () => auditSecretFreeActions(session.actions, claudeDirectSecrets, label),
    `secret-scan-failed:${label}-action`,
    label,
  )
}

function assertClaudeDirectActions(
  session: MemoryTargetSession,
  expected: RecordedTargetAction[],
  label: string,
): void {
  assertSecretFreeStructured("action", session.actions, claudeDirectSecrets, label, (safeActions) => {
    expect(safeActions).toEqual(expected)
  })
}

function expectInOrder(frame: string, names: readonly string[]): void {
  let cursor = 0
  for (const name of names) {
    const next = frame.indexOf(name, cursor)
    expect(next ?? -1).toBeGreaterThanOrEqual(cursor)
    cursor = (next ?? 0) + name.length
  }
}

test.each(["codex", "claude"] as const)(
  "Failover route overlay saves a Target-bound draft and applies one immutable plan for %s",
  async (target) => {
    const primary = provider({
      id: "00000000-0000-4000-8000-000000000041",
      name: "Primary Rail",
      routeHealth: { state: "degraded" },
    })
    const fallback = provider({
      id: "00000000-0000-4000-8000-000000000042",
      position: 1,
      name: "Fallback Rail",
      routeHealth: { state: "healthy" },
    })
    const initial = view({
      target,
      managementRevision: 4,
      viewSequence: 7,
      providers: [primary, fallback],
      currentProviderId: primary.id,
      servingProviderId: fallback.id,
      failover: {
        draftRevision: 2,
        draftMembers: [{ providerId: primary.id, providerRevision: primary.providerRevision }],
        activePlan: {
          id: "00000000-0000-4000-8000-000000000043",
          epoch: "00000000-0000-4000-8000-000000000044",
          members: [{
            position: 0,
            providerId: primary.id,
            providerRevision: primary.providerRevision,
            name: primary.name,
            model: primary.model,
            protocol: primary.protocol,
            authentication: primary.authentication,
          }],
        },
      },
    })
    const apply = deferred<ActionOutcome>()
    let authoritative = initial
    const session = new MemoryTargetSession(initial, async (action) => {
      if (action.kind === "save-failover-draft") {
        authoritative = {
          ...authoritative,
          managementRevision: authoritative.managementRevision + 1,
          viewSequence: authoritative.viewSequence + 1,
          failover: {
            ...authoritative.failover!,
            draftRevision: authoritative.failover!.draftRevision + 1,
            draftMembers: structuredClone(action.members),
          },
        }
        return { status: "applied", view: authoritative }
      }
      if (action.kind === "apply-failover-chain") return await apply.promise
      return { status: "applied", view: authoritative }
    })
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 100,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/route")
      setup.mockInput.pressEnter()
      const opened = await setup.waitForFrame((frame) => frame.includes("Failover Route"))
      expect(opened).toContain("Current: Primary Rail · Serving: Fallback Rail")
      expect(opened).toContain("01 · Current · Primary Rail · Complete · Synchronized")
      expect(opened).toContain("Degraded")
      expect(opened).toContain("Draft matches the active route")

      await setup.mockInput.typeText("a")
      await waitForSecretFreeCondition(
        setup,
        () => session.actions.length === 1,
        () => auditSecretFreeActions(session.actions, compatibilitySecrets, `route-save-${target}`),
        `secret-scan-failed:route-save-${target}`,
        `route-save-${target}`,
      )
      const saved = await setup.waitForFrame((frame) => frame.includes("02 · Fallback · Fallback Rail"))
      expect(saved).toContain("Draft differs from the active route")
      assertSecretFreeStructured("action", session.actions, compatibilitySecrets, `route-save-${target}`, (actions) => {
        expect(actions).toEqual([{
          kind: "save-failover-draft",
          members: [
            { providerId: primary.id, providerRevision: 1 },
            { providerId: fallback.id, providerRevision: 1 },
          ],
        }])
      })

      setup.mockInput.pressEnter()
      await waitForSecretFreeCondition(
        setup,
        () => session.actions.length === 2,
        () => auditSecretFreeActions(session.actions, compatibilitySecrets, `route-apply-${target}`),
        `secret-scan-failed:route-apply-${target}`,
        `route-apply-${target}`,
      )
      setup.mockInput.pressEscape()
      const pending = await setup.waitForFrame((frame) => frame.includes("Applying immutable route plan"))
      expect(pending).toContain("Failover Route")

      const draft = authoritative.failover!.draftMembers
      authoritative = {
        ...authoritative,
        managementRevision: authoritative.managementRevision + 1,
        viewSequence: authoritative.viewSequence + 1,
        failover: {
          ...authoritative.failover!,
          activePlan: {
            id: "00000000-0000-4000-8000-000000000045",
            epoch: "00000000-0000-4000-8000-000000000046",
            members: draft.map((member, position) => {
              const declaration = authoritative.providers.find((candidate) => candidate.id === member.providerId)!
              return {
                position,
                providerId: member.providerId,
                providerRevision: member.providerRevision,
                name: declaration.name,
                model: declaration.model,
                protocol: declaration.protocol,
                authentication: declaration.authentication,
              }
            }),
          },
        },
      }
      apply.resolve({ status: "applied", view: authoritative })
      const applied = await setup.waitForFrame((frame) => frame.includes("Draft matches the active route") && frame.includes("000000000046"))
      expect(applied).toContain("Fallback Rail")
      expect(session.actions[1]).toEqual({ kind: "apply-failover-chain", draftRevision: 3 })
    } finally {
      setup.renderer.destroy()
    }
  },
)

async function fillProviderDraft(
  mockInput: Awaited<ReturnType<typeof testRender>>["mockInput"],
  fields: readonly [string, string, string, string],
): Promise<void> {
  await mockInput.typeText(fields[0])
  mockInput.pressTab()
  await mockInput.typeText(fields[1])
  mockInput.pressTab()
  await mockInput.typeText(fields[2])
  mockInput.pressTab()
  await mockInput.typeText(fields[3])
}

function auditReconciliationSessions(sessions: readonly MemoryTargetSession[], label: string): void {
  for (const [index, session] of sessions.entries()) {
    auditSecretFreeActions({
      actions: session.actions,
      compatibilityResolutions: session.compatibilityResolutions,
      applies: session.reconciliationApplies,
      discovery: session.discoveryRequests,
      reachability: session.reachabilityChecks,
    }, claudeDirectSecrets, `${label}-${index}`)
    auditSecretFreePreview(session.reconciliationPreviewResults, claudeDirectSecrets, `${label}-${index}`)
    auditSecretFreePreview(session.compatibilityProbes, claudeDirectSecrets, `${label}-${index}`)
    auditSecretFreeView(session.get(), claudeDirectSecrets, `${label}-${index}`)
  }
}

function auditCompatibilityOutputs(session: MemoryTargetSession, label: string): void {
  auditSecretFreeActions({
    actions: session.actions,
    resolutions: session.compatibilityResolutions,
  }, compatibilitySecrets, label)
  auditSecretFreePreview(session.compatibilityProbes, compatibilitySecrets, label)
}

async function waitForReconciliationFrame(
  setup: Awaited<ReturnType<typeof testRender>>,
  sessions: readonly MemoryTargetSession[],
  predicate: (frame: string) => boolean,
  label: string,
): Promise<string> {
  auditReconciliationSessions(sessions, label)
  return await waitForSecretFreeFrame(setup, (frame) => {
    auditReconciliationSessions(sessions, label)
    return predicate(frame)
  }, claudeDirectSecrets, label)
}

async function waitForReconciliationState(
  setup: Awaited<ReturnType<typeof testRender>>,
  sessions: readonly MemoryTargetSession[],
  predicate: () => boolean,
  label: string,
): Promise<void> {
  await waitForSecretFreeCondition(
    setup,
    predicate,
    () => auditReconciliationSessions(sessions, label),
    `secret-scan-failed:${label}-action`,
    label,
  )
}

test("/providers renders provenance kinds and generated state with secret-free selected Provider detail", async () => {
  const first = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Generated Provider",
    provenance: { kind: "universal-provider", key: "00000000-0000-4000-8000-000000000010" },
    generated: true,
    activeReferences: ["current", "activated-snapshot"],
  })
  const second = provider({
    id: "00000000-0000-4000-8000-000000000012",
    position: 1,
    name: "Preset Provider",
    provenance: { kind: "preset", key: "openai-api-responses" },
  })
  const third = provider({
    id: "00000000-0000-4000-8000-000000000013",
    position: 2,
    name: "Ordinary Provider",
  })
  const fourth = provider({
    id: "00000000-0000-4000-8000-000000000014",
    position: 3,
    name: "Future Provider",
    provenance: { kind: "future-kind-must-not-render", key: "future-key-must-not-render" },
  })
  const session = new MemoryTargetSession(view({
    providers: [first, second, third, fourth],
    currentProviderId: first.id,
    activatedSnapshot: {
      id: "00000000-0000-4000-8000-000000000002",
      providerId: first.id,
      model: first.model,
      protocol: "openai-responses",
      authentication: "openai-bearer",
      epoch: "00000000-0000-4000-8000-000000000003",
    },
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    const frame = setup.captureCharFrame()

    expect(frame).toContain("Providers")
    expectInOrder(frame, ["Generated Provider", "Preset Provider", "Ordinary Provider", "Future Provider"])
    expect(frame).toContain("Complete")
    expect(frame).toContain("Universal Provider")
    expect(frame).toContain("Preset")
    expect(frame).toContain("Ordinary")
    expect(frame).toContain("Other provenance")
    expect(frame).toContain("Generated")
    expect(frame).toContain("Credential Reference present")
    expect(frame).toContain("Current")
    expect(frame).toContain("Activated Snapshot")
    expect(frame).not.toContain(credentialSecret)
    expect(frame).not.toContain(credentialUuid)
    expect(frame).not.toContain("future-kind-must-not-render")
    expect(frame).not.toContain("future-key-must-not-render")

    setup.mockInput.pressKey("down")
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Preset Provider")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Edit Provider")
    expect(setup.captureCharFrame()).toContain("Preset Provider")
  } finally {
    setup.renderer.destroy()
  }
})

test("Generated Provider editor locks Universal fields and saves only the Target Overlay", async () => {
  const generated = provider({
    id: "00000000-0000-4000-8000-000000000071",
    name: "Shared Claude",
    baseUrl: "https://shared.example/v1",
    model: "claude-old",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
    routingRequirement: "direct-compatible",
    credential: "missing",
    generated: true,
    universalProviderId: "00000000-0000-4000-8000-000000000070",
    synchronization: "current",
    provenance: { kind: "universal-provider", key: "00000000-0000-4000-8000-000000000070" },
    ownership: {
      name: "universal-provider", baseUrl: "universal-provider", model: "target-overlay",
      protocol: "target-fixed", authentication: "target-overlay",
      routingRequirement: "target-overlay", credential: "universal-provider",
    },
  })
  const initial = view({ target: "claude", providers: [generated] })
  const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: initial }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Managed by its Universal Provider"))
    setup.mockInput.pressEnter()
    const editor = await setup.waitForFrame((frame) => frame.includes("Generated Provider Target Overlay"))
    expect(editor).toContain("Universal-owned fields are read-only")
    expect(editor).toContain("Shared Claude")
    expect(editor).toContain("https://shared.example/v1")
    expect(editor).toContain("Target fixed")

    await setup.mockInput.typeText("-overlay")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(" ")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(" ")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toEqual({
      kind: "update-provider",
      providerId: generated.id,
      providerRevision: generated.providerRevision,
      name: "Shared Claude",
      baseUrl: "https://shared.example/v1",
      model: "claude-old-overlay",
      authentication: "anthropic-bearer",
      routingRequirement: "takeover-required",
      credential: { kind: "keep" },
    })

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Managed by its Universal Provider"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.renderOnce()
    expect(session.actions).toHaveLength(1)

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    await setup.waitForFrame((frame) => frame.includes("Shared Claude Copy"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "duplicate-provider",
      sourceProviderId: generated.id,
      sourceProviderRevision: generated.providerRevision,
      credential: { kind: "without" },
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("Provider picker remains renderable at every required terminal size", async () => {
  const session = new MemoryTargetSession(view({ providers: [provider({})] }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    for (const [width, height] of [[1, 1], [2, 2], [20, 5], [40, 10], [80, 24], [121, 30]] as const) {
      setup.resize(width, height)
      await setup.renderOnce()
      expect(() => setup.captureCharFrame()).not.toThrow()
    }
  } finally {
    setup.renderer.destroy()
  }
})

test("provider move commands send an exact identity permutation and wait for the action outcome", async () => {
  let resolve!: (outcome: ActionOutcome) => void
  const pending = new Promise<ActionOutcome>((next) => { resolve = next })
  const first = provider({ id: "00000000-0000-4000-8000-000000000011", name: "First Provider" })
  const second = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Second Provider" })
  const third = provider({ id: "00000000-0000-4000-8000-000000000013", position: 2, name: "Third Provider" })
  const initial = view({ providers: [first, second, third] })
  const session = new MemoryTargetSession(initial, async () => await pending)
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    setup.resize(121, 30)
    await setup.renderOnce()
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("u")
    await setup.renderOnce()
    expect(session.actions).toEqual([])

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("n")
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions).toEqual([{
      kind: "reorder-providers",
      providerIds: [second.id, first.id, third.id],
    }])
    expectInOrder(setup.captureCharFrame(), ["First Provider ·", "Second Provider ·", "Third Provider ·"])

    resolve({ status: "applied", view: view({ providers: [{ ...second }, { ...first }, { ...third }], viewSequence: 2 }) })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    expectInOrder(setup.captureCharFrame(), ["Second Provider ·", "First Provider ·", "Third Provider ·"])
  } finally {
    setup.renderer.destroy()
  }
})

test("deletion confirms before dispatching and keeps the picker open with authoritative active references", async () => {
  const first = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Referenced Provider",
    activeReferences: ["current", "activated-snapshot"],
  })
  const initial = view({ providers: [first] })
  const session = new MemoryTargetSession(initial, async () => {
    session.setView(initial)
    throw { code: "stale-revision", message: "backend-message-must-not-render" }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    expect(confirmation).toContain("Referenced Provider")
    expect(session.actions).toEqual([])

    setup.mockInput.pressEscape()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Providers")
    expect(session.actions).toEqual([])

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await Promise.resolve()
    await setup.renderOnce()
    expect(session.actions).toEqual([{
      kind: "delete-provider",
      providerId: first.id,
      providerRevision: first.providerRevision,
    }])
    const picker = setup.captureCharFrame()
    expect(picker).toContain("Current")
    expect(picker).toContain("Activated Snapshot")
    expect(picker).not.toContain("backend-message-must-not-render")
  } finally {
    setup.renderer.destroy()
  }
})

test("a failed credential replacement keeps dirty fields but retries an edit with Keep", async () => {
  const savedSecret = "replacement-secret-must-not-render"
  const selected = provider({ credential: "missing", completeness: "incomplete", missingFields: ["credential"] })
  let attempts = 0
  let sawReplacementSecret = false
  const session = new MemoryTargetSession(view({ providers: [selected] }), async (action) => {
    attempts++
    if (action.kind === "update-provider" && action.credential.kind === "replace") {
      sawReplacementSecret = action.credential.value === savedSecret
    }
    if (attempts === 1) throw { code: "invalid-provider", message: "server-message-must-not-render" }
    return { status: "applied", view: view({ providers: [selected], viewSequence: 2 }) }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    await setup.mockInput.typeText(" dirty")
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(savedSecret)
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    await setup.renderOnce()

    expect(setup.captureCharFrame()).toContain("First Provider dirty")
    expect(setup.captureCharFrame()).not.toContain(savedSecret)
    expect(setup.captureCharFrame()).not.toContain("server-message-must-not-render")
    expect(session.actions[0]).toMatchObject({
      kind: "update-provider",
      credential: { kind: "replace", valuePresent: true },
    })
    expect(sawReplacementSecret).toBeTrue()
    const persistedReplacementSecret = JSON.stringify(session.actions).includes(savedSecret)
    expect(persistedReplacementSecret).toBeFalse()

    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "update-provider",
      name: "First Provider dirty",
      credential: { kind: "keep" },
    })
    expect(JSON.stringify(session.actions[1])).not.toContain(savedSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("a failed explicit credential removal retries an edit with Remove", async () => {
  const results: ProviderFormResult[] = []
  let keymap!: ReturnType<typeof useMuxviaKeymap>
  function Harness() {
    keymap = useMuxviaKeymap()
    return <ProviderForm
      target="codex"
      mode="edit"
      initialDraft={{
        name: "First Provider",
        baseUrl: "https://first.example/v1",
        model: "first-model",
        providerId: "00000000-0000-4000-8000-000000000011",
        providerRevision: 1,
      }}
      credentialPresence="present"
      pending={false}
      t={createTranslator("en")}
      onDirtyChange={() => {}}
      onCancel={() => {}}
      onSave={async (result) => {
        results.push(result)
        return false
      }}
    />
  }
  const setup = await testRender(() => (
    <MuxviaKeymapProvider><OverlayProvider><Harness /></OverlayProvider></MuxviaKeymapProvider>
  ), { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    keymap.dispatchCommand("provider.credential.remove")
    await setup.renderOnce()
    keymap.dispatchCommand("provider.save")
    await setup.waitFor(() => results.length === 1)
    await setup.renderOnce()

    expect(results[0]).toMatchObject({ kind: "update-provider", credential: { kind: "remove" } })

    keymap.dispatchCommand("provider.save")
    await setup.waitFor(() => results.length === 2)
    expect(results[1]).toMatchObject({ kind: "update-provider", credential: { kind: "remove" } })
  } finally {
    setup.renderer.destroy()
  }
})

test("a successful delete selects a remaining Provider for the next named action", async () => {
  const first = provider({ id: "00000000-0000-4000-8000-000000000011", name: "Deleted Provider" })
  const second = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Remaining Provider", model: "remaining-model" })
  const afterDelete = view({ providers: [second], viewSequence: 2 })
  const session = new MemoryTargetSession(view({ providers: [first, second] }), async (action) => {
    if (action.kind === "delete-provider") return { status: "applied", view: afterDelete }
    return { status: "applied", view: afterDelete }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => session.actions.length === 1)
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()

    expect(setup.captureCharFrame()).toContain("Remaining Provider")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Edit Provider")
    expect(setup.captureCharFrame()).toContain("remaining-model")
  } finally {
    setup.renderer.destroy()
  }
})

test("a stale Provider edit refreshes its revision before the next save", async () => {
  const stale = provider({ providerRevision: 1 })
  const authoritative = provider({ providerRevision: 2, name: "Authoritative Provider" })
  const authoritativeView = view({ providers: [{ ...authoritative }], viewSequence: 2 })
  let attempts = 0
  const session = new MemoryTargetSession(view({ providers: [stale] }), async (action) => {
    attempts++
    if (attempts === 1) {
      session.setView(authoritativeView)
      throw { code: "stale-provider-revision", message: "stale-server-message-must-not-render" }
    }
    return { status: "applied", view: view({ providers: [{ ...authoritative }], viewSequence: 3 }) }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    await setup.mockInput.typeText(" dirty")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    await setup.renderOnce()

    expect(setup.captureCharFrame()).toContain("First Provider dirty")
    expect(setup.captureCharFrame()).not.toContain("stale-server-message-must-not-render")
    expect(session.actions[0]).toMatchObject({ kind: "update-provider", providerRevision: 1 })

    await setup.mockInput.typeText(" again")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "update-provider",
      name: "First Provider dirty again",
      providerRevision: 2,
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("blank edits Keep credentials for either saved presence while creates start with Remove", async () => {
  for (const credential of ["present", "missing"] as const) {
    const selected = provider({ credential })
    const session = new MemoryTargetSession(view({ providers: [selected] }))
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.renderOnce()
      setup.mockInput.pressEnter()
      await setup.renderOnce()
      setup.mockInput.pressEnter()
      await setup.waitFor(() => session.actions.length === 1)
      expect(session.actions[0]).toMatchObject({ kind: "update-provider", credential: { kind: "keep" } })
    } finally {
      setup.renderer.destroy()
    }
  }

  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    await setup.waitForFrame((frame) => frame.includes("Blank"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter save"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toMatchObject({ kind: "create-provider", credential: { kind: "remove" } })
  } finally {
    setup.renderer.destroy()
  }
})

test("Preset source selection copies an ordinary draft without discovery and saves provenance", async () => {
  const session = new MemoryTargetSession(view({
    providerPresets: [{
      key: "openai-api-responses",
      baseUrl: "https://api.openai.com/v1",
      model: "",
      protocol: "openai-responses",
      authentication: "openai-bearer",
    }],
  }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    const sources = await setup.waitForFrame((frame) => frame.includes("Blank"))
    expect(sources).toContain("OpenAI API (Responses)")

    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    const editor = await setup.waitForFrame((frame) => frame.includes("https://api.openai.com/v1"))
    expect(editor).toContain("API credential")
    expect(editor).not.toContain(credentialSecret)

    await setup.mockInput.typeText("Official Copy")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions).toEqual([{
      kind: "create-provider",
      name: "Official Copy",
      baseUrl: "https://api.openai.com/v1",
      model: "",
      authentication: "openai-bearer",
      credential: { kind: "remove" },
      presetKey: "openai-api-responses",
    }])
  } finally {
    setup.renderer.destroy()
  }
})

test("either Target opens one shared Universal Provider catalog with dual synchronization rails", async () => {
  const catalog = new MemoryUniversalProviderSession(universalCatalog())
  const codex = new MemoryTargetSession(view())
  const claude = new MemoryTargetSession(view({ target: "claude" }))
  const setup = await testRender(() => <App
    sessions={{ codex, claude }}
    universalSession={catalog}
  />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    for (const key of ["1", "2"] as const) {
      setup.mockInput.pressKey(key)
      await setup.mockInput.typeText("/universal-providers")
      setup.mockInput.pressEnter()
      const frame = await setup.waitForFrame((next) => next.includes("Shared Frontier"))
      expect(frame).toContain("Universal Providers")
      expect(frame).toContain("Codex CLI · Current")
      expect(frame).toContain("Claude Code · Pending")
      setup.mockInput.pressEscape()
      await setup.renderOnce()
      setup.mockInput.pressEscape()
      await setup.renderOnce()
    }
  } finally {
    setup.renderer.destroy()
  }
})

test("Preset draft edits both Target overlays and synchronizes only after confirmation", async () => {
  const presetCatalog: UniversalProviderCatalogView = {
    revision: 1,
    viewSequence: 1,
    providers: [],
    presets: [{
      key: "openai-api-responses",
      name: "OpenAI API",
      baseUrl: "https://api.openai.com/v1",
      targets: [
        { target: "codex", enabled: true, model: "", authentication: "openai-bearer", routingRequirement: "direct-compatible" },
        { target: "claude", enabled: false, model: "", authentication: "anthropic-api-key", routingRequirement: "direct-compatible" },
      ],
    }],
  }
  const createdId = "00000000-0000-4000-8000-000000000070"
  let current = presetCatalog
  const catalog = new MemoryUniversalProviderSession(presetCatalog, async (action) => {
    if (action.kind === "create-universal-provider") {
      current = {
        revision: 2,
        viewSequence: 2,
        presets: presetCatalog.presets,
        providers: [{
          id: createdId,
          position: 0,
          providerRevision: 1,
          name: action.name,
          baseUrl: action.baseUrl,
          credential: action.credential.kind === "replace" ? "present" : "missing",
          provenance: { kind: "preset", key: action.presetKey! },
          targets: action.targets.map((target, index) => ({
            ...target,
            overlayRevision: 1,
            generatedProviderId: `00000000-0000-4000-8000-00000000007${index + 1}`,
            synchronization: "pending" as const,
            activeReferences: [],
          })),
        }],
      }
    } else if (action.kind === "synchronize-universal-provider") {
      current = {
        ...current,
        revision: 3,
        viewSequence: 3,
        providers: current.providers.map((provider) => ({
          ...provider,
          providerRevision: 2,
          targets: provider.targets.map((target) => ({ ...target, synchronization: "current" as const })),
        })),
      }
    }
    return { status: "applied", view: current }
  })
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} universalSession={catalog} />, {
    width: 80, height: 30, useThread: false, kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/universal-providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("No Universal Providers"))
    setup.mockInput.pressKey("c")
    await setup.waitForFrame((frame) => frame.includes("Create Universal Provider from"))
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Create Universal Provider") && frame.includes("Target projection"))

    await setup.mockInput.typeText(" Shared")
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("gpt-shared")
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(" ")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("claude-shared")
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(" ")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => catalog.actions.length === 1)
    expect(catalog.actions[0]).toMatchObject({
      kind: "create-universal-provider",
      name: "OpenAI API Shared",
      baseUrl: "https://api.openai.com/v1",
      credential: { kind: "remove" },
      presetKey: "openai-api-responses",
      targets: [
        { target: "codex", enabled: true, model: "gpt-shared", authentication: "openai-bearer", routingRequirement: "direct-compatible" },
        { target: "claude", enabled: true, model: "claude-shared", authentication: "anthropic-api-key", routingRequirement: "takeover-required" },
      ],
    })
    const pending = await setup.waitForFrame((frame) => frame.includes("OpenAI API Shared"))
    expect(pending).toContain("Codex CLI · Pending")
    expect(pending).toContain("Claude Code · Pending")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("s")
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Synchronize Universal Provider?"))
    expect(confirmation).toContain("OpenAI API Shared")
    expect(catalog.actions).toHaveLength(1)
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => catalog.actions.length === 2)
    expect(catalog.actions[1]).toEqual({
      kind: "synchronize-universal-provider",
      providerId: createdId,
      providerRevision: 1,
    })
    const synchronized = await setup.waitForFrame((frame) => frame.includes("Codex CLI · Current"))
    expect(synchronized).toContain("Claude Code · Current")
  } finally {
    setup.renderer.destroy()
  }
})

test.each(["invalid-universal-provider", "compatibility-acknowledgement-required"])(
  "Universal Provider workflow scans %s failures before rendering",
  async (problemCode) => {
    const universalSecret = "universal-provider-credential-secret-must-not-render"
    const secrets = [universalSecret, configSecret, backendSecret, settingsSecret] as const
    const catalog = new MemoryUniversalProviderSession(universalCatalog(), async () => {
      throw {
        code: problemCode,
        message: backendSecret,
        configuration: configSecret,
        settings: { raw: settingsSecret },
      }
    })
    const session = new MemoryTargetSession(view())
    const setup = await testRender(() => <App session={session} universalSession={catalog} />, {
      width: 80, height: 30, useThread: false, kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/universal-providers")
      setup.mockInput.pressEnter()
      await waitForSecretFreeFrame(setup, (frame) => frame.includes("Shared Frontier"), secrets, "universal-open")
      setup.mockInput.pressEnter()
      await waitForSecretFreeFrame(setup, (frame) => frame.includes("Edit Universal Provider"), secrets, "universal-edit")
      setup.mockInput.pressTab()
      setup.mockInput.pressTab()
      await setup.mockInput.typeText(universalSecret)
      setup.mockInput.pressEnter()
      await waitForSecretFreeCondition(
        setup,
        () => catalog.actions.length === 1,
        () => auditSecretFreeActions(catalog.actions, secrets, "universal-action"),
        "secret-scan-failed:universal-action",
        "universal-action",
      )
      await setup.renderOnce()
      const failure = setup.captureCharFrame()
      auditSecretFreeFrame(failure, secrets, "universal-error")
      const renderedCode = problemCode === "invalid-universal-provider"
        ? "invalid-universal-"
        : "compatibility"
      expect(failure.includes(renderedCode)).toBeTrue()
      expect(failure.includes("Universal Provider action failed")).toBeTrue()
      auditSecretFreeActions(catalog.actions, secrets, "universal-action-final")
    } finally {
      setup.renderer.destroy()
    }
  },
)

test("Universal Provider catalog keeps English and Chinese parity at every supported terminal size", async () => {
  for (const locale of ["en", "zh-CN"] as const) {
    const catalog = new MemoryUniversalProviderSession(universalCatalog())
    const session = new MemoryTargetSession(view())
    const setup = await testRender(() => <App session={session} universalSession={catalog} locale={locale} />, {
      width: 80, height: 30, useThread: false, kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/universal-providers")
      setup.mockInput.pressEnter()
      const initial = await setup.waitForFrame((frame) => frame.includes("Shared Frontier"))
      expect(initial).toContain(locale === "en" ? "Universal Providers" : "通用 Provider")
      expect(initial).toContain(locale === "en" ? "Pending" : "待同步")
      for (const [width, height] of [[1, 1], [2, 2], [20, 5], [40, 10], [80, 24], [121, 30]] as const) {
        setup.resize(width, height)
        await setup.renderOnce()
        expect(() => setup.captureCharFrame()).not.toThrow()
      }
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("Universal Provider edit, detached duplicate, reference blocker, and delete stay in one overlay stack", async () => {
  const original = universalCatalog().providers[0]!
  let current: UniversalProviderCatalogView = {
    ...universalCatalog(),
    providers: [{
      ...original,
      targets: original.targets.map((target) => target.target === "codex"
        ? { ...target, activeReferences: ["current" as const] }
        : target),
    }],
  }
  let blockDelete = true
  const detachedId = "00000000-0000-4000-8000-000000000079"
  const catalog = new MemoryUniversalProviderSession(current, async (action) => {
    if (action.kind === "update-universal-provider") {
      current = {
        ...current,
        revision: 2,
        viewSequence: 2,
        providers: current.providers.map((provider) => provider.id === action.providerId
          ? { ...provider, providerRevision: 2, name: action.name, baseUrl: action.baseUrl, targets: provider.targets.map((target) => ({
            ...target,
            ...action.targets.find((candidate) => candidate.target === target.target),
          })) }
          : provider),
      }
    } else if (action.kind === "duplicate-universal-provider") {
      current = {
        ...current,
        revision: 3,
        viewSequence: 3,
        providers: [...current.providers, {
          ...current.providers[0]!,
          id: detachedId,
          position: 1,
          providerRevision: 1,
          name: action.name,
          baseUrl: action.baseUrl,
          credential: "missing",
          provenance: null,
          targets: current.providers[0]!.targets.map((target) => ({
            ...target,
            ...action.targets.find((candidate) => candidate.target === target.target),
            generatedProviderId: null,
            synchronization: "pending" as const,
            activeReferences: [],
          })),
        }],
      }
    } else if (action.kind === "delete-universal-provider") {
      if (blockDelete) throw { code: "generated-provider-referenced", message: backendSecret }
      current = {
        ...current,
        revision: current.revision + 1,
        viewSequence: current.viewSequence + 1,
        providers: current.providers.filter((provider) => provider.id !== action.providerId),
      }
    }
    return { status: "applied", view: current }
  })
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} universalSession={catalog} />, {
    width: 80, height: 30, useThread: false, kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/universal-providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Codex CLI references"))

    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Edit Universal Provider"))
    await setup.mockInput.typeText(" Updated")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => catalog.actions.length === 1)
    expect(catalog.actions[0]).toMatchObject({ kind: "update-universal-provider", name: "Shared Frontier Updated" })
    await setup.waitForFrame((frame) => frame.includes("Shared Frontier Updated"))

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    await setup.waitForFrame((frame) => frame.includes("Duplicate as detached Universal Provider"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => catalog.actions.length === 2)
    expect(catalog.actions[1]).toMatchObject({
      kind: "duplicate-universal-provider",
      name: "Shared Frontier Updated Copy",
      credential: { kind: "without" },
    })
    await setup.waitForFrame((frame) => frame.includes("Shared Frontier Updated Copy"))

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Universal Provider?"))
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => catalog.actions.length === 3)
    await setup.renderOnce()
    const blocked = setup.captureCharFrame()
    auditSecretFreeFrame(blocked, [backendSecret], "universal-reference-blocker")
    expect(blocked.includes("generated-provider-")).toBeTrue()
  } finally {
    setup.renderer.destroy()
  }
})

test("pending Universal Provider action is nondismissible and suppresses duplicate dispatch", async () => {
  const pending = deferred<UniversalProviderOutcome>()
  const initial = universalCatalog()
  const catalog = new MemoryUniversalProviderSession(initial, async () => await pending.promise)
  const session = new MemoryTargetSession(view())
  const setup = await testRender(() => <App session={session} universalSession={catalog} />, {
    width: 80, height: 30, useThread: false, kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/universal-providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Shared Frontier"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Edit Universal Provider"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => catalog.actions.length === 1)
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Edit Universal Provider")

    setup.mockInput.pressEscape()
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Edit Universal Provider")
    expect(catalog.actions).toHaveLength(1)

    pending.resolve({
      status: "applied",
      view: { ...initial, revision: 2, viewSequence: 2 },
    })
    await setup.waitForFrame((frame) => frame.includes("Shared Frontier") && frame.includes("C create"))
    expect(catalog.actions).toHaveLength(1)
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude Provider editor selects Bearer authentication and dispatches only its Target session", async () => {
  const codex = new MemoryTargetSession(view())
  const claudeInitial = view({
    target: "claude",
    providerPresets: [{
      key: "anthropic-api-messages",
      baseUrl: "https://api.anthropic.com/v1",
      model: "",
      protocol: "anthropic-messages",
      authentication: "anthropic-api-key",
    }],
  })
  const claude = new MemoryTargetSession(claudeInitial, async () => ({ status: "applied", view: claudeInitial }))
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API (Messages)"))
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Anthropic API key"))
    await setup.renderOnce()

    await setup.mockInput.typeText("Claude Bearer")
    setup.mockInput.pressTab()
    await setup.renderOnce()
    setup.mockInput.pressTab()
    await setup.renderOnce()
    await setup.mockInput.typeText("claude-test")
    setup.mockInput.pressTab()
    await setup.renderOnce()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("h")
    setup.mockInput.pressTab()
    await setup.renderOnce()
    await setup.mockInput.typeText(credentialSecret)

    const safeFrame = setup.captureCharFrame()
    expect(safeFrame).not.toContain(credentialSecret)
    expect(safeFrame).toContain("Anthropic Bearer token")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.actions.length === 1)
    expect(claude.actions).toEqual([{
      kind: "create-provider",
      name: "Claude Bearer",
      baseUrl: "https://api.anthropic.com/v1",
      model: "claude-test",
      authentication: "anthropic-bearer",
      credential: { kind: "replace", valuePresent: true },
      presetKey: "anthropic-api-messages",
    }])
    expect(codex.actions).toEqual([])
    expect(JSON.stringify(claude.actions)).not.toContain(credentialSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude Subscription Bridge preset is fixed credentialless and discloses its exact risk boundary", async () => {
  const controlledSources = {
    credential: credentialSecret,
    account: bridgeAccountSecret,
    config: configSecret,
    backend: new Error(backendSecret),
    settings: { raw: settingsSecret },
  }
  assertControlledSecretSource(controlledSources, bridgeSecrets, "subscription-bridge-workflow-source")
  const claude = new MemoryTargetSession(view({
    target: "claude",
    providerPresets: [{
      key: "codex-subscription-bridge",
      baseUrl: "https://chatgpt.com/backend-api/codex",
      model: "",
      protocol: "anthropic-messages",
      authentication: "codex-subscription",
      controlledSources,
    } as TargetView["providerPresets"][number]],
  }))
  const setup = await testRender(() => <App session={claude} />, {
    width: 100,
    height: 38,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    const sources = await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("Codex Subscription Bridge"),
      bridgeSecrets,
      "subscription-bridge-source-picker",
    )

    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    const editor = await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("Undocumented ChatGPT Codex interface"),
      bridgeSecrets,
      "subscription-bridge-editor",
    )
    expect(editor).toContain("https://chatgpt.com/backend-api/codex")
    expect(editor).toContain("Subscription Account authentication · no Provider credential")
    expect(editor).toContain("Takeover required · Subscription Account binding required")
    expect(editor).toContain("shared subscription quota")
    expect(editor).toContain("May stop working without notice")
    expect(editor).toContain("applicable account and subscription terms")
    expect(editor).toContain("Not officially supported or endorsed")
    expect(editor).toContain("Tested models · gpt-5.6 and gpt-5.6-luna · text and tools")
    expect(editor).toContain("Compatibility Deviations")
    expect(editor).toContain("count_tokens")
    expect(editor).not.toContain("API credential")

    await setup.mockInput.typeText("Subscription Route")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("gpt-5.6")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("h")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("r")
    setup.mockInput.pressEnter()
    await waitForSecretFreeCondition(
      setup,
      () => claude.actions.length === 1,
      () => auditSecretFreeActions(claude.actions, bridgeSecrets, "subscription-bridge-save"),
      "secret-scan-failed:subscription-bridge-save-action",
      "subscription-bridge-save",
    )
    expect(claude.actions).toEqual([{
      kind: "create-provider",
      name: "Subscription Route",
      baseUrl: "https://chatgpt.com/backend-api/codex",
      model: "gpt-5.6",
      authentication: "codex-subscription",
      credential: { kind: "remove" },
      presetKey: "codex-subscription-bridge",
    }])
  } finally {
    setup.renderer.destroy()
  }
})

test("Subscription Bridge workflow scanner fails with one fixed diagnostic for every controlled source", () => {
  const controlledSources = {
    credential: credentialSecret,
    account: bridgeAccountSecret,
    config: configSecret,
    backend: new Error(backendSecret),
    settings: { raw: settingsSecret },
  }
  assertControlledSecretSource(controlledSources, bridgeSecrets, "subscription-bridge-scanner-source")
  auditSecretFreeFrame("safe subscription bridge frame", bridgeSecrets, "subscription-bridge-controlled")
  for (const secret of bridgeSecrets) {
    let diagnostic = ""
    try {
      auditSecretFreeFrame(`safe prefix ${secret} safe suffix`, bridgeSecrets, "subscription-bridge-controlled")
    } catch (error) {
      diagnostic = error instanceof Error ? error.message : ""
    }
    expect(diagnostic).toBe("secret-scan-failed:subscription-bridge-controlled-frame")
    for (const forbidden of bridgeSecrets) expect(diagnostic).not.toContain(forbidden)
  }
})

test("Claude owns edit reorder duplicate and delete actions without touching Codex", async () => {
  const first = provider({
    id: "00000000-0000-4000-8000-000000000071",
    name: "Claude First",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
    routingRequirement: "takeover-required",
  })
  const second = provider({
    id: "00000000-0000-4000-8000-000000000072",
    position: 1,
    name: "Claude Second",
    protocol: "anthropic-messages",
    authentication: "anthropic-bearer",
    routingRequirement: "takeover-required",
  })
  let nextView = view({ target: "claude", providers: [first, second] })
  const codex = new MemoryTargetSession(view())
  const claude = new MemoryTargetSession(nextView, async (action) => {
    const providers = [...nextView.providers]
    if (action.kind === "reorder-providers") {
      nextView = view({
        target: "claude",
        viewSequence: nextView.viewSequence + 1,
        providers: action.providerIds.map((id, position) => ({ ...providers.find((item) => item.id === id)!, position })),
      })
    } else if (action.kind === "duplicate-provider") {
      const duplicate = provider({
        id: "00000000-0000-4000-8000-000000000073",
        position: providers.length,
        name: action.name,
        baseUrl: action.baseUrl,
        model: action.model,
        protocol: "anthropic-messages",
        authentication: "anthropic-api-key",
        routingRequirement: "takeover-required",
        credential: "missing",
      })
      nextView = view({ target: "claude", viewSequence: nextView.viewSequence + 1, providers: [...providers, duplicate] })
    } else if (action.kind === "update-provider") {
      nextView = view({
        target: "claude",
        viewSequence: nextView.viewSequence + 1,
        providers: providers.map((item) => item.id === action.providerId ? { ...item, name: action.name, providerRevision: item.providerRevision + 1 } : item),
      })
    } else if (action.kind === "delete-provider") {
      nextView = view({
        target: "claude",
        viewSequence: nextView.viewSequence + 1,
        providers: providers.filter((item) => item.id !== action.providerId).map((item, position) => ({ ...item, position })),
      })
    } else {
      throw new Error(`unexpected ${action.kind}`)
    }
    return { status: "applied", view: nextView }
  })
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude First"))

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("n")
    await setup.waitFor(() => claude.actions.length === 1)
    expect(claude.actions[0]).toEqual({ kind: "reorder-providers", providerIds: [second.id, first.id] })

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    await setup.waitForFrame((frame) => frame.includes("Reuse Credential Reference?"))
    setup.mockInput.pressKey("n")
    await setup.waitForFrame((frame) => frame.includes("Claude First Copy"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.actions.length === 2)
    expect(claude.actions[1]).toMatchObject({ kind: "duplicate-provider", sourceProviderId: first.id, credential: { kind: "without" } })

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude Second"))
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Edit Provider"))
    await setup.mockInput.typeText(" Updated")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.actions.length === 3)
    expect(claude.actions[2]).toMatchObject({ kind: "update-provider", providerId: first.id, name: "Claude First Updated" })

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Claude First Updated"))
    const pickerFocus = setup.renderer.currentFocusedRenderable
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Claude First Updated") && !frame.includes("Delete Provider?"))
    expect(setup.renderer.currentFocusedRenderable).toBe(pickerFocus)
    expect(pickerFocus?.isDestroyed).toBeFalse()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await setup.waitForFrame((frame) => frame.includes("Delete Provider?"))
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => claude.actions.length === 4)
    expect(claude.actions[3]).toMatchObject({ kind: "delete-provider", providerId: first.id })
    expect(codex.actions).toEqual([])
    expect(JSON.stringify(claude.actions)).not.toContain(credentialSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("duplicate credential confirmation keeps cancel without and reuse distinct and replacement wins", async () => {
  const source = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Source Provider",
    credential: "present",
  })
  const tail = provider({
    id: "00000000-0000-4000-8000-000000000012",
    position: 1,
    name: "Tail Provider",
  })
  const createdIds = [
    "00000000-0000-4000-8000-000000000021",
    "00000000-0000-4000-8000-000000000022",
    "00000000-0000-4000-8000-000000000023",
  ]
  let sawDuplicateReplacementSecret = false
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(view({ providers: [source, tail] }), async (action): Promise<ActionOutcome> => {
    if (action.kind !== "duplicate-provider") throw new Error("unexpected action")
    if (action.credential.kind === "replace") {
      sawDuplicateReplacementSecret = action.credential.value === credentialSecret
    }
    const duplicate = provider({
      id: createdIds[session.actions.length - 1]!,
      position: 1,
      name: action.name,
      baseUrl: action.baseUrl,
      model: action.model,
      credential: action.credential.kind === "without" ? "missing" : "present",
    })
    return {
      status: "applied",
      view: view({
        viewSequence: session.actions.length + 1,
        providers: [source, duplicate, { ...tail, position: 2 }],
      }),
    }
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })

  const openPicker = async () => {
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·") && frame.includes("Source Provider"))
  }
  const requestDuplicate = async () => {
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    return await setup.waitForFrame((frame) => frame.includes("Credential Reference"))
  }

  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await openPicker()

    const confirmation = await requestDuplicate()
    expect(confirmation).toContain("Source Provider")
    expect(confirmation).toContain("Credential Reference present")
    expect(confirmation).not.toContain(credentialUuid)
    expect(confirmation).not.toContain(credentialSecret)
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Providers") && !frame.includes("Reuse Credential Reference?"))
    expect(session.actions).toEqual([])

    await requestDuplicate()
    setup.mockInput.pressKey("n")
    const withoutEditor = await setup.waitForFrame((frame) => frame.includes("Source Provider Copy"))
    expect(withoutEditor).toContain("https://first.example/v1")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toEqual({
      kind: "duplicate-provider",
      sourceProviderId: source.id,
      sourceProviderRevision: source.providerRevision,
      name: "Source Provider Copy",
      baseUrl: source.baseUrl,
      model: source.model,
      credential: { kind: "without" },
    })

    await openPicker()
    await requestDuplicate()
    setup.mockInput.pressKey("y")
    await setup.waitForFrame((frame) => frame.includes("Source Provider Copy"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 2)
    expect(session.actions[1]).toMatchObject({
      kind: "duplicate-provider",
      sourceProviderId: source.id,
      sourceProviderRevision: source.providerRevision,
      credential: { kind: "reuse-source" },
    })

    await openPicker()
    await requestDuplicate()
    setup.mockInput.pressKey("y")
    await setup.waitForFrame((frame) => frame.includes("Source Provider Copy"))
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(credentialSecret)
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 3)
    expect(session.actions[2]).toMatchObject({
      kind: "duplicate-provider",
      credential: { kind: "replace", valuePresent: true },
    })
    expect(sawDuplicateReplacementSecret).toBeTrue()
    const persistedReplacementSecret = JSON.stringify(session.actions).includes(credentialSecret)
    expect(persistedReplacementSecret).toBeFalse()
    expect(setup.captureCharFrame()).not.toContain(credentialSecret)

    await openPicker()
    const rows = setup.captureCharFrame()
    expectInOrder(rows, ["Source Provider ·", "Source Provider Copy ·", "Tail Provider ·"])
  } finally {
    setup.renderer.destroy()
  }
})

test("duplicate without a source credential opens directly with an explicit without intent", async () => {
  const source = provider({ credential: "missing" })
  const session = new MemoryTargetSession(view({ providers: [source] }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·") && frame.includes("First Provider"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("c")
    const editor = await setup.waitForFrame((frame) => frame.includes("First Provider Copy"))
    expect(editor).not.toContain("Reuse Credential Reference?")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toMatchObject({
      kind: "duplicate-provider",
      credential: { kind: "without" },
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("Provider Direct Activation defaults to Current and dispatches the selected row exact identity", async () => {
  const first = provider({ id: "00000000-0000-4000-8000-000000000011", name: "First Provider" })
  const second = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Second Provider" })
  for (const testCase of [
    { currentProviderId: second.id, moveDown: false, expectedId: second.id },
    { currentProviderId: first.id, moveDown: true, expectedId: second.id },
  ]) {
    const initial = view({ providers: [first, second], currentProviderId: testCase.currentProviderId })
    const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: initial }))
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes("First Provider") && frame.includes("Second Provider"))
      if (testCase.moveDown) setup.mockInput.pressKey("down")
      setup.mockInput.pressKey("x", { ctrl: true })
      setup.mockInput.pressKey("a")
      await setup.waitFor(() => session.actions.length === 1)

      expect(session.actions).toEqual([{
        kind: "activate-provider",
        providerId: testCase.expectedId,
        mode: "direct",
      }])
      expect(setup.captureCharFrame()).not.toContain(credentialSecret)
      expect(JSON.stringify(session.actions)).not.toContain(credentialSecret)
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("Claude picker binds Direct to leader a and Takeover to leader o without duplicate dispatch", async () => {
  const controlledSources = controlledClaudeDirectSources()
  assertControlledSecretSource(controlledSources, claudeDirectSecrets, "claude-picker-binding-source")
  const first = provider({
    id: "00000000-0000-4000-8000-000000000071",
    name: "Claude First Provider",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
  })
  const selected = provider({
    id: "00000000-0000-4000-8000-000000000072",
    position: 1,
    name: "Claude Selected Provider",
    protocol: "anthropic-messages",
    authentication: "anthropic-bearer",
  })
  const initial = view({ target: "claude", providers: [first, selected], currentProviderId: first.id })
  const codex = new MemoryTargetSession(view())
  const claude = new MemoryTargetSession(initial, async () => {
    assertControlledSecretSource(controlledSources, claudeDirectSecrets, "claude-picker-binding-session")
    return { status: "applied", view: initial }
  })
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    const picker = await waitForClaudeDirectFrame(setup, (frame) => frame.includes("Claude Selected Provider"), "claude-picker-binding-direct")
    expect(picker).toContain("ctrl+x a direct")
    expect(picker).toContain("ctrl+x o takeover")
    setup.mockInput.pressKey("down")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await waitForClaudeDirectActions(setup, claude, 1, "claude-picker-binding-direct-action")
    assertClaudeDirectActions(claude, [{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "direct",
    }], "claude-picker-binding-direct-action")

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await waitForClaudeDirectFrame(setup, (frame) => frame.includes("Claude Selected Provider"), "claude-picker-binding-takeover")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("o")
    await waitForClaudeDirectActions(setup, claude, 2, "claude-picker-binding-takeover-action")
    assertClaudeDirectActions(claude, [
      { kind: "activate-provider", providerId: selected.id, mode: "direct" },
      { kind: "activate-provider", providerId: selected.id, mode: "takeover" },
    ], "claude-picker-binding-takeover-action")
    expect(codex.actions).toEqual([])
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude picker renders the exact Direct and Takeover bindings in English and Chinese", async () => {
  const claudeProvider = provider({
    id: "00000000-0000-4000-8000-000000000077",
    name: "Localized Claude Provider",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
  })
  for (const testCase of [
    { locale: "en" as const, direct: "ctrl+x a direct", takeover: "ctrl+x o takeover" },
    { locale: "zh-CN" as const, direct: "ctrl+x a 直接激活", takeover: "ctrl+x o Takeover" },
  ]) {
    const codex = new MemoryTargetSession(view())
    const claude = new MemoryTargetSession(view({ target: "claude", providers: [claudeProvider] }))
    const setup = await testRender(() => <App sessions={{ codex, claude }} locale={testCase.locale} />, {
      width: 121,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("2")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      const picker = await waitForClaudeDirectFrame(
        setup,
        (frame) => frame.includes("Localized Claude Provider"),
        `claude-picker-help-${testCase.locale}`,
      )
      expect(picker).toContain(testCase.direct)
      expect(picker).toContain(testCase.takeover)
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("Claude Direct uses the same projected and authoritative Takeover-required confirmation", async () => {
  const controlledSources = controlledClaudeDirectSources()
  assertControlledSecretSource(controlledSources, claudeDirectSecrets, "claude-takeover-confirm-source")
  const selected = provider({
    id: "00000000-0000-4000-8000-000000000073",
    name: "Claude Subscription Target",
    protocol: "anthropic-messages",
    authentication: "anthropic-bearer",
    routingRequirement: "takeover-required",
  })
  const projected = view({ target: "claude", providers: [selected], currentProviderId: selected.id })
  const directCompatible = view({
    ...projected,
    managementRevision: 2,
    viewSequence: 2,
    providers: [{ ...selected, routingRequirement: "direct-compatible" }],
  })
  const authoritative = view({
    ...projected,
    managementRevision: 3,
    viewSequence: 3,
  })
  const codex = new MemoryTargetSession(view())
  let claude!: MemoryTargetSession
  claude = new MemoryTargetSession(projected, async (action) => {
    assertControlledSecretSource(controlledSources, claudeDirectSecrets, "claude-takeover-confirm-session")
    if (action.kind === "activate-provider" && action.mode === "direct") {
      claude.setView(authoritative)
      throw {
        code: "takeover-required",
        message: backendSecret,
        credential: credentialSecret,
        settings: settingsSecret,
      }
    }
    return { status: "applied", view: authoritative }
  })
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await waitForClaudeDirectFrame(setup, (frame) => frame.includes(selected.name), "claude-takeover-confirm-picker")
    const pickerFocus = setup.renderer.currentFocusedRenderable as InputRenderable

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    const projectedConfirm = await waitForClaudeDirectFrame(
      setup,
      (frame) => frame.includes("Enable Target Takeover?"),
      "claude-takeover-confirm-projected",
    )
    expect(projectedConfirm).toContain("Enable Takeover")
    expect(projectedConfirm).toContain("Cancel")
    expect(projectedConfirm).not.toContain("Direct Activation applied")
    assertClaudeDirectActions(claude, [], "claude-takeover-confirm-projected-action")
    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    expect(setup.renderer.currentFocusedRenderable).toBe(pickerFocus)

    claude.pushView(directCompatible)
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await waitForClaudeDirectActions(setup, claude, 1, "claude-takeover-confirm-authoritative-action")
    assertControlledSecretSource(claude.lastError, claudeDirectSecrets, "claude-takeover-confirm-error-source")
    const authoritativeConfirm = await waitForClaudeDirectFrame(
      setup,
      (frame) => frame.includes("Enable Target Takeover?"),
      "claude-takeover-confirm-authoritative",
    )
    expect(authoritativeConfirm).not.toContain(backendSecret)
    expect(authoritativeConfirm).not.toContain(credentialSecret)
    assertClaudeDirectActions(claude, [{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "direct",
    }], "claude-takeover-confirm-authoritative-action")

    setup.mockInput.pressEnter()
    setup.mockInput.pressEnter()
    await waitForClaudeDirectActions(setup, claude, 2, "claude-takeover-confirm-takeover-action")
    assertClaudeDirectActions(claude, [
      { kind: "activate-provider", providerId: selected.id, mode: "direct" },
      { kind: "activate-provider", providerId: selected.id, mode: "takeover" },
    ], "claude-takeover-confirm-takeover-action")
    expect(codex.actions).toEqual([])
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude picker Direct gates duplicate input and installs one restart-guided success", async () => {
  const controlledSources = controlledClaudeDirectSources()
  assertControlledSecretSource(controlledSources, claudeDirectSecrets, "claude-picker-pending-source")
  const selected = provider({
    id: "00000000-0000-4000-8000-000000000074",
    name: "Pending Claude Direct",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
  })
  const initial = view({ target: "claude", providers: [selected] })
  const applied = view({
    ...initial,
    managementRevision: 2,
    viewSequence: 2,
    mode: "direct",
    currentProviderId: selected.id,
    managedConfiguration: { state: "managed", path: "/tmp/home/.claude/settings.json", restartRequired: true },
    activatedSnapshot: {
      id: "00000000-0000-4000-8000-000000000075",
      providerId: selected.id,
      model: selected.model,
      protocol: "anthropic-messages",
      authentication: "anthropic-api-key",
      epoch: "00000000-0000-4000-8000-000000000076",
    },
  })
  const pending = deferred<ActionOutcome>()
  const codex = new MemoryTargetSession(view())
  const claude = new MemoryTargetSession(initial, async () => {
    assertControlledSecretSource(controlledSources, claudeDirectSecrets, "claude-picker-pending-session")
    return await pending.promise
  })
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, {
    width: 80,
    height: 24,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await waitForClaudeDirectFrame(setup, (frame) => frame.includes(selected.name), "claude-picker-pending-open")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await waitForClaudeDirectActions(setup, claude, 1, "claude-picker-pending-action")
    const pendingFrame = await waitForClaudeDirectFrame(
      setup,
      (frame) => frame.includes("Applying Direct Activation…"),
      "claude-picker-pending-frame",
    )
    expect(pendingFrame).toContain("Providers")
    expect(pendingFrame).not.toContain(credentialSecret)

    setup.mockInput.pressKey("p", { ctrl: true })
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    assertSecretFreeStructured("action", claude.actions, claudeDirectSecrets, "claude-picker-pending-gated", (safeActions) => {
      expect(safeActions).toHaveLength(1)
    })
    const guardedFrame = await waitForClaudeDirectFrame(
      setup,
      (frame) => frame.includes("Applying Direct Activation…") && !frame.includes("Search commands"),
      "claude-picker-pending-gated-frame",
    )
    expect(guardedFrame).not.toContain("Search commands")

    pending.resolve({ status: "applied", view: applied })
    const completed = await waitForClaudeDirectFrame(
      setup,
      (frame) => frame.includes("Direct Activation applied: Pending Claude Direct")
        && frame.includes("Restart Claude Code to use the managed configuration."),
      "claude-picker-pending-complete",
    )
    expect(completed).toContain("Mode       Direct")
    expect(completed.match(/Direct Activation applied:/g)).toHaveLength(1)
    expect(completed).not.toContain("Providers")
    expect(completed).not.toContain(credentialSecret)
    assertClaudeDirectActions(claude, [{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "direct",
    }], "claude-picker-pending-final-action")
    const activityLines = completed.split("\n").filter((line) => line.includes("Direct Activation applied:"))
    assertSecretFreeStructured("activity", activityLines, claudeDirectSecrets, "claude-picker-pending-activity", (safeActivities) => {
      expect(safeActivities).toHaveLength(1)
    })
    assertSecretFreeStructured("view", claude.get(), claudeDirectSecrets, "claude-picker-pending-view", (safeView) => {
      expect(safeView.mode).toBe("direct")
      expect(safeView.currentProviderId).toBe(selected.id)
    })
    expect(codex.actions).toEqual([])
  } finally {
    pending.resolve({ status: "applied", view: applied })
    setup.renderer.destroy()
  }
})

for (const action of ["activate", "edit"] as const) {
  test(`authoritative removal synchronizes the visible Provider fallback before ${action}`, async () => {
    const fallback = provider({ id: "00000000-0000-4000-8000-000000000011", name: "Fallback Provider" })
    const removed = provider({ id: "00000000-0000-4000-8000-000000000012", position: 1, name: "Removed Provider" })
    const initial = view({ providers: [fallback, removed], currentProviderId: removed.id })
    const authoritative = view({
      ...initial,
      managementRevision: 2,
      viewSequence: 2,
      providers: [fallback],
      currentProviderId: null,
    })
    const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: authoritative }))
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes("Removed Provider"))

      session.pushView(authoritative)
      const fallbackFrame = await setup.waitForFrame((frame) => frame.includes("Fallback Provider") && !frame.includes("Removed Provider"))
      expect(fallbackFrame).toContain("Fallback Provider")

      if (action === "activate") {
        setup.mockInput.pressKey("x", { ctrl: true })
        setup.mockInput.pressKey("a")
        for (let pass = 0; pass < 4; pass++) await Promise.resolve()
        expect(session.actions).toEqual([{
          kind: "activate-provider",
          providerId: "00000000-0000-4000-8000-000000000011",
          mode: "direct",
        }])
      } else {
        setup.mockInput.pressEnter()
        const editor = await setup.waitForFrame((frame) => frame.includes("Edit Provider"))
        expect(editor).toContain("Fallback Provider")
      }
    } finally {
      setup.renderer.destroy()
    }
  })
}

test("Provider Direct Activation disables picker actions while pending then closes onto restart guidance", async () => {
  const selected = provider({ id: "00000000-0000-4000-8000-000000000011", name: "Pending Direct Provider" })
  const initial = view({ providers: [selected] })
  const applied = view({
    ...initial,
    managementRevision: 2,
    viewSequence: 2,
    mode: "direct",
    currentProviderId: selected.id,
    managedConfiguration: { state: "managed", path: "/tmp/home/.codex/config.toml", restartRequired: true },
    activatedSnapshot: {
      id: "00000000-0000-4000-8000-000000000021",
      providerId: selected.id,
      model: selected.model,
      protocol: "openai-responses",
      authentication: "openai-bearer",
      epoch: "00000000-0000-4000-8000-000000000022",
    },
  })
  const pending = deferred<ActionOutcome>()
  const session = new MemoryTargetSession(initial, async () => await pending.promise)
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Pending Direct Provider"))
    setup.mockInput.pressEscape()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitFor(() => session.actions.length === 1)
    const pendingFrame = await setup.waitForFrame((frame) => frame.includes("Applying Direct Activation…"))
    expect(pendingFrame).toContain("Providers")

    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    setup.mockInput.pressKey("p", { ctrl: true })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const guardedFrame = setup.captureCharFrame()
    expect(guardedFrame).toContain("Applying Direct Activation…")
    expect(guardedFrame).toContain("Providers")
    expect(guardedFrame).not.toContain("Search commands")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(setup.captureCharFrame()).toContain("Applying Direct Activation…")
    expect(setup.captureCharFrame()).not.toContain("Delete Provider?")
    expect(setup.captureCharFrame()).not.toContain("Edit Provider")
    expect(session.actions).toHaveLength(1)

    pending.resolve({ status: "applied", view: applied })
    const completed = await setup.waitForFrame((frame) =>
      frame.includes("Direct Activation applied: Pending Direct Provider")
      && frame.includes("Restart Codex to use the managed configuration.")
    )
    expect(completed).toContain("Mode       Direct")
    expect(completed).not.toContain("Providers")
    expect(completed).not.toContain(credentialSecret)
    expect(session.actions).toEqual([{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "direct",
    }])
  } finally {
    pending.resolve({ status: "applied", view: applied })
    setup.renderer.destroy()
  }
})

test("picker Direct failures close onto localized stable guidance without raw backend text", async () => {
  const cases = [
    {
      selected: provider({
        id: "00000000-0000-4000-8000-000000000011",
        name: "Incomplete Picker Provider",
        credential: "missing",
        completeness: "incomplete",
        missingFields: ["credential"],
      }),
      expected: "Complete the required Provider fields and retry.",
      backend: false,
    },
    {
      selected: provider({
        id: "00000000-0000-4000-8000-000000000012",
        name: "Active Takeover Provider",
      }),
      expected: "Disable Target Takeover before using Direct Activation.",
      backend: true,
    },
  ] as const

  for (const testCase of cases) {
    const initial = view({ providers: [testCase.selected] })
    let session!: MemoryTargetSession
    session = new MemoryTargetSession(initial, async () => {
      session.setView(view({
        ...initial,
        managementRevision: 2,
        viewSequence: 2,
        providers: [{ ...testCase.selected, name: "Authoritative Active Provider" }],
      }))
      throw { code: "takeover-active", message: "backend-picker-secret-must-not-render" }
    })
    const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      await setup.waitForFrame((frame) => frame.includes(testCase.selected.name))
      setup.mockInput.pressKey("x", { ctrl: true })
      setup.mockInput.pressKey("a")
      const failure = await setup.waitForFrame((frame) => frame.includes(testCase.expected))

      expect(failure).not.toContain("Providers")
      expect(failure).not.toContain("backend-picker-secret-must-not-render")
      expect(failure).not.toContain(credentialSecret)
      expect(session.actions).toHaveLength(testCase.backend ? 1 : 0)
    } finally {
      setup.renderer.destroy()
    }
  }
})

test("a Takeover-required Provider offers only Takeover or cancel with scoped single dispatch", async () => {
  const selected = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Subscription Target",
    routingRequirement: "takeover-required",
  })
  const initial = view({ providers: [selected], currentProviderId: selected.id })
  const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: initial }))
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Subscription Target"))
    const pickerFocus = setup.renderer.currentFocusedRenderable as InputRenderable

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Enable Target Takeover?"))
    expect(confirmation).toContain("Subscription Target requires Target Takeover for")
    expect(confirmation).toContain("activation.")
    expect(confirmation).toContain("Enable Takeover")
    expect(confirmation).toContain("Cancel")
    expect(confirmation).not.toContain(credentialSecret)
    expect(session.actions).toEqual([])

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    expect(session.actions).toEqual([])
    expect(setup.captureCharFrame()).toContain("Providers")
    const restoredFocus = setup.renderer.currentFocusedRenderable as InputRenderable
    expect(restoredFocus).toBe(pickerFocus)
    expect(pickerFocus.isDestroyed).toBeFalse()
    expect(restoredFocus.isDestroyed).toBeFalse()
    expect(restoredFocus.placeholder).toBe("Navigate Providers")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitForFrame((frame) => frame.includes("Enable Target Takeover?"))
    setup.mockInput.pressKey("y")
    setup.mockInput.pressKey("y")
    await setup.waitFor(() => session.actions.length === 1)
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    setup.mockInput.pressKey("y")
    await setup.renderOnce()

    expect(session.actions).toEqual([{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "takeover",
    }])
    expect(JSON.stringify(session.actions)).not.toContain(credentialSecret)
  } finally {
    setup.renderer.destroy()
  }
})

test("authoritative takeover-required preserves the pending picker and restores it after cancel", async () => {
  const selected = provider({
    id: "00000000-0000-4000-8000-000000000011",
    name: "Changed Requirement Provider",
    routingRequirement: "direct-compatible",
  })
  const authoritative = view({
    managementRevision: 2,
    viewSequence: 2,
    providers: [{ ...selected, routingRequirement: "takeover-required" }],
    currentProviderId: selected.id,
  })
  const pendingDirect = deferred<ActionOutcome>()
  let session!: MemoryTargetSession
  session = new MemoryTargetSession(view({ providers: [selected] }), async () => await pendingDirect.promise)
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Changed Requirement Provider"))
    const pickerFocus = setup.renderer.currentFocusedRenderable as InputRenderable
    setup.mockInput.pressEscape()
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    await setup.waitFor(() => session.actions.length === 1)
    await setup.waitForFrame((frame) => frame.includes("Applying Direct Activation…"))

    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    setup.mockInput.pressKey("p", { ctrl: true })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    session.setView(authoritative)
    pendingDirect.reject({ code: "takeover-required", message: "backend-takeover-secret-must-not-render" })
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Enable Target Takeover?"))

    expect(session.actions).toEqual([{
      kind: "activate-provider",
      providerId: selected.id,
      mode: "direct",
    }])
    expect(confirmation).not.toContain("backend-takeover-secret-must-not-render")
    expect(confirmation).not.toContain("Action failed")
    expect(confirmation).not.toContain(credentialSecret)

    setup.mockInput.pressEscape()
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const restored = setup.captureCharFrame()
    expect(restored).toContain("Providers")
    expect(restored).not.toContain("Commands")
    const restoredFocus = setup.renderer.currentFocusedRenderable as InputRenderable
    expect(restoredFocus).toBe(pickerFocus)
    expect(pickerFocus.isDestroyed).toBeFalse()
    expect(restoredFocus.isDestroyed).toBeFalse()
    expect(restoredFocus.placeholder).toBe("Navigate Providers")
    expect(session.actions).toHaveLength(1)
    expect(JSON.stringify(session.actions)).not.toContain("backend-takeover-secret-must-not-render")
  } finally {
    pendingDirect.reject({ code: "takeover-required" })
    setup.renderer.destroy()
  }
})

test("compatibility renderer scanner rejects a raw ControlProblem message and extension with a fixed diagnostic", async () => {
  const source = controlledCompatibilityProblem("compatibility-acknowledgement-required")
  assertControlledSecretSource(source, compatibilitySecrets, "compatibility-renderer-mutation-source")
  const setup = await testRender(() => <box flexDirection="column">
    <text>{source.message}</text>
    <text>{String((source as unknown as { configDiagnostic: string }).configDiagnostic)}</text>
  </box>, { width: 80, height: 24, useThread: false })
  let diagnostic = ""
  try {
    await waitForSecretFreeFrame(
      setup,
      () => true,
      compatibilitySecrets,
      "compatibility-renderer-mutation",
    )
  } catch (error) {
    diagnostic = error instanceof Error ? error.message : String(error)
  } finally {
    setup.renderer.destroy()
  }
  expect(diagnostic).toBe("secret-scan-failed:compatibility-renderer-mutation-frame")
  for (const secret of compatibilitySecrets) expect(diagnostic).not.toContain(secret)
})

test.each(["codex", "claude"] as const)(
  "First unmanaged unknown-compatible %s uses compatibility-only preview and public exact acknowledgement",
  async (target) => {
    const initial = view({
      target,
      managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
      problems: [controlledCompatibilityProblem("compatibility-acknowledgement-required")],
    })
    const acknowledged = view({
      ...initial,
      viewSequence: initial.viewSequence + 1,
      problems: [],
    })
    const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: acknowledged }))
    session.compatibilityProbeHandler = async () => ({
      target,
      managementRevision: initial.managementRevision,
      compatibility: {
        version: `${target}-unknown-8.1`,
        classification: "unknown-compatible",
        acknowledgementRequired: true,
      },
    })
    assertControlledSecretSource(initial.problems, compatibilitySecrets, `unmanaged-compatibility-source-${target}`)
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      const previewFrame = await waitForSecretFreeFrame(
        setup,
        (frame) => frame.includes(`Untested but compatible · ${target}-unknown-8.1`),
        compatibilitySecrets,
        `unmanaged-compatibility-probe-${target}`,
      )
      auditSecretFreeActions({
        actions: session.actions,
        probes: session.compatibilityProbes,
        resolutions: session.compatibilityResolutions,
      }, compatibilitySecrets, `unmanaged-compatibility-probe-${target}`)
      expect(session.compatibilityProbes).toHaveLength(1)
      expect(session.reconciliationPreviews).toHaveLength(0)
      expect(previewFrame).not.toContain("Adopt observed configuration")
      expect(previewFrame).not.toContain("Reapply committed configuration")
      expect(previewFrame).not.toContain("Restore pre-Muxvia configuration")
      expect(previewFrame).toContain("Command-line flags and resumed sessions may still")
      expect(previewFrame).toContain("override this configuration.")

      setup.mockInput.pressKey("y")
      setup.mockInput.pressKey("y")
      await waitForSecretFreeCondition(
        setup,
        () => session.actions.length === 1,
        () => auditSecretFreeActions({
          actions: session.actions,
          probes: session.compatibilityProbes,
          resolutions: session.compatibilityResolutions,
        }, compatibilitySecrets, `unmanaged-compatibility-action-${target}`),
        `secret-scan-failed:unmanaged-compatibility-action-${target}-action`,
        `unmanaged-compatibility-acknowledgement-${target}`,
      )
      assertSecretFreeStructured("action", session.actions, compatibilitySecrets, `unmanaged-compatibility-action-${target}`, (safeActions) => {
        expect(safeActions).toEqual([{
          kind: "resolve-compatibility",
          version: `${target}-unknown-8.1`,
        }])
      })
      const completed = await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes(`Compatibility acknowledgement recorded: ${target}-unknown-8.1`),
        `unmanaged-compatibility-resolution-activity-${target}`,
      )
      expect(session.get().problems).toEqual([])
      expect(completed.match(/Compatibility acknowledgement recorded:/g)).toHaveLength(1)
      const restoredFocus = setup.renderer.currentFocusedRenderable as InputRenderable
      expect(restoredFocus.placeholder).toBe("Run a target action…")
      expect(restoredFocus.isDestroyed).toBeFalse()
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each(["codex", "claude"] as const)(
  "Tested %s Probe resolves a stale compatibility blocker without acknowledgement copy",
  async (target) => {
    const initial = view({
      target,
      problems: [controlledCompatibilityProblem("incompatible-target-cli")],
    })
    const resolved = view({
      ...initial,
      viewSequence: initial.viewSequence + 1,
      problems: [],
    })
    const session = new MemoryTargetSession(initial, async () => ({ status: "applied", view: resolved }))
    session.compatibilityProbeHandler = async () => ({
      target,
      managementRevision: initial.managementRevision,
      compatibility: {
        version: `${target}-tested-8.2`,
        classification: "tested",
        acknowledgementRequired: false,
      },
    })
    assertControlledSecretSource(initial.problems, compatibilitySecrets, `tested-compatibility-source-${target}`)
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      const probeFrame = await waitForSecretFreeFrame(
        setup,
        (frame) => frame.includes(`Tested · ${target}-tested-8.2`),
        compatibilitySecrets,
        `tested-compatibility-probe-${target}`,
      )
      auditCompatibilityOutputs(session, `tested-compatibility-probe-${target}`)
      expect(probeFrame).toContain("Command-line flags and resumed sessions may still")
      expect(probeFrame).toContain("override this configuration.")
      expect(probeFrame).toContain("Y resolve tested version")
      expect(probeFrame).not.toContain("I acknowledge")
      expect(probeFrame).not.toContain("acknowledge exact version")

      setup.mockInput.pressKey("y")
      setup.mockInput.pressKey("y")
      await waitForSecretFreeCondition(
        setup,
        () => session.actions.length === 1,
        () => auditCompatibilityOutputs(session, `tested-compatibility-resolution-${target}`),
        `secret-scan-failed:tested-compatibility-resolution-${target}-action`,
        `tested-compatibility-resolution-${target}`,
      )
      assertSecretFreeStructured("action", session.actions, compatibilitySecrets, `tested-compatibility-action-${target}`, (safeActions) => {
        expect(safeActions).toEqual([{
          kind: "resolve-compatibility",
          version: `${target}-tested-8.2`,
        }])
      })
      const completed = await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes(`Compatibility status resolved: ${target}-tested-8.2`),
        `tested-compatibility-activity-${target}`,
      )
      expect(completed.match(/Compatibility status resolved:/g)).toHaveLength(1)
      const restoredFocus = setup.renderer.currentFocusedRenderable as InputRenderable
      expect(restoredFocus.placeholder).toBe("Run a target action…")
      expect(restoredFocus.isDestroyed).toBeFalse()
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each(["codex", "claude"] as const)(
  "First unmanaged incompatible %s exposes exact read-only compatibility guidance",
  async (target) => {
    const initial = view({
      target,
      managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
      problems: [controlledCompatibilityProblem("incompatible-target-cli")],
    })
    const session = new MemoryTargetSession(initial)
    session.compatibilityProbeHandler = async () => ({
      target,
      managementRevision: initial.managementRevision,
      compatibility: {
        version: `${target}-incompatible-8.1`,
        classification: "incompatible",
        acknowledgementRequired: false,
      },
    })
    assertControlledSecretSource(initial.problems, compatibilitySecrets, `incompatible-compatibility-source-${target}`)
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      const frame = await waitForSecretFreeFrame(
        setup,
        (current) => current.includes(`Incompatible · ${target}-incompatible-8.1`),
        compatibilitySecrets,
        `unmanaged-incompatible-preview-${target}`,
      )
      auditCompatibilityOutputs(session, `unmanaged-incompatible-preview-${target}`)
      expect(frame).toContain("Read-only inspection · Esc cancel")
      expect(frame).not.toContain("Adopt observed configuration")
      expect(frame).not.toContain("Reapply committed configuration")
      expect(frame).not.toContain("Restore pre-Muxvia configuration")
      setup.mockInput.pressKey("y")
      setup.mockInput.pressEnter()
      await Promise.resolve()
      await setup.renderOnce()
      auditSecretFreeFrame(setup.captureCharFrame(), compatibilitySecrets, `unmanaged-incompatible-actions-${target}`)
      auditCompatibilityOutputs(session, `unmanaged-incompatible-actions-${target}`)
      expect(session.actions).toHaveLength(0)
      expect(session.reconciliationPreviews).toHaveLength(0)
    } finally {
      setup.renderer.destroy()
    }
  },
)

test("A replayed compatibility resolution closes the overlay without duplicate activity", async () => {
  const initial = view({
    problems: [controlledCompatibilityProblem("compatibility-acknowledgement-required")],
  })
  const resolved = view({ ...initial, viewSequence: 2, problems: [] })
  const session = new MemoryTargetSession(initial, async () => ({ status: "replayed", view: resolved }))
  session.compatibilityProbeHandler = async () => ({
    target: "codex",
    managementRevision: initial.managementRevision,
    compatibility: {
      version: "codex-unknown-replayed",
      classification: "unknown-compatible",
      acknowledgementRequired: true,
    },
  })
  assertControlledSecretSource(initial.problems, compatibilitySecrets, "compatibility-replay-source")
  const setup = await testRender(() => <App session={session} />, {
    width: 80,
    height: 30,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("codex-unknown-replayed"),
      compatibilitySecrets,
      "compatibility-replay-probe",
    )
    auditCompatibilityOutputs(session, "compatibility-replay-probe")
    setup.mockInput.pressKey("y")
    await waitForSecretFreeCondition(
      setup,
      () => session.get().problems.length === 0,
      () => auditCompatibilityOutputs(session, "compatibility-replay-resolution"),
      "secret-scan-failed:compatibility-replay-resolution-action",
      "compatibility-replay-resolution",
    )
    auditSecretFreeView(session.get(), compatibilitySecrets, "compatibility-replay-resolved-view")
    const completed = await waitForSecretFreeFrame(
      setup,
      (frame) => session.actions.length === 1 && !frame.includes("codex-unknown-replayed"),
      compatibilitySecrets,
      "compatibility-replay-completed",
    )
    expect(completed).not.toContain("Compatibility acknowledgement recorded")
    expect(completed).not.toContain("Compatibility status resolved")
    expect(session.actions).toHaveLength(1)
  } finally {
    setup.renderer.destroy()
  }
})

test("Compatibility resolution stays bound to its Probe revision across a peer-session push", async () => {
  const initial = view({
    managementRevision: 1,
    viewSequence: 1,
    problems: [{ code: "compatibility-acknowledgement-required", message: "Compatibility acknowledgement required" }],
  })
  const raced = view({ ...initial, managementRevision: 2, viewSequence: 2 })
  const resolved = view({ ...raced, managementRevision: 3, viewSequence: 3, problems: [] })
  const staleError = Object.assign(
    new AggregateError([new Error(backendSecret)], "Target state changed"),
    {
      code: "stale-revision",
      credentialDiagnostic: credentialSecret,
      configDiagnostic: configSecret,
      settingsDiagnostic: settingsSecret,
    },
  )
  assertControlledSecretSource(staleError, compatibilitySecrets, "compatibility-revision-error-source")
  const session = new MemoryTargetSession(initial)
  session.compatibilityProbeHandler = async () => {
    const managementRevision = session.compatibilityProbes.length + 1
    return {
      target: "codex",
      managementRevision,
      compatibility: {
        version: `codex-unknown-r${managementRevision}`,
        classification: "unknown-compatible",
        acknowledgementRequired: true,
      },
    }
  }
  session.compatibilityResolutionHandler = async () => {
    if (session.compatibilityResolutions.length === 1) {
      throw staleError
    }
    return { status: "applied", view: resolved }
  }
  const setup = await testRender(() => <App session={session} />, {
    width: 80,
    height: 30,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(
      setup,
      [session],
      (frame) => frame.includes("codex-unknown-r1"),
      "compatibility-revision-first-probe",
    )
    session.pushView(raced)
    setup.mockInput.pressKey("y")
    await waitForReconciliationState(
      setup,
      [session],
      () => session.compatibilityResolutions.length === 1,
      "compatibility-revision-stale-resolution",
    )
    assertSecretFreeStructured("action", session.compatibilityResolutions, compatibilitySecrets, "compatibility-revision-stale-input", (safe) => {
      expect(safe).toEqual([{ version: "codex-unknown-r1", managementRevision: 1 }])
    })
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const stale = setup.captureCharFrame()
    auditSecretFreeFrame(stale, compatibilitySecrets, "compatibility-revision-stale-guidance")
    auditCompatibilityOutputs(session, "compatibility-revision-stale-guidance")
    auditSecretFreeView(session.get(), compatibilitySecrets, "compatibility-revision-stale-guidance")
    expect(stale).toContain("Target state changed")
    expect(stale).toContain("R probe again")
    expect(stale).not.toContain("Compatibility acknowledgement recorded")
    expect(session.get().problems).toHaveLength(1)

    setup.mockInput.pressKey("r")
    await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("codex-unknown-r2"),
      compatibilitySecrets,
      "compatibility-revision-fresh-probe",
    )
    auditCompatibilityOutputs(session, "compatibility-revision-fresh-probe")
    auditSecretFreeView(session.get(), compatibilitySecrets, "compatibility-revision-fresh-probe")
    setup.mockInput.pressKey("y")
    const complete = await waitForSecretFreeFrame(
      setup,
      (frame) => frame.includes("Compatibility acknowledgement recorded: codex-unknown-r2"),
      compatibilitySecrets,
      "compatibility-revision-fresh-resolution",
    )
    auditCompatibilityOutputs(session, "compatibility-revision-fresh-resolution")
    auditSecretFreeView(session.get(), compatibilitySecrets, "compatibility-revision-fresh-resolution")
    assertSecretFreeStructured("action", session.compatibilityResolutions, compatibilitySecrets, "compatibility-revision-resolution-inputs", (safe) => {
      expect(safe).toEqual([
        { version: "codex-unknown-r1", managementRevision: 1 },
        { version: "codex-unknown-r2", managementRevision: 2 },
      ])
    })
    expect(complete.match(/Compatibility acknowledgement recorded:/g)).toHaveLength(1)
  } finally {
    setup.renderer.destroy()
  }
})

test.each(["codex", "claude"] as const)(
  "Reconciliation previews and applies unknown-compatible Adopt for %s with exact origin focus and one activity",
  async (target) => {
    const initial = view({
      target,
      problems: [
        { code: "configuration-drift", message: "Configuration drift" },
        { code: "untested-target-cli", message: "Untested Target CLI" },
      ],
    })
    const applied = view({
      ...initial,
      managementRevision: 2,
      viewSequence: 2,
      managedConfiguration: { state: "managed", path: null, restartRequired: true },
      problems: [],
    })
    const session = new MemoryTargetSession(initial)
    session.previewHandler = async (strategy) => reconciliationPreview(target, strategy, {
      compatibility: { version: "9.9.9", classification: "unknown-compatible", acknowledgementRequired: true },
    })
    session.reconciliationHandler = async () => ({ status: "applied", view: applied })
    assertControlledSecretSource(controlledClaudeDirectSources(), claudeDirectSecrets, `reconciliation-${target}`)
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      const targetFrame = await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes("Reconcile Managed Configuration"),
        `reconciliation-entry-${target}`,
      )
      expect(targetFrame).not.toContain(backendSecret)
      const originFocus = setup.renderer.currentFocusedRenderable as InputRenderable
      const globalKeyboardListeners = setup.renderer.keyInput.listenerCount("keypress")

      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Adopt observed configuration"), `reconciliation-open-${target}`)
      expect(setup.renderer.keyInput.listenerCount("keypress")).toBe(globalKeyboardListeners)
      setup.mockInput.pressKey("a")
      await waitForReconciliationState(setup, [session], () => session.reconciliationPreviews.length === 1, `reconciliation-preview-call-${target}`)
      const previewFrame = await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes("Untested but compatible · 9.9.9") && frame.includes("Changed"),
        `reconciliation-preview-${target}`,
      )
      expect(previewFrame).toContain("No observable shadow source")
      expect(previewFrame).toContain("Command-line flags and resumed sessions")
      expect(previewFrame).toContain(target === "codex"
        ? "Restart Codex after applying this reconciliation."
        : "Restart Claude Code after applying this reconciliation.")

      setup.mockInput.pressKey("y")
      setup.mockInput.pressEnter()
      await waitForReconciliationState(setup, [session], () => session.reconciliationApplies.length === 1, `reconciliation-apply-call-${target}`)
      const appliedFrame = await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Reconciliation applied: Adopt"), `reconciliation-applied-${target}`)
      auditSecretFreePreview(session.reconciliationPreviewResults, claudeDirectSecrets, `reconciliation-preview-${target}`)
      auditSecretFreeActions(session.reconciliationApplies, claudeDirectSecrets, `reconciliation-action-${target}`)
      auditSecretFreeView(session.get(), claudeDirectSecrets, `reconciliation-view-${target}`)
      assertSecretFreeStructured("action", session.reconciliationApplies, claudeDirectSecrets, `reconciliation-exact-apply-${target}`, (safeApplies) => {
        expect(safeApplies).toEqual([{
          strategy: "adopt",
          observationToken: "00000000-0000-4000-8000-000000000090",
          acknowledgeVersion: "9.9.9",
        }])
      })
      expect(appliedFrame).toContain(target === "codex"
        ? "Restart Codex to use the managed configuration."
        : "Restart Claude Code to use the managed configuration.")
      expect(appliedFrame.match(/Reconciliation applied: Adopt/g)).toHaveLength(1)
      expect(setup.renderer.currentFocusedRenderable).toBe(originFocus)
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each([
  { target: "codex" as const, strategy: "adopt" as const, key: "a", source: "codex-profile" as const },
  { target: "codex" as const, strategy: "reapply" as const, key: "r", source: "codex-profile" as const },
  { target: "codex" as const, strategy: "restore" as const, key: "s", source: "codex-profile" as const },
  { target: "claude" as const, strategy: "adopt" as const, key: "a", source: "claude-shared" as const },
  { target: "claude" as const, strategy: "reapply" as const, key: "r", source: "claude-shared" as const },
  { target: "claude" as const, strategy: "restore" as const, key: "s", source: "claude-shared" as const },
])(
  "Reconciliation blocks $target $strategy apply for a shadowing preview and restores exact origin focus on cancel",
  async ({ target, strategy, key, source }) => {
    const initial = view({
      target,
      problems: [{ code: "shadowing-configuration", message: "safe" }],
    })
    const session = new MemoryTargetSession(initial)
    session.previewHandler = async (requested) => reconciliationPreview(target, requested, {
      shadowSources: [source],
    })
    session.reconciliationHandler = async () => ({
      status: "applied",
      view: view({ target, managementRevision: 2, viewSequence: 2 }),
    })
    const setup = await testRender(() => <App sessions={{ [target]: session }} />, {
      width: 80,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      const shell = await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes("Reconcile Managed Configuration"),
        `reconciliation-shadow-shell-${target}-${strategy}`,
      )
      expect(shell).not.toContain(backendSecret)
      const originFocus = setup.renderer.currentFocusedRenderable as InputRenderable

      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes("Adopt observed configuration"),
        `reconciliation-shadow-open-${target}-${strategy}`,
      )
      setup.mockInput.pressKey(key)
      const previewFrame = await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes(source === "codex-profile" ? "Codex profile" : "Claude shared settings"),
        `reconciliation-shadow-preview-${target}-${strategy}`,
      )
      expect(previewFrame).toContain("Shadowing Configuration")
      expect(previewFrame).toContain("Command-line flags and resumed sessions")
      expect(setup.renderer.currentFocusedRenderable).not.toBe(originFocus)

      setup.mockInput.pressEnter()
      for (let pass = 0; pass < 4; pass++) await Promise.resolve()
      await setup.renderOnce()
      const blockedFrame = setup.captureCharFrame()
      auditSecretFreeFrame(blockedFrame, claudeDirectSecrets, `reconciliation-shadow-blocked-${target}-${strategy}`)
      auditReconciliationSessions([session], `reconciliation-shadow-blocked-${target}-${strategy}`)
      expect(session.reconciliationApplies).toHaveLength(0)
      expect(blockedFrame.replace(/\s+/g, " ")).toContain("Shadowing configuration blocks reconciliation. Remove or disable the shadow source, then preview again.")
      expect(blockedFrame).toContain(source === "codex-profile" ? "Codex profile" : "Claude shared settings")
      expect(blockedFrame).toContain(strategy === "adopt"
        ? "Adopt observed configuration"
        : strategy === "reapply"
          ? "Reapply committed configuration"
          : "Restore pre-Muxvia configuration")

      setup.mockInput.pressEscape()
      await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes(target === "codex" ? "Codex · Control Plane" : "Claude · Control Plane")
          && !frame.includes("Adopt observed configuration"),
        `reconciliation-shadow-cancel-${target}-${strategy}`,
      )
      expect(setup.renderer.currentFocusedRenderable).toBe(originFocus)
      expect(originFocus.isDestroyed).toBeFalse()
    } finally {
      setup.renderer.destroy()
    }
  },
)

test.each([
  {
    target: "codex" as const,
    locale: "en" as const,
    expected: "Shadowing configuration blocks reconciliation. Remove or disable the shadow source, then preview again.",
    legacyPrefix: "Managed routing values are shadowed by",
  },
  {
    target: "claude" as const,
    locale: "zh-CN" as const,
    expected: "遮蔽配置阻止协调。请移除或停用遮蔽来源，然后重新预览。",
    legacyPrefix: "受管理的路由值被",
  },
])(
  "Reconciliation $locale race maps a source-free shadowing apply failure to exact localized copy",
  async ({ target, locale, expected, legacyPrefix }) => {
    const initial = view({
      target,
      problems: [{ code: "configuration-drift", message: "safe" }],
    })
    const session = new MemoryTargetSession(initial)
    session.previewHandler = async (strategy) => reconciliationPreview(target, strategy)
    const raceError = Object.assign(
      new AggregateError([new Error(backendSecret)], "safe"),
      { code: "shadowing-configuration", settings: settingsSecret },
    )
    raceError.stack = `at reconciliation (${credentialSecret})`
    assertControlledSecretSource(raceError, claudeDirectSecrets, `reconciliation-shadow-race-source-${locale}`)
    session.reconciliationHandler = async () => { throw raceError }
    const setup = await testRender(() => <App sessions={{ [target]: session }} locale={locale} />, {
      width: 80,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey(target === "codex" ? "1" : "2")
      await setup.mockInput.typeText("/reconcile")
      setup.mockInput.pressEnter()
      await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes(locale === "en" ? "Adopt observed configuration" : "采用观测到的配置"),
        `reconciliation-shadow-race-open-${locale}`,
      )
      setup.mockInput.pressKey("r")
      await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.includes(locale === "en" ? "No observable shadow source" : "没有可观测的遮蔽源"),
        `reconciliation-shadow-race-clean-preview-${locale}`,
      )
      setup.mockInput.pressEnter()
      const failureFrame = await waitForReconciliationFrame(
        setup,
        [session],
        (frame) => frame.replace(/\s+/g, " ").includes(expected) || frame.includes(legacyPrefix),
        `reconciliation-shadow-race-error-${locale}`,
      )
      expect(session.reconciliationApplies).toHaveLength(1)
      expect(failureFrame.replace(/\s+/g, " ")).toContain(expected)
      expect(failureFrame).not.toContain("{source}")
      expect(failureFrame).not.toContain(legacyPrefix)
      expect(failureFrame).toContain(locale === "en" ? "Reapply committed configuration" : "重新应用已提交配置")
    } finally {
      setup.renderer.destroy()
    }
  },
)

test("Reconciliation stale apply stays open, uses a fixed error, and never retries automatically", async () => {
  const initial = view({ problems: [{ code: "configuration-drift", message: "Configuration drift" }] })
  const session = new MemoryTargetSession(initial)
  let token = 90
  session.previewHandler = async (strategy) => reconciliationPreview("codex", strategy, {
    observationToken: `00000000-0000-4000-8000-0000000000${token++}`,
  })
  session.reconciliationHandler = async () => {
    const error = Object.assign(new Error(`raw ${backendSecret}`), { code: "stale-reconciliation-preview", settings: settingsSecret })
    error.stack = `at ${credentialSecret}`
    throw error
  }
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Adopt observed configuration"), "reconciliation-stale-open")
    setup.mockInput.pressKey("r")
    await waitForReconciliationState(setup, [session], () => session.reconciliationPreviews.length === 1, "reconciliation-stale-preview-call")
    setup.mockInput.pressEnter()
    await waitForReconciliationState(setup, [session], () => session.reconciliationApplies.length === 1, "reconciliation-stale-apply-call")
    const stale = await waitForReconciliationFrame(
      setup,
      [session],
      (frame) => frame.includes("Target state changed. Preview the reconciliation again."),
      "reconciliation-stale",
    )
    expect(stale).toContain("Reapply committed configuration")
    assertSecretFreeStructured("preview", session.reconciliationPreviews, claudeDirectSecrets, "reconciliation-stale-strategy", (safePreviews) => {
      expect(safePreviews).toEqual(["reapply"])
    })
    await Promise.resolve()
    auditReconciliationSessions([session], "reconciliation-stale-no-retry")
    expect(session.reconciliationPreviews).toHaveLength(1)

    setup.mockInput.pressKey("r")
    await waitForReconciliationState(setup, [session], () => session.reconciliationPreviews.length === 2, "reconciliation-stale-repreview")
    expect(session.reconciliationApplies).toHaveLength(1)
  } finally {
    setup.renderer.destroy()
  }
})

test("Reconciliation target-busy keeps the exact Restore preview open with fixed retry guidance", async () => {
  const session = new MemoryTargetSession(view({
    problems: [{ code: "configuration-drift", message: "Configuration drift" }],
  }))
  session.previewHandler = async (strategy) => reconciliationPreview("codex", strategy)
  session.reconciliationHandler = async () => {
    throw Object.assign(new AggregateError([new Error(backendSecret)], "safe"), {
      code: "target-busy",
      settings: settingsSecret,
    })
  }
  const setup = await testRender(() => <App session={session} />, {
    width: 80,
    height: 30,
    useThread: false,
    kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Adopt observed configuration"), "reconciliation-busy-open")
    setup.mockInput.pressKey("s")
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Restore pre-Muxvia configuration") && frame.includes("Tested · 9.9.9"), "reconciliation-busy-preview")
    setup.mockInput.pressEnter()
    await waitForReconciliationState(setup, [session], () => session.reconciliationApplies.length === 1, "reconciliation-busy-apply")
    const frame = await waitForReconciliationFrame(setup, [session], (current) => current.includes("This Target has active model requests."), "reconciliation-busy-error")
    expect(frame).toContain("Restore pre-Muxvia configuration")
    expect(frame).not.toContain("Reconciliation applied")
  } finally {
    setup.renderer.destroy()
  }
})

test("Reconciliation apply pending is nondismissible, suppresses background commands, and dispatches once", async () => {
  const initial = view({ problems: [{ code: "configuration-drift", message: "safe" }] })
  const pending = deferred<ActionOutcome>()
  const session = new MemoryTargetSession(initial)
  session.previewHandler = async (strategy) => reconciliationPreview("codex", strategy)
  session.reconciliationHandler = async () => await pending.promise
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Adopt observed configuration"), "reconciliation-pending-open")
    setup.mockInput.pressKey("s")
    await waitForReconciliationState(setup, [session], () => session.reconciliationPreviews.length === 1, "reconciliation-pending-preview")
    setup.mockInput.pressEnter()
    setup.mockInput.pressEnter()
    setup.mockInput.pressEscape()
    setup.mockInput.pressKey("p", { ctrl: true })
    await waitForReconciliationState(setup, [session], () => session.reconciliationApplies.length === 1, "reconciliation-pending-apply")
    const frame = await waitForReconciliationFrame(setup, [session], (current) => current.includes("Applying reconciliation…"), "reconciliation-pending-frame")
    expect(frame).toContain("Restore pre-Muxvia configuration")
    expect(frame).not.toContain("Search commands")
    expect(frame).not.toContain("Choose a target")
    expect(session.reconciliationApplies).toHaveLength(1)

    pending.resolve({ status: "applied", view: view({ managementRevision: 2, viewSequence: 2 }) })
    await waitForReconciliationFrame(setup, [session], (current) => current.includes("Reconciliation applied: Restore"), "reconciliation-pending-success")
    expect(session.reconciliationApplies).toHaveLength(1)
  } finally {
    pending.reject(new Error("cleanup"))
    setup.renderer.destroy()
  }
})

test("Cancelled Reconciliation preview cannot mutate or close a later Target workflow", async () => {
  const codexPreview = deferred<ReconciliationPreview>()
  const codex = new MemoryTargetSession(view({ problems: [{ code: "configuration-drift", message: "safe" }] }))
  codex.previewHandler = async () => await codexPreview.promise
  const claude = new MemoryTargetSession(view({ target: "claude", problems: [{ code: "configuration-drift", message: "safe" }] }))
  claude.previewHandler = async (strategy) => reconciliationPreview("claude", strategy)
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Adopt observed configuration"), "reconciliation-cancel-codex-open")
    setup.mockInput.pressKey("a")
    await waitForReconciliationState(setup, [codex, claude], () => codex.reconciliationPreviews.length === 1, "reconciliation-cancel-codex-preview")
    setup.mockInput.pressEscape()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Codex · Control Plane") && !frame.includes("Adopt observed configuration"), "reconciliation-cancel-codex-closed")
    setup.mockInput.pressEscape()
    setup.mockInput.pressKey("2")
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Claude · Control Plane"), "reconciliation-cancel-claude-target")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Adopt observed configuration"), "reconciliation-cancel-claude-open")
    setup.mockInput.pressKey("r")
    await waitForReconciliationState(setup, [codex, claude], () => claude.reconciliationPreviews.length === 1, "reconciliation-cancel-claude-preview")

    codexPreview.resolve(reconciliationPreview("codex", "adopt"))
    await Promise.resolve()
    await setup.renderOnce()
    const current = setup.captureCharFrame()
    auditSecretFreeFrame(current, claudeDirectSecrets, "reconciliation-cancel-current-frame")
    auditReconciliationSessions([codex, claude], "reconciliation-cancel-current")
    expect(current).toContain("Claude Code")
    expect(current).toContain("Reapply committed configuration")
    expect(current).not.toContain("Codex · Control Plane")
    expect(codex.reconciliationApplies).toHaveLength(0)
  } finally {
    setup.renderer.destroy()
  }
})

test("drift gates Provider saves and activation target-locally while read-only reachability and the healthy peer remain available", async () => {
  const selected = provider({ id: "00000000-0000-4000-8000-000000000011" })
  const codexInitial = view({
    providers: [selected],
    currentProviderId: selected.id,
    problems: [{ code: "configuration-drift", message: "Configuration drift" }],
  })
  const codex = new MemoryTargetSession(codexInitial)
  codex.reachabilityHandler = async () => ({
    status: "reachable",
    httpStatus: 200,
    ttfbMs: 1,
    checkedAtUnixMs: 1,
    retryCount: 0,
    slow: false,
    endpointOrigin: "https://safe.example",
  })
  codex.discoveryHandler = async () => ({
    status: "success",
    models: [{ id: "read-only-model", displayName: null }],
    attempts: 1,
    elapsedMs: 1,
    endpointOrigin: "https://safe.example",
  })
  const claudeProvider = provider({
    id: "00000000-0000-4000-8000-000000000012",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
  })
  const claudeInitial = view({ target: "claude", providers: [claudeProvider], currentProviderId: claudeProvider.id })
  const claude = new MemoryTargetSession(claudeInitial)
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Managed configuration changed outside Muxvia"), "reconciliation-gate-activation")
    expect(codex.actions).toHaveLength(0)

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Navigate Providers"), "reconciliation-gate-inspection")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("t")
    await waitForReconciliationState(setup, [codex, claude], () => codex.reachabilityChecks.length === 1, "reconciliation-gate-reachability-call")
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Reachable · HTTP 200"), "reconciliation-gate-reachability")
    expect(codex.actions).toHaveLength(0)

    setup.mockInput.pressEnter()
    await waitForReconciliationState(setup, [codex, claude], () => codex.discoveryRequests.length === 1, "reconciliation-gate-discovery-call")
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("models available"), "reconciliation-gate-discovery")
    assertSecretFreeStructured("action", codex.discoveryRequests, claudeDirectSecrets, "reconciliation-gate-discovery-action", (safeRequests) => {
      expect(safeRequests).toEqual([{
        kind: "saved",
        providerId: selected.id,
        providerRevision: selected.providerRevision,
      }])
    })

    setup.mockInput.pressEscape()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Codex · Control Plane") && !frame.includes("Navigate Providers"), "reconciliation-gate-editor-closed")
    await setup.mockInput.typeText("/provider")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Blank"), "reconciliation-gate-source")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Enter save"), "reconciliation-gate-draft")
    await fillProviderDraft(setup.mockInput, ["Blocked", "https://safe.example", "model", credentialSecret])
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Managed configuration changed outside Muxvia"), "reconciliation-gate-save")
    expect(codex.actions).toHaveLength(0)

    setup.mockInput.pressEscape()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Codex · Control Plane") && !frame.includes("Enter save"), "reconciliation-gate-draft-closed")
    setup.mockInput.pressEscape()
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Choose a target"), "reconciliation-gate-home")
    setup.mockInput.pressKey("2")
    await waitForReconciliationFrame(setup, [codex, claude], (frame) => frame.includes("Claude · Control Plane"), "reconciliation-gate-peer")
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("d")
    await waitForReconciliationState(setup, [codex, claude], () => claude.actions.length === 1, "reconciliation-gate-peer-action")
    assertSecretFreeStructured("action", claude.actions, claudeDirectSecrets, "reconciliation-gate-peer-exact", (safeActions) => {
      expect(safeActions).toEqual([{ kind: "activate-provider", providerId: claudeProvider.id, mode: "direct" }])
    })
  } finally {
    setup.renderer.destroy()
  }
})

test.each(["shadowing-configuration", "incompatible-target-cli", "untested-target-cli"])(
  "reconciliation managed-write gate keeps %s read-only while Provider inspection remains available",
  async (code) => {
    const selected = provider({})
    const session = new MemoryTargetSession(view({
      providers: [selected],
      currentProviderId: selected.id,
      problems: [{ code, message: "Managed write blocked" }],
    }))
    const setup = await testRender(() => <App session={session} />, {
      width: 80,
      height: 30,
      useThread: false,
      kittyKeyboard: true,
    })
    try {
      await setup.renderOnce()
      setup.mockInput.pressKey("1")
      setup.mockInput.pressKey("x", { ctrl: true })
      setup.mockInput.pressKey("d")
      await setup.renderOnce()
      auditReconciliationSessions([session], `reconciliation-readonly-${code}`)
      expect(session.actions).toHaveLength(0)
      auditSecretFreeFrame(setup.captureCharFrame(), claudeDirectSecrets, `reconciliation-readonly-${code}`)

      await setup.mockInput.typeText("/providers")
      setup.mockInput.pressEnter()
      const picker = await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Navigate Providers"), `reconciliation-readonly-picker-${code}`)
      expect(picker).toContain("First Provider")
      expect(picker).not.toContain(backendSecret)
    } finally {
      setup.renderer.destroy()
    }
  },
)

test("compatibility version change clears the exact acknowledgement and incompatible preview stays read-only", async () => {
  const initial = view({ problems: [{ code: "untested-target-cli", message: "safe" }] })
  const session = new MemoryTargetSession(initial)
  let version = "1.0.0"
  session.previewHandler = async (strategy) => reconciliationPreview("codex", strategy, {
    compatibility: { version, classification: "unknown-compatible", acknowledgementRequired: true },
  })
  session.reconciliationHandler = async () => ({ status: "applied", view: view({ managementRevision: 2, viewSequence: 2 }) })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 30, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/reconcile")
    setup.mockInput.pressEnter()
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Adopt observed configuration"), "reconciliation-version-open")
    setup.mockInput.pressKey("a")
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("1.0.0"), "reconciliation-version-one")
    setup.mockInput.pressKey("y")

    version = "2.0.0"
    setup.mockInput.pressKey("r")
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("2.0.0"), "reconciliation-version-two")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(session.reconciliationApplies).toHaveLength(0)
    const acknowledgementFrame = setup.captureCharFrame()
    auditSecretFreeFrame(acknowledgementFrame, claudeDirectSecrets, "reconciliation-version-acknowledgement")
    auditReconciliationSessions([session], "reconciliation-version-acknowledgement")
    expect(acknowledgementFrame).toContain("Acknowledge the exact Target CLI version before applying.")

    version = "3.0.0"
    session.previewHandler = async (strategy) => reconciliationPreview("codex", strategy, {
      compatibility: { version, classification: "incompatible", acknowledgementRequired: false },
    })
    setup.mockInput.pressKey("s")
    await waitForReconciliationFrame(setup, [session], (frame) => frame.includes("Incompatible · 3.0.0"), "reconciliation-version-incompatible")
    setup.mockInput.pressEnter()
    await setup.renderOnce()
    expect(session.reconciliationApplies).toHaveLength(0)
    const incompatibleFrame = setup.captureCharFrame()
    auditSecretFreeFrame(incompatibleFrame, claudeDirectSecrets, "reconciliation-version-incompatible-error")
    auditReconciliationSessions([session], "reconciliation-version-incompatible-error")
    expect(incompatibleFrame).toContain("This Target CLI is incompatible with managed changes.")
  } finally {
    setup.renderer.destroy()
  }
})
