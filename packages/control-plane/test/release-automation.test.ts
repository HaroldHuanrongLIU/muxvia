import { expect, test } from "bun:test"
import { readFile } from "node:fs/promises"
import { resolve } from "node:path"

test("release automation owns all four native archives and every audit gate", async () => {
  const [workflow, ci, controlPlaneBuild] = await Promise.all([
    readFile(resolve(".github/workflows/release.yml"), "utf8"),
    readFile(resolve(".github/workflows/ci.yml"), "utf8"),
    readFile(resolve("scripts/build-control-plane.ts"), "utf8"),
  ])
  for (const target of [
    "darwin-arm64",
    "darwin-x64",
    "linux-glibc-arm64",
    "linux-glibc-x64",
  ]) expect(workflow).toContain(`target: ${target}`)
  for (const gate of [
    "release:bundle inspect",
    "release:bundle scan",
    "release:bundle smoke",
    "release:bundle public-manifest",
    "sh scripts/install.sh",
    "gh release create",
  ]) expect(workflow).toContain(gate)
  expect(workflow).toContain("actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02")
  expect(workflow).toContain("actions/download-artifact@634f93cb2916e3fdff6788551b99b062d0335ce0")
  expect(workflow).toContain("release:control-plane")
  expect(ci).toContain("release:control-plane")
  expect(controlPlaneBuild).toContain("plugins: [solidPlugin]")
  expect(controlPlaneBuild).toContain("autoloadBunfig: false")
  expect(controlPlaneBuild).toContain("autoloadDotenv: false")
  expect(workflow).toContain("install -m 0755 scripts/install.sh release/install.sh")
  expect(workflow.match(/MUXVIA_INSTALLER_TESTING=1/g)).toHaveLength(2)
})

test("npm release automation smokes every platform and moves latest only after published verification", async () => {
  const workflow = await readFile(resolve(".github/workflows/release.yml"), "utf8")
  for (const target of [
    "darwin-arm64",
    "darwin-x64",
    "linux-glibc-arm64",
    "linux-glibc-x64",
  ]) {
    expect(workflow).toContain(`target: ${target}`)
  }
  expect(workflow).toContain("@muxvia/${target}@${release}")
  for (const gate of [
    "release:npm assemble-platform",
    "release:npm inspect-platform",
    "release:npm inspect-launcher",
    "npm install",
    "--offline",
    "--ignore-scripts",
    "node_modules/.bin/muxvia",
    "npm publish",
    "npm pack",
    "npm pack ./packages/npm-launcher",
    "npm dist-tag add",
  ]) expect(workflow).toContain(gate)
  expect(workflow).toContain(
    'cp "$tarballs/muxvia-${{ matrix.target }}-${{ steps.release.outputs.release }}.tgz" build/',
  )
  expect(workflow).toContain(
    "build/muxvia-${{ matrix.target }}-${{ steps.release.outputs.release }}.tgz",
  )
  expect(workflow).not.toContain(
    "build/npm/tarballs/muxvia-${{ matrix.target }}-${{ steps.release.outputs.release }}.tgz",
  )
  expect(workflow.indexOf("Publish and verify every npm platform package")).toBeLessThan(
    workflow.indexOf("Publish and verify the npm launcher without moving its public tag"),
  )
  expect(workflow.indexOf("Publish and verify the npm launcher without moving its public tag")).toBeLessThan(
    workflow.indexOf("Move the verified npm launcher to the public distribution tag"),
  )
})

test("release archives carry licenses, provenance, and accurate unsigned macOS guidance", async () => {
  const [notices, extraction, releaseDocumentation] = await Promise.all([
    readFile(resolve("THIRD_PARTY_NOTICES.md"), "utf8"),
    readFile(resolve("EXTRACTION_MANIFEST.json"), "utf8"),
    readFile(resolve("docs/releases.md"), "utf8"),
  ])
  expect(notices).toContain("Copyright (c) 2025 Jason Young")
  expect(notices).toContain("Copyright (c) 2025 opencode")
  expect(JSON.parse(extraction).materials.length).toBeGreaterThan(0)
  expect(releaseDocumentation).toContain("unsigned and unnotarized")
  expect(releaseDocumentation).toContain("verified-download installation")
  expect(releaseDocumentation).toContain("brew upgrade muxvia")
  expect(releaseDocumentation).toContain("Approval may be required separately")
  expect(releaseDocumentation).toContain("MUXVIA_UPDATE_CHECK=0")
  expect(releaseDocumentation).toContain("no product telemetry")
})

