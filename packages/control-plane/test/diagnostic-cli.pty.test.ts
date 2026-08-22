import { expect, test } from "bun:test"
import { chmod, lstat, mkdir, mkdtemp, readFile, stat, symlink, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { isAbsolute, join, resolve } from "node:path"
import { createServer } from "node:net"

import { RpcClient } from "../src/control/rpc-client"

const cli = resolve(import.meta.dir, "../src/index.tsx")
const service = resolve(import.meta.dir, "../../../target/debug/muxvia-routing")
const fakeCodex = resolve(import.meta.dir, "../../../tests/e2e/fixtures/fake-codex")
const exitDeadlineMs = 8_000

function deferred<T>() {
  let resolvePromise!: (value: T | PromiseLike<T>) => void
  const promise = new Promise<T>((resolveDeferred) => { resolvePromise = resolveDeferred })
  return { promise, resolve: resolvePromise }
}

async function runCli(args: string[], environment: Record<string, string | undefined> = {}) {
  const proc = Bun.spawn([process.execPath, cli, ...args], {
    env: { ...process.env, ...environment },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  const [exitCode, stdout, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
  ])
  return { exitCode, stdout, stderr }
}

async function waitForSocket(path: string): Promise<void> {
  await Promise.race([
    (async () => {
      while (true) {
        try {
          if ((await stat(path)).isSocket()) return
        } catch {}
        await Bun.sleep(10)
      }
    })(),
    Bun.sleep(exitDeadlineMs).then(() => { throw new Error("control socket timeout") }),
  ])
}

test("paths dispatches before OpenTUI and reports every effective product and Target path", async () => {
  const root = await mkdtemp("/tmp/mxpaths-")
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const output: Uint8Array[] = []
  const terminal = new Bun.Terminal({
    cols: 80,
    rows: 24,
    name: "xterm-256color",
    data: (_terminal, data) => output.push(Uint8Array.from(data)),
  })
  const baselineFlags = terminal.localFlags
  const proc = Bun.spawn([
    process.execPath,
    cli,
    "paths",
    "--json",
    "--service",
    service,
    "--socket",
    join(muxviaHome, "run/control.sock"),
  ], {
    terminal,
    env: {
      ...process.env,
      HOME: userHome,
      CODEX_HOME: join(userHome, "unsupported-codex-home"),
      CLAUDE_CONFIG_DIR: join(userHome, "unsupported-claude-home"),
    },
  })

  try {
    const exitCode = await Promise.race([
      proc.exited,
      Bun.sleep(exitDeadlineMs).then(() => { throw new Error("paths command timeout") }),
    ])
    const raw = new TextDecoder().decode(Buffer.concat(output.map((chunk) => Buffer.from(chunk))))
    expect(exitCode).toBe(0)
    expect(terminal.localFlags).toBe(baselineFlags)
    expect(raw).not.toContain("\x1b[?1049h")
    expect(raw).not.toContain("\x1b[?1049l")
    expect(raw).not.toContain("MUXVIA")
    expect(JSON.parse(raw.trim())).toEqual({
      ok: true,
      command: "paths",
      paths: {
        userHome,
        muxvia: {
          home: muxviaHome,
          state: join(muxviaHome, "state"),
          database: join(muxviaHome, "state/muxvia.db"),
          subscriptionAccounts: join(muxviaHome, "state/subscription-accounts.json"),
          runtime: join(muxviaHome, "run"),
          controlSocket: join(muxviaHome, "run/control.sock"),
          serviceLock: join(muxviaHome, "service.lock"),
          logs: join(muxviaHome, "logs"),
          backups: join(muxviaHome, "backups"),
          exports: join(muxviaHome, "exports"),
        },
        targets: {
          codex: {
            configurationHome: join(userHome, ".codex"),
            managedConfiguration: join(userHome, ".codex/config.toml"),
            environmentHome: join(userHome, "unsupported-codex-home"),
            environmentHomeSupported: false,
          },
          claude: {
            configurationHome: join(userHome, ".claude"),
            managedConfiguration: join(userHome, ".claude/settings.json"),
            environmentHome: join(userHome, "unsupported-claude-home"),
            environmentHomeSupported: false,
          },
        },
        bundle: {
          controlPlane: cli,
          routingService: service,
        },
      },
    })
  } finally {
    if (proc.exitCode === null) proc.kill("SIGKILL")
    await proc.exited.catch(() => {})
    terminal.close()
  }
})

test("version is renderer-free and Provider, model, account, routing, and configuration CRUD stay absent", async () => {
  const version = await runCli(["version", "--json"])
  expect(version).toEqual({
    exitCode: 0,
    stdout: `${JSON.stringify({
      ok: true,
      command: "version",
      product: "muxvia",
      release: "muxvia-dev",
      routingService: {
        release: "0.1.0",
        rpc: { major: 1, minor: 0 },
      },
    })}\n`,
    stderr: "",
  })

  for (const command of ["provider", "model", "account", "routing", "config"]) {
    const rejected = await runCli([command, "list", "--json"])
    expect(rejected.exitCode).toBe(64)
    expect(rejected.stdout).toBe("")
    expect(JSON.parse(rejected.stderr)).toEqual({
      ok: false,
      problem: {
        code: "unsupported-command",
        message: "Unsupported noninteractive command",
      },
    })
    expect(rejected.stderr).not.toContain(command)
  }

  const missingOption = await runCli(["paths", "--socket", "--json"])
  expect(missingOption).toEqual({
    exitCode: 64,
    stdout: "",
    stderr: `${JSON.stringify({
      ok: false,
      problem: { code: "invalid-invocation", message: "CLI invocation is invalid" },
    })}\n`,
  })

  const missingHome = await runCli(["paths", "--json"], { HOME: undefined })
  expect(missingHome).toEqual({
    exitCode: 64,
    stdout: "",
    stderr: `${JSON.stringify({
      ok: false,
      problem: { code: "invalid-environment", message: "Effective paths could not be resolved" },
    })}\n`,
  })
})

test("status reads a real private UDS without extending an idle Routing Service lifetime", async () => {
  const root = await mkdtemp(join(tmpdir(), "muxvia-status-"))
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const socket = join(muxviaHome, "run/control.sock")
  await mkdir(userHome)
  const routing = Bun.spawn([
    service,
    "--home",
    muxviaHome,
  ], {
    env: { ...process.env, HOME: userHome },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  try {
    await waitForSocket(socket)
    const status = await runCli([
      "status",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome })
    expect(status.stderr).toBe("")
    expect(status.exitCode).toBe(0)
    const report = JSON.parse(status.stdout)
    expect(report).toMatchObject({
      ok: true,
      command: "status",
      service: {
        state: "running",
        release: "0.1.0",
        rpc: { major: 1, minor: 0 },
      },
      targets: {
        codex: {
          mode: "unmanaged",
          takeover: { state: "inactive", endpoint: null },
          managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
          recovery: { intentId: null, state: "clean" },
          routeHealth: { state: "unobserved" },
          problems: [],
        },
        claude: {
          mode: "unmanaged",
          takeover: { state: "inactive", endpoint: null },
          managedConfiguration: { state: "unmanaged", path: null, restartRequired: false },
          recovery: { intentId: null, state: "clean" },
          routeHealth: { state: "unobserved" },
          problems: [],
        },
      },
    })
    expect(report.service.epoch).toMatch(/^[0-9a-f-]{36}$/)
    expect(status.stdout).not.toContain("credential")
    expect(await Promise.race([
      routing.exited,
      Bun.sleep(exitDeadlineMs).then(() => { throw new Error("idle service did not exit") }),
    ])).toBe(0)
  } finally {
    if (routing.exitCode === null) routing.kill("SIGKILL")
    await routing.exited.catch(() => {})
  }

  const stopped = await runCli([
    "status",
    "--json",
    "--service",
    service,
    "--socket",
    socket,
  ], { HOME: userHome })
  expect(stopped).toEqual({
    exitCode: 0,
    stdout: `${JSON.stringify({
      ok: true,
      command: "status",
      service: { state: "stopped", release: null, rpc: null, epoch: null },
      targets: { codex: null, claude: null },
    })}\n`,
    stderr: "",
  })
}, 20_000)

test("backup create and inspect use one sensitive private artifact without exposing contents", async () => {
  const providerSecret = "RECOVERY_CLI_PROVIDER_SECRET_17021"
  const replacementSecret = "RECOVERY_CLI_REPLACEMENT_SECRET_18021"
  const refreshSecret = "RECOVERY_CLI_REFRESH_SECRET_17022"
  const root = await mkdtemp(join(tmpdir(), "muxvia-backup-"))
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const socket = join(muxviaHome, "run/control.sock")
  await mkdir(join(muxviaHome, "state"), { recursive: true })
  await writeFile(
    join(muxviaHome, "state/subscription-accounts.json"),
    JSON.stringify({
      version: 1,
      accounts: {
        account: {
          account_id: "account",
          refresh_token: refreshSecret,
          authenticated_at: 1_700_000_000,
          state: "authorized",
        },
      },
      default_account_id: "account",
    }),
    { mode: 0o600 },
  )
  const routing = Bun.spawn([
    service,
    "--home",
    muxviaHome,
    "--test-codex-executable",
    fakeCodex,
  ], {
    env: { ...process.env, HOME: userHome, MUXVIA_INTEGRATION_TEST: "1" },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  let keeper: RpcClient | undefined
  try {
    await waitForSocket(socket)
    keeper = await RpcClient.connect(socket, "recovery-backup-cli-test")
    const session = await keeper.openTarget("codex")
    await session.act({
      kind: "create-provider",
      name: "Recovery CLI",
      baseUrl: "https://recovery-cli.invalid/v1",
      model: "recovery-cli-model",
      credential: { kind: "replace", value: providerSecret },
      presetKey: null,
    })

    const created = await runCli([
      "backup",
      "create",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome })
    expect(created.exitCode).toBe(0)
    expect(created.stderr).toBe("")
    const creation = JSON.parse(created.stdout)
    expect(creation).toMatchObject({
      ok: true,
      command: "backup",
      operation: "create",
      sensitive: true,
      inspection: {
        formatVersion: 1,
        databaseSchemaVersion: 17,
        sensitive: true,
        compatibility: "compatible",
      },
    })
    expect(creation.path).toStartWith(join(muxviaHome, "backups"))
    expect(creation.inspection.entries).toHaveLength(4)
    expect((await stat(creation.path)).mode & 0o777).toBe(0o600)
    expect(created.stdout).not.toContain(providerSecret)
    expect(created.stdout).not.toContain(refreshSecret)

    const inspected = await runCli([
      "backup",
      "inspect",
      creation.path,
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome })
    expect(inspected.exitCode).toBe(0)
    expect(inspected.stderr).toBe("")
    expect(JSON.parse(inspected.stdout)).toEqual({
      ok: true,
      command: "backup",
      operation: "inspect",
      sensitive: true,
      path: creation.path,
      inspection: creation.inspection,
    })
    expect(inspected.stdout).not.toContain(providerSecret)
    expect(inspected.stdout).not.toContain(refreshSecret)

    await session.act({
      kind: "create-provider",
      name: "Replacement Recovery CLI",
      baseUrl: "https://replacement-recovery-cli.invalid/v1",
      model: "replacement-recovery-cli-model",
      credential: { kind: "replace", value: replacementSecret },
      presetKey: null,
    })

    const unacknowledged = await runCli([
      "backup",
      "restore",
      creation.path,
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome })
    expect(unacknowledged).toEqual({
      exitCode: 64,
      stdout: "",
      stderr: `${JSON.stringify({
        ok: false,
        problem: {
          code: "recovery-backup-restore-acknowledgement-required",
          message: "Recovery Backup restore requires explicit acknowledgement that it replaces the current installation",
        },
      })}\n`,
    })

    const restored = await runCli([
      "backup",
      "restore",
      creation.path,
      "--acknowledge-replace-current-installation",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome })
    expect(restored.exitCode).toBe(0)
    expect(restored.stderr).toBe("")
    const restore = JSON.parse(restored.stdout)
    expect(restore).toMatchObject({
      ok: true,
      command: "backup",
      operation: "restore",
      sensitive: true,
      restoredSnapshotId: creation.inspection.snapshotId,
      resumedTakeovers: [],
      restartTargetClis: true,
    })
    expect(isAbsolute(restore.preRestoreBackupPath)).toBeTrue()
    expect((await stat(restore.preRestoreBackupPath)).mode & 0o777).toBe(0o600)
    expect(restored.stdout).not.toContain(providerSecret)
    expect(restored.stdout).not.toContain(replacementSecret)
    expect(restored.stdout).not.toContain(refreshSecret)

    const restoredClient = await RpcClient.connect(socket, "recovery-backup-restored-cli-test")
    const restoredSession = await restoredClient.openTarget("codex")
    expect(restoredSession.get().providers.map((provider) => provider.name)).toEqual([
      "Recovery CLI",
    ])
    await restoredSession.close()
    await restoredClient.close()

    const relative = await runCli([
      "backup",
      "inspect",
      "relative.muxvia-recovery",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome })
    expect(relative).toEqual({
      exitCode: 64,
      stdout: "",
      stderr: `${JSON.stringify({
        ok: false,
        problem: { code: "invalid-invocation", message: "CLI invocation is invalid" },
      })}\n`,
    })
    await session.close()
    expect(await Promise.race([
      routing.exited,
      Bun.sleep(exitDeadlineMs).then(() => { throw new Error("idle service did not exit") }),
    ])).toBe(0)
  } finally {
    await keeper?.close().catch(() => {})
    if (routing.exitCode === null) routing.kill("SIGKILL")
    await routing.exited.catch(() => {})
  }
}, 20_000)

test("doctor is read-only across bundle, permissions, homes, symlinks, shadows, and Target compatibility", async () => {
  const root = await mkdtemp(join(tmpdir(), "muxvia-doctor-"))
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const stateHome = join(muxviaHome, "state")
  const runHome = join(muxviaHome, "run")
  const binHome = join(root, "bin")
  const targetBinHome = join(root, "target-bin")
  const actualCodexHome = join(root, "actual-codex-home")
  const claudeHome = join(userHome, ".claude")
  const claudeSettingsTarget = join(root, "external-claude-settings.json")
  const databaseSecret = "DOCTOR_DATABASE_SECRET_MUST_NOT_ESCAPE"
  const accountSecret = "DOCTOR_REFRESH_TOKEN_MUST_NOT_ESCAPE"
  const configSecret = "DOCTOR_CONFIG_SECRET_MUST_NOT_ESCAPE"
  const targetHelpPadding = "x".repeat(5_000)
  await mkdir(userHome)
  await mkdir(muxviaHome, { mode: 0o755 })
  await mkdir(stateHome, { mode: 0o700 })
  await mkdir(runHome, { mode: 0o700 })
  await mkdir(join(muxviaHome, "exports"), { mode: 0o755 })
  await mkdir(binHome)
  await mkdir(targetBinHome)
  await mkdir(actualCodexHome)
  await mkdir(claudeHome)
  await writeFile(join(stateHome, "muxvia.db"), databaseSecret, { mode: 0o600 })
  await writeFile(
    join(stateHome, "subscription-accounts.json"),
    JSON.stringify({ refreshToken: accountSecret }),
    { mode: 0o600 },
  )
  await writeFile(
    join(actualCodexHome, "config.toml"),
    `profile = "work"\nunrelated = "${configSecret}"\n`,
    { mode: 0o600 },
  )
  await symlink(actualCodexHome, join(userHome, ".codex"))
  await writeFile(claudeSettingsTarget, "{}\n", { mode: 0o600 })
  await symlink(claudeSettingsTarget, join(claudeHome, "settings.json"))
  await writeFile(join(targetBinHome, "codex"), `#!/bin/sh
case "$1" in
  --version) echo 'codex-cli 0.147.0' ;;
  --help) echo 'Usage: codex --config VALUE ${targetHelpPadding}' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  await writeFile(join(targetBinHome, "claude"), `#!/bin/sh
case "$1" in
  --version) echo '2.1.228 (Claude Code)' ;;
  --help) echo 'Usage: claude --settings FILE --model MODEL ${targetHelpPadding}' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  await chmod(join(targetBinHome, "codex"), 0o700)
  await chmod(join(targetBinHome, "claude"), 0o700)
  await symlink(join(targetBinHome, "codex"), join(binHome, "codex"))
  await symlink(join(targetBinHome, "claude"), join(binHome, "claude"))

  const before = {
    database: await readFile(join(stateHome, "muxvia.db")),
    accounts: await readFile(join(stateHome, "subscription-accounts.json")),
    config: await readFile(join(actualCodexHome, "config.toml")),
    muxviaMode: (await stat(muxviaHome)).mode & 0o777,
    settingsLink: (await lstat(join(claudeHome, "settings.json"))).isSymbolicLink(),
  }
  const result = await runCli([
    "doctor",
    "--json",
    "--service",
    service,
    "--socket",
    join(runHome, "control.sock"),
  ], {
    HOME: userHome,
    PATH: `${binHome}:${process.env.PATH ?? ""}`,
    CODEX_HOME: join(userHome, "unsupported-codex-home"),
    CLAUDE_CONFIG_DIR: join(userHome, "unsupported-claude-home"),
    CLAUDE_CODE_USE_BEDROCK: "1",
  })
  expect(result.exitCode).toBe(78)
  expect(result.stderr).toBe("")
  const report = JSON.parse(result.stdout)
  expect(report.ok).toBeFalse()
  expect(report.command).toBe("doctor")
  const checks = Object.fromEntries(report.checks.map((check: any) => [check.id, check]))
  expect(checks["bundle.control-plane"]).toMatchObject({ status: "pass", code: "present" })
  expect(checks["bundle.routing-service"]).toMatchObject({ status: "pass", code: "verified" })
  expect(checks["permissions.muxvia-home"]).toMatchObject({ status: "fail", code: "permissions-too-open" })
  expect(checks["permissions.database"]).toMatchObject({ status: "pass", code: "private" })
  expect(checks["permissions.subscription-accounts"]).toMatchObject({ status: "pass", code: "private" })
  expect(checks["permissions.exports"]).toMatchObject({ status: "pass", code: "shareable-artifacts" })
  expect(checks["home.codex"]).toMatchObject({ status: "warning", code: "unsupported-configuration-home" })
  expect(checks["home.claude"]).toMatchObject({ status: "warning", code: "unsupported-configuration-home" })
  expect(checks["symlink.codex-configuration-home"]).toMatchObject({ status: "pass", code: "directory-symlink-canonicalized" })
  expect(checks["symlink.codex-managed-configuration"]).toMatchObject({ status: "pass", code: "regular-file" })
  expect(checks["symlink.claude-managed-configuration"]).toMatchObject({ status: "fail", code: "managed-file-symlink" })
  expect(checks["shadow.codex-profile"]).toMatchObject({ status: "warning", code: "shadowing-configuration" })
  expect(checks["shadow.claude-selector"]).toMatchObject({ status: "warning", code: "shadowing-configuration" })
  expect(checks["compatibility.codex"]).toMatchObject({ status: "pass", code: "tested", version: "codex-cli 0.147.0" })
  expect(checks["compatibility.claude"]).toMatchObject({ status: "pass", code: "tested", version: "2.1.228 (Claude Code)" })
  expect(report.disclosures.credentialStorage).toEqual({
    applicationEncryption: "none",
    filesystemPermissions: "best-effort-private",
    message: "Provider credentials and subscription refresh tokens are stored locally without application-level encryption.",
  })
  if (process.platform === "darwin") {
    expect(report.disclosures.macos).toEqual({
      appleVerification: "not-established",
      message: "The initial macOS release may be unsigned and unnotarized; Gatekeeper may require explicit Operator approval.",
    })
  }
  for (const secret of [databaseSecret, accountSecret, configSecret]) {
    expect(result.stdout).not.toContain(secret)
  }
  expect(await readFile(join(stateHome, "muxvia.db"))).toEqual(before.database)
  expect(await readFile(join(stateHome, "subscription-accounts.json"))).toEqual(before.accounts)
  expect(await readFile(join(actualCodexHome, "config.toml"))).toEqual(before.config)
  expect((await stat(muxviaHome)).mode & 0o777).toBe(before.muxviaMode)
  expect((await lstat(join(claudeHome, "settings.json"))).isSymbolicLink()).toBe(before.settingsLink)
}, 20_000)

test("service start is bounded and leaves no idle service or operating-system startup artifact", async () => {
  const root = await mkdtemp("/tmp/mxs-")
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const socket = join(muxviaHome, "run/control.sock")
  await mkdir(userHome)
  const startedAt = Date.now()
  const result = await runCli([
    "service",
    "start",
    "--json",
    "--service",
    service,
    "--socket",
    socket,
  ], { HOME: userHome })
  expect(Date.now() - startedAt).toBeLessThan(8_000)
  expect(result).toEqual({
    exitCode: 0,
    stdout: `${JSON.stringify({
      ok: true,
      command: "service-start",
      service: {
        state: "idle-exited",
        started: true,
        release: "0.1.0",
        recoveredTakeovers: [],
      },
    })}\n`,
    stderr: "",
  })
  expect(await Bun.file(socket).exists()).toBeFalse()
  expect(await Bun.file(join(muxviaHome, "state/muxvia.db")).exists()).toBeTrue()
  expect(await Bun.file(join(userHome, "Library/LaunchAgents/dev.muxvia.plist")).exists()).toBeFalse()
  expect(await Bun.file(join(userHome, ".config/systemd/user/muxvia.service")).exists()).toBeFalse()
}, 20_000)

test("service start is bounded against a peer that accepts the UDS but never negotiates", async () => {
  const root = await mkdtemp("/tmp/mxstall-")
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const runHome = join(muxviaHome, "run")
  const socket = join(runHome, "control.sock")
  await mkdir(runHome, { recursive: true })
  const stalled = createServer(() => {})
  await new Promise<void>((resolveListen, rejectListen) => {
    stalled.once("error", rejectListen)
    stalled.listen(socket, resolveListen)
  })
  try {
    const startedAt = Date.now()
    const result = await runCli([
      "service",
      "start",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome })
    expect(Date.now() - startedAt).toBeLessThan(8_000)
    expect(result).toEqual({
      exitCode: 70,
      stdout: "",
      stderr: `${JSON.stringify({
        ok: false,
        problem: {
          code: "service-start-failed",
          message: "Routing Service could not be started",
        },
      })}\n`,
    })
    expect(await Bun.file(join(muxviaHome, "state/muxvia.db")).exists()).toBeFalse()
    expect((await stat(socket)).isSocket()).toBeTrue()
  } finally {
    await new Promise<void>((resolveClose) => stalled.close(() => resolveClose()))
  }
}, 15_000)

test("service stop safely restores Managed Configuration and waits for a committed stream to drain", async () => {
  const root = await mkdtemp("/tmp/mxstop-")
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const socket = join(muxviaHome, "run/control.sock")
  const configHome = join(userHome, ".codex")
  const config = join(configHome, "config.toml")
  const binHome = join(root, "bin")
  const before = Buffer.from("# operator-owned\nunrelated = \"keep\"\n")
  await mkdir(userHome)
  await mkdir(configHome)
  await mkdir(binHome)
  await writeFile(config, before, { mode: 0o640 })
  await writeFile(join(binHome, "codex"), `#!/bin/sh
case "$1" in
  --version) echo 'codex-cli 0.147.0' ;;
  --help) echo 'Usage: codex --config VALUE' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  await writeFile(join(binHome, "claude"), `#!/bin/sh
case "$1" in
  --version) echo '2.1.228 (Claude Code)' ;;
  --help) echo 'Usage: claude --settings FILE --model MODEL' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  const releaseStream = deferred<void>()
  const upstream = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch: () => new Response(new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new TextEncoder().encode("data: {\"type\":\"response.output_text.delta\",\"delta\":\"before\"}\n\n"))
        void releaseStream.promise.then(() => {
          try {
            controller.enqueue(new TextEncoder().encode("data: [DONE]\n\n"))
            controller.close()
          } catch {}
        })
      },
    }), { headers: { "content-type": "text/event-stream" } }),
  })
  const targetPath = `${binHome}:${process.env.PATH ?? ""}`
  const routing = Bun.spawn([service, "--home", muxviaHome], {
    env: { ...process.env, HOME: userHome, PATH: targetPath },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  try {
    await waitForSocket(socket)
    const client = await RpcClient.connect(socket, "diagnostic-safe-stop-test")
    const session = await client.openTarget("codex")
    const saved = await session.act({
      kind: "create-provider",
      name: "held upstream",
      baseUrl: `${upstream.url.origin}/v1`,
      model: "held-model",
      credential: { kind: "replace", value: "SAFE_STOP_PROVIDER_SECRET" },
      presetKey: null,
    })
    const providerId = saved.view.providers[0]?.id
    if (!providerId) throw new Error("provider was not created")
    const activated = await session.act({
      kind: "activate-provider",
      providerId,
      mode: "takeover",
    })
    const endpoint = activated.view.takeover.endpoint
    if (!endpoint) throw new Error("takeover endpoint was not activated")
    await session.close()
    const managed = await readFile(config, "utf8")
    const credential = managed.match(/"X-Muxvia-Routing-Credential"\s*=\s*"([^"]+)"/)?.[1]
    if (!credential) throw new Error("routing credential was not installed")

    const committed = deferred<void>()
    const routed = fetch(`${endpoint}/v1/responses`, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        "X-Muxvia-Routing-Credential": credential,
      },
      body: JSON.stringify({ model: "caller", stream: true, input: "hello" }),
    }).then(async (response) => {
      const reader = response.body?.getReader()
      if (!reader) throw new Error("routed stream had no body")
      const first = await reader.read()
      if (first.done) throw new Error("routed stream did not commit")
      committed.resolve()
      while (!(await reader.read()).done) {}
    })
    await committed.promise

    let stopSettled = false
    const stopping = runCli([
      "service",
      "stop",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome, PATH: targetPath }).then((result) => {
      stopSettled = true
      return result
    })
    await Promise.race([
      (async () => {
        while (!Buffer.from(await readFile(config)).equals(before)) await Bun.sleep(10)
      })(),
      Bun.sleep(exitDeadlineMs).then(() => { throw new Error("Managed Configuration was not restored") }),
    ])
    expect(stopSettled).toBeFalse()
    releaseStream.resolve()
    await routed
    const stopped = await stopping
    expect(stopped).toEqual({
      exitCode: 0,
      stdout: `${JSON.stringify({
        ok: true,
        command: "service-stop",
        mode: "safe",
        disabledTakeovers: ["codex"],
        managedConfiguration: "restored",
        streams: "drained",
        service: { state: "stopped" },
      })}\n`,
      stderr: "",
    })
    expect(await readFile(config)).toEqual(before)
    expect((await stat(config)).mode & 0o777).toBe(0o640)
    expect(await Bun.file(socket).exists()).toBeFalse()
    expect(stopped.stdout).not.toContain(credential)
    expect(await Promise.race([
      routing.exited,
      Bun.sleep(exitDeadlineMs).then(() => { throw new Error("Routing Service did not exit") }),
    ])).toBe(0)
  } finally {
    releaseStream.resolve()
    upstream.stop(true)
    if (routing.exitCode === null) routing.kill("SIGKILL")
    await routing.exited.catch(() => {})
  }
}, 30_000)

