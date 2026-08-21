#!/usr/bin/env bun

import { cp, lstat, mkdir, readFile, readdir, realpath, writeFile } from "node:fs/promises"
import { isAbsolute, join, relative, resolve, sep } from "node:path"

import {
  type BundleTarget,
  validatePackagedReleaseBundle,
} from "../packages/control-plane/src/release-bundle"

export const npmTargets = {
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
} as const satisfies Record<BundleTarget, {
  packageName: string
  os: "darwin" | "linux"
  cpu: "arm64" | "x64"
  libc?: "glibc"
}>

type PackageOptions = {
  root: string
  release: string
  target: BundleTarget
  build: string
}

function exactKeys(value: unknown, keys: string[]): value is Record<string, unknown> {
  return typeof value === "object"
    && value !== null
    && !Array.isArray(value)
    && JSON.stringify(Object.keys(value).sort()) === JSON.stringify(keys.sort())
}

function rejectUnless(condition: boolean, reason: string): asserts condition {
  if (!condition) throw new Error(`npm-package-invalid:${reason}`)
}

function packageJson(options: Omit<PackageOptions, "root">) {
  const target = npmTargets[options.target]
  return {
    name: target.packageName,
    version: options.release,
    description: `Complete Muxvia Release Bundle for ${options.target}`,
    license: "MIT",
    repository: {
      type: "git",
      url: "git+https://github.com/HaroldHuanrongLIU/muxvia.git",
    },
    os: [target.os],
    cpu: [target.cpu],
    ...("libc" in target ? { libc: [target.libc] } : {}),
    files: ["bundle"],
    muxviaBundle: {
      schemaVersion: 1,
      product: "muxvia",
      release: options.release,
      target: options.target,
      build: options.build,
      rpc: { major: 1, minor: 0 },
    },
  }
}

function expectedIdentity(options: PackageOptions) {
  return {
    release: options.release,
    routingRelease: options.release,
    target: options.target,
    build: options.build,
    rpc: { major: 1, minor: 0 },
  } as const
}

export async function inspectPlatformPackage(options: PackageOptions): Promise<void> {
  const root = await realpath(options.root)
  rejectUnless(JSON.stringify((await readdir(root)).sort()) === JSON.stringify(["bundle", "package.json"]), "file-set")
  const actual = JSON.parse(await readFile(join(root, "package.json"), "utf8")) as unknown
  const expected = packageJson(options)
  rejectUnless(
    exactKeys(actual, Object.keys(expected))
    && JSON.stringify(actual) === JSON.stringify(expected),
    "metadata",
  )
  await validatePackagedReleaseBundle(
    join(root, "bundle/muxvia"),
    expectedIdentity(options),
  )
}

export async function assemblePlatformPackage(options: PackageOptions & { output: string }): Promise<void> {
  const sourceRoot = await realpath(options.root)
  const output = resolve(options.output)
  const relativeOutput = relative(sourceRoot, output)
  rejectUnless(
    relativeOutput !== ""
    && (relativeOutput === ".." || relativeOutput.startsWith(`..${sep}`) || isAbsolute(relativeOutput)),
    "output-overlaps-bundle",
  )
  await validatePackagedReleaseBundle(
    join(sourceRoot, "muxvia"),
    expectedIdentity(options),
  )
  await mkdir(output, { recursive: true, mode: 0o755 })
  rejectUnless((await readdir(output)).length === 0, "output-not-empty")
  await cp(sourceRoot, join(output, "bundle"), {
    recursive: true,
    errorOnExist: true,
    force: false,
    verbatimSymlinks: true,
  })
  await writeFile(join(output, "package.json"), `${JSON.stringify(packageJson(options), null, 2)}\n`, {
    mode: 0o644,
    flag: "wx",
  })
  await inspectPlatformPackage({ ...options, root: output })
}

export async function inspectLauncherPackage(root: string, release: string): Promise<void> {
  const packageRoot = await realpath(root)
  const value = JSON.parse(await readFile(join(packageRoot, "package.json"), "utf8")) as Record<string, unknown>
  rejectUnless(value.name === "muxvia" && value.version === release, "launcher-identity")
  rejectUnless(value.scripts === undefined, "launcher-lifecycle-scripts")
  rejectUnless(
    JSON.stringify(value.bin) === JSON.stringify({ muxvia: "bin/muxvia.js" })
    && JSON.stringify(value.files) === JSON.stringify(["bin", "lib", "README.md", "LICENSE"]),
    "launcher-files",
  )
  const expectedDependencies = Object.fromEntries(
    Object.values(npmTargets).map((target) => [target.packageName, release]),
  )
  rejectUnless(
    JSON.stringify(value.optionalDependencies) === JSON.stringify(expectedDependencies),
    "launcher-optional-dependencies",
  )
  for (const relativePath of ["bin/muxvia.js", "lib/launcher.js", "README.md", "LICENSE"]) {
    const metadata = await lstat(join(packageRoot, relativePath))
    rejectUnless(metadata.isFile() && !metadata.isSymbolicLink(), `launcher-file-${relativePath}`)
  }
  rejectUnless(
    await readFile(join(packageRoot, "LICENSE"), "utf8")
      === await readFile(new URL("../LICENSE", import.meta.url), "utf8"),
    "launcher-license",
  )
}

function option(args: string[], name: string): string {
  const index = args.indexOf(name)
  const value = index >= 0 ? args[index + 1] : undefined
  if (!value || value.startsWith("--")) throw new Error(`missing ${name}`)
  return value
}

function targetOption(args: string[]): BundleTarget {
  const target = option(args, "--target")
  if (!(target in npmTargets)) throw new Error("invalid --target")
  return target as BundleTarget
}

async function main(): Promise<void> {
  const command = Bun.argv[2]
  const args = Bun.argv.slice(3)
  if (command === "assemble-platform") {
    await assemblePlatformPackage({
      root: resolve(option(args, "--root")),
      output: resolve(option(args, "--output")),
      release: option(args, "--release"),
      target: targetOption(args),
      build: option(args, "--build"),
    })
    return
  }
  if (command === "inspect-platform") {
    await inspectPlatformPackage({
      root: resolve(option(args, "--root")),
      release: option(args, "--release"),
      target: targetOption(args),
      build: option(args, "--build"),
    })
    return
  }
  if (command === "inspect-launcher") {
    await inspectLauncherPackage(
      resolve(option(args, "--root")),
      option(args, "--release"),
    )
    return
  }
  throw new Error("expected assemble-platform, inspect-platform, or inspect-launcher")
}

if (import.meta.main) await main()
