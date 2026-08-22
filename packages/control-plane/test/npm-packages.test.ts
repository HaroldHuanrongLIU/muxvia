import { afterEach, expect, test } from "bun:test"
import { mkdir, mkdtemp, readFile, readdir, realpath, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

import {
  BUNDLE_MANIFEST_FILE,
  type BundleTarget,
  createBundleManifest,
  runtimeBundleTarget,
} from "../src/release-bundle"
import {
  assemblePlatformPackage,
  inspectLauncherPackage,
  inspectPlatformPackage,
  npmTargets,
} from "../../../scripts/npm-packages"

const roots: string[] = []
const release = "0.1.1"
const build = "0123456789abcdef"

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

async function bundleFixture(
  target: BundleTarget,
  controlPlane = "control-plane",
): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), `muxvia-npm-${target}-`))
  roots.push(root)
  for (const [name, contents, mode] of [
    ["muxvia", controlPlane, 0o755],
    ["muxvia-routing", "routing-service", 0o755],
    ["LICENSE", "license", 0o644],
    ["THIRD_PARTY_NOTICES.md", "notices", 0o644],
    ["EXTRACTION_MANIFEST.json", "extractions", 0o644],
  ] as const) await writeFile(join(root, name), contents, { mode })
  const manifest = await createBundleManifest({ root, release, target, build })
  await writeFile(join(root, BUNDLE_MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o644 })
  return root
}

async function capture(command: string[], environment: Record<string, string>): Promise<string> {
  const process = Bun.spawn(command, {
    stdout: "pipe",
    stderr: "pipe",
    env: { ...Bun.env, ...environment },
  })
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(process.stdout).text(),
    new Response(process.stderr).text(),
    process.exited,
  ])
  if (exitCode !== 0) throw new Error(`command failed (${exitCode}): ${stderr}`)
  return stdout.trim()
}

for (const target of Object.keys(npmTargets) as BundleTarget[]) {
  test(`assembles a network-free ${target} package containing one complete Release Bundle`, async () => {
    const bundle = await bundleFixture(target)
    const parent = await mkdtemp(join(tmpdir(), `muxvia-npm-package-${target}-`))
    roots.push(parent)
    const output = join(parent, "package")
    await assemblePlatformPackage({ root: bundle, output, release, target, build })
    await inspectPlatformPackage({ root: output, release, target, build })

    expect((await readdir(output)).sort()).toEqual(["bundle", "package.json"])
    expect((await readdir(join(output, "bundle"))).sort()).toEqual([
      "EXTRACTION_MANIFEST.json",
      "LICENSE",
      "THIRD_PARTY_NOTICES.md",
      "muxvia",
      "muxvia-release.json",
      "muxvia-routing",
    ])
    const metadata = JSON.parse(await readFile(join(output, "package.json"), "utf8"))
    expect(metadata.name).toBe(npmTargets[target].packageName)
    expect(metadata.version).toBe(release)
    expect(metadata.scripts).toBeUndefined()
    expect(metadata.muxviaBundle).toEqual({
      schemaVersion: 1,
      product: "muxvia",
      release,
      target,
      build,
      rpc: { major: 1, minor: 0 },
    })

    const pack = Bun.spawn(["npm", "pack", "--dry-run", "--json", output], {
      stdout: "pipe",
      stderr: "pipe",
      env: { ...Bun.env, npm_config_cache: join(parent, "npm-cache") },
    })
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(pack.stdout).text(),
      new Response(pack.stderr).text(),
      pack.exited,
    ])
    expect(exitCode, stderr).toBe(0)
    const packed = JSON.parse(stdout)[0]
    expect(packed.files.map((file: { path: string }) => file.path).sort()).toEqual([
      "bundle/EXTRACTION_MANIFEST.json",
      "bundle/LICENSE",
      "bundle/THIRD_PARTY_NOTICES.md",
      "bundle/muxvia",
      "bundle/muxvia-release.json",
      "bundle/muxvia-routing",
      "package.json",
    ])
  })
}

test("launcher metadata pins every platform package to the exact product release without lifecycle scripts", async () => {
  const root = resolve("packages/npm-launcher")
  await inspectLauncherPackage(root, release)
  const metadata = JSON.parse(await readFile(join(root, "package.json"), "utf8"))
  expect(metadata.scripts).toBeUndefined()
  expect(metadata.optionalDependencies).toEqual({
    "@muxvia/darwin-arm64": release,
    "@muxvia/darwin-x64": release,
    "@muxvia/linux-glibc-arm64": release,
    "@muxvia/linux-glibc-x64": release,
  })
  for (const version of Object.values(metadata.optionalDependencies)) {
    expect(version).toMatch(/^\d+\.\d+\.\d+$/)
  }
  expect(await readFile(join(root, "LICENSE"), "utf8")).toBe(await readFile(resolve("LICENSE"), "utf8"))

  const parent = await mkdtemp(join(tmpdir(), "muxvia-npm-launcher-pack-"))
  roots.push(parent)
  const packed = JSON.parse(await capture(
    ["npm", "pack", "--dry-run", "--json", root],
    { npm_config_cache: join(parent, "npm-cache") },
  ))[0]
  expect(packed.files.map((file: { path: string }) => file.path)).toContain("LICENSE")
})

test("packaged integrity changes fail inspection", async () => {
  const target = "linux-glibc-x64"
  const bundle = await bundleFixture(target)
  const parent = await mkdtemp(join(tmpdir(), "muxvia-npm-tamper-"))
  roots.push(parent)
  const output = join(parent, "package")
  await assemblePlatformPackage({ root: bundle, output, release, target, build })
  await writeFile(join(output, "bundle/muxvia"), "tampered")
  expect(inspectPlatformPackage({ root: output, release, target, build })).rejects.toThrow(
    "release-bundle-invalid",
  )
})

test("installs and executes the matching local tarballs without registry access or lifecycle scripts", async () => {
  const target = runtimeBundleTarget()
  const bundle = await bundleFixture(
    target,
    "#!/bin/sh\nprintf '%s|%s\\n' \"$MUXVIA_BUNDLE_ROOT\" \"$*\"\n",
  )
  const parent = await mkdtemp(join(tmpdir(), "muxvia-npm-smoke-"))
  roots.push(parent)
  const platformRoot = join(parent, "platform")
  const tarballs = join(parent, "tarballs")
  const installRoot = join(parent, "install")
  const cache = join(parent, "npm-cache")
  await mkdir(tarballs)
  await assemblePlatformPackage({ root: bundle, output: platformRoot, release, target, build })
  const environment = { npm_config_cache: cache, npm_config_update_notifier: "false" }
  const platformTarball = await capture(
    ["npm", "pack", platformRoot, "--pack-destination", tarballs, "--silent"],
    environment,
  )
  const launcherTarball = await capture(
    ["npm", "pack", resolve("packages/npm-launcher"), "--pack-destination", tarballs, "--silent"],
    environment,
  )
  await capture([
    "npm", "install",
    "--prefix", installRoot,
    "--offline",
    "--ignore-scripts",
    "--omit=optional",
    "--package-lock=false",
    join(tarballs, launcherTarball),
    join(tarballs, platformTarball),
  ], environment)
  const output = await capture(
    [join(installRoot, "node_modules/.bin/muxvia"), "version", "--json"],
    {},
  )
  const [passedRoot, args] = output.split("|")
  expect(passedRoot).toBe(await realpath(join(
    installRoot,
    "node_modules",
    ...npmTargets[target].packageName.split("/"),
    "bundle",
  )))
  expect(args).toBe("version --json")
})
