import { afterEach, describe, expect, test } from "bun:test"
import { createHash } from "node:crypto"
import { EventEmitter } from "node:events"
import { chmod, mkdir, mkdtemp, readFile, realpath, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  LauncherRepairError,
  prepareLaunch,
  runLauncher,
  runtimeTarget,
} from "../lib/launcher.js"

const roots = []
const build = "0123456789abcdef"
const release = "0.1.0"
const targetMetadata = {
  "darwin-arm64": { packageName: "@muxvia/darwin-arm64", os: "darwin", cpu: "arm64" },
  "darwin-x64": { packageName: "@muxvia/darwin-x64", os: "darwin", cpu: "x64" },
  "linux-glibc-arm64": { packageName: "@muxvia/linux-glibc-arm64", os: "linux", cpu: "arm64", libc: "glibc" },
  "linux-glibc-x64": { packageName: "@muxvia/linux-glibc-x64", os: "linux", cpu: "x64", libc: "glibc" },
}
const contracts = [
  ["control-plane", "muxvia", true, "control-plane"],
  ["routing-service", "muxvia-routing", true, "routing-service"],
  ["license", "LICENSE", false, "license"],
  ["third-party-notices", "THIRD_PARTY_NOTICES.md", false, "notices"],
  ["extraction-manifest", "EXTRACTION_MANIFEST.json", false, "extractions"],
]

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

async function fixture(target = "linux-glibc-x64") {
  const targetInfo = targetMetadata[target]
  const root = await mkdtemp(join(tmpdir(), "muxvia-npm-launcher-"))
  roots.push(root)
  const packageRoot = join(root, "node_modules", ...targetInfo.packageName.split("/"))
  const bundleRoot = join(packageRoot, "bundle")
  await mkdir(bundleRoot, { recursive: true })
  const files = []
  for (const [role, path, executable, contents] of contracts) {
    const file = join(bundleRoot, path)
    await writeFile(file, contents, { mode: executable ? 0o755 : 0o644 })
    await chmod(file, executable ? 0o755 : 0o644)
    files.push({
      role,
      path,
      executable,
      byteLength: Buffer.byteLength(contents),
      sha256: createHash("sha256").update(contents).digest("hex"),
    })
  }
  const manifest = {
    schemaVersion: 1,
    product: "muxvia",
    release,
    target,
    build,
    rpc: { major: 1, minor: 0 },
    files,
  }
  await writeFile(join(bundleRoot, "muxvia-release.json"), `${JSON.stringify(manifest)}\n`, { mode: 0o644 })
  const packageJson = {
    name: targetInfo.packageName,
    version: release,
    description: `Complete Muxvia Release Bundle for ${target}`,
    license: "MIT",
    repository: {
      type: "git",
      url: "git+https://github.com/HaroldHuanrongLIU/muxvia.git",
    },
    os: [targetInfo.os],
    cpu: [targetInfo.cpu],
    ...(targetInfo.libc ? { libc: [targetInfo.libc] } : {}),
    files: ["bundle"],
    muxviaBundle: {
      schemaVersion: 1,
      product: "muxvia",
      release,
      target,
      build,
      rpc: { major: 1, minor: 0 },
    },
  }
  const packageJsonPath = join(packageRoot, "package.json")
  await writeFile(packageJsonPath, `${JSON.stringify(packageJson)}\n`, { mode: 0o644 })
  return { packageJsonPath, packageJson, bundleRoot, manifest, targetInfo }
}

test("maps only the four supported native targets", () => {
  expect(runtimeTarget("darwin", "arm64")).toBe("darwin-arm64")
  expect(runtimeTarget("darwin", "x64")).toBe("darwin-x64")
  expect(runtimeTarget("linux", "arm64", "glibc")).toBe("linux-glibc-arm64")
  expect(runtimeTarget("linux", "x64", "glibc")).toBe("linux-glibc-x64")
  expect(() => runtimeTarget("linux", "x64", "musl")).toThrow("unsupported-target")
  expect(() => runtimeTarget("win32", "x64")).toThrow("unsupported-target")
})

for (const target of Object.keys(targetMetadata)) {
  test(`resolves only the exact ${target} optional package and returns an absolute bundle root`, async () => {
    const value = await fixture(target)
    const requests = []
    const prepared = await prepareLaunch({
      target,
      launcherVersion: release,
      resolvePackageJson(specifier) {
        requests.push(specifier)
        return value.packageJsonPath
      },
    })

    expect(requests).toEqual([`${value.targetInfo.packageName}/package.json`])
    expect(prepared.bundleRoot).toBe(await realpath(value.bundleRoot))
    expect(prepared.executable).toBe(join(await realpath(value.bundleRoot), "muxvia"))
  })
}

