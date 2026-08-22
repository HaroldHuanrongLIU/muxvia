#!/usr/bin/env bun

import { createHash } from "node:crypto"
import {
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises"
import { tmpdir } from "node:os"
import { basename, dirname, join, resolve } from "node:path"

import {
  type BundleManifest,
  type BundleTarget,
  validatePackagedReleaseBundle,
} from "../packages/control-plane/src/release-bundle"
import { createHomebrewFormula, type PublicReleaseManifest } from "./homebrew-formula"
import { inspectLauncherPackage, inspectPlatformPackage } from "./npm-packages"

export const qualificationTargets = [
  "darwin-arm64",
  "darwin-x64",
  "linux-glibc-arm64",
  "linux-glibc-x64",
] as const satisfies readonly BundleTarget[]

type TargetCli = "codex" | "claude"
type EvidenceBoundary = "first" | "latest"

export interface QualificationPolicy {
  schemaVersion: 1
  product: "muxvia"
  release: string
  supportedTargets: BundleTarget[]
  compatibility: Record<TargetCli, {
    package: string
    first: string
    latest: string
  }>
  configurationHomes: ["~/.codex", "~/.claude"]
  protocols: ["OpenAI Responses", "Anthropic Messages", "Anthropic token counting"]
}

interface CompatibilityReceipt {
  schemaVersion: 1
  product: "muxvia"
  release: string
  build: string
  targetCli: TargetCli
  evidence: EvidenceBoundary
  package: string
  packageVersion: string
  observedVersion: string
  classification: "tested"
}

function rejectUnless(condition: boolean, reason: string): asserts condition {
  if (!condition) throw new Error(`release-qualification-invalid:${reason}`)
}

function exactKeys(value: unknown, keys: string[]): value is Record<string, unknown> {
  return typeof value === "object"
    && value !== null
    && !Array.isArray(value)
    && JSON.stringify(Object.keys(value).sort()) === JSON.stringify([...keys].sort())
}

function isVersion(value: unknown): value is string {
  return typeof value === "string" && /^\d+\.\d+\.\d+$/.test(value)
}

function isBuild(value: unknown): value is string {
  return typeof value === "string" && /^[0-9A-Za-z._-]{7,128}$/.test(value)
}

async function readJson(path: string): Promise<unknown> {
  return JSON.parse(await readFile(path, "utf8")) as unknown
}

export async function readQualificationPolicy(path: string): Promise<QualificationPolicy> {
  const value = await readJson(path)
  rejectUnless(exactKeys(value, [
    "schemaVersion",
    "product",
    "release",
    "supportedTargets",
    "compatibility",
    "configurationHomes",
    "protocols",
  ]), "policy-shape")
  rejectUnless(value.schemaVersion === 1 && value.product === "muxvia" && isVersion(value.release), "policy-identity")
  rejectUnless(
    JSON.stringify(value.supportedTargets) === JSON.stringify(qualificationTargets),
    "policy-targets",
  )
  rejectUnless(exactKeys(value.compatibility, ["codex", "claude"]), "policy-compatibility")
  for (const target of ["codex", "claude"] as const) {
    const entry = value.compatibility[target]
    rejectUnless(exactKeys(entry, ["package", "first", "latest"]), `policy-${target}-shape`)
    rejectUnless(
      typeof entry.package === "string" && isVersion(entry.first) && isVersion(entry.latest),
      `policy-${target}-versions`,
    )
  }
  rejectUnless(
    JSON.stringify(value.configurationHomes) === JSON.stringify(["~/.codex", "~/.claude"]),
    "policy-configuration-homes",
  )
  rejectUnless(
    JSON.stringify(value.protocols) === JSON.stringify([
      "OpenAI Responses",
      "Anthropic Messages",
      "Anthropic token counting",
    ]),
    "policy-protocols",
  )
  return value as unknown as QualificationPolicy
}

async function sha256(path: string): Promise<string> {
  return createHash("sha256").update(await readFile(path)).digest("hex")
}

async function capture(
  command: string[],
  options: { cwd?: string; env?: Record<string, string | undefined>; accepted?: number[] } = {},
): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  const child = Bun.spawn(command, {
    cwd: options.cwd,
    env: { ...Bun.env, ...options.env },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
  ])
  rejectUnless((options.accepted ?? [0]).includes(exitCode), `command:${basename(command[0] ?? "unknown")}:${exitCode}:${stderr.trim()}`)
  return { exitCode, stdout, stderr }
}

