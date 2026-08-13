# OpenCode-style Control Plane Shell Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the T01 single-screen Codex UI with the accepted OpenCode-style Muxvia Home and Target shell, using one command identity across shortcuts, slash commands, and the command palette while preserving terminal safety and all existing Codex takeover behavior.

**Architecture:** Keep the Rust Routing Service and the target-scoped RPC contract unchanged. Build a TypeScript/OpenTUI shell around the existing Codex `TargetSession`: a centralized `@opentui/keymap` command runtime drives Home/Target navigation and actions, one overlay stack owns modal UI and focus restoration, and flat English/Simplified-Chinese catalogs own every visible product string. A local Claude Code context is selectable from Home but honestly reports that management is unavailable in this build; T05 remains the owner of the Claude RPC, configuration, and model-transport implementation.

**Tech Stack:** Bun 1.3.14; TypeScript 5.8.2; Solid 1.9.12; OpenTUI core/solid/keymap 0.4.3; Bun.Terminal PTY; Bun test; existing Rust/Cargo verification unchanged.

## Global Constraints

- Work only in the Control Plane package and its tests unless a verification script or CI assertion must be updated; do not change Rust Routing Service behavior, SQLite, managed configuration, model HTTP routes, RPC wire schema, `TargetAction`, or `TargetSession` semantics.
- Pin `@opentui/keymap` to exactly `0.4.3`; keep `@opentui/core` and `@opentui/solid` at exactly `0.4.3`, Bun at `1.3.14`, TypeScript at `5.8.2`, and Solid at `1.9.12`.
- Reimplement the accepted interaction grammar from pinned OpenCode commit `0abbcddac233e313bcb67608a527929910df861c`; do not copy source text or domain behavior. If implementation later copies a substantive upstream block, record its path and preserve its MIT notice before committing.
- Home is the first screen and treats Codex CLI and Claude Code as selectable contexts. Selecting Claude opens a local shell context with localized “not available in this build” text and a working return path; it must not call `open-target claude`, fabricate a `TargetView`, or imply Claude management works.
- The product has no app bar, top tabs, permanent navigation, dashboard-card grid, global status strip, Cmd+K binding, split-footer mode, or global “Terminal too small” page.
- Home uses a centered Muxvia identity, one action prompt, two target quick rows, a contextual tip, and a sparse footer. Target uses a continuous status/activity stream, a fixed action prompt, and an optional current-target sidebar only when terminal width is greater than 120 columns.
- Keep Mode, Current Target Provider, Serving Provider, Routing Service, Managed Configuration, and Activated Snapshot visibly distinct and text-labelled; color is never the only status signal.
- All product actions are named commands. Shortcut, slash, and palette paths call `keymap.dispatchCommand()` with the same stable `CommandId`; focused text entry may handle characters locally but cannot create an independent global-action path.
- The keymap priority is top overlay `300`, focused editor `200`, current route `100`, global `0`. The `ctrl+x` leader has a 2,000 ms timeout; `ctrl+p` opens commands; `<leader>b` toggles only the current Target context sidebar.
- One overlay stack renders only its top entry. `Esc` closes the top overlay, otherwise cancels the focused editor, otherwise navigates from Target to Home. Closing an overlay restores the prior live focus target.
- `Ctrl+C` requests exit. A clean shell exits immediately; a dirty Provider editor opens confirmation. Cancel preserves draft and focus; confirm clears sensitive draft state before renderer destruction.
- Default renderer mode remains OpenTUI’s `alternate-screen`, `OTUI_USE_ALTERNATE_SCREEN=true|false` remains effective, and code never passes or selects `split-footer`.
- Normal command exit, SIGINT, SIGTERM, SIGHUP, renderer/session closure, and thrown render errors all run one idempotent cleanup: remove listeners, clear terminal title, close the Control Plane session once, and destroy the renderer once. No Control Plane cleanup operation stops a still-needed Routing Service.
- English and Simplified Chinese catalogs have exactly matching keys and cover visible shell, action, status, editor, confirmation, and error strings. Operator data such as Provider names, URLs, and model IDs is never translated.
- Render and resize without crashing at `1x1`, `2x2`, `20x5`, `40x10`, `80x24`, and `121x30`. Regions clamp, wrap, fold, hide, or scroll independently.
- PTY evidence must use Bun 1.3.14’s built-in `Bun.Terminal` and direct `Bun.spawn({ terminal })` on macOS/Linux; do not add node-pty, Python, `script`, or a shell wrapper.
- Preserve T01 credential hygiene: Provider secrets exist only in transient editor state, render as bullets, are cleared on submit/cancel/unmount/confirmed exit, and never appear in frames, activity, errors, or RPC projections.
- Every production behavior follows witnessed RED → GREEN. Each task ends with focused tests, typecheck, `git diff --check`, a self-review, and one commit. Do not modify unrelated files or rename the existing verification script merely for tidiness.

---

### Task 1: Central command catalog and layered OpenTUI keymap

**Files:**
- Modify: `packages/control-plane/package.json`
- Modify: `bun.lock`
- Create: `packages/control-plane/src/commands/types.ts`
- Create: `packages/control-plane/src/commands/catalog.ts`
- Create: `packages/control-plane/src/commands/keymap.tsx`
- Create: `packages/control-plane/test/commands.test.tsx`

