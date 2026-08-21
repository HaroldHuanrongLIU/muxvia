import { createHash } from "node:crypto"
import { spawn } from "node:child_process"
import { createReadStream } from "node:fs"
import { lstat, readFile, readdir, realpath } from "node:fs/promises"
import { createRequire } from "node:module"
import { constants as osConstants } from "node:os"
import { dirname, isAbsolute, join } from "node:path"

const targets = {
  "darwin-arm64": { packageName: "@muxvia/darwin-arm64", os: "darwin", cpu: "arm64" },
  "darwin-x64": { packageName: "@muxvia/darwin-x64", os: "darwin", cpu: "x64" },
  "linux-glibc-arm64": {
    packageName: "@muxvia/linux-glibc-arm64",
    os: "linux",
    cpu: "arm64",
    libc: "glibc",
  },
  "linux-glibc-x64": {
    packageName: "@muxvia/linux-glibc-x64",
    os: "linux",
    cpu: "x64",
    libc: "glibc",
  },
}

const fileContracts = [
  { role: "control-plane", path: "muxvia", executable: true },
  { role: "routing-service", path: "muxvia-routing", executable: true },
  { role: "license", path: "LICENSE", executable: false },
  { role: "third-party-notices", path: "THIRD_PARTY_NOTICES.md", executable: false },
  { role: "extraction-manifest", path: "EXTRACTION_MANIFEST.json", executable: false },
]

const repairPrefix = "muxvia: the required exact-version optional package is missing or invalid."

export class LauncherRepairError extends Error {
  constructor(packageName, version) {
    super(`${repairPrefix} Repair with: npm install --include=optional muxvia@${version} (requires ${packageName}@${version}).`)
  }
}

function glibcRuntime() {
  if (process.platform !== "linux") return undefined
  try {
    return process.report?.getReport()?.header?.glibcVersionRuntime ? "glibc" : "musl"
  } catch {
    return undefined
  }
}

export function runtimeTarget(
  platform = process.platform,
  architecture = process.arch,
  libc = glibcRuntime(),
) {
  if (platform === "darwin" && (architecture === "arm64" || architecture === "x64")) {
    return `darwin-${architecture}`
  }
  if (
    platform === "linux"
    && libc === "glibc"
    && (architecture === "arm64" || architecture === "x64")
  ) return `linux-glibc-${architecture}`
  throw new Error(`unsupported-target:${platform}-${architecture}${libc ? `-${libc}` : ""}`)
}

function exactKeys(value, keys) {
  return value !== null
    && typeof value === "object"
    && !Array.isArray(value)
    && JSON.stringify(Object.keys(value).sort()) === JSON.stringify([...keys].sort())
}

function validString(value, pattern) {
  return typeof value === "string" && pattern.test(value)
}

async function sha256(path) {
  const digest = createHash("sha256")
  for await (const chunk of createReadStream(path)) digest.update(chunk)
  return digest.digest("hex")
}

async function regularFile(path, executable) {
  const metadata = await lstat(path)
  return metadata.isFile()
    && !metadata.isSymbolicLink()
    && (executable ? (metadata.mode & 0o111) !== 0 : (metadata.mode & 0o111) === 0)
}

function parsePlatformPackage(value, expected) {
  if (!exactKeys(value, [
    "name", "version", "description", "license", "repository", "os", "cpu", "files",
    ...(expected.libc ? ["libc"] : []), "muxviaBundle",
  ])) throw new Error("platform-package-fields")
  if (
    value.name !== expected.packageName
    || value.version !== expected.release
    || value.license !== "MIT"
    || JSON.stringify(value.os) !== JSON.stringify([expected.os])
    || JSON.stringify(value.cpu) !== JSON.stringify([expected.cpu])
    || JSON.stringify(value.files) !== JSON.stringify(["bundle"])
    || (expected.libc && JSON.stringify(value.libc) !== JSON.stringify([expected.libc]))
  ) throw new Error("platform-package-identity")
  const descriptor = value.muxviaBundle
  if (!exactKeys(descriptor, ["schemaVersion", "product", "release", "target", "build", "rpc"])) {
    throw new Error("platform-package-metadata")
  }
  if (
    descriptor.schemaVersion !== 1
    || descriptor.product !== "muxvia"
    || descriptor.release !== expected.release
    || descriptor.target !== expected.target
    || !validString(descriptor.build, /^[0-9A-Za-z._-]{7,128}$/)
    || !exactKeys(descriptor.rpc, ["major", "minor"])
    || descriptor.rpc.major !== 1
    || descriptor.rpc.minor !== 0
  ) throw new Error("platform-package-metadata")
  return descriptor
}

