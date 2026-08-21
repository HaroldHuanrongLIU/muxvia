# Release Bundle and notify-only update design

## Scope

T20 ships one auditable GitHub Release archive for each supported native target:

- `darwin-arm64`
- `darwin-x64`
- `linux-glibc-arm64`
- `linux-glibc-x64`

Each archive contains `muxvia`, the private `muxvia-routing` sidecar, `muxvia-release.json`, the MIT license, third-party notices, and an extraction manifest. This ticket does not add Homebrew, npm, a managed installer, automatic installation, telemetry, or remote diagnostics.

## Binding boundary

`muxvia-release.json` is the only binding manifest. It fixes the product, release, native target, source build identity, RPC version, required file names, executable roles, byte lengths, and SHA-256 hashes. A release-compiled Control Plane validates the complete bundle before parsing a diagnostic command, starting the Routing Service, or attempting handover. A release-compiled Routing Service performs the same validation before reporting lifecycle metadata or creating runtime state, so direct sidecar invocation cannot bypass the boundary.

Development builds have no embedded bundle identity and continue to run from the repository. Release builds embed release, target, and build values at compile time. A bundled Control Plane accepts only the sibling Routing Service named by the manifest; `--service` cannot select an unrelated executable.

## Update notification boundary

The TUI makes a bodyless `GET` to one fixed public GitHub Release manifest URL. It stores only the last attempt time and last public release value under `~/.muxvia/state/update-check.json`, with private permissions. An atomic local lock prevents concurrent checks. The attempt time is persisted before network access, so success and failure are both rate-limited to at most once per 24 hours.

`MUXVIA_UPDATE_CHECK=0` disables the check. The response can produce only a translated home-screen notice when a strictly newer semantic version exists. There is no download, installation, POST request, query parameter, telemetry, configuration, usage, or diagnostic upload path.

## Release automation

A tag workflow builds on four native GitHub-hosted runners, assembles and inspects each archive, runs an extracted installation smoke test, scans unpacked files for release-forbidden secrets, then publishes all archives and the public update manifest together. Existing CI continues to run the full Rust/Bun suite and compatibility goldens on macOS and Linux; release jobs repeat bundle-specific inspection and smoke checks on every architecture.