test("service force stop requires the exact danger acknowledgement and leaves Managed Configuration in place", async () => {
  const root = await mkdtemp("/tmp/mxforce-")
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const socket = join(muxviaHome, "run/control.sock")
  const configHome = join(userHome, ".codex")
  const config = join(configHome, "config.toml")
  const binHome = join(root, "bin")
  const before = Buffer.from("# operator-owned\nunrelated = \"keep\"\n")
  await mkdir(userHome)
  await mkdir(configHome)
  await mkdir(binHome)
  await writeFile(config, before, { mode: 0o640 })
  await writeFile(join(binHome, "codex"), `#!/bin/sh
case "$1" in
  --version) echo 'codex-cli 0.147.0' ;;
  --help) echo 'Usage: codex --config VALUE' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  await writeFile(join(binHome, "claude"), `#!/bin/sh
case "$1" in
  --version) echo '2.1.228 (Claude Code)' ;;
  --help) echo 'Usage: claude --settings FILE --model MODEL' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  const targetPath = `${binHome}:${process.env.PATH ?? ""}`
  const routing = Bun.spawn([service, "--home", muxviaHome], {
    env: { ...process.env, HOME: userHome, PATH: targetPath },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  try {
    await waitForSocket(socket)
    const client = await RpcClient.connect(socket, "diagnostic-force-stop-test")
    const session = await client.openTarget("codex")
    const saved = await session.act({
      kind: "create-provider",
      name: "force fixture",
      baseUrl: "https://force-stop.invalid/v1",
      model: "force-model",
      credential: { kind: "replace", value: "FORCE_STOP_PROVIDER_SECRET" },
      presetKey: null,
    })
    const providerId = saved.view.providers[0]?.id
    if (!providerId) throw new Error("provider was not created")
    const activated = await session.act({ kind: "activate-provider", providerId, mode: "takeover" })
    const endpoint = activated.view.takeover.endpoint
    if (!endpoint) throw new Error("takeover endpoint was not activated")
    await session.close()
    const managed = await readFile(config)
    expect(managed.toString()).toContain(endpoint)

    const refused = await runCli([
      "service",
      "stop",
      "--force",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome, PATH: targetPath })
    expect(refused).toEqual({
      exitCode: 64,
      stdout: "",
      stderr: `${JSON.stringify({
        ok: false,
        problem: {
          code: "force-acknowledgement-required",
          message: "Force stop requires explicit acknowledgement that managed Target files may remain pointed at a dead endpoint",
        },
      })}\n`,
    })
    expect((await stat(socket)).isSocket()).toBeTrue()
    expect(await readFile(config)).toEqual(managed)

    const stopped = await runCli([
      "service",
      "stop",
      "--force",
      "--acknowledge-managed-target-files-may-remain-pointed-at-dead-endpoint",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome, PATH: targetPath })
    expect(stopped).toEqual({
      exitCode: 0,
      stdout: `${JSON.stringify({
        ok: true,
        command: "service-stop",
        mode: "force",
        service: { state: "stopped" },
        warning: "Managed Target files may remain pointed at a dead endpoint.",
      })}\n`,
      stderr: "",
    })
    expect(await Promise.race([
      routing.exited,
      Bun.sleep(exitDeadlineMs).then(() => { throw new Error("force-stopped Routing Service did not exit") }),
    ])).toBe(0)
    expect(await Bun.file(socket).exists()).toBeFalse()
    expect(await readFile(config)).toEqual(managed)
    expect((await stat(config)).mode & 0o777).toBe(0o640)
    expect(stopped.stdout).not.toContain("FORCE_STOP_PROVIDER_SECRET")
  } finally {
    if (routing.exitCode === null) routing.kill("SIGKILL")
    await routing.exited.catch(() => {})
  }
}, 30_000)

test("service start recovers every configured Target Takeover and safe stop restores both Targets", async () => {
  const root = await mkdtemp("/tmp/mxrecover-")
  const userHome = join(root, "operator")
  const muxviaHome = join(userHome, ".muxvia")
  const socket = join(muxviaHome, "run/control.sock")
  const binHome = join(root, "bin")
  const codexConfig = join(userHome, ".codex/config.toml")
  const claudeConfig = join(userHome, ".claude/settings.json")
  const codexBefore = Buffer.from("# codex operator\nunrelated = \"keep\"\n")
  const claudeBefore = Buffer.from("{\n  \"operator\": \"keep\"\n}\n")
  await mkdir(join(userHome, ".codex"), { recursive: true })
  await mkdir(join(userHome, ".claude"), { recursive: true })
  await mkdir(binHome)
  await writeFile(codexConfig, codexBefore, { mode: 0o640 })
  await writeFile(claudeConfig, claudeBefore, { mode: 0o600 })
  await writeFile(join(binHome, "codex"), `#!/bin/sh
case "$1" in
  --version) echo 'codex-cli 0.147.0' ;;
  --help) echo 'Usage: codex --config VALUE' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  await writeFile(join(binHome, "claude"), `#!/bin/sh
case "$1" in
  --version) echo '2.1.228 (Claude Code)' ;;
  --help) echo 'Usage: claude --settings FILE --model MODEL' ;;
  *) exit 64 ;;
esac
`, { mode: 0o700 })
  const targetPath = `${binHome}:${process.env.PATH ?? ""}`
  const first = Bun.spawn([service, "--home", muxviaHome], {
    env: { ...process.env, HOME: userHome, PATH: targetPath },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  try {
    await waitForSocket(socket)
    const endpoints: Record<string, string> = {}
    for (const target of ["codex", "claude"] as const) {
      const client = await RpcClient.connect(socket, "diagnostic-recovery-test")
      const session = await client.openTarget(target, target === "claude" ? {
        claudeConfigDir: null,
        selectorState: "unset",
        hostManagedState: "unmanaged",
        cwd: userHome,
      } : undefined)
      const saved = await session.act({
        kind: "create-provider",
        name: `${target} recovery`,
        baseUrl: target === "codex" ? "https://codex-recovery.invalid/v1" : "https://claude-recovery.invalid/v1",
        model: `${target}-recovery-model`,
        credential: { kind: "replace", value: `${target.toUpperCase()}_RECOVERY_PROVIDER_SECRET` },
        authentication: target === "codex" ? "openai-bearer" : "anthropic-api-key",
        presetKey: null,
      })
      const providerId = saved.view.providers[0]?.id
      if (!providerId) throw new Error(`${target} provider was not created`)
      const activated = await session.act({ kind: "activate-provider", providerId, mode: "takeover" })
      const endpoint = activated.view.takeover.endpoint
      if (!endpoint) throw new Error(`${target} takeover was not activated`)
      endpoints[target] = endpoint
      await session.close()
    }
    const codexManaged = await readFile(codexConfig)
    const claudeManaged = await readFile(claudeConfig)
    first.kill("SIGKILL")
    await first.exited

    const started = await runCli([
      "service",
      "start",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome, PATH: targetPath })
    expect(started).toEqual({
      exitCode: 0,
      stdout: `${JSON.stringify({
        ok: true,
        command: "service-start",
        service: {
          state: "running",
          started: true,
          release: "0.1.0",
          recoveredTakeovers: ["codex", "claude"],
        },
      })}\n`,
      stderr: "",
    })
    expect(await readFile(codexConfig)).toEqual(codexManaged)
    expect(await readFile(claudeConfig)).toEqual(claudeManaged)
    for (const target of ["codex", "claude"] as const) {
      const client = await RpcClient.connect(socket, "diagnostic-recovery-observer")
      const session = await client.openTarget(target)
      const expectedEndpoint = endpoints[target]
      if (!expectedEndpoint) throw new Error(`${target} endpoint was not recorded`)
      expect(session.get().takeover.endpoint).toBe(expectedEndpoint)
      await session.close()
    }

    const stopped = await runCli([
      "service",
      "stop",
      "--json",
      "--service",
      service,
      "--socket",
      socket,
    ], { HOME: userHome, PATH: targetPath })
    expect(JSON.parse(stopped.stdout)).toMatchObject({
      ok: true,
      command: "service-stop",
      mode: "safe",
      disabledTakeovers: ["codex", "claude"],
      service: { state: "stopped" },
    })
    expect(stopped.exitCode).toBe(0)
    expect(await readFile(codexConfig)).toEqual(codexBefore)
    expect(await readFile(claudeConfig)).toEqual(claudeBefore)
    expect((await stat(codexConfig)).mode & 0o777).toBe(0o640)
    expect((await stat(claudeConfig)).mode & 0o777).toBe(0o600)
  } finally {
    if (first.exitCode === null) first.kill("SIGKILL")
    await first.exited.catch(() => {})
  }
}, 30_000)