describe("closed platform and Release Bundle identity", () => {
  for (const [label, mutate] of [
    ["platform package version", ({ packageJson }) => { packageJson.version = "0.1.1" }],
    ["product", ({ packageJson }) => { packageJson.muxviaBundle.product = "other" }],
    ["target", ({ manifest }) => { manifest.target = "darwin-x64" }],
    ["build", ({ manifest }) => { manifest.build = "different" }],
    ["RPC", ({ manifest }) => { manifest.rpc.minor = 1 }],
    ["integrity", ({ manifest }) => { manifest.files[0].sha256 = "0".repeat(64) }],
  ]) {
    test(`rejects a ${label} mismatch`, async () => {
      const value = await fixture()
      mutate(value)
      await writeFile(value.packageJsonPath, JSON.stringify(value.packageJson))
      await writeFile(join(value.bundleRoot, "muxvia-release.json"), JSON.stringify(value.manifest))
      expect(prepareLaunch({
        target: "linux-glibc-x64",
        launcherVersion: release,
        resolvePackageJson: () => value.packageJsonPath,
      })).rejects.toBeInstanceOf(LauncherRepairError)
    })
  }
})

test("missing and invalid optional packages use one repair message and never start Muxvia", async () => {
  const expected = "muxvia: the required exact-version optional package is missing or invalid. Repair with: npm install --include=optional muxvia@0.1.0 (requires @muxvia/linux-glibc-x64@0.1.0).\n"
  for (const resolvePackageJson of [
    () => { throw new Error("missing") },
    async () => join((await fixture()).bundleRoot, "missing-package.json"),
  ]) {
    let stderr = ""
    let started = false
    const exitCode = await runLauncher({
      target: "linux-glibc-x64",
      launcherVersion: release,
      resolvePackageJson,
      stderr: { write: (chunk) => { stderr += chunk } },
      spawnProcess: () => { started = true },
    })
    expect(exitCode).toBe(78)
    expect(stderr).toBe(expected)
    expect(started).toBeFalse()
  }
})

test("starts the verified control plane with the absolute bundle root and preserves arguments", async () => {
  const value = await fixture()
  let invocation
  const exitCode = await runLauncher({
    target: "linux-glibc-x64",
    launcherVersion: release,
    resolvePackageJson: () => value.packageJsonPath,
    args: ["version", "--json"],
    spawnProcess(executable, args, options) {
      invocation = { executable, args, options }
      const child = new EventEmitter()
      queueMicrotask(() => child.emit("exit", 0, null))
      return child
    },
  })

  expect(exitCode).toBe(0)
  expect(invocation.executable).toBe(join(await realpath(value.bundleRoot), "muxvia"))
  expect(invocation.args).toEqual(["version", "--json"])
  expect(invocation.options.env.MUXVIA_BUNDLE_ROOT).toBe(await realpath(value.bundleRoot))
})

test("a platform executable start failure uses the same deterministic repair message", async () => {
  const value = await fixture()
  let stderr = ""
  const exitCode = await runLauncher({
    target: "linux-glibc-x64",
    launcherVersion: release,
    resolvePackageJson: () => value.packageJsonPath,
    stderr: { write: (chunk) => { stderr += chunk } },
    spawnProcess() {
      const child = new EventEmitter()
      queueMicrotask(() => child.emit("error", new Error("exec failed")))
      return child
    },
  })

  expect(exitCode).toBe(78)
  expect(stderr).toBe(
    "muxvia: the required exact-version optional package is missing or invalid. Repair with: npm install --include=optional muxvia@0.1.0 (requires @muxvia/linux-glibc-x64@0.1.0).\n",
  )
})

test("the launcher contains no install, update, or package-tree write path", async () => {
  const source = await readFile(new URL("../lib/launcher.js", import.meta.url), "utf8")
  expect(source).not.toMatch(/\b(writeFile|rename|copyFile|mkdir|rm)\b/)
  expect(source).not.toMatch(/spawnProcess\(["']npm["']/)
  expect(source.match(/npm install/g)).toHaveLength(1)
  const metadata = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"))
  expect(metadata.scripts).toBeUndefined()
})