**Interfaces:**
- Produces `CommandId`, `CommandScope`, `CommandTextKey`, `CommandDefinition`, `commandCatalog`, `commandsForScope(scope)`, and `resolveSlash(input, scope)`.
- Produces `<MuxviaKeymapProvider presenter?>`, `useMuxviaKeymap()`, and `useCommandLayer({ scope, priority, enabled?, handlers, onExecute? })`.
- All later UI calls `useMuxviaKeymap().dispatchCommand(id)`; no later task adds a second command dispatcher.

- [ ] **Step 1: Add exact command/keymap tests before the dependency or modules exist**

Create `packages/control-plane/test/commands.test.tsx` with a real OpenTUI test renderer. The test must register one handler for `target.codex.open`, invoke it through a `1` shortcut, `resolveSlash("/codex", "home")` followed by `dispatchCommand`, and a palette-style direct `dispatchCommand`, and record the dispatched command names from `keymap.on("dispatch", ...)`:

```tsx
test("shortcut slash and palette dispatch one target command identity", async () => {
  const executed: string[] = []
  const dispatched: string[] = []
  // commandHarness mounts MuxviaKeymapProvider and a real home command layer,
  // subscribes to dispatch events, and exposes its renderer and keymap.
  const setup = await commandHarness({
    handlers: { "target.codex.open": () => executed.push("target.codex.open") },
    onDispatch: (name) => dispatched.push(name),
  })
  try {
    setup.mockInput.pressKey("1")
    await setup.renderOnce()
    setup.keymap.dispatchCommand(resolveSlash("/codex", "home")!)
    setup.keymap.dispatchCommand("target.codex.open")
    expect(executed).toEqual([
      "target.codex.open",
      "target.codex.open",
      "target.codex.open",
    ])
    expect(dispatched.filter((name) => name === "target.codex.open")).toHaveLength(3)
  } finally {
    setup.renderer.destroy()
  }
})
```

Add separate assertions that `ctrl+p` resolves `command.palette.show`, `ctrl+x` then `b` resolves `target.sidebar.toggle`, `resolveSlash("/provider", "claude")` returns `undefined`, and an overlay layer at priority `300` consumes `escape` before route/global layers.

- [ ] **Step 2: Run the focused test and witness RED**

Run:

```bash
bun test packages/control-plane/test/commands.test.tsx
```

Expected: FAIL because `src/commands/catalog.ts` and `src/commands/keymap.tsx` do not exist (and `@opentui/keymap` is not installed).

- [ ] **Step 3: Pin the dependency and define the one command catalog**

Add `"@opentui/keymap": "0.4.3"` to `packages/control-plane/package.json` dependencies and run `bun install` to update `bun.lock`.

Define these exact stable IDs and default routes in `commands/catalog.ts`:

```ts
export const commandCatalog = [
  command("command.palette.show", "command.palette", null, ["ctrl+p"], ["global"], true),
  command("target.codex.open", "command.target.codex", "codex", ["1"], ["home"], true),
  command("target.claude.open", "command.target.claude", "claude", ["2"], ["home"], true),
  command("target.home", "command.target.home", "home", ["escape"], ["codex", "claude"], true),
  command("target.sidebar.toggle", "command.target.sidebar", "sidebar", ["<leader>b"], ["codex"], true),
  command("provider.create", "command.provider.create", "provider", ["<leader>p"], ["codex"], true),
  command("provider.save", "command.provider.save", null, ["return"], ["editor"], false),
  command("provider.cancel", "command.provider.cancel", null, ["escape"], ["editor"], false),
  command("target.takeover.apply", "command.takeover.apply", "takeover", ["<leader>a"], ["codex"], true),
  command("app.exit.request", "command.app.exit", "quit", ["ctrl+c", "<leader>q"], ["global"], true),
  command("overlay.close", "command.overlay.close", null, ["escape"], ["overlay"], false),
  command("app.exit.confirm", "command.app.exit.confirm", null, ["return", "y"], ["confirm"], false),
  command("app.exit.cancel", "command.app.exit.cancel", null, ["escape", "n"], ["confirm"], false),
] as const satisfies readonly CommandDefinition[]
```

`CommandDefinition` has exact fields `{ id, titleKey, descriptionKey, slashName, bindings, scopes, palette }`. The `command` helper derives `descriptionKey` by appending `.description` to the supplied title key. `slashName` is `string | null`; do not overload empty arrays as two different field types.

`resolveSlash` trims surrounding whitespace, requires exactly one `/name` token with no arguments in T02, and only returns a command whose scope includes the current scope. Unknown and out-of-scope slash names return `undefined`.

- [ ] **Step 4: Implement the pinned keymap provider and layer adapter**

`MuxviaKeymapProvider` must call `createDefaultOpenTuiKeymap(renderer)`, then register `registerBaseLayoutFallback`, `registerTimedLeader({ trigger: "ctrl+x", name: "leader", timeoutMs: 2_000 })`, `registerEscapeClearsPendingSequence`, and `registerBackspacePopsPendingSequence`. Dispose all registrations on Solid cleanup.