function expectedObservedVersion(target: TargetCli, version: string): string {
  return target === "codex" ? `codex-cli ${version}` : `${version} (Claude Code)`
}

export async function createCompatibilityReceipt(options: {
  policyPath: string
  muxvia: string
  target: TargetCli
  evidence: EvidenceBoundary
  version: string
  release: string
  build: string
  output: string
}): Promise<CompatibilityReceipt> {
  const policy = await readQualificationPolicy(options.policyPath)
  rejectUnless(options.release === policy.release, "compatibility-release")
  rejectUnless(isBuild(options.build), "compatibility-build")
  const expectedVersion = policy.compatibility[options.target][options.evidence]
  rejectUnless(options.version === expectedVersion, "compatibility-version")
  const home = await mkdtemp(join(tmpdir(), "muxvia-compatibility-"))
  try {
    const version = JSON.parse((await capture(
      [resolve(options.muxvia), "version", "--json"],
      { env: { HOME: home, MUXVIA_UPDATE_CHECK: "0" } },
    )).stdout) as { product?: unknown; release?: unknown }
    rejectUnless(version.product === "muxvia" && version.release === options.release, "compatibility-product")
    const doctor = JSON.parse((await capture(
      [resolve(options.muxvia), "doctor", "--json"],
      { env: { HOME: home, MUXVIA_UPDATE_CHECK: "0" }, accepted: [0, 78] },
    )).stdout) as { checks?: Array<Record<string, unknown>> }
    const check = doctor.checks?.find((candidate) => candidate.id === `compatibility.${options.target}`)
    rejectUnless(
      check?.status === "pass"
      && check.code === "tested"
      && check.version === expectedObservedVersion(options.target, options.version),
      `compatibility-${options.target}-${options.evidence}`,
    )
    const receipt: CompatibilityReceipt = {
      schemaVersion: 1,
      product: "muxvia",
      release: options.release,
      build: options.build,
      targetCli: options.target,
      evidence: options.evidence,
      package: policy.compatibility[options.target].package,
      packageVersion: options.version,
      observedVersion: check.version,
      classification: "tested",
    }
    await mkdir(dirname(resolve(options.output)), { recursive: true })
    await writeFile(resolve(options.output), `${JSON.stringify(receipt, null, 2)}\n`, { flag: "wx", mode: 0o644 })
    return receipt
  } finally {
    await rm(home, { recursive: true, force: true })
  }
}

function parsePublicManifest(value: unknown, policy: QualificationPolicy): PublicReleaseManifest {
  rejectUnless(exactKeys(value, ["schemaVersion", "product", "release", "bundles"]), "public-manifest-shape")
  rejectUnless(value.schemaVersion === 1 && value.product === "muxvia" && value.release === policy.release, "public-manifest-identity")
  rejectUnless(Array.isArray(value.bundles) && value.bundles.length === qualificationTargets.length, "public-manifest-bundles")
  for (let index = 0; index < qualificationTargets.length; index += 1) {
    const target = qualificationTargets[index]!
    const bundle = value.bundles[index]
    rejectUnless(exactKeys(bundle, ["target", "archive", "sha256"]), `public-manifest-${target}-shape`)
    rejectUnless(
      bundle.target === target
      && bundle.archive === `muxvia-${policy.release}-${target}.tar.gz`
      && typeof bundle.sha256 === "string"
      && /^[0-9a-f]{64}$/.test(bundle.sha256),
      `public-manifest-${target}`,
    )
  }
  return value as unknown as PublicReleaseManifest
}

function parseReceipt(value: unknown, policy: QualificationPolicy, build: string): CompatibilityReceipt {
  rejectUnless(exactKeys(value, [
    "schemaVersion",
    "product",
    "release",
    "build",
    "targetCli",
    "evidence",
    "package",
    "packageVersion",
    "observedVersion",
    "classification",
  ]), "receipt-shape")
  rejectUnless(
    value.schemaVersion === 1
    && value.product === "muxvia"
    && value.release === policy.release
    && value.build === build
    && (value.targetCli === "codex" || value.targetCli === "claude")
    && (value.evidence === "first" || value.evidence === "latest")
    && value.classification === "tested",
    "receipt-identity",
  )
  const target = value.targetCli
  const evidence = value.evidence
  const expected = policy.compatibility[target]
  rejectUnless(
    value.package === expected.package
    && value.packageVersion === expected[evidence]
    && value.observedVersion === expectedObservedVersion(target, expected[evidence]),
    `receipt-${target}-${evidence}`,
  )
  return value as unknown as CompatibilityReceipt
}

