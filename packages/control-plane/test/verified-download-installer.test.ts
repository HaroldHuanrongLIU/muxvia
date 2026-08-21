import { afterEach, expect, test } from "bun:test"
import { createHash } from "node:crypto"
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  stat,
  symlink,
  writeFile,
} from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

const installer = resolve("scripts/install.sh")
const bundleManifestFile = "muxvia-release.json"
const isolatedSystemPath = "/usr/bin:/bin:/usr/sbin:/sbin"
const roots: string[] = []
const targets = [
  "darwin-arm64",
  "darwin-x64",
  "linux-glibc-arm64",
  "linux-glibc-x64",
] as const
type BundleTarget = typeof targets[number]

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

async function temporaryRoot(label: string): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), `muxvia-installer-${label}-`))
  roots.push(root)
  return root
}

async function run(command: string[], options: {
  home?: string
  environment?: Record<string, string>
} = {}): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  const process = Bun.spawn(command, {
    stdout: "pipe",
    stderr: "pipe",
    env: {
      ...Bun.env,
      ...(options.home ? {
        HOME: options.home,
        PATH: `${join(options.home, ".muxvia/bin")}:${isolatedSystemPath}`,
      } : {}),
      ...options.environment,
    },
  })
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(process.stdout).text(),
    new Response(process.stderr).text(),
    process.exited,
  ])
  return { exitCode, stdout, stderr }
}

function testPlatform(target: BundleTarget): { os: string; architecture: string } {
  switch (target) {
    case "darwin-arm64": return { os: "Darwin", architecture: "arm64" }
    case "darwin-x64": return { os: "Darwin", architecture: "x86_64" }
    case "linux-glibc-arm64": return { os: "Linux", architecture: "aarch64" }
    case "linux-glibc-x64": return { os: "Linux", architecture: "x86_64" }
  }
}

async function sha256(path: string): Promise<string> {
  return createHash("sha256").update(Buffer.from(await Bun.file(path).arrayBuffer())).digest("hex")
}

type PublishedBundle = { target: BundleTarget; archive: string; sha256: string }

async function publishRelease(options: {
  root: string
  release: string
  targets?: readonly BundleTarget[]
  corruptBindingTarget?: BundleTarget
  bundleProduct?: string
  publicProduct?: string
  corruptArchiveHash?: boolean
}): Promise<void> {
  const build = createHash("sha256").update(options.release).digest("hex")
  const releaseDirectory = join(options.root, "releases", `v${options.release}`)
  const sourceDirectory = join(options.root, "source", options.release)
  await mkdir(releaseDirectory, { recursive: true })
  await mkdir(sourceDirectory, { recursive: true })
  const bundles: PublishedBundle[] = []

  for (const target of options.targets ?? targets) {
    const bundleName = `muxvia-${options.release}-${target}`
    const bundleRoot = join(sourceDirectory, bundleName)
    await mkdir(bundleRoot)
    await writeFile(join(bundleRoot, "muxvia"), `#!/bin/sh\nprintf '%s\\n' '${options.release}:${target}:control-plane'\n`, { mode: 0o755 })
    await writeFile(join(bundleRoot, "muxvia-routing"), `#!/bin/sh\nprintf '%s\\n' '${options.release}:${target}:routing-service'\n`, { mode: 0o755 })
    await writeFile(join(bundleRoot, "LICENSE"), "license\n", { mode: 0o644 })
    await writeFile(join(bundleRoot, "THIRD_PARTY_NOTICES.md"), "notices\n", { mode: 0o644 })
    await writeFile(join(bundleRoot, "EXTRACTION_MANIFEST.json"), "{}\n", { mode: 0o644 })
    const contracts = [
      { role: "control-plane", path: "muxvia", executable: true },
      { role: "routing-service", path: "muxvia-routing", executable: true },
      { role: "license", path: "LICENSE", executable: false },
      { role: "third-party-notices", path: "THIRD_PARTY_NOTICES.md", executable: false },
      { role: "extraction-manifest", path: "EXTRACTION_MANIFEST.json", executable: false },
    ] as const
    const files = await Promise.all(contracts.map(async (contract) => {
      const path = join(bundleRoot, contract.path)
      return { ...contract, byteLength: (await stat(path)).size, sha256: await sha256(path) }
    }))
    await writeFile(join(bundleRoot, bundleManifestFile), `${JSON.stringify({
      schemaVersion: 1,
      product: options.bundleProduct ?? "muxvia",
      release: options.release,
      target,
      build,
      rpc: { major: 1, minor: 0 },
      files,
    }, null, 2)}\n`, { mode: 0o644 })
    if (options.corruptBindingTarget === target) {
      await writeFile(join(bundleRoot, "muxvia-routing"), "tampered\n", { flag: "a" })
    }
    const archive = `${bundleName}.tar.gz`
    const archivePath = join(releaseDirectory, archive)
    const archived = await run(["tar", "-czf", archivePath, "-C", sourceDirectory, bundleName])
    expect(archived).toMatchObject({ exitCode: 0, stderr: "" })
    bundles.push({ target, archive, sha256: await sha256(archivePath) })
  }

  if (options.corruptArchiveHash) bundles[0]!.sha256 = "0".repeat(64)
  await writeFile(join(options.root, "muxvia-latest.json"), `${JSON.stringify({
    schemaVersion: 1,
    product: options.publicProduct ?? "muxvia",
    release: options.release,
    bundles,
  }, null, 2)}\n`)
}