`useCommandLayer` must use `@opentui/keymap/solid`’s `useBindings`, register commands with stable `textKey`, `slashName`, `namespace: "palette"`, and `hidden` metadata, and register each shortcut as `{ key, cmd: id }`. Its optional `enabled` accessor defaults true and is attached to every command/binding through one registered keymap field, so inactive background layers disappear from active queries and dispatch. Its command `run` calls the supplied handler once and reports `onExecute(id)` once. Missing handlers return `false`; they do not throw or fall through into another action.

`MuxviaKeymapProvider` accepts an optional `presenter(textKeys) => ({ title, description })`; without it, metadata uses the stable title/description keys. Task 2 supplies a catalog-backed presenter from `App`. This keeps localization scoped to one render tree and avoids mutable global language state.

- [ ] **Step 5: Verify GREEN and commit**

Run:

```bash
bun install --frozen-lockfile
bun test packages/control-plane/test/commands.test.tsx
bun run typecheck
git diff --check
```

Expected: command tests pass, the installed lock entry is exactly `@opentui/keymap@0.4.3`, and typecheck/diff checks are clean.

Commit:

```bash
git add packages/control-plane/package.json bun.lock packages/control-plane/src/commands packages/control-plane/test/commands.test.tsx
git commit -m "feat: centralize control plane commands"
```

### Task 2: Complete English and Simplified-Chinese message catalogs

**Files:**
- Create: `packages/control-plane/src/i18n/en.ts`
- Create: `packages/control-plane/src/i18n/zh-cn.ts`
- Create: `packages/control-plane/src/i18n/index.ts`
- Create: `packages/control-plane/test/localization.test.ts`

**Interfaces:**
- Consumes the command text-key literals from Task 1.
- Produces `Locale = "en" | "zh-CN"`, `MessageKey`, `Translator`, `resolveLocale(env)`, `createTranslator(locale)`, `messageKeyForProblem(code)`, and `labelTargetState(t, value)`.
- `Translator` is `(key: MessageKey, values?: Record<string, string | number>) => string` and replaces exact `{name}` placeholders without evaluating markup.

- [ ] **Step 1: Write catalog parity, locale, interpolation, and error-code RED tests**

Create tests that assert:

```ts
test("catalogs have identical keys", () => {
  expect(Object.keys(zhCN).sort()).toEqual(Object.keys(en).sort())
})

test("locale precedence and Chinese variants are deterministic", () => {
  expect(resolveLocale({ LANG: "zh_CN.UTF-8" })).toBe("zh-CN")
  expect(resolveLocale({ LANG: "en_US.UTF-8", LC_MESSAGES: "zh-CN" })).toBe("zh-CN")
  expect(resolveLocale({ LANG: "zh_CN", LC_ALL: "C" })).toBe("en")
  expect(resolveLocale({})).toBe("en")
})

test("stable backend codes map to localized copy without backend messages", () => {
  const t = createTranslator("zh-CN")
  expect(t(messageKeyForProblem("stale-revision"))).toContain("状态")
  expect(messageKeyForProblem("unrecognized-code")).toBe("error.generic")
})
```

Also assert interpolation preserves Operator values verbatim: `t("activity.provider.saved", { name: "模型-A" })` contains `模型-A` in both locales.

- [ ] **Step 2: Run and witness RED**

Run:

```bash
bun test packages/control-plane/test/localization.test.ts
```

Expected: FAIL because the catalogs do not exist.

- [ ] **Step 3: Implement flat, type-equal catalogs**

The English catalog is the type source. It must contain every key below; the Chinese catalog uses `satisfies Record<MessageKey, string>` so missing/extra keys fail typecheck:

