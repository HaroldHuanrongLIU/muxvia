import { expect, test } from "bun:test"
import { readFile } from "node:fs/promises"
import { resolve } from "node:path"

test("release automation owns all four native archives and every audit gate", async () => {
  const workflow = await readFile(resolve(".github/workflows/release.yml"), "utf8")
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
  expect(workflow).toContain("--no-compile-autoload-bunfig")
  expect(workflow).toContain("--no-compile-autoload-dotenv")
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
    "npm dist-tag add",
  ]) expect(workflow).toContain(gate)
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
  expect(releaseDocumentation).toContain("MUXVIA_UPDATE_CHECK=0")
  expect(releaseDocumentation).toContain("no product telemetry")
})
