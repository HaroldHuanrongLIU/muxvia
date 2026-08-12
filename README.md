# Muxvia

Muxvia is a terminal-native control plane for managing model access for AI coding command-line tools. It brings provider, model, routing, health, and configuration management into an OpenCode-style TUI while the managed CLIs continue running in their own terminals.

## Status

Muxvia is currently in the design and specification phase. There is no usable release yet.

The first release is scoped to Codex CLI and Claude Code on macOS and Linux. It will support direct provider activation, local routing and failover, and bridging a ChatGPT-backed Codex subscription to Claude Code.

## Design sources

- [Domain language](./CONTEXT.md)
- [Architecture decision records](./docs/adr/)
- [Accepted OpenCode-style Control Plane](./docs/adr/0011-build-a-focused-opentui-control-plane.md)

## License

Muxvia is licensed under the [MIT License](./LICENSE).