```ts
export const en = {
  "command.palette.title": "Commands",
  "command.palette.search": "Search commands",
  "command.palette": "Open commands",
  "command.target.codex": "Open Codex CLI",
  "command.target.claude": "Open Claude Code",
  "command.target.home": "Return home",
  "command.target.sidebar": "Toggle target context",
  "command.provider.create": "Create Provider",
  "command.provider.save": "Save Provider",
  "command.provider.cancel": "Cancel Provider editor",
  "command.takeover.apply": "Apply Target Takeover",
  "command.app.exit": "Exit Muxvia",
  "command.overlay.close": "Close dialog",
  "command.app.exit.confirm": "Discard and exit",
  "command.app.exit.cancel": "Keep editing",
  "home.tip": "Tip: type /codex or /claude, or press ctrl+p for all commands.",
  "home.footer.targets": "Local targets · no telemetry",
  "home.target.codex": "Codex CLI",
  "home.target.codex.detail": "Providers, configuration, and routed model access",
  "home.target.claude": "Claude Code",
  "home.target.claude.detail": "Selectable context · management arrives in a later slice",
  "target.codex": "Codex CLI",
  "target.claude": "Claude Code",
  "target.claude.unavailable": "Claude Code management is not available in this build.",
  "target.claude.return": "Use Esc or /home to return.",
  "prompt.home": "Choose a target or enter a command…",
  "prompt.target": "Run a target action…",
  "prompt.unknown": "Unknown or unavailable command: {command}",
  "prompt.meta.home": "Targets",
  "prompt.meta.codex": "Codex · Control Plane",
  "prompt.meta.claude": "Claude · Preview",
  "prompt.hint.commands": "ctrl+p commands",
  "prompt.hint.exit": "ctrl+c exit",
  "prompt.hint.back": "esc back",
  "prompt.hint.sidebar": "ctrl+x b context",
  "status.mode": "Mode",
  "status.current": "Current",
  "status.serving": "Serving",
  "status.service": "Service",
  "status.config": "Config",
  "status.snapshot": "Snapshot",
  "state.unmanaged": "Unmanaged",
  "state.takeover": "Takeover",
  "state.direct": "Direct",
  "state.running": "Running",
  "state.ready": "Ready",
  "state.inactive": "Inactive",
  "state.active": "Active",
  "state.applied": "Applied",
  "state.recovery-required": "Recovery required",
  "state.unknown": "Unknown ({value})",
  "value.none": "—",
  "provider.heading": "Provider",
  "provider.model": "Model",
  "provider.credential": "Credential",
  "provider.credential.present": "Present",
  "provider.credential.absent": "Absent",
  "provider.editor.title": "Provider",
  "provider.field.name": "Name",
  "provider.field.base-url": "Base URL",
  "provider.field.model": "Model",
  "provider.field.credential": "Credential",
  "provider.placeholder.name": "Fixture Provider",
  "provider.placeholder.base-url": "https://provider.example/v1",
  "provider.placeholder.model": "gpt-model",
  "provider.placeholder.credential": "API credential",
  "provider.editor.help": "Enter save · Esc cancel",
  "provider.editor.saving": "Saving…",
  "activity.heading": "Recent activity",
  "activity.provider.saved": "Provider saved: {name}",
  "activity.takeover.applied": "Target Takeover applied: {name}",
  "activity.state.updated": "Target state updated.",
  "activity.provider.required": "Create a Provider before applying Target Takeover.",
  "activity.restart": "Restart Codex to use the managed configuration.",
  "activity.applying": "Applying Target Takeover…",
  "sidebar.heading": "Target context",
  "sidebar.revision": "Management revision",
  "sidebar.sequence": "View sequence",
  "sidebar.endpoint": "Takeover endpoint",
  "sidebar.recovery": "Recovery",
  "exit.title": "Discard Provider draft?",
  "exit.message": "Unsaved Provider fields will be lost.",
  "exit.confirm": "Discard and exit",
  "exit.cancel": "Keep editing",
  "error.stale-revision": "Target state changed. Retry the action.",
  "error.invalid-provider": "Provider details are invalid. Check the fields and retry.",
  "error.incomplete-provider": "Complete the required Provider fields and retry.",
  "error.recovery-required": "Resolve Target recovery before making changes.",
  "error.service-unavailable": "Routing Service is unavailable.",
  "error.incompatible-target-cli": "This Target CLI is incompatible with managed changes.",
  "error.untested-target-cli": "This Target CLI version is untested; review the compatibility warning.",
  "error.unsupported-configuration-home": "Only the default Target configuration home is supported.",
  "error.configuration-collision": "The managed configuration namespace is already owned by another entry.",
  "error.configuration-write-failed": "Managed configuration could not be written safely.",
  "error.internal-failure": "The action failed internally. Use the correlation reference from diagnostics.",
  "error.generic": "Action failed ({code}). Review Target state and retry.",
} as const
```

Translate every value naturally into Simplified Chinese; keep command tokens, `Codex`, `Claude`, URLs, and placeholders unchanged where they are identifiers.

- [ ] **Step 4: Implement locale and safe lookup helpers**

`resolveLocale` uses `LC_ALL`, then `LC_MESSAGES`, then `LANG`; a normalized value beginning `zh`, `zh-cn`, or `zh_cn` selects `zh-CN`, all others select `en`. `messageKeyForProblem` maps every explicit `error.*` code listed above and returns `error.generic` otherwise. `labelTargetState` maps known state literals to `state.*` and formats unknown values through `state.unknown`.

Add `createCommandPresenter(t)` returning localized `{ title, description }` metadata for Task 1's `MuxviaKeymapProvider presenter` prop. `App` passes the presenter for its locale; no catalog state is global or shared between tests.

- [ ] **Step 5: Verify and commit**

Run:

```bash
bun test packages/control-plane/test/localization.test.ts
bun run typecheck
git diff --check
```

Commit:

```bash
git add packages/control-plane/src/i18n packages/control-plane/test/localization.test.ts
git commit -m "feat: localize control plane shell"
```

### Task 3: OpenCode-style Home, target routing, and action prompt

**Files:**
- Create: `packages/control-plane/src/ui/logo.tsx`
- Create: `packages/control-plane/src/ui/action-prompt.tsx`
- Create: `packages/control-plane/src/ui/home.tsx`
- Create: `packages/control-plane/src/ui/claude-context.tsx`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/app.tsx`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/app-lifecycle.test.tsx`

**Interfaces:**
- Produces `ShellRoute = { kind: "home" } | { kind: "target"; target: "codex" | "claude" }`.
- Produces `<ActionPrompt scope placeholder metadata onUnknown>`; exact slash submission calls `useMuxviaKeymap().dispatchCommand(resolveSlash(...))`.
- `AppProps` remains centered on the existing `TargetSession` and adds optional `locale?: Locale`; default locale is resolved once by `run()` from process environment.

- [ ] **Step 1: Replace direct-start expectations with failing Home/route tests**

Update the renderer tests so first paint asserts a Home frame in this order:

