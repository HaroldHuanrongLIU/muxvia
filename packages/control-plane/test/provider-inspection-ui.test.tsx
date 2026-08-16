import { expect, test } from "bun:test"
import { testRender } from "@opentui/solid"

import type { TargetSession } from "../src/control/target-session"
import type {
  ActionOutcome,
  DiscoverySource,
  ModelDiscoveryResult,
  ReachabilityResult,
  TargetAction,
  TargetView,
} from "../src/control/types"
import { App } from "../src/ui/app"

const providerSecret = "provider-secret-must-not-escape"
const routingSecret = "routing-secret-must-not-escape"
const endpointQuerySecret = "endpoint-query-must-not-persist"

function deferred<T>() {
  let resolve!: (value: T) => void
  const promise = new Promise<T>((next) => { resolve = next })
  return { promise, resolve }
}

function provider(overrides: Partial<TargetView["providers"][number]> = {}): TargetView["providers"][number] {
  return {
    id: "00000000-0000-4000-8000-000000000011",
    position: 0,
    providerRevision: 7,
    name: "Inspection Provider",
    baseUrl: "https://inspection.example/v1",
    model: "manual-model",
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
    managementRevision: 3,
    viewSequence: 4,
    service: { epoch: "00000000-0000-4000-8000-000000000001", state: "running" },
    mode: "unmanaged",
    takeover: { state: "inactive", endpoint: null },
    routeHealth: { state: "unobserved" },
    providers: [],
    providerPresets: [{
      key: "openai-api-responses",
      baseUrl: "https://api.openai.com/v1",
      model: "",
      protocol: "openai-responses",
      authentication: "openai-bearer",
    }],
    currentProviderId: null,
    servingProviderId: null,
    managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
    recovery: { intentId: null, state: "clean" },
    activatedSnapshot: null,
    problems: [],
    ...overrides,
  }
}

