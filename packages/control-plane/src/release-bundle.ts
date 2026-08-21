import { createHash } from "node:crypto"
import { createReadStream } from "node:fs"
import { lstat, readFile, readdir, realpath, stat } from "node:fs/promises"
import { dirname, isAbsolute, join } from "node:path"
import { z } from "zod"

export const BUNDLE_MANIFEST_FILE = "muxvia-release.json"

const bundleTargets = [
  "darwin-arm64",
  "darwin-x64",
  "linux-glibc-arm64",
  "linux-glibc-x64",
] as const

const fileContracts = [
  { role: "control-plane", path: "muxvia", executable: true },
  { role: "routing-service", path: "muxvia-routing", executable: true },
  { role: "license", path: "LICENSE", executable: false },
  { role: "third-party-notices", path: "THIRD_PARTY_NOTICES.md", executable: false },
  { role: "extraction-manifest", path: "EXTRACTION_MANIFEST.json", executable: false },
] as const

const bundleFileSchema = z.object({
  role: z.enum(fileContracts.map((file) => file.role)),
  path: z.string().min(1),
  executable: z.boolean(),
  byteLength: z.number().int().nonnegative(),
  sha256: z.string().regex(/^[0-9a-f]{64}$/),
}).strict()

const bundleManifestSchema = z.object({
  schemaVersion: z.literal(1),
  product: z.literal("muxvia"),
  release: z.string().regex(/^\d+\.\d+\.\d+$/),
  target: z.enum(bundleTargets),
  build: z.string().regex(/^[0-9A-Za-z._-]{7,128}$/),
  rpc: z.object({
    major: z.number().int().nonnegative(),
    minor: z.number().int().nonnegative(),
  }).strict(),
  files: z.array(bundleFileSchema).length(fileContracts.length),
}).strict()

export type BundleTarget = typeof bundleTargets[number]
export type BundleManifest = z.infer<typeof bundleManifestSchema>

export interface ExpectedBundleIdentity {
  release: string
  routingRelease: string
  target: BundleTarget
  build: string
  rpc: { major: number; minor: number }
}

export interface ValidatedReleaseBundle {
  root: string
  routingServicePath: string
  manifest: BundleManifest
}

export class ReleaseBundleError extends Error {
  readonly code = "release-bundle-invalid"

  constructor(reason: string) {
    super(`release-bundle-invalid:${reason}`)
  }
}

declare const MUXVIA_BUNDLE_RELEASE: string
declare const MUXVIA_ROUTING_RELEASE: string
declare const MUXVIA_BUNDLE_TARGET: BundleTarget
declare const MUXVIA_BUNDLE_BUILD: string

export function embeddedBundleIdentity(): ExpectedBundleIdentity | undefined {
  if (
    typeof MUXVIA_BUNDLE_RELEASE !== "string"
    || typeof MUXVIA_ROUTING_RELEASE !== "string"
    || typeof MUXVIA_BUNDLE_TARGET !== "string"
    || typeof MUXVIA_BUNDLE_BUILD !== "string"
  ) return undefined
  return {
    release: MUXVIA_BUNDLE_RELEASE,
    routingRelease: MUXVIA_ROUTING_RELEASE,
    target: MUXVIA_BUNDLE_TARGET,
    build: MUXVIA_BUNDLE_BUILD,
    rpc: { major: 1, minor: 0 },
  }
}

export function runtimeBundleTarget(
  platform: NodeJS.Platform = process.platform,
  architecture: string = process.arch,
): BundleTarget {
  if (platform === "darwin" && architecture === "arm64") return "darwin-arm64"
  if (platform === "darwin" && architecture === "x64") return "darwin-x64"
  if (platform === "linux" && architecture === "arm64") return "linux-glibc-arm64"
  if (platform === "linux" && architecture === "x64") return "linux-glibc-x64"
  throw new ReleaseBundleError("unsupported-target")
}

async function sha256(path: string): Promise<string> {
  const hash = createHash("sha256")
  for await (const chunk of createReadStream(path)) hash.update(chunk)
  return hash.digest("hex")
}