```ts
expectInOrder(frame, [
  "MUXVIA",
  "Codex CLI",
  "Providers, configuration, and routed model access",
  "Claude Code",
  "Selectable context",
  "Choose a target or enter a command",
  "ctrl+p commands",
])
expect(frame).not.toContain("Mode       Unmanaged")
expect(frame).not.toContain("Overview")
expect(frame).not.toContain("Providers | Routing")
```

Then press `1`, render, and assert the existing Codex status labels. Return with `Esc`; submit `/claude` through the prompt and assert the localized unavailable context plus `/home`/Esc return path. Assert the memory session received no extra control operation merely by selecting Claude.

Update lifecycle tests to wait for Home, not the old direct Codex frame.

- [ ] **Step 2: Run the focused tests and witness RED**

Run:

```bash
bun test packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
```

Expected: FAIL because the current App renders Codex immediately and has no Home or Claude context.

- [ ] **Step 3: Build the responsive Home identity and prompt**

`Logo` renders a four-row block wordmark when width is at least 60, a bold `MUXVIA` wordmark otherwise, and nothing beyond the renderable cell at `1x1`. It uses only the existing monospace scale, `theme.primary`, `theme.text`, and cell spacing.

`Home` uses flexible blank space above and below the identity, two bare quick rows (`[1] Codex CLI`, `[2] Claude Code`), one left-rail `ActionPrompt`, one tip, and a bare footer. It does not draw borders around rows or add navigation chrome.

`ActionPrompt` uses `backgroundColor={theme.element}`, a left primary rail, horizontal padding of two cells when width permits, an OpenTUI `<input>`, one metadata/hint line, and no surrounding card border. On submit it clears the visible input, resolves the slash for the current scope, and dispatches the resolved command. Unknown input calls `onUnknown` with the original input but never executes a fallback action.

- [ ] **Step 4: Replace App’s direct keyboard screen with route command layers**

Wrap the shell in `MuxviaKeymapProvider`. Initialize route to `{ kind: "home" }`. Register:

- global handlers for `command.palette.show` (returns `false` until Task 4 supplies the overlay handler) and `app.exit.request` (clean renderer destruction);
- Home handlers that select Codex or Claude;
- Codex/Claude route handlers for `target.home`;
- Codex handlers for existing Provider/create and takeover behavior plus local sidebar state; these bodies call the exact pre-existing form/action functions through the named command IDs.

Keep the real Codex `TargetSession` subscription alive exactly once for the App lifetime. Claude selection changes only local `ShellRoute` and never touches the session or RPC types.

In `run`, compute `const locale = resolveLocale(process.env)` and render `<App session={session!} locale={locale} />`. Do not add a locale flag or persistence in T02.

- [ ] **Step 5: Verify all existing lifecycle guarantees and commit**

Run:

```bash
bun test packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
bun run typecheck
git diff --check
```

Expected: Home, Codex, and Claude shell tests pass; startup deadlines, sidecar spawn, connection cancellation, session closure, and renderer cleanup tests remain green.

Commit:

```bash
git add packages/control-plane/src/app.tsx packages/control-plane/src/ui packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
git commit -m "feat: add target home shell"
```

### Task 4: One overlay stack, focus restoration, and command palette

**Files:**
- Create: `packages/control-plane/src/ui/overlay-stack.tsx`
- Create: `packages/control-plane/src/ui/command-palette.tsx`
- Create: `packages/control-plane/test/overlays.test.tsx`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/ui/action-prompt.tsx`
- Modify: `packages/control-plane/test/commands.test.tsx`

**Interfaces:**
- Produces `OverlayEntry { id: string; element: JSX.Element; onClose?: () => void }` and `OverlayController { depth; push; replace; closeTop; clear }` through `<OverlayProvider>`/`useOverlay()`.
- Produces `<CommandPalette />`, which queries active `namespace: "palette"` entries from the one Muxvia keymap and dispatches the selected entry’s exact command name.

- [ ] **Step 1: Write RED tests for top-only rendering, modal precedence, and focus restoration**

The overlay test must mount a focused prompt input, push overlay A, push overlay B, and assert only B’s text is visible. Pressing `Esc` must close only B; pressing again closes A; after the second close, `renderer.currentFocusedRenderable` must be the original still-live prompt input.

Add a nested-modal precedence test: while overlay B is open, pressing `1`, `ctrl+p`, or `<leader>b` cannot execute Home/Target/global handlers; only overlay commands execute.

Extend the command identity test to select `target.codex.open` from the rendered palette and prove the keymap dispatch event name is exactly `target.codex.open`, matching shortcut and slash paths. The harness sets `enabled: () => overlay.depth === 0` on global/route/editor layers; the overlay layer alone remains enabled while depth is nonzero.

- [ ] **Step 2: Run and witness RED**

Run:

```bash
bun test packages/control-plane/test/overlays.test.tsx packages/control-plane/test/commands.test.tsx
```

Expected: FAIL because no overlay provider or palette exists.

- [ ] **Step 3: Implement the single stack and focus contract**

On the first `push`/`replace`, capture `renderer.currentFocusedRenderable` and blur it. The Provider renders only `stack.at(-1)`. `closeTop` invokes only that entry’s `onClose`, removes it, and restores focus only when the stack becomes empty. Focus restoration runs on the next microtask and verifies the renderable is not destroyed and is still under `renderer.root`. `clear` calls each registered `onClose` once, empties the stack, and restores focus once.

The overlay surface fills the terminal with a `RGBA.fromInts(0, 0, 0, 150)` backdrop and centers its panel horizontally near one-quarter height. Clamp medium width to `max(1, min(60, terminalWidth - 2))`; never use border, radius, shadow, or blur. Register overlay commands at priority `300`.

- [ ] **Step 4: Implement the command palette on the same stack**

The palette queries `getCommandEntries({ visibility: "active", namespace: "palette" })`, excludes itself and hidden commands, filters localized title/description by the focused search input, and caps visible rows to `max(1, floor(height / 2) - 6)`. Up/down changes the selected row, Enter clears the overlay then calls `keymap.dispatchCommand(entry.command.name)`, and the selected row uses a full `theme.primary` fill with bold contrasting text.

Wire `command.palette.show` to `overlay.replace(<CommandPalette />)`. `ActionPrompt` disables focus while an overlay exists so background input cannot consume modal keys. `App` also passes `enabled: () => overlay.depth === 0` to global, route, and editor command layers; modal safety therefore covers unrelated keys, direct command dispatch, and active palette queries rather than relying only on matching-key priority.

- [ ] **Step 5: Verify and commit**

Run:

```bash
bun test packages/control-plane/test/overlays.test.tsx packages/control-plane/test/commands.test.tsx packages/control-plane/test/app-render.test.tsx
bun run typecheck
git diff --check
```

Commit:

```bash
git add packages/control-plane/src/ui/overlay-stack.tsx packages/control-plane/src/ui/command-palette.tsx packages/control-plane/src/ui/action-prompt.tsx packages/control-plane/src/ui/app.tsx packages/control-plane/test
git commit -m "feat: add command overlays"
```

### Task 5: Named Provider editor actions and dirty-exit confirmation

**Files:**
- Create: `packages/control-plane/src/ui/exit-confirmation.tsx`
- Modify: `packages/control-plane/src/ui/provider-form.tsx`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/app-lifecycle.test.tsx`