async function install(options: {
  home: string
  assets: string
  target: BundleTarget
  environment?: Record<string, string>
}): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  const platform = testPlatform(options.target)
  return await run(["sh", installer], {
    home: options.home,
    environment: {
      MUXVIA_INSTALLER_TESTING: "1",
      MUXVIA_INSTALLER_TEST_OS: platform.os,
      MUXVIA_INSTALLER_TEST_ARCH: platform.architecture,
      MUXVIA_INSTALLER_TEST_GLIBC: "1",
      MUXVIA_INSTALLER_TEST_MANIFEST_URL: `file://${join(options.assets, "muxvia-latest.json")}`,
      MUXVIA_INSTALLER_TEST_RELEASES_URL: `file://${join(options.assets, "releases")}`,
      ...options.environment,
    },
  })
}

async function activeBundle(home: string): Promise<string> {
  const installRoot = join(home, ".muxvia/install")
  const active = (await readFile(join(installRoot, "active-version"), "utf8")).trim()
  return join(installRoot, "versions", active)
}

async function launch(home: string): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  return await run([join(home, ".muxvia/bin/muxvia"), "version", "--json"], { home })
}

test("the verified-download installer selects and activates each supported Release Bundle", async () => {
  const assets = await temporaryRoot("four-target-assets")
  await publishRelease({ root: assets, release: "0.1.0" })

  for (const target of targets) {
    const home = await temporaryRoot(target)
    const result = await install({ home, assets, target })
    expect(result).toMatchObject({ exitCode: 0, stderr: "" })
    expect(result.stdout).toContain(`Muxvia 0.1.0 installed for ${target}`)
    expect(await launch(home)).toEqual({
      exitCode: 0,
      stdout: `0.1.0:${target}:control-plane\n`,
      stderr: "",
    })
    expect((await readdir(await activeBundle(home))).sort()).toEqual([
      "EXTRACTION_MANIFEST.json",
      "LICENSE",
      "THIRD_PARTY_NOTICES.md",
      "muxvia",
      "muxvia-release.json",
      "muxvia-routing",
    ])
  }
}, 15_000)

