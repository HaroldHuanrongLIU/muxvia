# Release archives and updates

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

## Update notifications

The interactive Control Plane checks the fixed public GitHub release manifest at most once per 24 hours. It sends a bodyless `GET` with no configuration, usage, diagnostics, or other local state, and only displays a notice when a newer release exists. It never downloads or installs an update.

Set `MUXVIA_UPDATE_CHECK=0` in the Control Plane environment to disable the request. For a verified-download installation, re-run the reviewed installer above. Homebrew and npm installations must be updated through their package manager. Every path validates and activates a complete Release Bundle; never replace only `muxvia` or `muxvia-routing`.

Muxvia has no product telemetry, analytics, crash-report upload, remote diagnostics, or configuration/usage upload path.
