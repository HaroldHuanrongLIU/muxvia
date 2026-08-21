# Muxvia

Muxvia is a terminal-native control plane for managing model access for AI coding command-line tools. It brings provider, model, routing, health, and configuration management into an OpenCode-style TUI while the managed CLIs continue running in their own terminals.

## Status

Muxvia is pre-release software. The repository implements the accepted macOS and glibc Linux product slices, including Codex and Claude provider management, direct and takeover activation, failover, subscription accounts, import/export, private recovery backups, service handover, and auditable release bundles.

## Build and test

Requirements: Rust stable and Bun 1.3.14.

```sh
bun install --frozen-lockfile
cargo build -p muxvia-routing
bun run verify
```

The automated process and end-to-end tests create a temporary `HOME`. They do not use the Operator's `~/.codex` or `~/.muxvia` configuration.

For a disposable development launch, build the private service first and pass absolute paths to the Control Plane:

```sh
MUXVIA_DEMO_ROOT="$(mktemp -d)"
mkdir -p "$MUXVIA_DEMO_ROOT/home"
HOME="$MUXVIA_DEMO_ROOT/home" bun run packages/control-plane/src/index.tsx \
  --service "$(pwd)/target/debug/muxvia-routing" \
  --socket "$MUXVIA_DEMO_ROOT/home/.muxvia/run/control.sock"
printf 'Temporary demo home: %s\n' "$MUXVIA_DEMO_ROOT"
```

This command confines Managed Configuration to the printed temporary home. Remove only that printed directory after exiting and confirming it is under the system temporary directory.

## Releases

GitHub Release tags build one complete, integrity-bound archive for macOS arm64, macOS x86-64, Linux glibc arm64, and Linux glibc x86-64. The official Homebrew tap selects and verifies the matching macOS archive while retaining the complete bundle under Homebrew ownership. See [release and Homebrew installation, repair, unsigned macOS behavior, and notify-only updates](./docs/releases.md).

## Design sources

- [Domain language](./CONTEXT.md)
- [Architecture decision records](./docs/adr/)
- [Codex Subscription Bridge compatibility and risk notice](./docs/subscription-bridge.md)
- [Accepted OpenCode-style Control Plane](./docs/adr/0011-build-a-focused-opentui-control-plane.md)
- [Third-party notices](./THIRD_PARTY_NOTICES.md)
- [Source extraction manifest](./EXTRACTION_MANIFEST.json)

## License

Muxvia is licensed under the [MIT License](./LICENSE).