**Interfaces:**
- `ProviderForm` produces `ProviderFormRef { isDirty(): boolean; clearSensitive(): void; focus(): void }` and reports `onDirtyChange(dirty)`.
- `<ExitConfirmation onConfirm onCancel>` uses only `app.exit.confirm` and `app.exit.cancel` commands at priority `300` in the confirmation overlay.
- The global `app.exit.request` handler is the sole exit request path used by Ctrl+C, `/quit`, and the palette.

- [ ] **Step 1: Write RED tests for dirty draft safety**

Change the old “Ctrl+C exits while Provider form owns focus” expectation into three tests:

1. Open Codex, submit `/provider`, type at least one normal field and the credential sentinel, then press Ctrl+C. Assert the renderer remains alive and the confirmation overlay is visible.
2. Press `n`/Esc. Assert the overlay closes, the Provider draft remains (normal fields still render), the credential sentinel never renders, and focus returns to the editor.
3. Reopen confirmation and press `y`/Enter. Assert renderer/session cleanup happens once and no captured frame, error, activity entry, or serialized test value contains the credential sentinel.

Add clean-shell cases proving Ctrl+C and `/quit` destroy immediately without confirmation.

- [ ] **Step 2: Run and witness RED**

Run:

```bash
bun test packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
```

Expected: dirty Ctrl+C currently destroys immediately, so the confirmation assertion fails.

- [ ] **Step 3: Move editor actions onto named command bindings**

Keep local character and paste handling only for the masked credential because OpenTUI 0.4.3 has no password/mask prop. Replace editor-global Enter/Esc logic with a priority-`200` `useCommandLayer("editor")`: `provider.save` calls the existing save operation; `provider.cancel` clears the credential and closes the editor. Tab/focus movement may remain focused-editor behavior but must consume its key event.

Any change to name, base URL, model, or credential makes the form dirty. Successful submit clears the credential before awaiting RPC and resets dirty only after an applied outcome. Cancel and unmount clear the credential. A failed save leaves non-secret fields available to correct but never restores secret bytes.

- [ ] **Step 4: Implement the confirmation through the overlay stack**

`app.exit.request` checks the live Provider form ref. If not dirty, it destroys the renderer. If dirty, it replaces the current overlay with `<ExitConfirmation>`. Cancel closes the top overlay only. Confirm calls `clearSensitive()`, closes the editor/overlay state, then destroys the renderer. The confirm component renders localized title/body plus bare primary/muted choices; it does not create its own keyboard listener.

- [ ] **Step 5: Re-prove save/apply and credential hygiene, then commit**

Run:

```bash
bun test packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
bun run typecheck
rg -n "provider-secret-must-not-render|routing-secret-must-not-render" packages/control-plane/src && exit 1 || true
git diff --check
```

Expected: dirty/clean exit tests pass, Provider save and takeover tests remain green, and the production-source secret sentinel scan is empty.

Commit:

```bash
git add packages/control-plane/src/ui packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
git commit -m "feat: protect dirty provider drafts"
```

### Task 6: Continuous Target feed, contextual sidebar, and extreme-size rendering

