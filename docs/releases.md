# Release archives and updates

## v0.1 support and risk boundaries

Muxvia v0.1 supports only the default Target CLI configuration homes `~/.codex` and `~/.claude`. Its public routing protocol surface is limited to OpenAI Responses, Anthropic Messages, and Anthropic token counting. The release qualification policy records the exact first and latest Target CLI versions proved by release CI: Codex CLI 0.147.0 and 0.149.0, and Claude Code 2.1.228 and 2.1.239. Versions outside those evidence points may pass a capability probe, but Muxvia labels them untested rather than expanding the public compatibility claim.

Provider credentials, subscription refresh tokens, and routing secrets are stored locally in plaintext without application-level encryption, under best-effort restrictive filesystem permissions. A failed routed request may retain at most 64 KiB of its upstream error payload plus a truncation marker, so diagnostics and local state can contain sensitive request material. Recovery Backups are sensitive private artifacts that may contain credentials and operational state; they are not shareable Provider Configuration Exports. Store and transfer them accordingly.

The Subscription Bridge uses an undocumented compatibility interface and carries the support, breakage, shared-quota, and account-terms risks documented in [Subscription Bridge](./subscription-bridge.md). Muxvia has no product telemetry and never downloads or installs an update. It only emits a notify-only update notice; installation, upgrade, repair, and removal remain explicit Operator or package-manager actions. The macOS Gatekeeper and unsigned/unnotarized guidance below applies to both bundled executables.

## Install through verified download

Download and review the installer before running it:

```sh
curl -fsSLo /tmp/muxvia-install.sh \
  https://github.com/HaroldHuanrongLIU/muxvia/releases/latest/download/install.sh
sh /tmp/muxvia-install.sh
export PATH="$HOME/.muxvia/bin:$PATH"
muxvia version
```

The installer selects one exact archive for the current macOS or glibc Linux architecture. It verifies the public release identity, the archive SHA-256, and every file, mode, and SHA-256 bound by `muxvia-release.json` before changing the active version. Complete versioned Release Bundles and the atomic active-version pointer remain under `~/.muxvia`; the installer does not copy either executable elsewhere.

Run the same reviewed installer again to update. A failed download, archive or bundle verification, staging operation, or active-version switch leaves the prior active Release Bundle unchanged. The installer refuses an existing Homebrew- or npm-owned `muxvia` command and directs the Operator to `brew upgrade muxvia` or `npm install --global muxvia@latest` instead; it never writes into those package-manager trees.

## Install with npm

On a supported macOS or glibc Linux machine with Node.js 18 or newer, install the public launcher and its exact-version optional platform package together:

```sh
npm install --global --include=optional muxvia
muxvia version
```

The launcher selects only `@muxvia/darwin-arm64`, `@muxvia/darwin-x64`, `@muxvia/linux-glibc-arm64`, or `@muxvia/linux-glibc-x64` for the current machine. That platform package contains the complete Release Bundle. The launcher verifies the package version, product, target, build, RPC version, complete file set, modes, lengths, and SHA-256 hashes before starting `muxvia`; it passes the absolute bundle root and never looks up `muxvia-routing` through `PATH`.

npm lifecycle scripts do not download or install executable content. If the selected optional package is missing or invalid, Muxvia does not start and prints one repair command. Run that command without replacing individual files:

```sh
npm install --global --include=optional muxvia@<version>
```

Upgrade this installation through npm, for example with `npm install --global --include=optional muxvia@latest`. Muxvia never writes into or self-upgrades the npm-owned package tree.

## Install a GitHub Release archive

Download the archive matching the machine exactly:

| Machine | Archive target |
| --- | --- |
| Apple silicon macOS | `darwin-arm64` |
| Intel macOS | `darwin-x64` |
| arm64 Linux with glibc | `linux-glibc-arm64` |
| x86-64 Linux with glibc | `linux-glibc-x64` |

Extract the archive without moving, renaming, or replacing individual files. Run `./muxvia version` from the extracted directory. `muxvia-routing` is a private sidecar and must not be installed, invoked, or upgraded separately. Both executables verify the release manifest and every bundled file before performing product work.

The initial macOS archives and verified-download installations are unsigned and unnotarized. macOS Gatekeeper may prevent the first launch because Apple has not verified the developer or notarized the archive. Review the installer, archive, and release checksums first. If local policy permits it, attempt the blocked executable once and then use **System Settings → Privacy & Security → Open Anyway** for that exact executable. Approval may be required separately for `muxvia` and `muxvia-routing`; do not disable Gatekeeper globally or recursively remove quarantine from unrelated files. Re-run `muxvia version` afterward. The release does not claim Apple verification.

## Install from the official Homebrew tap

Homebrew selects the matching `darwin-arm64` or `darwin-x64` GitHub Release archive and verifies its SHA-256 checksum from the formula:

```sh
brew tap HaroldHuanrongLIU/muxvia
brew install HaroldHuanrongLIU/muxvia/muxvia
muxvia version
muxvia doctor
```

The formula keeps `muxvia`, private `muxvia-routing`, and `muxvia-release.json` together with the license and notice files under `$(brew --prefix muxvia)/libexec`. Only `muxvia` is linked into Homebrew's public `bin`; do not invoke, move, or upgrade `muxvia-routing` separately. Muxvia validates the complete Release Bundle before product work and never writes into or self-upgrades the Homebrew-owned installation tree.

The initial Homebrew macOS build is unsigned and unnotarized. Gatekeeper may block the first launch of either executable because Apple has not verified the developer or notarized the archive. After reviewing the formula, archive checksum, and release, attempt the launch once and, if local policy permits it, approve that blocked executable through **System Settings → Privacy & Security → Open Anyway**. The formula does not claim Apple verification.

Update and uninstall only through Homebrew:

```sh
brew update
brew upgrade muxvia
brew uninstall muxvia
```

If the bundle is missing or `muxvia doctor` reports a bundle failure, do not copy individual files into the Cellar. Repair and verify the whole keg:

```sh
brew reinstall muxvia
brew test muxvia
muxvia doctor
```

Third-party notices remain inside the installed bundle at `$(brew --prefix muxvia)/libexec/THIRD_PARTY_NOTICES.md`. Each tag build deterministically generates `muxvia.rb` from the public release manifest, verifies it, and smoke-tests install, diagnostics, Control Plane and sidecar startup, a Homebrew-owned revision upgrade, and uninstall on native Apple silicon and Intel runners before publishing it with the GitHub Release. Only after that Release succeeds, the workflow downloads the released manifest and formula, verifies them together again, and automatically commits the exact formula to `Formula/muxvia.rb` in the official `HaroldHuanrongLIU/homebrew-muxvia` tap. Only this final publication job receives the dedicated `HOMEBREW_TAP_TOKEN`; generation, verification, and smoke testing require no tap credential.

## Update notifications

The interactive Control Plane checks the fixed public GitHub release manifest at most once per 24 hours. It sends a bodyless `GET` with no configuration, usage, diagnostics, or other local state, and only displays a notice when a newer release exists. It never downloads or installs an update.

Set `MUXVIA_UPDATE_CHECK=0` in the Control Plane environment to disable the request. For a verified-download installation, re-run the reviewed installer above. For a Homebrew installation, run `brew update && brew upgrade muxvia`; Muxvia does not run those commands or write into the Cellar. npm installations must be updated through npm. For a GitHub Release archive, download and verify a complete replacement archive. Every path validates and activates a complete Release Bundle; never replace only `muxvia` or `muxvia-routing`.

Muxvia has no product telemetry, analytics, crash-report upload, remote diagnostics, or configuration/usage upload path.
