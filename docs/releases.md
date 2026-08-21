# Release archives and updates

## Install a GitHub Release archive

Download the archive matching the machine exactly:

| Machine | Archive target |
| --- | --- |
| Apple silicon macOS | `darwin-arm64` |
| Intel macOS | `darwin-x64` |
| arm64 Linux with glibc | `linux-glibc-arm64` |
| x86-64 Linux with glibc | `linux-glibc-x64` |

Extract the archive without moving, renaming, or replacing individual files. Run `./muxvia version` from the extracted directory. `muxvia-routing` is a private sidecar and must not be installed, invoked, or upgraded separately. Both executables verify the release manifest and every bundled file before performing product work.

The initial macOS archives are unsigned and unnotarized. macOS Gatekeeper may prevent the first launch because Apple has not verified the developer or notarized the archive. Review the downloaded archive and release checksums first; if local policy permits it, use Finder's **Open** action or **System Settings → Privacy & Security → Open Anyway** for that exact binary. The release does not claim Apple verification.

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

Set `MUXVIA_UPDATE_CHECK=0` in the Control Plane environment to disable the request. For a Homebrew installation, run `brew update && brew upgrade muxvia`; Muxvia does not run those commands or write into the Cellar. For a GitHub Release archive, download and verify a complete replacement archive. Never replace only `muxvia` or `muxvia-routing`.

Muxvia has no product telemetry, analytics, crash-report upload, remote diagnostics, or configuration/usage upload path.
