<p align="center"><strong>Muxvia</strong> is a terminal-native control plane for managing model access across AI coding command-line tools.</p>
<p align="center">Manage providers, models, routing, health, and configuration for Codex CLI and Claude Code while each tool continues running in its own terminal.</p>

---

## Quickstart

### Installing and running Muxvia

Muxvia supports Apple silicon and Intel macOS, plus arm64 and x86-64 Linux systems using glibc.

Install Muxvia with one command:

```shell
curl -fsSL https://github.com/HaroldHuanrongLIU/muxvia/releases/latest/download/install.sh | sh
```

The installer detects your shell and adds `~/.muxvia/bin` to the appropriate profile with an idempotent Muxvia-managed block. It prints the command needed to use `muxvia` in the current terminal; new terminals pick up the configured `PATH` automatically. Set `MUXVIA_NO_PATH_UPDATE=1` to skip the profile change.

Muxvia can also be installed with npm or the official Homebrew tap:

```shell
# Install using npm (Node.js 18 or newer)
npm install --global --include=optional muxvia
```

```shell
# Install using Homebrew
brew tap HaroldHuanrongLIU/muxvia
brew install HaroldHuanrongLIU/muxvia/muxvia
```

Then run `muxvia` to open the Control Plane. To check the installation and detected Target CLIs, run:

```shell
muxvia doctor
```

<details>
<summary>You can also download a complete bundle from the <a href="https://github.com/HaroldHuanrongLIU/muxvia/releases/latest">latest GitHub Release</a>.</summary>

Choose the archive that matches your machine:

- macOS
  - Apple silicon/arm64: `muxvia-<version>-darwin-arm64.tar.gz`
  - Intel/x86-64: `muxvia-<version>-darwin-x64.tar.gz`
- Linux with glibc
  - arm64: `muxvia-<version>-linux-glibc-arm64.tar.gz`
  - x86-64: `muxvia-<version>-linux-glibc-x64.tar.gz`

Keep the extracted bundle together. `muxvia-routing` is a private sidecar and must not be moved, invoked, or upgraded separately.

</details>

> [!IMPORTANT]
> The initial macOS builds are unsigned and unnotarized. Review the [Gatekeeper guidance](./docs/releases.md#install-a-github-release-archive) before approving either bundled executable. Muxvia supports only the default `~/.codex` and `~/.claude` configuration homes in v0.1.

### Using Muxvia with Codex CLI and Claude Code

Run `muxvia`, select Codex CLI or Claude Code, and configure a provider. Muxvia can apply a provider directly to the selected CLI or enable Target Takeover for local routing, failover, route health, and subscription-backed access.

Muxvia manages configuration and routing; it does not host or wrap your coding sessions. Continue running Codex CLI and Claude Code in their own terminals.

<details>
<summary>Building and testing from source</summary>

Building Muxvia requires Rust stable and Bun 1.3.14:

```shell
bun install --frozen-lockfile
cargo build -p muxvia-routing
bun run verify
```

The automated process and end-to-end tests use a temporary `HOME`; they do not use the Operator's `~/.codex` or `~/.muxvia` configuration.

</details>

## Docs

- [**Installation, updates, and platform support**](./docs/releases.md)
- [**Domain language**](./CONTEXT.md)
- [**Architecture decisions**](./docs/adr/)
- [**Subscription Bridge compatibility and risk notice**](./docs/subscription-bridge.md)
- [**Third-party notices**](./THIRD_PARTY_NOTICES.md)

This repository is licensed under the [MIT License](./LICENSE).
