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

## Update notifications

The interactive Control Plane checks the fixed public GitHub release manifest at most once per 24 hours. It sends a bodyless `GET` with no configuration, usage, diagnostics, or other local state, and only displays a notice when a newer release exists. It never downloads or installs an update.

Set `MUXVIA_UPDATE_CHECK=0` in the Control Plane environment to disable the request. Update by downloading and verifying a complete replacement archive through the same installation channel; never replace only `muxvia` or `muxvia-routing`.

Muxvia has no product telemetry, analytics, crash-report upload, remote diagnostics, or configuration/usage upload path.