test("updates reuse full validation and failed download, verification, or activation preserves the active version", async () => {
  const assets = await temporaryRoot("rollback-assets")
  const home = await temporaryRoot("rollback-home")
  const target = "darwin-arm64" as const
  await publishRelease({ root: assets, release: "0.1.0", targets: [target] })
  expect(await install({ home, assets, target })).toMatchObject({ exitCode: 0 })
  const originalBundle = await activeBundle(home)

  await publishRelease({ root: assets, release: "0.2.0", targets: [target] })
  const activationFailure = await install({
    home,
    assets,
    target,
    environment: { MUXVIA_INSTALLER_TEST_FAIL_BEFORE_ACTIVATION: "1" },
  })
  expect(activationFailure).toMatchObject({ exitCode: 1, stderr: "muxvia-installer:activation-failed\n" })
  expect(await activeBundle(home)).toBe(originalBundle)
  expect((await launch(home)).stdout).toBe("0.1.0:darwin-arm64:control-plane\n")

  expect(await install({ home, assets, target })).toMatchObject({ exitCode: 0, stderr: "" })
  const updatedBundle = await activeBundle(home)
  expect(updatedBundle).not.toBe(originalBundle)
  expect((await launch(home)).stdout).toBe("0.2.0:darwin-arm64:control-plane\n")
  expect(await readdir(originalBundle)).toContain("muxvia-routing")
  expect(await readdir(updatedBundle)).toContain("muxvia-routing")

  await publishRelease({ root: assets, release: "0.3.0", targets: [target], corruptBindingTarget: target })
  const verificationFailure = await install({ home, assets, target })
  expect(verificationFailure.exitCode).toBe(1)
  expect(verificationFailure.stderr).toContain("muxvia-installer:bundle-invalid:file-length:routing-service")
  expect(await activeBundle(home)).toBe(updatedBundle)

  const downloadFailure = await install({
    home,
    assets,
    target,
    environment: { MUXVIA_INSTALLER_TEST_MANIFEST_URL: `file://${join(assets, "missing.json")}` },
  })
  expect(downloadFailure.exitCode).toBe(1)
  expect(downloadFailure.stderr).toContain("muxvia-installer:download-failed")
  expect(await activeBundle(home)).toBe(updatedBundle)
}, 15_000)

test("release metadata, archive hash, and binding identity failures cannot create an active installation", async () => {
  for (const failure of ["public-product", "archive-hash", "bundle-product"] as const) {
    const assets = await temporaryRoot(`metadata-${failure}`)
    const home = await temporaryRoot(`metadata-home-${failure}`)
    await publishRelease({
      root: assets,
      release: "0.1.0",
      targets: ["darwin-x64"],
      ...(failure === "public-product"
        ? { publicProduct: "other" }
        : failure === "archive-hash"
          ? { corruptArchiveHash: true }
          : { bundleProduct: "other" }),
    })
    const result = await install({ home, assets, target: "darwin-x64" })
    expect(result.exitCode).toBe(1)
    expect(result.stderr).toContain(failure === "public-product"
      ? "muxvia-installer:release-metadata-invalid"
      : failure === "archive-hash"
        ? "muxvia-installer:archive-hash-mismatch"
        : "muxvia-installer:bundle-invalid:identity")
    expect(readFile(join(home, ".muxvia/install/active-version"), "utf8")).rejects.toThrow()
  }
})

test("Homebrew and npm ownership conflicts fail deterministically before installer writes", async () => {
  for (const owner of ["homebrew", "npm"] as const) {
    const root = await temporaryRoot(`ownership-${owner}`)
    const home = join(root, "home")
    const bin = join(root, "bin")
    const owned = owner === "homebrew"
      ? join(root, "opt/homebrew/Cellar/muxvia/0.1.0/bin/muxvia")
      : join(root, "lib/node_modules/muxvia/bin/muxvia")
    await mkdir(home, { recursive: true })
    await mkdir(bin, { recursive: true })
    await mkdir(resolve(owned, ".."), { recursive: true })
    await writeFile(owned, "#!/bin/sh\nexit 0\n", { mode: 0o755 })
    await symlink(owned, join(bin, "muxvia"))

    const result = await run(["sh", installer], {
      home,
      environment: { PATH: `${bin}:${isolatedSystemPath}` },
    })
    expect(result).toEqual({
      exitCode: 1,
      stdout: "",
      stderr: owner === "homebrew"
        ? "muxvia-installer:ownership-conflict:homebrew:run-brew-upgrade-muxvia\n"
        : "muxvia-installer:ownership-conflict:npm:run-npm-install-global-muxvia\n",
    })
    expect(readFile(join(home, ".muxvia/install/owner"), "utf8")).rejects.toThrow()
  }
})

test("package-manager ownership markers are never overwritten", async () => {
  for (const owner of ["homebrew", "npm"] as const) {
    const home = await temporaryRoot(`marker-${owner}`)
    const installRoot = join(home, ".muxvia/install")
    await mkdir(installRoot, { recursive: true })
    await writeFile(join(installRoot, "owner"), `${owner}\n`)
    const result = await run(["sh", installer], { home })
    expect(result.exitCode).toBe(1)
    expect(result.stderr).toContain(`muxvia-installer:ownership-conflict:${owner}`)
    expect(await readFile(join(installRoot, "owner"), "utf8")).toBe(`${owner}\n`)
    expect(await readdir(installRoot)).toEqual(["owner"])
  }
})