test("release automation gates the official Homebrew formula on both macOS architectures", async () => {
  const [workflow, formula, smoke] = await Promise.all([
    readFile(resolve(".github/workflows/release.yml"), "utf8"),
    readFile(resolve("scripts/homebrew-formula.ts"), "utf8"),
    readFile(resolve("scripts/homebrew-smoke.ts"), "utf8"),
  ])
  const homebrewJob = workflow.slice(workflow.indexOf("  homebrew:"), workflow.indexOf("  publish:"))
  for (const value of [
    "target: darwin-arm64",
    "runner: macos-15",
    "target: darwin-x64",
    "runner: macos-15-intel",
    "release:homebrew generate",
    "release:homebrew verify",
    "release:homebrew:smoke",
  ]) expect(homebrewJob).toContain(value)
  expect(workflow).toContain("needs: qualification")
  expect(workflow).toContain("--output release/muxvia.rb")

  for (const value of [
    'on_arm do',
    'on_intel do',
    'libexec.install(',
    'bin.install_symlink libexec/"muxvia"',
    'sha256',
  ]) expect(formula).toContain(value)
  for (const value of [
    '["brew", "install"',
    '"version", "--json"',
    '"doctor", "--json"',
    'smokeTui(',
    '"--lifecycle-metadata"',
    '["brew", "upgrade"',
    '["brew", "uninstall"',
    "delete environment.HOMEBREW_NO_INSTALL_CLEANUP",
    "delete environment.HOMEBREW_NO_CLEANUP_FORMULAE",
    '["brew", "list", "--versions", "muxvia"], environment, [0, 1]',
    'absent.stdout.trim() === ""',
  ]) expect(smoke).toContain(value)
})

test("the published GitHub Release updates the official Homebrew tap with its exact verified formula", async () => {
  const workflow = await readFile(resolve(".github/workflows/release.yml"), "utf8")
  const publishJob = workflow.slice(workflow.indexOf("  publish:"), workflow.indexOf("  publish-homebrew-tap:"))
  const tapJob = workflow.slice(workflow.indexOf("  publish-homebrew-tap:"))

  expect(publishJob).toContain('gh release create "$GITHUB_REF_NAME"')
  expect(publishJob).toContain("release/muxvia.rb")
  expect(tapJob).toContain("needs: publish")
  expect(tapJob).toContain("repository: HaroldHuanrongLIU/homebrew-muxvia")
  expect(tapJob).toContain("token: ${{ secrets.HOMEBREW_TAP_TOKEN }}")
  expect(tapJob).toContain("--pattern muxvia-latest.json")
  expect(tapJob).toContain("--pattern muxvia.rb")
  expect(tapJob).toContain("--manifest released/muxvia-latest.json")
  expect(tapJob).toContain("--formula released/muxvia.rb")
  expect(tapJob).toContain("cp released/muxvia.rb homebrew-muxvia/Formula/muxvia.rb")
  expect(tapJob).toContain("cmp -s released/muxvia.rb homebrew-muxvia/Formula/muxvia.rb")
  expect(tapJob).toContain("git -C homebrew-muxvia add Formula/muxvia.rb")
  expect(tapJob.indexOf("release:homebrew verify")).toBeLessThan(tapJob.indexOf("git -C homebrew-muxvia push origin HEAD"))
})

test("release publication is gated by four-channel qualification and real compatibility boundaries", async () => {
  const [workflow, policy, diagnostics, codexProbe, claudeProbe] = await Promise.all([
    readFile(resolve(".github/workflows/release.yml"), "utf8"),
    readFile(resolve("release/qualification-policy.json"), "utf8").then(JSON.parse),
    readFile(resolve("packages/control-plane/src/diagnostic-cli.ts"), "utf8"),
    readFile(resolve("crates/routing-service/src/codex/probe.rs"), "utf8"),
    readFile(resolve("crates/routing-service/src/claude/probe.rs"), "utf8"),
  ])
  const qualification = workflow.slice(workflow.indexOf("  qualification:"), workflow.indexOf("  publish:"))
  const publish = workflow.slice(workflow.indexOf("  publish:"), workflow.indexOf("  publish-homebrew-tap:"))

  expect(workflow).toContain("bun run verify")
  expect(workflow).toContain("needs: quality")
  expect(workflow).toContain("release:qualification compatibility")
  expect(workflow).toContain("node target-cli/node_modules/@anthropic-ai/claude-code/install.cjs")
  expect(workflow).toContain("release:qualification record")
  expect(workflow).toContain('pattern: "*"')
  expect(qualification).toContain("needs: [bundle, homebrew, compatibility]")
  expect(publish).toContain("needs: qualification")
  expect(publish).toContain("release/muxvia-qualification.json")
  for (const [target, entry] of Object.entries(policy.compatibility) as Array<[
    string,
    { package: string; first: string; latest: string },
  ]>) {
    expect(workflow).toContain(`target: ${target}`)
    expect(workflow).toContain(`package: "${entry.package}"`)
    expect(workflow).toContain(`version: ${entry.first}`)
    expect(workflow).toContain(`version: ${entry.latest}`)
    for (const version of [entry.first, entry.latest]) {
      expect(diagnostics).toContain(version)
      expect(target === "codex" ? codexProbe : claudeProbe).toContain(version)
    }
  }
  for (const lifecycleGate of ["doctor --json", "npm uninstall", "brew", "rm -rf \"$installer_home/.muxvia/install\""]) {
    expect(workflow).toContain(lifecycleGate)
  }
})