**Files:**
- Create: `packages/control-plane/src/ui/target-sidebar.tsx`
- Create: `packages/control-plane/test/responsive-shell.test.tsx`
- Modify: `packages/control-plane/src/ui/target-view.tsx`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/theme.ts`
- Modify: `packages/control-plane/test/app-render.test.tsx`

**Interfaces:**
- Produces `ActivityEntry { id: number; kind: "info" | "success" | "warning" | "error"; messageKey: MessageKey; values?: Record<string, string | number> }`.
- `TargetView` receives `view`, `activities`, and `t`; it renders status and temporary activity only, never persistence-shaped data or credentials.
- `TargetSidebar` receives the current secret-free `TargetView` and translator; it is context, not navigation.

- [ ] **Step 1: Add the exact size matrix and visual-exclusion RED tests**

Create one table-driven renderer test that starts at `80x24`, then calls `resize(width, height)` and awaits a frame for each exact size:

```ts
const sizes = [[1, 1], [2, 2], [20, 5], [40, 10], [80, 24], [121, 30]] as const
for (const [width, height] of sizes) {
  setup.resize(width, height)
  await setup.renderOnce()
  expect(() => setup.captureCharFrame()).not.toThrow()
}
```

Assert no frame contains `Terminal too small`, `Overview`, top tabs, or a global status strip. At `80x24`, `Target context` is absent. At `121x30`, it is visible by default; after Ctrl+X then B it disappears while the main status labels remain. Home and Claude contexts also pass the same no-crash matrix.

- [ ] **Step 2: Run and witness RED**

Run:

```bash
bun test packages/control-plane/test/responsive-shell.test.tsx
```

Expected: FAIL because no contextual sidebar or responsive shell contract exists.

- [ ] **Step 3: Build the continuous feed and activity projection**

The Codex Target route is a row with one flexible main `scrollbox` and an optional sidebar. The feed order is:

1. target identity;
2. six distinct status lines;
3. Provider declarations;
4. restart/prob­lem messages mapped through catalog keys;
5. `Recent activity` entries in append order.

Append one temporary activity after Provider save, takeover apply, explicit command failure, and a subscribed `TargetView` whose `viewSequence` increases. Cap the in-memory list at 50 entries by dropping oldest entries. Do not store activity, add timestamps, read SQLite, or turn it into T09 request history.

Do not render backend `problem.message`; render the localized key selected from `problem.code` plus the stable code only in the generic fallback.

- [ ] **Step 4: Add the target-only wide sidebar and complete theme tokens**

Extend `theme.ts` with the pinned tokens:

```ts
secondary: "#5c9cf5",
accent: "#9d7cd8",
info: "#56b6c2",
borderSubtle: "#3c3c3c",
border: "#484848",
borderActive: "#606060",
```

Initialize sidebar-open to true. Render it only when route is Codex, width is greater than 120, and sidebar-open is true. Clamp width to at most 42 cells and at least 1 available cell. It shows revision, view sequence, takeover endpoint, and recovery status; it contains no links or commands to other product sections.

Use `useTerminalDimensions()` at the shell root. At widths below five, remove horizontal padding. All computed widths/heights use `Math.max(0, ...)`; no negative `maxWidth`, fixed height, or list limit reaches OpenTUI.

- [ ] **Step 5: Verify render, localization, and regression behavior, then commit**

Run:

```bash
bun test packages/control-plane/test/responsive-shell.test.tsx packages/control-plane/test/app-render.test.tsx packages/control-plane/test/localization.test.ts
bun run typecheck
git diff --check
```

Commit:

```bash
git add packages/control-plane/src/theme.ts packages/control-plane/src/ui packages/control-plane/test
git commit -m "feat: add responsive target stream"
```

### Task 7: Idempotent signal cleanup and real PTY lifecycle proof

**Files:**
- Modify: `packages/control-plane/src/app.tsx`
- Modify: `packages/control-plane/test/app-lifecycle.test.tsx`
- Create: `packages/control-plane/test/fixtures/pty-control-plane.tsx`
- Create: `packages/control-plane/test/terminal-lifecycle.pty.test.ts`
- Modify: `tests/e2e/walking-skeleton.test.ts`
- Modify: `packages/control-plane/test/walking-skeleton.e2e.tsx`

**Interfaces:**
- Produces exported `createProductionRenderer(): Promise<CliRenderer>` for the real PTY fixture; it retains exact production options and never accepts a `screenMode`.
- Adds `SignalName = "SIGHUP" | "SIGINT" | "SIGTERM"` and `SignalSource { listen(name, handler): () => void }` to `RunPorts`.
- The PTY fixture uses the production `run`, production renderer factory, and a fake `TargetSession`; it does not duplicate terminal setup/teardown.

- [ ] **Step 1: Add signal and idempotent-cleanup RED unit tests**

Extend lifecycle ports with a manual signal source. For each signal, start `run`, wait for Home, emit the signal twice, and assert:

- listener unregistered once;
- session closed once;
- title cleared;
- renderer destroyed once;
- `process.exit` not called;
- no sidecar stop operation exists or runs.

Add a render rejection after the renderer is mounted and assert the same cleanup before the original error is rethrown.

- [ ] **Step 2: Run unit tests and witness RED**

Run:

```bash
bun test packages/control-plane/test/app-lifecycle.test.tsx
```

Expected: signal tests fail because `RunPorts` has no signal seam and `run` registers no cleanup listeners.

- [ ] **Step 3: Implement one cleanup owner**

Export the existing renderer creation as:

```ts
export function createProductionRenderer(): Promise<CliRenderer> {
  return createCliRenderer({
    exitOnCtrlC: false,
    useKittyKeyboard: {},
    autoFocus: false,
  })
}
```

Production `SignalSource.listen` uses `process.on`/`process.off`. Immediately after renderer creation, register SIGHUP/SIGINT/SIGTERM handlers that only call `renderer.destroy()` if it is still live. In `finally`, unregister every listener before clearing title/closing session/destroying. Keep the existing `destroyed` promise and `isDestroyed` checks so all concurrent causes converge on one cleanup.

- [ ] **Step 4: Write the real Bun.Terminal fixture and RED PTY table**

The fixture creates a secret-free fake Codex `TargetSession`, passes it through injected `RunPorts`, and calls production `run`. Its renderer port delegates to `createProductionRenderer()`. After `render(node, renderer)` mounts, send IPC `{ type: "ready", screenMode: renderer.screenMode }`. On session close send `{ type: "session-closed" }` exactly once.

For the exceptional scenario only, the render port waits after mount for IPC `{ type: "crash" }` and then rejects with `new Error("injected-render-failure")`; the fixture catches it and sets exit code `70` after `run` finishes cleanup.

The parent test creates a new `Bun.Terminal` and child for each case, explicitly sets `TERM=xterm-256color`, and runs this sequential table:

```ts
const cases = [
  { name: "default-normal", screen: undefined, exit: "command", alternate: true },
  { name: "explicit-alternate", screen: "true", exit: "command", alternate: true },
  { name: "explicit-main", screen: "false", exit: "command", alternate: false },
  { name: "sigint", screen: undefined, exit: "SIGINT", alternate: true },
  { name: "sigterm", screen: undefined, exit: "SIGTERM", alternate: true },
  { name: "exception", screen: undefined, exit: "exception", alternate: true },
] as const
```

Before spawn record `terminal.localFlags`; on `ready` record running flags. Trigger normal exit by writing `/quit\r`, signals with `proc.kill("SIGINT" | "SIGTERM")`, and exception with `proc.send({ type: "crash" })`. Await `proc.exited` with an eight-second deadline. Before closing the terminal, record final flags.

For every case assert running flags differ from baseline, final flags equal baseline, one `session-closed` message arrived, `screenMode !== "split-footer"`, and the process exits in time. Join all `data` callback byte slices before checking ANSI. Alternate cases contain both `\x1b[?1049h` and `\x1b[?1049l`; main mode contains neither. The exception exits `70`; signal cases do not assert a platform-specific numeric exit code. In test `finally`, SIGKILL is timeout cleanup only and is never an acceptance path.

Run and witness RED:

```bash
bun test packages/control-plane/test/terminal-lifecycle.pty.test.ts
```

Expected: FAIL until the fixture and signal-safe production lifecycle exist.

- [ ] **Step 5: Adapt the real Codex walking skeleton to Home and named commands**

In both walking-skeleton tests, first choose Codex through `/codex` or shortcut `1`. Replace the legacy `p` and `a` keys with `/provider` and `/takeover` submissions through `ActionPrompt`; form character entry remains unchanged. Keep every prior assertion for real process startup, Provider persistence, takeover configuration, wrong/valid routing credentials, SSE/header/body transparency, Serving push, secret scans, real-home canaries, and explicit shutdown.

After destroying the Control Plane renderer/session, retain the existing second authenticated model request and `service.exitCode === null` assertion. This remains the process-level proof that terminal cleanup does not stop a Routing Service required by active takeover.

- [ ] **Step 6: Run the complete local verification and self-review**

Run:

```bash
bun test packages/control-plane/test/terminal-lifecycle.pty.test.ts
bun test packages/control-plane/test
bun test tests/e2e/walking-skeleton.test.ts
bun run typecheck
bun run verify
git diff --check
git status --short
```

The full `bun run verify` requires real local UDS/loopback permissions. Expected: all Rust tests remain unchanged/green; all TypeScript renderer, command, overlay, localization, responsive, lifecycle, PTY, and real-process tests pass; no worktree changes exist outside the planned files.

Self-review the final branch against GitHub #3:

- first screen Home with two selectable contexts;
- no forbidden dashboard chrome;
- command identity proven across shortcut/slash/palette;
- one overlay stack and focus restoration;
- dirty Ctrl+C confirmation;
- `en`/`zh-CN` key parity and localized backend-code handling;
- all six requested terminal sizes;
- alternate/main PTY plus normal/SIGINT/SIGTERM/exception restoration;
- active Routing Service survives Control Plane exit;
- no Rust/RPC/Claude backend expansion.

- [ ] **Step 7: Commit the terminal and end-to-end slice**

```bash
git add packages/control-plane/src/app.tsx packages/control-plane/test tests/e2e/walking-skeleton.test.ts
git commit -m "feat: harden terminal shell lifecycle"
```

After this commit, use `superpowers:requesting-code-review` for a whole-branch Standards + #3 spec review. Fix every Critical/Important finding with a witnessed RED/GREEN, rerun `bun run verify`, then use `superpowers:finishing-a-development-branch` to merge locally, push `main`, wait for both macOS and Ubuntu GitHub Actions jobs, and close #3 only after both are green.
