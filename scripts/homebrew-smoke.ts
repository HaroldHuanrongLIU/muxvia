#!/usr/bin/env bun

import { createHash } from "node:crypto"
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises"
import { join, resolve } from "node:path"

const args = Bun.argv.slice(2)

function option(name: string): string {
  const index = args.indexOf(name)
  const value = index >= 0 ? args[index + 1] : undefined
  if (!value || value.startsWith("--")) throw new Error(`missing ${name}`)
  return value
}

async function capture(
  command: string[],
  environment: Record<string, string | undefined>,
  acceptedExitCodes = [0],
): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  const process = Bun.spawn(command, {
    env: environment,
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  const [exitCode, stdout, stderr] = await Promise.all([
    process.exited,
    new Response(process.stdout).text(),
    new Response(process.stderr).text(),
  ])
  if (!acceptedExitCodes.includes(exitCode)) {
    throw new Error(`${command.join(" ")} failed (${exitCode}): ${stderr.trim()}`)
  }
  return { exitCode, stdout, stderr }
}

function rejectUnless(condition: boolean, message: string): asserts condition {
  if (!condition) throw new Error(message)
}

async function writeTargetFixture(path: string, target: "codex" | "claude"): Promise<void> {
  const version = target === "codex" ? "codex-cli 0.106.0" : "2.1.37 (Claude Code)"
  const help = target === "codex"
    ? "Usage: codex --config VALUE"
    : "Usage: claude --settings FILE --model MODEL"
  await writeFile(path, `#!/bin/sh
case "\${1-}" in
  --version) printf '%s\\n' '${version}' ;;
  --help) printf '%s\\n' '${help}' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  await chmod(path, 0o700)
}

async function bundleSnapshot(root: string): Promise<string> {
  const names = (await readdir(root)).sort()
  const expectedNames = [
    "EXTRACTION_MANIFEST.json",
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
    "muxvia",
    "muxvia-release.json",
    "muxvia-routing",
  ]
  rejectUnless(
    JSON.stringify(names) === JSON.stringify(expectedNames),
    `Homebrew libexec is not exactly one Release Bundle: ${names.join(", ")}`,
  )
  const files = []
  for (const name of names) {
    const path = join(root, name)
    const metadata = await lstat(path)
    rejectUnless(metadata.isFile() && !metadata.isSymbolicLink(), `invalid bundle member: ${name}`)
    files.push({
      name,
      mode: metadata.mode & 0o777,
      sha256: createHash("sha256").update(await readFile(path)).digest("hex"),
    })
  }
  return JSON.stringify(files)
}

async function smokeTui(
  executable: string,
  environment: Record<string, string | undefined>,
  cwd: string,
): Promise<void> {
  const output: Uint8Array[] = []
  const terminal = new Bun.Terminal({
    cols: 100,
    rows: 28,
    name: "xterm-256color",
    data: (_terminal, data) => output.push(Uint8Array.from(data)),
  })
  const process = Bun.spawn([executable], { terminal, env: environment, cwd })
  try {
    await Promise.race([
      (async () => {
        while (process.exitCode === null) {
          const screen = new TextDecoder().decode(Buffer.concat(output.map((chunk) => Buffer.from(chunk))))
          if (screen.includes("MUXVIA")) return
          await Bun.sleep(25)
        }
        const screen = new TextDecoder().decode(Buffer.concat(output.map((chunk) => Buffer.from(chunk))))
        throw new Error(`TUI exited before rendering (${process.exitCode}): ${screen}`)
      })(),
      Bun.sleep(10_000).then(() => { throw new Error("TUI startup timed out") }),
    ])
    process.kill("SIGTERM")
    const exitCode = await Promise.race([
      process.exited,
      Bun.sleep(8_000).then(() => { throw new Error("TUI shutdown timed out") }),
    ])
    rejectUnless(exitCode === 0, `TUI exited with ${exitCode}`)
  } finally {
    if (process.exitCode === null) process.kill("SIGKILL")
    await process.exited.catch(() => {})
    terminal.close()
  }
}

async function waitForIdle(executable: string, environment: Record<string, string | undefined>): Promise<void> {
  await Promise.race([
    (async () => {
      while (true) {
        const status = await capture([executable, "status", "--json"], environment)
        if (JSON.parse(status.stdout).service?.state === "stopped") return
        await Bun.sleep(50)
      }
    })(),
    Bun.sleep(8_000).then(() => { throw new Error("Routing Service did not exit after TUI smoke") }),
  ])
}

async function verifyInstalledBundle(
  executable: string,
  release: string,
  environment: Record<string, string | undefined>,
): Promise<{ prefix: string; snapshot: string }> {
  const prefix = await realpath(
    (await capture(["brew", "--prefix", "muxvia"], environment)).stdout.trim(),
  )
  const root = join(prefix, "libexec")
  rejectUnless(await realpath(executable) === join(root, "muxvia"), "public executable does not resolve into libexec")
  const snapshot = await bundleSnapshot(root)

  const version = JSON.parse((await capture([executable, "version", "--json"], environment)).stdout)
  rejectUnless(version.product === "muxvia", "Homebrew version product mismatch")
  rejectUnless(version.release === release, "Homebrew version release mismatch")
  rejectUnless(version.routingService?.release === release, "Homebrew sidecar release mismatch")

  const sidecar = JSON.parse((await capture([
    join(root, "muxvia-routing"),
    "--lifecycle-metadata",
  ], environment)).stdout)
  rejectUnless(sidecar.product === "muxvia-routing", "Homebrew sidecar product mismatch")
  rejectUnless(sidecar.release === release, "Homebrew sidecar metadata mismatch")

  const doctorResult = await capture([executable, "doctor", "--json"], environment, [0, 78])
  const doctor = JSON.parse(doctorResult.stdout)
  const checks = Object.fromEntries(doctor.checks.map((check: { id: string }) => [check.id, check])) as Record<
    string,
    { status?: string; code?: string }
  >
  rejectUnless(checks["bundle.control-plane"]?.status === "pass", "doctor rejected Homebrew control plane")
  rejectUnless(checks["bundle.routing-service"]?.status === "pass", "doctor rejected Homebrew sidecar")
  rejectUnless(
    doctor.disclosures?.macos?.appleVerification === "not-established",
    "doctor overstated Apple verification",
  )
  rejectUnless(await bundleSnapshot(root) === snapshot, "diagnostics mutated the Homebrew Release Bundle")
  return { prefix, snapshot }
}

async function main(): Promise<void> {
  rejectUnless(process.platform === "darwin", "Homebrew smoke requires macOS")
  const formula = resolve(option("--formula"))
  const upgradeFormula = resolve(option("--upgrade-formula"))
  const release = option("--release")
  const root = await mkdtemp("/tmp/mxhb-")
  const home = join(root, "home")
  const fixtureBin = join(root, "bin")
  await mkdir(home, { mode: 0o700 })
  await mkdir(fixtureBin, { mode: 0o700 })
  await writeTargetFixture(join(fixtureBin, "codex"), "codex")
  await writeTargetFixture(join(fixtureBin, "claude"), "claude")
  const environment = {
    ...Bun.env,
    HOME: home,
    PATH: `${fixtureBin}:${Bun.env.PATH ?? ""}`,
    TERM: "xterm-256color",
    HOMEBREW_NO_AUTO_UPDATE: "1",
    MUXVIA_UPDATE_CHECK: "0",
  }
  let installed = false
  let tapped = false
  try {
    await capture(["brew", "tap-new", "--no-git", "muxvia/smoke"], environment)
    tapped = true
    const tapRoot = (await capture(["brew", "--repository", "muxvia/smoke"], environment)).stdout.trim()
    const tapFormula = join(tapRoot, "Formula/muxvia.rb")
    await copyFile(formula, tapFormula)
    await capture(["brew", "install", "muxvia/smoke/muxvia"], environment)
    installed = true
    const executable = (await capture(["brew", "--prefix"], environment)).stdout.trim() + "/bin/muxvia"
    const initial = await verifyInstalledBundle(executable, release, environment)
    await capture(["brew", "test", "muxvia"], environment)
    await smokeTui(executable, environment, root)
    await waitForIdle(executable, environment)
    rejectUnless(
      await bundleSnapshot(join(initial.prefix, "libexec")) === initial.snapshot,
      "Control Plane startup mutated the Homebrew Release Bundle",
    )

    await copyFile(upgradeFormula, tapFormula)
    await capture(["brew", "upgrade", "muxvia"], environment)
    const upgraded = await verifyInstalledBundle(executable, release, environment)
    rejectUnless(upgraded.prefix !== initial.prefix, "Homebrew did not activate the revision upgrade")
    rejectUnless(upgraded.snapshot === initial.snapshot, "Homebrew upgrade split or changed the Release Bundle")

    await capture(["brew", "uninstall", "muxvia"], environment)
    installed = false
    const absent = await capture(["brew", "list", "--versions", "muxvia"], environment, [1])
    rejectUnless(absent.exitCode === 1, "Homebrew uninstall left muxvia installed")
    await lstat(executable).then(
      () => { throw new Error("Homebrew uninstall left the public executable linked") },
      () => {},
    )
  } finally {
    if (installed) await capture(["brew", "uninstall", "--force", "muxvia"], environment, [0, 1]).catch(() => {})
    if (tapped) await capture(["brew", "untap", "muxvia/smoke"], environment, [0, 1]).catch(() => {})
    await rm(root, { recursive: true, force: true })
  }
}

await main()
