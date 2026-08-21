#!/usr/bin/env bun

import { createHash } from "node:crypto"
import { createReadStream } from "node:fs"
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  open,
  readFile,
  readdir,
  writeFile,
} from "node:fs/promises"
import { basename, dirname, join, resolve } from "node:path"

import {
  BUNDLE_MANIFEST_FILE,
  type BundleTarget,
  createBundleManifest,
  validateReleaseBundle,
} from "../packages/control-plane/src/release-bundle"

const command = Bun.argv[2]
const args = Bun.argv.slice(3)

function option(name: string): string {
  const index = args.indexOf(name)
  const value = index >= 0 ? args[index + 1] : undefined
  if (!value || value.startsWith("--")) throw new Error(`missing ${name}`)
  return value
}

function targetOption(): BundleTarget {
  const target = option("--target")
  if (!["darwin-arm64", "darwin-x64", "linux-glibc-arm64", "linux-glibc-x64"].includes(target)) {
    throw new Error("invalid --target")
  }
  return target as BundleTarget
}

async function requireRegular(path: string): Promise<void> {
  const metadata = await lstat(path)
  if (!metadata.isFile() || metadata.isSymbolicLink()) throw new Error(`not a regular file: ${path}`)
}

async function copyMember(source: string, destination: string, mode: number): Promise<void> {
  await requireRegular(source)
  await copyFile(source, destination)
  await chmod(destination, mode)
}

async function assemble(): Promise<void> {
  const root = resolve(option("--output"))
  const release = option("--release")
  const target = targetOption()
  const build = option("--build")
  const controlPlane = resolve(option("--control-plane"))
  const routingService = resolve(option("--routing-service"))
  await mkdir(root, { recursive: true, mode: 0o755 })
  if ((await readdir(root)).length !== 0) throw new Error("bundle output must be empty")
  await copyMember(controlPlane, join(root, "muxvia"), 0o755)
  await copyMember(routingService, join(root, "muxvia-routing"), 0o755)
  for (const name of ["LICENSE", "THIRD_PARTY_NOTICES.md", "EXTRACTION_MANIFEST.json"] as const) {
    await copyMember(resolve(name), join(root, name), 0o644)
  }
  const manifest = await createBundleManifest({ root, release, target, build })
  await writeFile(join(root, BUNDLE_MANIFEST_FILE), `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o644 })
}

async function inspect(): Promise<void> {
  const root = resolve(option("--root"))
  const release = option("--release")
  const target = targetOption()
  const build = option("--build")
  await validateReleaseBundle(join(root, "muxvia"), {
    release,
    routingRelease: release,
    target,
    build,
    rpc: { major: 1, minor: 0 },
  })
  const extraction = JSON.parse(await readFile(join(root, "EXTRACTION_MANIFEST.json"), "utf8")) as {
    schemaVersion?: unknown
    materials?: unknown[]
  }
  if (extraction.schemaVersion !== 1 || !Array.isArray(extraction.materials) || extraction.materials.length === 0) {
    throw new Error("extraction manifest is incomplete")
  }
  const notices = await readFile(join(root, "THIRD_PARTY_NOTICES.md"), "utf8")
  if (!notices.includes("Copyright (c) 2025 Jason Young") || !notices.includes("Copyright (c) 2025 opencode")) {
    throw new Error("third-party notices are incomplete")
  }
}

async function capture(
  cmd: string[],
  environment: Record<string, string | undefined> = {},
  cwd?: string,
): Promise<string> {
  const process = Bun.spawn(cmd, {
    stdout: "pipe",
    stderr: "pipe",
    env: { ...Bun.env, ...environment },
    cwd,
  })
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(process.stdout).text(),
    new Response(process.stderr).text(),
    process.exited,
  ])
  if (exitCode !== 0) throw new Error(`smoke command failed (${exitCode}): ${stderr.trim()}`)
  return stdout
}

async function smoke(): Promise<void> {
  const root = resolve(option("--root"))
  const release = option("--release")
  const version = JSON.parse(await capture(
    [join(root, "muxvia"), "version", "--json"],
    { MUXVIA_UPDATE_CHECK: "0" },
    root,
  )) as { product?: unknown; release?: unknown; routingService?: { release?: unknown; rpc?: unknown } }
  if (
    version.product !== "muxvia"
    || version.release !== release
    || version.routingService?.release !== release
    || JSON.stringify(version.routingService.rpc) !== JSON.stringify({ major: 1, minor: 0 })
  ) throw new Error("control plane smoke metadata mismatch")
  const routing = JSON.parse(await capture(
    [join(root, "muxvia-routing"), "--lifecycle-metadata"],
    {},
    root,
  )) as {
    product?: unknown
    release?: unknown
    rpc?: unknown
  }
  if (
    routing.product !== "muxvia-routing"
    || routing.release !== release
    || JSON.stringify(routing.rpc) !== JSON.stringify({ major: 1, minor: 0 })
  ) throw new Error("routing service smoke metadata mismatch")
}

const forbiddenSecrets = [
  /-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----/,
  /\bgh[pousr]_[A-Za-z0-9_]{20,}\b/,
  /\bsk-ant-[A-Za-z0-9_-]{20,}\b/,
  /\bsk-[A-Za-z0-9]{32,}\b/,
  /\bAKIA[0-9A-Z]{16}\b/,
]

async function scan(): Promise<void> {
  const root = resolve(option("--root"))
  for (const name of await readdir(root)) {
    const contents = Buffer.from(await readFile(join(root, name))).toString("latin1")
    if (forbiddenSecrets.some((pattern) => pattern.test(contents))) {
      throw new Error(`release secret scan failed: ${name}`)
    }
  }
}

async function archive(): Promise<void> {
  const root = resolve(option("--root"))
  const destination = resolve(option("--output"))
  await open(destination, "wx").then((handle) => handle.close())
  const process = Bun.spawn([
    "tar", "-czf", destination, "-C", dirname(root), basename(root),
  ], {
    stdout: "inherit",
    stderr: "inherit",
    env: { ...Bun.env, COPYFILE_DISABLE: "1" },
  })
  if (await process.exited !== 0) throw new Error("archive creation failed")
}

async function hash(path: string): Promise<string> {
  const digest = createHash("sha256")
  for await (const chunk of createReadStream(path)) digest.update(chunk)
  return digest.digest("hex")
}

async function publicManifest(): Promise<void> {
  const release = option("--release")
  const archives = resolve(option("--archives"))
  const output = resolve(option("--output"))
  const bundles = []
  for (const target of ["darwin-arm64", "darwin-x64", "linux-glibc-arm64", "linux-glibc-x64"] as const) {
    const archive = `muxvia-${release}-${target}.tar.gz`
    const path = join(archives, archive)
    await requireRegular(path)
    bundles.push({
      target,
      archive,
      sha256: await hash(path),
    })
  }
  await writeFile(output, `${JSON.stringify({
    schemaVersion: 1,
    product: "muxvia",
    release,
    bundles,
  }, null, 2)}\n`, { mode: 0o644, flag: "wx" })
}

switch (command) {
  case "assemble": await assemble(); break
  case "inspect": await inspect(); break
  case "smoke": await smoke(); break
  case "scan": await scan(); break
  case "archive": await archive(); break
  case "public-manifest": await publicManifest(); break
  default: throw new Error("expected assemble, inspect, smoke, scan, archive, or public-manifest")
}
