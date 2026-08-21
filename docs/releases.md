# Release archives and updates

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

The initial macOS archives are unsigned and unnotarized. macOS Gatekeeper may prevent the first launch because Apple has not verified the developer or notarized the archive. Review the downloaded archive and release checksums first; if local policy permits it, use Finder's **Open** action or **System Settings → Privacy & Security → Open Anyway** for that exact binary. The release does not claim Apple verification.

## Update notifications

The interactive Control Plane checks the fixed public GitHub release manifest at most once per 24 hours. It sends a bodyless `GET` with no configuration, usage, diagnostics, or other local state, and only displays a notice when a newer release exists. It never downloads or installs an update.

Set `MUXVIA_UPDATE_CHECK=0` in the Control Plane environment to disable the request. Update by downloading and verifying a complete replacement archive through the same installation channel; never replace only `muxvia` or `muxvia-routing`.

Muxvia has no product telemetry, analytics, crash-report upload, remote diagnostics, or configuration/usage upload path.
