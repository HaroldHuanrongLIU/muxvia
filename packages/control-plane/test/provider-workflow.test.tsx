import { expect, test } from "bun:test"
import type { InputRenderable } from "@opentui/core"
import { testRender } from "@opentui/solid"

import { MuxviaKeymapProvider, useMuxviaKeymap } from "../src/commands/keymap"
import type { TargetSession } from "../src/control/target-session"
import type {
  ActionOutcome,
  DiscoverySource,
  ModelDiscoveryResult,
  ReachabilityResult,
  ReconciliationPreview,
  ReconciliationStrategy,
  TargetAction,
  TargetView,
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
const backendSecret = "backend-claude-direct-secret-must-not-render"
const settingsSecret = "settings-claude-direct-secret-must-not-render"
const claudeDirectSecrets = [credentialSecret, backendSecret, settingsSecret] as const

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
  readonly reachabilityChecks: string[] = []
  readonly discoveryRequests: DiscoverySource[] = []
  lastError: unknown
  readonly #listeners = new Set<(next: TargetView) => void>()
  #view: TargetView
  #handler: (action: TargetAction) => Promise<ActionOutcome>
  previewHandler?: (strategy: ReconciliationStrategy, signal?: AbortSignal) => Promise<ReconciliationPreview>
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
  async act(action: TargetAction): Promise<ActionOutcome> {
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
      applies: session.reconciliationApplies,
      discovery: session.discoveryRequests,
      reachability: session.reachabilityChecks,
    }, claudeDirectSecrets, `${label}-${index}`)
    auditSecretFreePreview(session.reconciliationPreviewResults, claudeDirectSecrets, `${label}-${index}`)
    auditSecretFreeView(session.get(), claudeDirectSecrets, `${label}-${index}`)
  }
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
    name: "Claude Bridge Provider",
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
    name: "Bridge Provider",
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
    await setup.waitForFrame((frame) => frame.includes("Bridge Provider"))
    const pickerFocus = setup.renderer.currentFocusedRenderable as InputRenderable

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("a")
    const confirmation = await setup.waitForFrame((frame) => frame.includes("Enable Target Takeover?"))
    expect(confirmation).toContain("Bridge Provider requires Target Takeover for activation.")
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