async function verifyPublicDocumentation(): Promise<void> {
  const [releases, bridge, notices] = await Promise.all([
    readFile(resolve("docs/releases.md"), "utf8"),
    readFile(resolve("docs/subscription-bridge.md"), "utf8"),
    readFile(resolve("THIRD_PARTY_NOTICES.md"), "utf8"),
  ])
  for (const phrase of [
    "`~/.codex` and `~/.claude`",
    "OpenAI Responses",
    "Anthropic Messages",
    "Anthropic token counting",
    "plaintext",
    "64 KiB",
    "Recovery Backups are sensitive",
    "no product telemetry",
    "never downloads or installs an update",
    "unsigned and unnotarized",
  ]) rejectUnless(releases.includes(phrase), `documentation:${phrase}`)
  rejectUnless(
    bridge.includes("not officially supported or endorsed by OpenAI or Anthropic")
    && bridge.includes("account and subscription terms"),
    "documentation:subscription-bridge",
  )
  rejectUnless(
    notices.includes("does not imply affiliation with or endorsement"),
    "documentation:affiliation",
  )
}

async function extractTarball(tarball: string, root: string): Promise<void> {
  await mkdir(root, { recursive: true })
  await capture(["tar", "-xzf", tarball, "-C", root])
}

export async function createQualificationRecord(options: {
  policyPath: string
  artifacts: string
  manifest: string
  formula: string
  installer: string
  launcher: string
  release: string
  build: string
  output: string
}): Promise<Record<string, unknown>> {
  const policy = await readQualificationPolicy(options.policyPath)
  rejectUnless(options.release === policy.release && isBuild(options.build), "record-identity")
  const artifacts = resolve(options.artifacts)
  const manifest = parsePublicManifest(await readJson(resolve(options.manifest)), policy)
  rejectUnless(
    await readFile(resolve(options.formula), "utf8") === createHomebrewFormula(manifest),
    "homebrew-formula",
  )
  await inspectLauncherPackage(resolve(options.launcher), options.release)
  rejectUnless(
    await readFile(resolve(options.installer), "utf8") === await readFile(resolve("scripts/install.sh"), "utf8"),
    "verified-download-installer",
  )
  await verifyPublicDocumentation()

  const temporary = await mkdtemp(join(tmpdir(), "muxvia-qualification-"))
  try {
    const archiveBundles: Array<{ target: BundleTarget; archiveSha256: string; manifest: BundleManifest }> = []
    const npmBundles: Array<{ target: BundleTarget; tarballSha256: string; manifest: BundleManifest }> = []
    const legalHashes = {
      license: await sha256(resolve("LICENSE")),
      thirdPartyNotices: await sha256(resolve("THIRD_PARTY_NOTICES.md")),
      extractionManifest: await sha256(resolve("EXTRACTION_MANIFEST.json")),
    }
    for (const target of qualificationTargets) {
      const archiveName = `muxvia-${options.release}-${target}.tar.gz`
      const archive = join(artifacts, archiveName)
      const publicBundle = manifest.bundles.find((bundle) => bundle.target === target)!
      const archiveSha256 = await sha256(archive)
      rejectUnless(archiveSha256 === publicBundle.sha256, `archive-hash-${target}`)
      const archiveRoot = join(temporary, `archive-${target}`)
      await extractTarball(archive, archiveRoot)
      const archiveBundle = await validatePackagedReleaseBundle(
        join(archiveRoot, `muxvia-${options.release}-${target}`, "muxvia"),
        { release: options.release, routingRelease: options.release, target, build: options.build, rpc: { major: 1, minor: 0 } },
      )

      const npmTarball = join(artifacts, `muxvia-${target}-${options.release}.tgz`)
      const npmRoot = join(temporary, `npm-${target}`)
      await extractTarball(npmTarball, npmRoot)
      await inspectPlatformPackage({ root: join(npmRoot, "package"), release: options.release, target, build: options.build })
      const archiveManifestText = await readFile(join(archiveBundle.root, "muxvia-release.json"), "utf8")
      const npmManifestText = await readFile(join(npmRoot, "package/bundle/muxvia-release.json"), "utf8")
      const npmManifest = JSON.parse(npmManifestText) as BundleManifest
      rejectUnless(
        npmManifestText === archiveManifestText,
        `cross-channel-manifest-${target}`,
      )
      const roleHashes = Object.fromEntries(archiveBundle.manifest.files.map((file) => [file.role, file.sha256]))
      rejectUnless(
        roleHashes.license === legalHashes.license
        && roleHashes["third-party-notices"] === legalHashes.thirdPartyNotices
        && roleHashes["extraction-manifest"] === legalHashes.extractionManifest,
        `legal-materials-${target}`,
      )
      archiveBundles.push({ target, archiveSha256, manifest: archiveBundle.manifest })
      npmBundles.push({ target, tarballSha256: await sha256(npmTarball), manifest: npmManifest })
    }

    const receipts: CompatibilityReceipt[] = []
    for (const target of ["codex", "claude"] as const) {
      for (const evidence of ["first", "latest"] as const) {
        const version = policy.compatibility[target][evidence]
        receipts.push(parseReceipt(await readJson(join(
          artifacts,
          `muxvia-compatibility-${target}-${evidence}-${version}.json`,
        )), policy, options.build))
      }
    }
    const record = {
      schemaVersion: 1,
      product: "muxvia",
      release: options.release,
      build: options.build,
      supportedTargets: [...qualificationTargets],
      channels: {
        githubArchives: archiveBundles,
        homebrew: {
          targets: ["darwin-arm64", "darwin-x64"],
          formulaSha256: await sha256(resolve(options.formula)),
        },
        verifiedDownload: {
          targets: [...qualificationTargets],
          manifestSha256: await sha256(resolve(options.manifest)),
          installerSha256: await sha256(resolve(options.installer)),
        },
        npm: {
          launcher: `muxvia@${options.release}`,
          launcherPackageSha256: await sha256(join(resolve(options.launcher), "package.json")),
          bundles: npmBundles,
        },
      },
      compatibility: receipts,
      qualityGates: [
        "compatibility-goldens",
        "configuration-fault-injection",
        "uds-and-loopback-security",
        "pty-restoration",
        "multi-process-lifecycle",
        "configuration-restore",
        "state-migration",
        "secret-scanning",
      ],
      claims: {
        configurationHomes: policy.configurationHomes,
        protocols: policy.protocols,
        telemetry: false,
        automaticInstall: false,
        appleVerification: "not-established",
      },
      legalHashes,
    }
    await mkdir(dirname(resolve(options.output)), { recursive: true })
    await writeFile(resolve(options.output), `${JSON.stringify(record, null, 2)}\n`, { flag: "wx", mode: 0o644 })
    return record
  } finally {
    await rm(temporary, { recursive: true, force: true })
  }
}