function parseManifest(value, expected) {
  if (!exactKeys(value, ["schemaVersion", "product", "release", "target", "build", "rpc", "files"])) {
    throw new Error("bundle-manifest-fields")
  }
  if (
    value.schemaVersion !== 1
    || value.product !== "muxvia"
    || value.release !== expected.release
    || value.target !== expected.target
    || value.build !== expected.build
    || !exactKeys(value.rpc, ["major", "minor"])
    || value.rpc.major !== expected.rpc.major
    || value.rpc.minor !== expected.rpc.minor
    || !Array.isArray(value.files)
    || value.files.length !== fileContracts.length
  ) throw new Error("bundle-manifest-identity")
  return value
}

async function validateBundle(packageJsonPath, expected) {
  const canonicalPackageJson = await realpath(packageJsonPath)
  if (!await regularFile(canonicalPackageJson, false)) throw new Error("platform-package-type")
  const packageRoot = dirname(canonicalPackageJson)
  const bundlePath = join(packageRoot, "bundle")
  const bundleMetadata = await lstat(bundlePath)
  if (!bundleMetadata.isDirectory() || bundleMetadata.isSymbolicLink()) throw new Error("bundle-root-type")
  const bundleRoot = await realpath(bundlePath)
  if (!isAbsolute(bundleRoot) || dirname(bundleRoot) !== packageRoot) throw new Error("bundle-root-path")
  const manifestPath = join(bundleRoot, "muxvia-release.json")
  if (!await regularFile(manifestPath, false)) throw new Error("bundle-manifest-type")
  const manifest = parseManifest(JSON.parse(await readFile(manifestPath, "utf8")), expected)
  const expectedNames = ["muxvia-release.json", ...fileContracts.map((file) => file.path)].sort()
  if (JSON.stringify((await readdir(bundleRoot)).sort()) !== JSON.stringify(expectedNames)) {
    throw new Error("bundle-file-set")
  }
  for (let index = 0; index < fileContracts.length; index += 1) {
    const contract = fileContracts[index]
    const file = manifest.files[index]
    if (
      !exactKeys(file, ["role", "path", "executable", "byteLength", "sha256"])
      || file.role !== contract.role
      || file.path !== contract.path
      || file.executable !== contract.executable
      || !Number.isSafeInteger(file.byteLength)
      || file.byteLength < 0
      || !validString(file.sha256, /^[0-9a-f]{64}$/)
    ) throw new Error("bundle-file-metadata")
    const path = join(bundleRoot, contract.path)
    const metadata = await lstat(path)
    if (
      !await regularFile(path, contract.executable)
      || metadata.size !== file.byteLength
      || await sha256(path) !== file.sha256
    ) throw new Error("bundle-file-integrity")
  }
  return { bundleRoot, executable: join(bundleRoot, "muxvia") }
}

export async function prepareLaunch(options = {}) {
  const launcherVersion = options.launcherVersion
    ?? JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8")).version
  const target = options.target ?? runtimeTarget()
  const expected = targets[target]
  if (!expected) throw new Error(`unsupported-target:${target}`)
  const resolvePackageJson = options.resolvePackageJson
    ?? ((specifier) => createRequire(import.meta.url).resolve(specifier))
  try {
    const packageJsonPath = resolvePackageJson(`${expected.packageName}/package.json`)
    const platformPackage = JSON.parse(await readFile(packageJsonPath, "utf8"))
    const descriptor = parsePlatformPackage(platformPackage, {
      ...expected,
      target,
      release: launcherVersion,
    })
    const bundle = await validateBundle(packageJsonPath, descriptor)
    return { ...bundle, target, packageName: expected.packageName, launcherVersion }
  } catch (error) {
    if (error instanceof LauncherRepairError) throw error
    throw new LauncherRepairError(expected.packageName, launcherVersion)
  }
}

function signalExitCode(signal) {
  const number = osConstants.signals[signal]
  return typeof number === "number" ? 128 + number : 1
}

export async function runLauncher(options = {}) {
  let prepared
  try {
    prepared = await prepareLaunch(options)
  } catch (error) {
    const message = error instanceof LauncherRepairError
      ? error.message
      : `muxvia: ${error instanceof Error ? error.message : "launcher failed"}.`
    ;(options.stderr ?? process.stderr).write(`${message}\n`)
    return 78
  }
  const spawnProcess = options.spawnProcess ?? spawn
  let child
  try {
    child = spawnProcess(prepared.executable, options.args ?? process.argv.slice(2), {
      stdio: "inherit",
      env: {
        ...process.env,
        MUXVIA_BUNDLE_ROOT: prepared.bundleRoot,
      },
    })
  } catch {
    ;(options.stderr ?? process.stderr).write(
      `${new LauncherRepairError(prepared.packageName, prepared.launcherVersion).message}\n`,
    )
    return 78
  }
  return await new Promise((resolve) => {
    child.once("error", () => {
      ;(options.stderr ?? process.stderr).write(
        `${new LauncherRepairError(prepared.packageName, prepared.launcherVersion).message}\n`,
      )
      resolve(78)
    })
    child.once("exit", (code, signal) => resolve(code ?? signalExitCode(signal)))
  })
}