type DiscoveryProjection =
  | { kind: "saved"; providerId: string; providerRevision: number }
  | {
    kind: "draft"
    baseUrl: string
    authentication: "openai-bearer" | "anthropic-api-key" | "anthropic-bearer"
    credentialSource: "missing" | "ephemeral" | "saved"
    savedProviderId?: string
    savedProviderRevision?: number
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

function redactEndpoint(endpoint: string): string {
  try {
    const parsed = new URL(endpoint)
    parsed.username = ""
    parsed.password = ""
    parsed.search = ""
    parsed.hash = ""
    return parsed.toString()
  } catch {
    return "[invalid-endpoint]"
  }
}

function projectDiscovery(source: DiscoverySource): DiscoveryProjection {
  if (source.kind === "saved") return source
  return {
    kind: "draft",
    baseUrl: redactEndpoint(source.baseUrl),
    authentication: source.authentication,
    credentialSource: source.credentialSource.kind,
    savedProviderId: source.credentialSource.kind === "saved" ? source.credentialSource.providerId : undefined,
    savedProviderRevision: source.credentialSource.kind === "saved" ? source.credentialSource.providerRevision : undefined,
  }
}

class InspectionTargetSession implements TargetSession {
  readonly actions: RecordedTargetAction[] = []
  readonly discoveries: DiscoveryProjection[] = []
  readonly discoverySignals: AbortSignal[] = []
  readonly reachabilityCalls: Array<{ providerId: string; providerRevision: number }> = []
  readonly reachabilitySignals: AbortSignal[] = []
  #view: TargetView
  #listeners = new Set<(next: TargetView) => void>()
  #discover: (source: DiscoverySource, signal: AbortSignal | undefined) => Promise<ModelDiscoveryResult>
  #reachability: (providerId: string, providerRevision: number, signal: AbortSignal | undefined) => Promise<ReachabilityResult>

  constructor(options: {
    initial: TargetView
    discover?: (source: DiscoverySource, signal: AbortSignal | undefined) => Promise<ModelDiscoveryResult>
    reachability?: (providerId: string, providerRevision: number, signal: AbortSignal | undefined) => Promise<ReachabilityResult>
  }) {
    this.#view = options.initial
    this.#discover = options.discover ?? (async () => ({
      status: "success",
      models: [],
      attempts: 1,
      elapsedMs: 1,
      endpointOrigin: "https://inspection.example",
    }))
    this.#reachability = options.reachability ?? (async () => ({
      status: "unreachable",
      failure: { category: "connect", httpStatus: null, attempts: 1, elapsedMs: 1, endpointOrigin: "https://inspection.example" },
      checkedAtUnixMs: 1,
      retryCount: 0,
    }))
  }

  get(): Readonly<TargetView> { return this.#view }
  async act(action: TargetAction): Promise<ActionOutcome> {
    this.actions.push(projectAction(action))
    return { status: "applied", view: this.#view }
  }
  async discoverModels(source: DiscoverySource, signal?: AbortSignal): Promise<ModelDiscoveryResult> {
    this.discoveries.push(projectDiscovery(source))
    if (signal) this.discoverySignals.push(signal)
    return await this.#discover(source, signal)
  }
  async checkReachability(providerId: string, providerRevision: number, signal?: AbortSignal): Promise<ReachabilityResult> {
    this.reachabilityCalls.push({ providerId, providerRevision })
    if (signal) this.reachabilitySignals.push(signal)
    return await this.#reachability(providerId, providerRevision, signal)
  }
  async previewReconciliation(): Promise<never> { throw new Error("reconciliation not configured in this fixture") }
  async applyReconciliation(): Promise<never> { throw new Error("reconciliation not configured in this fixture") }
  async previewCompatibility(): Promise<never> { throw new Error("compatibility preview not configured in this fixture") }
  async acknowledgeCompatibility(): Promise<never> { throw new Error("compatibility acknowledgement not configured in this fixture") }
  subscribe(listener: (next: TargetView) => void): () => void {
    this.#listeners.add(listener)
    return () => this.#listeners.delete(listener)
  }
  push(next: TargetView): void {
    this.#view = next
    for (const listener of this.#listeners) listener(next)
  }
  async whenClosed(): Promise<void> { return await new Promise(() => {}) }
  async close(): Promise<void> {}
}

function expectSecretFree(frame: string): void {
  expect(frame).not.toContain(providerSecret)
  expect(frame).not.toContain(routingSecret)
}

async function openSavedEditor(setup: Awaited<ReturnType<typeof testRender>>): Promise<void> {
  setup.mockInput.pressKey("1")
  await setup.mockInput.typeText("/providers")
  setup.mockInput.pressEnter()
  await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
  setup.mockInput.pressEnter()
  await setup.waitForFrame((frame) => frame.includes("Edit Provider"))
}

test("saved discovery mounts once, refresh uses the current draft, aborts the old call, and only selected models replace manual text", async () => {
  const pending = [deferred<ModelDiscoveryResult>(), deferred<ModelDiscoveryResult>(), deferred<ModelDiscoveryResult>()]
  let sawEphemeralSecret = false
  let sawRawDraftEndpoint = false
  let session!: InspectionTargetSession
  session = new InspectionTargetSession({
    initial: view({ providers: [provider()] }),
    discover: async (source): Promise<ModelDiscoveryResult> => {
      const index = session.discoveries.length - 1
      if (source.kind === "draft" && source.credentialSource.kind === "ephemeral") {
        sawEphemeralSecret = source.credentialSource.value === providerSecret
        sawRawDraftEndpoint = source.baseUrl === `https://inspection.example/v1/draft?token=${endpointQuerySecret}#fragment`
      }
      return await pending[index]!.promise
    },
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    await openSavedEditor(setup)
    await setup.waitFor(() => session.discoveries.length === 1)
    expect(session.discoveries).toEqual([{
      kind: "saved",
      providerId: "00000000-0000-4000-8000-000000000011",
      providerRevision: 7,
    }])
    expectSecretFree(setup.captureCharFrame())

    setup.mockInput.pressTab()
    await setup.mockInput.typeText(`/draft?token=${endpointQuerySecret}#fragment`)
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("-typed")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(providerSecret)
    expect(session.discoveries).toHaveLength(1)
    expectSecretFree(setup.captureCharFrame())

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("f")
    await setup.waitFor(() => session.discoveries.length === 2)
    expect(session.discoverySignals[0]!.aborted).toBeTrue()
    const persistedEndpointSecret = JSON.stringify(session.discoveries).includes(endpointQuerySecret)
    expect(persistedEndpointSecret).toBeFalse()
    expect(session.discoveries[1]).toEqual({
      kind: "draft",
      baseUrl: "https://inspection.example/v1/draft",
      authentication: "openai-bearer",
      credentialSource: "ephemeral",
      savedProviderId: undefined,
      savedProviderRevision: undefined,
    })
    expect(sawEphemeralSecret).toBeTrue()
    expect(sawRawDraftEndpoint).toBeTrue()

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("f")
    await setup.waitFor(() => session.discoveries.length === 3)
    expect(session.discoverySignals[1]!.aborted).toBeTrue()

    pending[2]!.resolve({
      status: "success",
      models: [
        { id: "newer-a", displayName: null },
        { id: "newer-b", displayName: "Newer B" },
      ],
      attempts: 1,
      elapsedMs: 8,
      endpointOrigin: "https://inspection.example",
    })
    await setup.waitForFrame((frame) => frame.includes("2 models available"))
    pending[1]!.resolve({
      status: "success",
      models: [{ id: "late-older", displayName: null }],
      attempts: 1,
      elapsedMs: 9,
      endpointOrigin: "https://inspection.example",
    })
    pending[0]!.resolve({
      status: "success",
      models: [{ id: "late-saved", displayName: null }],
      attempts: 1,
      elapsedMs: 10,
      endpointOrigin: "https://inspection.example",
    })
    await setup.renderOnce()
    const suggestions = setup.captureCharFrame()
    expect(suggestions).toContain("manual-model-typed")
    expect(suggestions).not.toContain("late-older")
    expect(suggestions).not.toContain("late-saved")
    expectSecretFree(suggestions)

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("m")
    const picker = await setup.waitForFrame((frame) => frame.includes("newer-a") && frame.includes("newer-b"))
    expectSecretFree(picker)
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    const selected = await setup.waitForFrame((frame) => frame.includes("newer-b") && !frame.includes("Select Model"))
    expect(selected).not.toContain("manual-model-typed")
    expectSecretFree(selected)
  } finally {
    setup.renderer.destroy()
  }
})

test("a pushed Provider revision invalidates the mounted saved discovery without starting another", async () => {
  const selected = provider({ providerRevision: 7 })
  const saved = deferred<ModelDiscoveryResult>()
  const session = new InspectionTargetSession({
    initial: view({ providers: [selected] }),
    discover: async () => await saved.promise,
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    await openSavedEditor(setup)
    await setup.waitFor(() => session.discoveries.length === 1)
    expect(session.discoverySignals[0]!.aborted).toBeFalse()

    session.push(view({
      viewSequence: 5,
      providers: [{ ...selected, providerRevision: 8 }],
    }))
    await setup.renderOnce()
    expect(session.discoverySignals[0]!.aborted).toBeTrue()
    expect(session.discoveries).toHaveLength(1)

    saved.resolve({
      status: "success",
      models: [{ id: "stale-rev7-model", displayName: null }],
      attempts: 1,
      elapsedMs: 5,
      endpointOrigin: "https://inspection.example",
    })
    await setup.renderOnce()
    const frame = setup.captureCharFrame()
    expect(frame).not.toContain("stale-rev7-model")
    expect(frame).not.toContain("models available")
    expectSecretFree(frame)
  } finally {
    setup.renderer.destroy()
  }
})

test("closing aborts saved discovery and its late result cannot affect newer Blank or Preset drafts", async () => {
  const saved = deferred<ModelDiscoveryResult>()
  const session = new InspectionTargetSession({
    initial: view({ providers: [provider()] }),
    discover: async () => await saved.promise,
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    await openSavedEditor(setup)
    await setup.waitFor(() => session.discoveries.length === 1)
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    expect(session.discoverySignals[0]!.aborted).toBeTrue()
    expectSecretFree(setup.captureCharFrame())

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    await setup.waitForFrame((frame) => frame.includes("Blank"))
    setup.mockInput.pressEnter()
    const blank = await setup.waitForFrame((frame) => frame.includes("API credential"))
    expect(session.discoveries).toHaveLength(1)
    expectSecretFree(blank)
    saved.resolve({
      status: "success",
      models: [{ id: "late-closed", displayName: null }],
      attempts: 1,
      elapsedMs: 5,
      endpointOrigin: "https://inspection.example",
    })
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("late-closed")

    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("p")
    await setup.waitForFrame((frame) => frame.includes("OpenAI API (Responses)"))
    setup.mockInput.pressKey("down")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("https://api.openai.com/v1"))
    await setup.mockInput.typeText("Preset Draft")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("/draft")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("manual")
    setup.mockInput.pressTab()
    await setup.mockInput.typeText(providerSecret)
    expect(session.discoveries).toHaveLength(1)
    expectSecretFree(setup.captureCharFrame())
  } finally {
    setup.renderer.destroy()
  }
})

test("failed discovery leaves manual model input and Provider save enabled", async () => {
  const selected = provider()
  const session = new InspectionTargetSession({
    initial: view({ providers: [selected] }),
    discover: async () => ({
      status: "failure",
      failure: {
        category: "authentication-rejected",
        httpStatus: 401,
        attempts: 1,
        elapsedMs: 12,
        endpointOrigin: "https://inspection.example",
      },
    }),
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    await openSavedEditor(setup)
    const failed = await setup.waitForFrame((frame) => frame.includes("Authentication rejected"))
    expect(failed).not.toContain("401 body")
    expectSecretFree(failed)
    setup.mockInput.pressTab()
    setup.mockInput.pressTab()
    await setup.mockInput.typeText("-fallback")
    setup.mockInput.pressEnter()
    await setup.waitFor(() => session.actions.length === 1)
    expect(session.actions[0]).toMatchObject({
      kind: "update-provider",
      model: "manual-model-fallback",
      credential: { kind: "keep" },
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("saved Provider Reachability renders a separate read-only observation without Target View or activity mutation", async () => {
  const selected = provider()
  const initial = view({ providers: [selected] })
  const session = new InspectionTargetSession({
    initial,
    reachability: async () => ({
      status: "reachable",
      httpStatus: 503,
      ttfbMs: 6501,
      checkedAtUnixMs: 1_755_168_000_000,
      retryCount: 1,
      slow: true,
      endpointOrigin: "https://inspection.example",
    }),
  })
  const before = JSON.stringify(session.get())
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("t")
    const result = await setup.waitForFrame((frame) => frame.includes("Reachable") && frame.includes("6501 ms"))
    expect(result).toContain("HTTP 503")
    expect(result).toContain("1 retry")
    expect(result).toContain("Slow")
    expect(result).not.toContain("Route Health")
    expect(result).not.toContain("Recent activity")
    expectSecretFree(result)
    expect(session.reachabilityCalls).toEqual([{
      providerId: selected.id,
      providerRevision: selected.providerRevision,
    }])
    expect(JSON.stringify(session.get())).toBe(before)
    expect(session.actions).toEqual([])
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude owns automatic and explicit Discovery plus neutral Reachability", async () => {
  const selected = provider({
    name: "Claude Inspection",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
    routingRequirement: "takeover-required",
  })
  const codex = new InspectionTargetSession({ initial: view() })
  const claudeInitial = view({
    target: "claude",
    providers: [selected],
    providerPresets: [{
      key: "anthropic-api-messages",
      baseUrl: "https://api.anthropic.com/v1",
      model: "",
      protocol: "anthropic-messages",
      authentication: "anthropic-api-key",
    }],
  })
  const claude = new InspectionTargetSession({
    initial: claudeInitial,
    discover: async () => ({
      status: "success",
      models: [{ id: "claude-inspection-model", displayName: "Claude Inspection Model" }],
      attempts: 1,
      elapsedMs: 4,
      endpointOrigin: "https://inspection.example",
    }),
    reachability: async () => ({
      status: "reachable",
      httpStatus: 200,
      ttfbMs: 18,
      checkedAtUnixMs: 1_755_168_000_000,
      retryCount: 0,
      slow: false,
      endpointOrigin: "https://inspection.example",
    }),
  })
  const before = JSON.stringify(claude.get())
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.discoveries.length === 1)
    const automatic = await setup.waitForFrame((frame) => frame.includes("1 models available"))
    expectSecretFree(automatic)
    expect(claude.discoveries[0]).toEqual({ kind: "saved", providerId: selected.id, providerRevision: selected.providerRevision })

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("f")
    await setup.waitFor(() => claude.discoveries.length === 2)
    expect(claude.discoveries[1]).toEqual({
      kind: "draft",
      baseUrl: selected.baseUrl,
      authentication: "anthropic-api-key",
      credentialSource: "saved",
      savedProviderId: selected.id,
      savedProviderRevision: selected.providerRevision,
    })
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Claude · Control Plane"))

    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("t")
    const reachable = await setup.waitForFrame((frame) => frame.includes("Reachable") && frame.includes("18 ms"))
    expectSecretFree(reachable)
    expect(reachable).not.toContain("Route Health")
    expect(claude.reachabilityCalls).toEqual([{ providerId: selected.id, providerRevision: selected.providerRevision }])
    expect(JSON.stringify(claude.get())).toBe(before)

    setup.mockInput.pressEscape()
    const target = await setup.waitForFrame((frame) => frame.includes("Route Health  Unobserved"))
    expectSecretFree(target)
    expect(codex.discoveries).toEqual([])
    expect(codex.reachabilityCalls).toEqual([])
    expect(codex.actions).toEqual([])
    expect(claude.actions).toEqual([])
  } finally {
    setup.renderer.destroy()
  }
})

test("Claude draft Discovery follows the editor authentication toggle", async () => {
  const selected = provider({
    name: "Claude Draft Auth",
    protocol: "anthropic-messages",
    authentication: "anthropic-api-key",
    routingRequirement: "takeover-required",
  })
  const claude = new InspectionTargetSession({
    initial: view({ target: "claude", providers: [selected] }),
    discover: async () => ({
      status: "success",
      models: [],
      attempts: 1,
      elapsedMs: 1,
      endpointOrigin: "https://inspection.example",
    }),
  })
  const codex = new InspectionTargetSession({ initial: view() })
  const setup = await testRender(() => <App sessions={{ codex, claude }} />, {
    width: 80, height: 24, useThread: false, kittyKeyboard: true,
  })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("2")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
    setup.mockInput.pressEnter()
    await setup.waitFor(() => claude.discoveries.length === 1)
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("h")
    await setup.waitForFrame((frame) => frame.includes("Anthropic Bearer token"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("f")
    await setup.waitFor(() => claude.discoveries.length === 2)
    expect(claude.discoveries[1]).toMatchObject({
      kind: "draft",
      authentication: "anthropic-bearer",
      credentialSource: "saved",
    })
  } finally {
    setup.renderer.destroy()
  }
})

test("Reachability pending and observations are scoped to the visible Provider revision", async () => {
  const selected = provider({ providerRevision: 7 })
  const rev7 = deferred<ReachabilityResult>()
  let session!: InspectionTargetSession
  session = new InspectionTargetSession({
    initial: view({ providers: [selected] }),
    reachability: async () => session.reachabilityCalls.length === 1
      ? await rev7.promise
      : {
        status: "reachable",
        httpStatus: 204,
        ttfbMs: 12,
        checkedAtUnixMs: 1_755_168_000_000,
        retryCount: 0,
        slow: false,
        endpointOrigin: "https://inspection.example",
      },
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("t")
    await setup.waitForFrame((frame) => frame.includes("Checking Reachability"))

    session.push(view({
      viewSequence: 5,
      providers: [{ ...selected, providerRevision: 8 }],
    }))
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("Checking Reachability")
    rev7.resolve({
      status: "reachable",
      httpStatus: 200,
      ttfbMs: 7_007,
      checkedAtUnixMs: 1_755_168_000_000,
      retryCount: 1,
      slow: true,
      endpointOrigin: "https://inspection.example",
    })
    await setup.renderOnce()
    expect(setup.captureCharFrame()).not.toContain("7007 ms")

    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("t")
    const rev8 = await setup.waitForFrame((frame) => frame.includes("HTTP 204"))
    expectSecretFree(rev8)
    expect(session.reachabilityCalls).toEqual([
      { providerId: selected.id, providerRevision: 7 },
      { providerId: selected.id, providerRevision: 8 },
    ])

    session.push(view({
      viewSequence: 6,
      providers: [{ ...selected, providerRevision: 9 }],
    }))
    await setup.renderOnce()
    const rev9 = setup.captureCharFrame()
    expect(rev9).not.toContain("HTTP 204")
    expectSecretFree(rev9)
  } finally {
    setup.renderer.destroy()
  }
})

test("Reachability close and rejections clear pending without blocking later checks", async () => {
  const selected = provider()
  const closePending = deferred<ReachabilityResult>()
  let session!: InspectionTargetSession
  session = new InspectionTargetSession({
    initial: view({ providers: [selected] }),
    reachability: async () => {
      switch (session.reachabilityCalls.length) {
        case 1: return await closePending.promise
        case 2: return {
          status: "unreachable",
          failure: {
            category: "cancelled",
            httpStatus: null,
            attempts: 0,
            elapsedMs: 0,
            endpointOrigin: null,
          },
          checkedAtUnixMs: 1_755_168_000_000,
          retryCount: 0,
        }
        case 3: throw { code: "cancelled", message: "operator cancellation" }
        case 4: throw { code: "tls", message: "backend-message-must-not-render" }
        default: return {
          status: "reachable",
          httpStatus: 202,
          ttfbMs: 21,
          checkedAtUnixMs: 1_755_168_000_000,
          retryCount: 0,
          slow: false,
          endpointOrigin: "https://inspection.example",
        }
      }
    },
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  const openPicker = async () => {
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    return await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
  }
  const check = async (count: number) => {
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("t")
    await setup.waitFor(() => session.reachabilityCalls.length === count)
  }

  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await openPicker()
    await check(1)
    await setup.waitForFrame((frame) => frame.includes("Checking Reachability"))
    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    expect(session.reachabilitySignals[0]!.aborted).toBeTrue()

    const reopened = await openPicker()
    expect(reopened).not.toContain("Checking Reachability")
    await check(2)
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const cancelled = setup.captureCharFrame()
    expect(cancelled).not.toContain("Checking Reachability")
    expect(cancelled).not.toContain("Unreachable")

    await check(3)
    for (let pass = 0; pass < 4; pass++) await Promise.resolve()
    await setup.renderOnce()
    const rejectedCancellation = setup.captureCharFrame()
    expect(rejectedCancellation).not.toContain("Checking Reachability")
    expect(rejectedCancellation).not.toContain("Unreachable")

    await check(4)
    const rejected = await setup.waitForFrame((frame) => frame.includes("TLS failed"))
    expect(rejected).toContain("Unreachable")
    expect(rejected).not.toContain("backend-message-must-not-render")
    expect(rejected).not.toContain("Checking Reachability")
    expectSecretFree(rejected)

    setup.mockInput.pressEscape()
    await setup.waitForFrame((frame) => frame.includes("Run a target action"))
    await openPicker()
    await check(5)
    const retried = await setup.waitForFrame((frame) => frame.includes("HTTP 202"))
    expectSecretFree(retried)
  } finally {
    setup.renderer.destroy()
  }
})

test("unreachable observation keeps safe HTTP status and retry metadata visible", async () => {
  const selected = provider()
  const session = new InspectionTargetSession({
    initial: view({ providers: [selected] }),
    reachability: async () => ({
      status: "unreachable",
      failure: {
        category: "upstream-status",
        httpStatus: 503,
        attempts: 2,
        elapsedMs: 16_002,
        endpointOrigin: "https://inspection.example",
      },
      checkedAtUnixMs: 1_755_168_000_000,
      retryCount: 1,
    }),
  })
  const setup = await testRender(() => <App session={session} />, { width: 80, height: 24, useThread: false, kittyKeyboard: true })
  try {
    await setup.renderOnce()
    setup.mockInput.pressKey("1")
    await setup.mockInput.typeText("/providers")
    setup.mockInput.pressEnter()
    await setup.waitForFrame((frame) => frame.includes("Enter edit ·"))
    setup.mockInput.pressKey("x", { ctrl: true })
    setup.mockInput.pressKey("t")
    const result = await setup.waitForFrame((frame) => frame.includes("Unreachable"))
    const normalized = result.replace(/\s+/g, " ")
    expect(result).toContain("HTTP 503")
    expect(normalized).toContain("1 retry")
    expect(result).not.toContain("16002")
    expectSecretFree(result)
  } finally {
    setup.renderer.destroy()
  }
})