function option(args: string[], name: string): string {
  const index = args.indexOf(name)
  const value = index >= 0 ? args[index + 1] : undefined
  if (!value || value.startsWith("--")) throw new Error(`missing ${name}`)
  return value
}

async function main(): Promise<void> {
  const command = Bun.argv[2]
  const args = Bun.argv.slice(3)
  if (command === "compatibility") {
    const target = option(args, "--target")
    const evidence = option(args, "--evidence")
    rejectUnless(target === "codex" || target === "claude", "compatibility-target")
    rejectUnless(evidence === "first" || evidence === "latest", "compatibility-evidence")
    await createCompatibilityReceipt({
      policyPath: resolve(option(args, "--policy")),
      muxvia: resolve(option(args, "--muxvia")),
      target,
      evidence,
      version: option(args, "--version"),
      release: option(args, "--release"),
      build: option(args, "--build"),
      output: resolve(option(args, "--output")),
    })
    return
  }
  if (command === "record") {
    await createQualificationRecord({
      policyPath: resolve(option(args, "--policy")),
      artifacts: resolve(option(args, "--artifacts")),
      manifest: resolve(option(args, "--manifest")),
      formula: resolve(option(args, "--formula")),
      installer: resolve(option(args, "--installer")),
      launcher: resolve(option(args, "--launcher")),
      release: option(args, "--release"),
      build: option(args, "--build"),
      output: resolve(option(args, "--output")),
    })
    return
  }
  throw new Error("expected compatibility or record")
}

if (import.meta.main) await main()