export async function createBundleManifest(options: {
  root: string
  release: string
  target: BundleTarget
  build: string
}): Promise<BundleManifest> {
  const files = await Promise.all(fileContracts.map(async (contract) => {
    const path = join(options.root, contract.path)
    const metadata = await stat(path)
    return {
      ...contract,
      byteLength: metadata.size,
      sha256: await sha256(path),
    }
  }))
  return bundleManifestSchema.parse({
    schemaVersion: 1,
    product: "muxvia",
    release: options.release,
    target: options.target,
    build: options.build,
    rpc: { major: 1, minor: 0 },
    files,
  })
}

function rejectUnless(condition: boolean, reason: string): asserts condition {
  if (!condition) throw new ReleaseBundleError(reason)
}

async function validateReleaseBundleForTarget(
  controlPlanePath: string,
  expected: ExpectedBundleIdentity,
  runtimeTarget: BundleTarget,
): Promise<ValidatedReleaseBundle> {
  try {
    rejectUnless(expected.target === runtimeTarget, "runtime-target-mismatch")
    rejectUnless(expected.release === expected.routingRelease, "component-release-mismatch")
    const canonicalControlPlane = await realpath(controlPlanePath)
    const root = dirname(canonicalControlPlane)
    const manifestPath = join(root, BUNDLE_MANIFEST_FILE)
    const manifestMetadata = await lstat(manifestPath)
    rejectUnless(manifestMetadata.isFile() && !manifestMetadata.isSymbolicLink(), "manifest-type")
    const manifest = bundleManifestSchema.parse(JSON.parse(await readFile(manifestPath, "utf8")))
    rejectUnless(manifest.release === expected.release, "release")
    rejectUnless(manifest.target === expected.target, "target")
    rejectUnless(manifest.build === expected.build, "build")
    rejectUnless(
      manifest.rpc.major === expected.rpc.major && manifest.rpc.minor === expected.rpc.minor,
      "rpc",
    )

    const expectedNames = [BUNDLE_MANIFEST_FILE, ...fileContracts.map((file) => file.path)].sort()
    rejectUnless(JSON.stringify((await readdir(root)).sort()) === JSON.stringify(expectedNames), "file-set")
    rejectUnless(new Set(manifest.files.map((file) => file.role)).size === fileContracts.length, "roles")

    for (let index = 0; index < fileContracts.length; index += 1) {
      const contract = fileContracts[index]!
      const file = manifest.files[index]!
      rejectUnless(
        file.role === contract.role
        && file.path === contract.path
        && file.executable === contract.executable,
        `file-contract-${contract.role}`,
      )
      const path = join(root, contract.path)
      const metadata = await lstat(path)
      rejectUnless(metadata.isFile() && !metadata.isSymbolicLink(), `file-type-${contract.role}`)
      rejectUnless(metadata.size === file.byteLength, `file-length-${contract.role}`)
      rejectUnless(
        contract.executable ? (metadata.mode & 0o111) !== 0 : (metadata.mode & 0o111) === 0,
        `file-mode-${contract.role}`,
      )
      rejectUnless(await sha256(path) === file.sha256, `file-hash-${contract.role}`)
    }
    rejectUnless(canonicalControlPlane === join(root, "muxvia"), "control-plane-path")
    return { root, routingServicePath: join(root, "muxvia-routing"), manifest }
  } catch (error) {
    if (error instanceof ReleaseBundleError) throw error
    throw new ReleaseBundleError("unreadable")
  }
}

export function validateReleaseBundle(
  controlPlanePath: string,
  expected: ExpectedBundleIdentity,
): Promise<ValidatedReleaseBundle> {
  return validateReleaseBundleForTarget(controlPlanePath, expected, runtimeBundleTarget())
}

export function validatePackagedReleaseBundle(
  controlPlanePath: string,
  expected: ExpectedBundleIdentity,
): Promise<ValidatedReleaseBundle> {
  return validateReleaseBundleForTarget(controlPlanePath, expected, expected.target)
}

export async function validatePassedBundleRoot(
  passedRoot: string | undefined,
  validatedRoot: string,
): Promise<void> {
  if (passedRoot === undefined) return
  try {
    rejectUnless(isAbsolute(passedRoot), "bundle-root")
    rejectUnless(await realpath(passedRoot) === validatedRoot, "bundle-root")
  } catch (error) {
    if (error instanceof ReleaseBundleError) throw error
    throw new ReleaseBundleError("bundle-root")
  }
}
