import { afterEach, describe, expect, test } from "bun:test"
import { chmod, mkdtemp, realpath, rm, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  BUNDLE_MANIFEST_FILE,
  type BundleManifest,
  createBundleManifest,
  runtimeBundleTarget,
  validatePassedBundleRoot,
  validateReleaseBundle,
} from "../src/release-bundle"

const roots: string[] = []

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

async function fixture(): Promise<{ root: string; manifest: BundleManifest }> {
  const root = await mkdtemp(join(tmpdir(), "muxvia-bundle-"))
  roots.push(root)
  for (const [name, contents, mode] of [
    ["muxvia", "control-plane", 0o755],
    ["muxvia-routing", "routing-service", 0o755],
    ["LICENSE", "license", 0o644],
    ["THIRD_PARTY_NOTICES.md", "notices", 0o644],
    ["EXTRACTION_MANIFEST.json", "extractions", 0o644],
  ] as const) {
    await writeFile(join(root, name), contents, { mode })
  }
  const manifest = await createBundleManifest({
    root,
    release: "0.1.0",
    target: runtimeBundleTarget(),
    build: "0123456789abcdef",
  })
  await writeFile(join(root, BUNDLE_MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`)
  return { root, manifest }
}

const expected = () => ({
  release: "0.1.0",
  routingRelease: "0.1.0",
  target: runtimeBundleTarget(),
  build: "0123456789abcdef",
  rpc: { major: 1, minor: 0 } as const,
})

test("a complete release bundle binds every required file", async () => {
  const { root } = await fixture()
  const result = await validateReleaseBundle(join(root, "muxvia"), expected())

  const canonicalRoot = await realpath(root)
  expect(result.root).toBe(canonicalRoot)
  expect(result.routingServicePath).toBe(join(canonicalRoot, "muxvia-routing"))
  expect(result.manifest.files.map((file) => file.role)).toEqual([
    "control-plane", "routing-service", "license", "third-party-notices", "extraction-manifest",
  ])
})

test("accepts only the absolute canonical bundle root passed by a package launcher", async () => {
  const { root } = await fixture()
  const canonicalRoot = await realpath(root)
  await validatePassedBundleRoot(canonicalRoot, canonicalRoot)
  await validatePassedBundleRoot(undefined, canonicalRoot)
  expect(validatePassedBundleRoot("relative", canonicalRoot)).rejects.toThrow("release-bundle-invalid")
  expect(validatePassedBundleRoot(tmpdir(), canonicalRoot)).rejects.toThrow("release-bundle-invalid")
})

describe("closed release identity", () => {
  for (const [label, mutate] of [
    ["product", (manifest: BundleManifest) => { manifest.product = "other" as "muxvia" }],
    ["release", (manifest: BundleManifest) => { manifest.release = "0.2.0" }],
    ["target", (manifest: BundleManifest) => { manifest.target = manifest.target === "darwin-arm64" ? "darwin-x64" : "darwin-arm64" }],
    ["build", (manifest: BundleManifest) => { manifest.build = "different" }],
    ["RPC", (manifest: BundleManifest) => { manifest.rpc.minor = 1 }],
    ["integrity", (manifest: BundleManifest) => { manifest.files[0]!.sha256 = "0".repeat(64) }],
    ["length", (manifest: BundleManifest) => { manifest.files[1]!.byteLength += 1 }],
  ] as const) {
    test(`rejects a ${label} mismatch`, async () => {
      const { root, manifest } = await fixture()
      mutate(manifest)
      await writeFile(join(root, BUNDLE_MANIFEST_FILE), JSON.stringify(manifest))
      expect(validateReleaseBundle(join(root, "muxvia"), expected())).rejects.toThrow("release-bundle-invalid")
    })
  }
})

test("rejects non-executable and linked bundle members", async () => {
  const first = await fixture()
  await chmod(join(first.root, "muxvia-routing"), 0o644)
  expect(validateReleaseBundle(join(first.root, "muxvia"), expected())).rejects.toThrow("release-bundle-invalid")

  const second = await fixture()
  await chmod(join(second.root, "LICENSE"), 0o755)
  expect(validateReleaseBundle(join(second.root, "muxvia"), expected())).rejects.toThrow("release-bundle-invalid")

  const third = await fixture()
  await rm(join(third.root, "LICENSE"))
  await symlink(join(first.root, "LICENSE"), join(third.root, "LICENSE"))
  expect(validateReleaseBundle(join(third.root, "muxvia"), expected())).rejects.toThrow("release-bundle-invalid")
})
