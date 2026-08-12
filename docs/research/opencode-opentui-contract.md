# OpenCode and OpenTUI implementation contract

This note records the primary-source constraints behind Muxvia's accepted Control Plane. The pinned visual and interaction baseline is OpenCode commit [`0abbcdd`](https://github.com/anomalyco/opencode/tree/0abbcddac233e313bcb67608a527929910df861c); current OpenTUI is discussed only as a possible future dependency upgrade.

## Pinned runtime

The pinned OpenCode workspace uses Bun `1.3.14`, TypeScript, Solid, and `@opentui/core`, `@opentui/solid`, and `@opentui/keymap` `0.4.3`. Its TUI is an independent workspace using `jsxImportSource: "@opentui/solid"` and the OpenTUI Solid preload. Sources: [root package](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/package.json), [TUI package](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/package.json), [tsconfig](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/tsconfig.json), [bunfig](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/bunfig.toml).

OpenCode pins Solid `1.9.10` with a local patch while OpenTUI `0.4.3` declares a `1.9.12` peer. Muxvia should not inherit that private patch blindly; its lockfile resolution must be verified when the first TUI slice is scaffolded.

## Renderer and dimensions

OpenCode does not pass `screenMode` to `createCliRenderer`. OpenTUI `0.4.3` defaults to `alternate-screen`; `OTUI_USE_ALTERNATE_SCREEN=true|false` overrides configuration, while `split-footer` remains a separate mode. Muxvia may explicitly request alternate screen for clarity but must preserve the upstream environment override and must not select split-footer. Sources: [OpenCode renderer setup](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/app.tsx), [OpenTUI 0.4.3 renderer](https://github.com/anomalyco/opentui/blob/5803b2cfa2942c45a3aedbb3601754e27f2cdc68/packages/core/src/renderer.ts).

The OpenCode root binds directly to `useTerminalDimensions()` and updates on renderer resize. It has no global minimum-size blocking screen. Pinned layouts nevertheless contain expressions such as `width - 2` and `height / 2 - 6`; Muxvia's extreme-size no-crash guarantee therefore requires its own clamping and tests rather than assuming the baseline already provides it. Sources: [OpenCode root](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/app.tsx), [OpenTUI dimension hook](https://github.com/anomalyco/opentui/blob/5803b2cfa2942c45a3aedbb3601754e27f2cdc68/packages/solid/src/elements/hooks.ts).

## Commands and overlays

The pinned implementation builds named commands over `@opentui/keymap` layers and modes. The palette, slash commands, and shortcuts share a command catalog. Dialogs render only the top overlay, push modal keymap state, and restore focus when `Esc` or `Ctrl+C` closes the top entry. Sources: [keymap](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/keymap.tsx), [dialog stack](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/ui/dialog.tsx), [command palette](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/component/command-palette.tsx).

OpenCode still contains a few direct keyboard handlers. ADR 0011 is intentionally stricter: Muxvia global behavior must go through its centralized command and keymap interface. Dirty-editor confirmation on `Ctrl+C` is a Muxvia requirement, not inherited behavior.

## Visual primitives

- Home is a vertically centered identity and single prompt with flexible blank space and a sparse footer. [Source](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/routes/home.tsx)
- Prompt styling is a panel fill with a single left rail, textarea, metadata, and bare hints rather than a bordered card. [Source](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/component/prompt/index.tsx)
- A session-like route is a continuous stream with a fixed prompt. A contextual sidebar appears only in a sufficiently wide terminal and is not product navigation. [Source](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/src/routes/session/index.tsx)

These primitives support ADR 0011's Target-as-context shell; they do not justify copying OpenCode's coding-session domain.

## Test seam

OpenTUI `0.4.3` supplies `createTestRenderer`, Solid `testRender`, `renderOnce`, `captureCharFrame`, resize, and mock input/mouse facilities. OpenCode separately mocks `createCliRenderer` for signal, exit, and resource-release tests. Sources: [lifecycle tests](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/test/app-lifecycle.test.tsx), [keymap tests](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/tui/test/keymap.test.tsx).

Muxvia should test its Control Plane through the renderer-facing interface with:

- resize/no-throw cases at `1×1`, `2×2`, `20×5`, `40×10`, `80×24`, and `121×30`;
- captured-frame assertions for folding, hiding, wrapping, and scrolling;
- direct command/keymap and overlay-priority assertions; and
- a real PTY integration test for alternate/main screen selection and terminal restoration on every exit path.

The test renderer defaults to main-screen behavior and skips real terminal setup, so it cannot replace the PTY test.

## Distribution boundary

OpenCode compiles with Bun and its Solid plugin, embeds native workers, installs OpenTUI platform optional packages, and selects libc for Linux builds. [Source](https://github.com/anomalyco/opencode/blob/0abbcddac233e313bcb67608a527929910df861c/packages/opencode/script/build.ts)

Muxvia must retain its own Release Bundle contract: `muxvia`, `muxvia-routing`, and their binding manifest are one unit. The first release builds only macOS arm64/x64 and Linux glibc arm64/x64. Noninteractive diagnostics must dispatch before renderer creation and lazy-load the TUI. OpenCode's in-product upgrade behavior must not be reused because ADR 0043 permits notification but forbids automatic installation.

Current OpenTUI `main` was verified at [`de64d210`](https://github.com/anomalyco/opentui/tree/de64d210e4f0163720fc1fbfa838d4d1aad47d53), package version `0.5.1`, with richer test helpers and the same broad renderer concepts. It is not the accepted baseline. Selecting it requires an explicit dependency upgrade gate and rerunning layout, PTY, native artifact, and installation-channel verification. Sources: [current package](https://github.com/anomalyco/opentui/blob/de64d210e4f0163720fc1fbfa838d4d1aad47d53/packages/core/package.json), [standalone documentation](https://github.com/anomalyco/opentui/blob/de64d210e4f0163720fc1fbfa838d4d1aad47d53/packages/web/src/content/docs/reference/standalone-executables.mdx).
