import { afterEach, expect, test } from "bun:test"
import { createHash } from "node:crypto"
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises"
import { tmpdir } from "node:os"
import { join, resolve } from "node:path"

import {
  type BundleTarget,
  createBundleManifest,
} from "../src/release-bundle"
import { createHomebrewFormula, type PublicReleaseManifest } from "../../../scripts/homebrew-formula"
import { assemblePlatformPackage } from "../../../scripts/npm-packages"
import {
  createQualificationRecord,
  qualificationTargets,
  readQualificationPolicy,
} from "../../../scripts/release-qualification"

const roots: string[] = []
const release = "0.1.1"
const build = "0123456789abcdef0123456789abcdef01234567"

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

async function temporaryRoot(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "muxvia-qualification-test-"))
  roots.push(root)
  return root
}

async function sha256(path: string): Promise<string> {
  return createHash("sha256").update(await readFile(path)).digest("hex")
}

async function run(command: string[], environment: Record<string, string> = {}): Promise<string> {
  const child = Bun.spawn(command, {
    env: { ...Bun.env, ...environment },
    stdout: "pipe",
    stderr: "pipe",
  })
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ])
  expect(exitCode, stderr).toBe(0)
  return stdout.trim()
}

async function createBundle(root: string, target: BundleTarget): Promise<string> {
  const bundle = join(root, `muxvia-${release}-${target}`)
  await mkdir(bundle)
  await writeFile(join(bundle, "muxvia"), "#!/bin/sh\nexit 0\n", { mode: 0o755 })
  await writeFile(join(bundle, "muxvia-routing"), "#!/bin/sh\nexit 0\n", { mode: 0o755 })
  await chmod(join(bundle, "muxvia"), 0o755)
  await chmod(join(bundle, "muxvia-routing"), 0o755)
  for (const name of ["LICENSE", "THIRD_PARTY_NOTICES.md", "EXTRACTION_MANIFEST.json"]) {
    await copyFile(resolve(name), join(bundle, name))
    await chmod(join(bundle, name), 0o644)
  }
  const manifest = await createBundleManifest({ root: bundle, release, target, build })
  await writeFile(join(bundle, "muxvia-release.json"), `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o644 })
  return bundle
}

async function prepareQualification(root: string): Promise<{
  artifacts: string
  manifest: string
  formula: string
  installer: string
  output: string
}> {
  const source = join(root, "source")
  const artifacts = join(root, "artifacts")
  await mkdir(source)
  await mkdir(artifacts)
  const publicBundles: PublicReleaseManifest["bundles"] = []
  for (const target of qualificationTargets) {
    const bundle = await createBundle(source, target)
    const archive = join(artifacts, `muxvia-${release}-${target}.tar.gz`)
    await run(["tar", "-czf", archive, "-C", source, `muxvia-${release}-${target}`])
    publicBundles.push({ target, archive: `muxvia-${release}-${target}.tar.gz`, sha256: await sha256(archive) })
    const platform = join(root, `platform-${target}`)
    await assemblePlatformPackage({ root: bundle, output: platform, release, target, build })
    await run(
      ["npm", "pack", platform, "--pack-destination", artifacts, "--silent"],
      { NPM_CONFIG_CACHE: join(root, "npm-cache") },
    )
  }

  const publicManifest: PublicReleaseManifest = {
    schemaVersion: 1,
    product: "muxvia",
    release,
    bundles: publicBundles,
  }
  const manifest = join(root, "muxvia-latest.json")
  const formula = join(root, "muxvia.rb")
  const installer = join(root, "install.sh")
  const output = join(root, "muxvia-qualification.json")
  await writeFile(manifest, `${JSON.stringify(publicManifest, null, 2)}\n`)
  await writeFile(formula, createHomebrewFormula(publicManifest))
  await copyFile(resolve("scripts/install.sh"), installer)

  const policy = await readQualificationPolicy(resolve("release/qualification-policy.json"))
  for (const target of ["codex", "claude"] as const) {
    for (const evidence of ["first", "latest"] as const) {
      const version = policy.compatibility[target][evidence]
      await writeFile(
        join(artifacts, `muxvia-compatibility-${target}-${evidence}-${version}.json`),
        `${JSON.stringify({
          schemaVersion: 1,
          product: "muxvia",
          release,
          build,
          targetCli: target,
          evidence,
          package: policy.compatibility[target].package,
          packageVersion: version,
          observedVersion: target === "codex" ? `codex-cli ${version}` : `${version} (Claude Code)`,
          classification: "tested",
        }, null, 2)}\n`,
      )
    }
  }
  return { artifacts, manifest, formula, installer, output }
}

test("four-channel qualification binds one release, build, manifests, legal hashes, and compatibility evidence", async () => {
  const root = await temporaryRoot()
  const paths = await prepareQualification(root)
  const record = await createQualificationRecord({
    policyPath: resolve("release/qualification-policy.json"),
    ...paths,
    launcher: resolve("packages/npm-launcher"),
    release,
    build,
  })

  expect(record).toMatchObject({
    schemaVersion: 1,
    product: "muxvia",
    release,
    build,
    supportedTargets: [...qualificationTargets],
    claims: {
      configurationHomes: ["~/.codex", "~/.claude"],
      protocols: ["OpenAI Responses", "Anthropic Messages", "Anthropic token counting"],
      telemetry: false,
      automaticInstall: false,
      appleVerification: "not-established",
    },
  })
  expect((record.compatibility as unknown[])).toHaveLength(4)
  expect(record.qualityGates).toEqual([
    "compatibility-goldens",
    "configuration-fault-injection",
    "uds-and-loopback-security",
    "pty-restoration",
    "multi-process-lifecycle",
    "configuration-restore",
    "state-migration",
    "secret-scanning",
  ])
  expect((record.channels as { githubArchives: unknown[] }).githubArchives).toHaveLength(4)
  expect((record.channels as { npm: { bundles: unknown[] } }).npm.bundles).toHaveLength(4)
  expect(JSON.parse(await readFile(paths.output, "utf8"))).toEqual(record)
}, 30_000)

test("qualification rejects a formula that is not generated from the bound archive manifest", async () => {
  const root = await temporaryRoot()
  const paths = await prepareQualification(root)
  await writeFile(paths.formula, "class Muxvia < Formula\nend\n")

  expect(createQualificationRecord({
    policyPath: resolve("release/qualification-policy.json"),
    ...paths,
    launcher: resolve("packages/npm-launcher"),
    release,
    build,
  })).rejects.toThrow("release-qualification-invalid:homebrew-formula")
}, 30_000)
