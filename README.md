# Muxvia

Muxvia is a terminal-native control plane for managing model access for AI coding command-line tools. It brings provider, model, routing, health, and configuration management into an OpenCode-style TUI while the managed CLIs continue running in their own terminals.

## Status

Muxvia is pre-release software. The T01 walking skeleton is implemented for development and test use; it is not a production release.

T01 proves one Codex Target Takeover path on macOS and Linux: a Provider can be created in the terminal UI, applied to the default Codex configuration, and used through an authenticated local Responses route. The Routing Service runs separately from the Control Plane and keeps an active takeover available after the TUI exits.

## Build and test T01

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

## T01 scope

T01 deliberately excludes the complete OpenCode-style shell, Claude Code, Direct Activation, Provider update/delete/reorder, Universal Providers, drift reconciliation UI, failover and circuit breaking, Subscription Accounts and the Subscription Bridge, usage and pricing, import/export, backups, release bundles, service handover, and production-grade detached lifecycle management.

## Design sources

- [Domain language](./CONTEXT.md)
- [Architecture decision records](./docs/adr/)
- [Codex Subscription Bridge compatibility and risk notice](./docs/subscription-bridge.md)
- [Accepted OpenCode-style Control Plane](./docs/adr/0011-build-a-focused-opentui-control-plane.md)

## License

Muxvia is licensed under the [MIT License](./LICENSE).
