import { expect, test } from "bun:test"

import {
  createHomebrewFormula,
  type PublicReleaseManifest,
} from "../../../scripts/homebrew-formula"

const manifest = (): PublicReleaseManifest => ({
  schemaVersion: 1,
  product: "muxvia",
  release: "0.1.0",
  bundles: [
    {
      target: "darwin-arm64",
      archive: "muxvia-0.1.0-darwin-arm64.tar.gz",
      sha256: "a".repeat(64),
    },
    {
      target: "darwin-x64",
      archive: "muxvia-0.1.0-darwin-x64.tar.gz",
      sha256: "b".repeat(64),
    },
    {
      target: "linux-glibc-arm64",
      archive: "muxvia-0.1.0-linux-glibc-arm64.tar.gz",
      sha256: "c".repeat(64),
    },
    {
      target: "linux-glibc-x64",
      archive: "muxvia-0.1.0-linux-glibc-x64.tar.gz",
      sha256: "d".repeat(64),
    },
  ],
})

test("official Homebrew formula binds both macOS archives into one private Release Bundle", () => {
  const formula = createHomebrewFormula(manifest())

  expect(formula).toContain("class Muxvia < Formula")
  expect(formula).toContain('version "0.1.0"')
  expect(formula).toContain("on_arm do")
  expect(formula).toContain("muxvia-0.1.0-darwin-arm64.tar.gz")
  expect(formula).toContain(`sha256 "${"a".repeat(64)}"`)
  expect(formula).toContain("on_intel do")
  expect(formula).toContain("muxvia-0.1.0-darwin-x64.tar.gz")
  expect(formula).toContain(`sha256 "${"b".repeat(64)}"`)
  expect(formula).not.toContain("linux-glibc")
  for (const member of [
    "muxvia",
    "muxvia-routing",
    "muxvia-release.json",
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
    "EXTRACTION_MANIFEST.json",
  ]) expect(formula).toContain(`"${member}"`)
  expect(formula).toContain('bin.install_symlink libexec/"muxvia"')
  expect(formula).not.toContain('bin.install_symlink libexec/"muxvia-routing"')
  expect(formula).toContain("unsigned and unnotarized")
  expect(formula).toContain("brew reinstall muxvia")
  expect(formula).toContain("THIRD_PARTY_NOTICES.md")
})

test("formula generation is deterministic and supports a CI-only revision upgrade", () => {
  const options = { archiveBaseUrl: "file:///tmp/muxvia-archives", revision: 1 }
  const first = createHomebrewFormula(manifest(), options)
  const second = createHomebrewFormula(manifest(), options)

  expect(first).toBe(second)
  expect(first).toContain("revision 1")
  expect(first).toContain("file:///tmp/muxvia-archives/muxvia-0.1.0-darwin-arm64.tar.gz")
  expect(first.endsWith("\n")).toBeTrue()
})

test("formula generation rejects incomplete or ambiguous macOS release metadata", () => {
  const missingIntel = manifest()
  missingIntel.bundles = missingIntel.bundles.filter((bundle) => bundle.target !== "darwin-x64")
  expect(() => createHomebrewFormula(missingIntel)).toThrow("missing darwin-x64 archive")

  const duplicateArm = manifest()
  duplicateArm.bundles.push({ ...duplicateArm.bundles[0]! })
  expect(() => createHomebrewFormula(duplicateArm)).toThrow("duplicate darwin-arm64 archive")

  const wrongName = manifest()
  wrongName.bundles[0]!.archive = "other.tar.gz"
  expect(() => createHomebrewFormula(wrongName)).toThrow("unexpected darwin-arm64 archive")

  const invalidHash = manifest()
  invalidHash.bundles[1]!.sha256 = "not-a-sha256"
  expect(() => createHomebrewFormula(invalidHash)).toThrow("invalid darwin-x64 sha256")
})
