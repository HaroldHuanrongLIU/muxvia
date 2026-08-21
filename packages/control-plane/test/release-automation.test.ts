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
    "gh release create",
  ]) expect(workflow).toContain(gate)
  expect(workflow).toContain("actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02")
  expect(workflow).toContain("actions/download-artifact@634f93cb2916e3fdff6788551b99b062d0335ce0")
  expect(workflow).toContain("release:control-plane")
  expect(ci).toContain("release:control-plane")
  expect(controlPlaneBuild).toContain("plugins: [solidPlugin]")
  expect(controlPlaneBuild).toContain("autoloadBunfig: false")
  expect(controlPlaneBuild).toContain("autoloadDotenv: false")
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
  expect(workflow).toContain("needs: [bundle, homebrew]")
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
  ]) expect(smoke).toContain(value)
})
