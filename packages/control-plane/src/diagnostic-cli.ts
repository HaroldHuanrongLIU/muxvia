import { spawn, type ChildProcess } from "node:child_process"
import { lstat, readFile, realpath } from "node:fs/promises"
import { dirname, isAbsolute, join, resolve } from "node:path"

import { claudePreflightContext } from "./control/claude-context"
import { ControlError, RpcClient } from "./control/rpc-client"
import type { TargetSession } from "./control/target-session"
import type { TargetView } from "./control/types"
import { embeddedBundleIdentity } from "./release-bundle"

const bundleIdentity = embeddedBundleIdentity()
export const controlPlaneRelease = bundleIdentity?.release ?? "muxvia-dev"
export const routingServiceRelease = bundleIdentity?.routingRelease ?? "0.1.0"

type Invocation = {
  command?: string
  subcommand?: string
  backupPath?: string
  json: boolean
  force: boolean
  forceAcknowledged: boolean
  backupRestoreAcknowledged: boolean
  controlPlanePath: string
  servicePath: string
  socketPath: string
  problem?: {
    code: "invalid-invocation"
    message: "CLI invocation is invalid"
  }
}

type PathReport = {
  userHome: string
  muxvia: {
    home: string
    state: string
    database: string
    subscriptionAccounts: string
    runtime: string
    controlSocket: string
    serviceLock: string
    logs: string
    backups: string
    exports: string
  }
  targets: {
    codex: TargetPaths
    claude: TargetPaths
  }
  bundle: {
    controlPlane: string
    routingService: string
  }
}

type TargetPaths = {
  configurationHome: string
  managedConfiguration: string
  environmentHome: string | null
  environmentHomeSupported: boolean
}

type StatusTarget = Pick<
  TargetView,
  "mode" | "takeover" | "managedConfiguration" | "recovery" | "routeHealth" | "problems"
>

type DoctorCheck = {
  id: string
  status: "pass" | "warning" | "fail"
  code: string
  version?: string
}

const probeTimeoutMs = 2_000
const probeOutputLimit = 4_096

function valueAfter(args: string[], flag: string): string | undefined {
  const index = args.indexOf(flag)
  const value = index >= 0 ? args[index + 1] : undefined
  if (index >= 0 && (!value || value.startsWith("--"))) throw new Error(`Missing ${flag}`)
  return value
}

export function parseInvocation(
  args: string[],
  environment = process.env,
  controlPlanePath = resolve(Bun.argv[1] ?? "muxvia"),
): Invocation {
  const userHome = environment.HOME
  const defaultServicePath = join(dirname(resolve(controlPlanePath)), "muxvia-routing")
  const defaultSocketPath = userHome
    ? join(userHome, ".muxvia/run/control.sock")
    : join(process.cwd(), ".muxvia/run/control.sock")
  let servicePath = defaultServicePath
  let socketPath = defaultSocketPath
  let invalid = args.some((argument) => (
    argument.startsWith("--")
    && ![
      "--json",
      "--force",
      "--acknowledge-managed-target-files-may-remain-pointed-at-dead-endpoint",
      "--acknowledge-replace-current-installation",
      "--service",
      "--socket",
    ].includes(argument)
  ))
  try {
    servicePath = valueAfter(args, "--service") ?? defaultServicePath
    socketPath = valueAfter(args, "--socket") ?? defaultSocketPath
  } catch {
    invalid = true
  }
  const optionValues = new Set<number>()
  for (const flag of ["--service", "--socket"]) {
    const index = args.indexOf(flag)
    if (index >= 0) {
      optionValues.add(index)
      optionValues.add(index + 1)
    }
  }
  const positional = args.filter((argument, index) => ![
    "--json",
    "--force",
    "--acknowledge-managed-target-files-may-remain-pointed-at-dead-endpoint",
    "--acknowledge-replace-current-installation",
  ].includes(argument) && !optionValues.has(index))
  const command = positional[0]
  const subcommand = positional[1]
  const backupPath = positional[2]
  const force = args.includes("--force")
  const forceAcknowledged = args.includes(
    "--acknowledge-managed-target-files-may-remain-pointed-at-dead-endpoint",
  )
  const backupRestoreAcknowledged = args.includes(
    "--acknowledge-replace-current-installation",
  )
  if (!isAbsolute(servicePath) || !isAbsolute(socketPath)) invalid = true
  if (
    (command === "service" && !["start", "stop"].includes(subcommand ?? ""))
    || (command === "backup" && !["create", "inspect", "restore"].includes(subcommand ?? ""))
    || (["paths", "version", "status", "doctor"].includes(command ?? "") && positional.length !== 1)
    || (command === "service" && positional.length !== 2)
    || (command === "backup" && subcommand === "create" && positional.length !== 2)
    || (command === "backup" && subcommand === "inspect" && (
      positional.length !== 3 || !backupPath || !isAbsolute(backupPath)
    ))
    || (command === "backup" && subcommand === "restore" && (
      positional.length !== 3 || !backupPath || !isAbsolute(backupPath)
    ))
    || (force && (command !== "service" || subcommand !== "stop"))
    || (forceAcknowledged && !force)
    || (backupRestoreAcknowledged && (command !== "backup" || subcommand !== "restore"))
  ) {
    invalid = true
  }
  return {
    command,
    subcommand,
    backupPath,
    json: args.includes("--json"),
    force,
    forceAcknowledged,
    backupRestoreAcknowledged,
    controlPlanePath: resolve(controlPlanePath),
    servicePath,
    socketPath,
    ...(invalid
      ? { problem: { code: "invalid-invocation", message: "CLI invocation is invalid" } as const }
      : {}),
  }
}

function targetPaths(
  configurationHome: string,
  managedFile: string,
  environmentHome: string | undefined,
): TargetPaths {
  const effectiveEnvironmentHome = environmentHome ? resolve(environmentHome) : null
  return {
    configurationHome,
    managedConfiguration: join(configurationHome, managedFile),
    environmentHome: effectiveEnvironmentHome,
    environmentHomeSupported: effectiveEnvironmentHome === null || effectiveEnvironmentHome === configurationHome,
  }
}

export function effectivePaths(
  invocation: Invocation,
  environment = process.env,
): PathReport {
  const userHome = environment.HOME
  if (!userHome || !isAbsolute(userHome)) throw new Error("HOME must be an absolute path")
  const muxviaHome = dirname(dirname(invocation.socketPath))
  return {
    userHome,
    muxvia: {
      home: muxviaHome,
      state: join(muxviaHome, "state"),
      database: join(muxviaHome, "state/muxvia.db"),
      subscriptionAccounts: join(muxviaHome, "state/subscription-accounts.json"),
      runtime: join(muxviaHome, "run"),
      controlSocket: invocation.socketPath,
      serviceLock: join(muxviaHome, "service.lock"),
      logs: join(muxviaHome, "logs"),
      backups: join(muxviaHome, "backups"),
      exports: join(muxviaHome, "exports"),
    },
    targets: {
      codex: targetPaths(join(userHome, ".codex"), "config.toml", environment.CODEX_HOME),
      claude: targetPaths(join(userHome, ".claude"), "settings.json", environment.CLAUDE_CONFIG_DIR),
    },
    bundle: {
      controlPlane: invocation.controlPlanePath,
      routingService: invocation.servicePath,
    },
  }
}

export async function dispatchDiagnostic(invocation: Invocation): Promise<boolean> {
  if (invocation.problem) {
    writeFailure(invocation, invocation.problem.code, invocation.problem.message, 64)
    return true
  }
  if (invocation.command === undefined) return false
  if (invocation.command === "paths") {
    try {
      const result = { ok: true, command: "paths", paths: effectivePaths(invocation) }
      if (invocation.json) {
        process.stdout.write(`${JSON.stringify(result)}\n`)
      } else {
        for (const [name, path] of flattenPaths(result.paths)) process.stdout.write(`${name}: ${path}\n`)
      }
    } catch {
      writeFailure(invocation, "invalid-environment", "Effective paths could not be resolved", 64)
    }
    return true
  }
  if (invocation.command === "version") {
    const result = {
      ok: true,
      command: "version",
      product: "muxvia",
      release: controlPlaneRelease,
      routingService: {
        release: routingServiceRelease,
        rpc: { major: 1, minor: 0 },
      },
    }
    process.stdout.write(invocation.json
      ? `${JSON.stringify(result)}\n`
      : `muxvia ${result.release}\nmuxvia-routing ${result.routingService.release} (RPC 1.0)\n`)
    return true
  }
  if (invocation.command === "status") {
    try {
      const result = await status(invocation)
      process.stdout.write(invocation.json
        ? `${JSON.stringify(result)}\n`
        : formatStatus(result))
    } catch {
      writeFailure(invocation, "status-failed", "Routing Service status is unavailable", 70)
    }
    return true
  }
  if (invocation.command === "doctor") {
    try {
      const result = await doctor(invocation)
      if (!result.ok) process.exitCode = 78
      process.stdout.write(invocation.json
        ? `${JSON.stringify(result)}\n`
        : formatDoctor(result))
    } catch {
      writeFailure(invocation, "doctor-failed", "Diagnostics could not be completed", 70)
    }
    return true
  }
  if (invocation.command === "backup" && invocation.subcommand === "create") {
    try {
      const result = await createRecoveryBackup(invocation)
      process.stdout.write(invocation.json
        ? `${JSON.stringify(result)}\n`
        : formatRecoveryBackup(result.path, result.inspection, "created"))
    } catch {
      writeFailure(
        invocation,
        "recovery-backup-creation-failed",
        "Sensitive Recovery Backup could not be created",
        70,
      )
    }
    return true
  }
  if (invocation.command === "backup" && invocation.subcommand === "inspect") {
    try {
      const result = await inspectRecoveryBackup(invocation)
      process.stdout.write(invocation.json
        ? `${JSON.stringify(result)}\n`
        : formatRecoveryBackup(result.path, result.inspection, "inspected"))
    } catch {
      writeFailure(
        invocation,
        "recovery-backup-inspection-failed",
        "Sensitive Recovery Backup could not be inspected",
        70,
      )
    }
    return true
  }
  if (invocation.command === "backup" && invocation.subcommand === "restore") {
    if (!invocation.backupRestoreAcknowledged) {
      writeFailure(
        invocation,
        "recovery-backup-restore-acknowledgement-required",
        "Recovery Backup restore requires explicit acknowledgement that it replaces the current installation",
        64,
      )
      return true
    }
    try {
      const result = await restoreRecoveryBackup(invocation)
      process.stdout.write(invocation.json
        ? `${JSON.stringify(result)}\n`
        : formatRecoveryBackupRestore(result))
    } catch (error) {
      writeRecoveryBackupRestoreFailure(invocation, error)
    }
    return true
  }
  if (invocation.command === "service" && invocation.subcommand === "start") {
    try {
      const result = await startService(invocation)
      process.stdout.write(invocation.json
        ? `${JSON.stringify(result)}\n`
        : formatServiceStart(result))
    } catch (error) {
      const known = error instanceof Error && [
        "relative-service-path",
        "invalid-release-bundle",
        "start-timeout",
        "idle-exit-timeout",
      ].includes(error.message)
        ? error.message
        : "service-start-failed"
      writeFailure(invocation, known, "Routing Service could not be started", 70)
    }
    return true
  }
  if (invocation.command === "service" && invocation.subcommand === "stop") {
    if (invocation.force && !invocation.forceAcknowledged) {
      writeFailure(
        invocation,
        "force-acknowledgement-required",
        "Force stop requires explicit acknowledgement that managed Target files may remain pointed at a dead endpoint",
        64,
      )
      return true
    }
    try {
      const result = invocation.force
        ? await forceStopService(invocation)
        : await stopServiceSafely(invocation)
      process.stdout.write(invocation.json
        ? `${JSON.stringify(result)}\n`
        : formatServiceStop(result))
    } catch {
      writeFailure(
        invocation,
        invocation.force ? "service-force-stop-failed" : "service-stop-failed",
        invocation.force
          ? "Routing Service could not be force stopped"
          : "Routing Service could not be stopped safely",
        70,
      )
    }
    return true
  }
  writeFailure(invocation, "unsupported-command", "Unsupported noninteractive command", 64)
  return true
}

async function createRecoveryBackup(invocation: Invocation) {
  const client = await connectForBackup(invocation)
  try {
    const result = await client.request({ kind: "create-recovery-backup" })
    if (result.kind !== "recovery-backup-created" || !isAbsolute(result.path)) {
      throw new Error("unexpected-recovery-backup-response")
    }
    return {
      ok: true,
      command: "backup",
      operation: "create",
      sensitive: true,
      path: result.path,
      inspection: result.inspection,
    } as const
  } finally {
    await client.close().catch(() => {})
  }
}

async function inspectRecoveryBackup(invocation: Invocation) {
  const path = invocation.backupPath
  if (!path || !isAbsolute(path)) throw new Error("invalid-recovery-backup-path")
  const client = await connectForBackup(invocation)
  try {
    const result = await client.request({ kind: "inspect-recovery-backup", path })
    if (result.kind !== "recovery-backup-inspection") {
      throw new Error("unexpected-recovery-backup-response")
    }
    return {
      ok: true,
      command: "backup",
      operation: "inspect",
      sensitive: true,
      path,
      inspection: result.inspection,
    } as const
  } finally {
    await client.close().catch(() => {})
  }
}

async function restoreRecoveryBackup(invocation: Invocation) {
  const path = invocation.backupPath
  if (!path || !isAbsolute(path)) throw new Error("invalid-recovery-backup-path")
  const client = await connectForBackup(invocation)
  try {
    const result = await client.request({
      kind: "restore-recovery-backup",
      path,
      acknowledgement: "replace-current-installation",
      claudeContext: claudePreflightContext(process.env),
    })
    if (
      result.kind !== "recovery-backup-restored"
      || !isAbsolute(result.preRestoreBackupPath)
    ) {
      throw new Error("unexpected-recovery-backup-response")
    }
    return {
      ok: true,
      command: "backup",
      operation: "restore",
      sensitive: true,
      restoredSnapshotId: result.restoredSnapshotId,
      preRestoreSnapshotId: result.preRestoreSnapshotId,
      preRestoreBackupPath: result.preRestoreBackupPath,
      resumedTakeovers: result.resumedTakeovers,
      restartTargetClis: result.restartTargetClis,
    } as const
  } finally {
    await client.close().catch(() => {})
  }
}

async function connectForBackup(invocation: Invocation): Promise<RpcClient> {
  const cancellation = new AbortController()
  const deadline = setTimeout(() => cancellation.abort(), probeTimeoutMs)
  try {
    return await RpcClient.connect(
      invocation.socketPath,
      controlPlaneRelease,
      cancellation.signal,
    )
  } finally {
    clearTimeout(deadline)
  }
}

function formatRecoveryBackup(
  path: string,
  inspection: Awaited<ReturnType<typeof createRecoveryBackup>>["inspection"],
  operation: "created" | "inspected",
): string {
  const entries = inspection.entries
    .map((entry) => `  ${entry.kind}: ${entry.present ? `${entry.byteLength} bytes` : "absent"}`)
    .join("\n")
  return [
    "SENSITIVE RECOVERY BACKUP — contains credentials and private installation state",
    `Recovery Backup ${operation}: ${path}`,
    `Snapshot: ${inspection.snapshotId}`,
    `Compatibility: ${inspection.compatibility}`,
    `Format: ${inspection.formatVersion}; database schema: ${inspection.databaseSchemaVersion}`,
    `Artifact: ${inspection.artifactSizeBytes} bytes; SHA-256 ${inspection.artifactSha256}`,
    "Entries:",
    entries,
    "",
  ].join("\n")
}

function formatRecoveryBackupRestore(
  result: Awaited<ReturnType<typeof restoreRecoveryBackup>>,
): string {
  const resumed = result.resumedTakeovers.length === 0
    ? "none"
    : result.resumedTakeovers.join(", ")
  return [
    "SENSITIVE RECOVERY BACKUP restored; the prior installation remains recoverable",
    `Restored snapshot: ${result.restoredSnapshotId}`,
    `Pre-restore Recovery Backup: ${result.preRestoreBackupPath}`,
    `Resumed takeovers: ${resumed}`,
    "Start new Target CLI processes; existing processes retain their prior configuration.",
    "",
  ].join("\n")
}

function writeRecoveryBackupRestoreFailure(invocation: Invocation, error: unknown): void {
  const controlError = error instanceof ControlError ? error : undefined
  const source = controlError?.recoveryBackupPath && isAbsolute(controlError.recoveryBackupPath)
    ? controlError.recoveryBackupPath
    : undefined
  const failure = {
    ok: false,
    problem: {
      code: controlError?.code ?? "recovery-backup-restore-failed",
      message: controlError?.message ?? "Sensitive Recovery Backup could not be restored",
      ...(source ? { source } : {}),
    },
  }
  process.exitCode = controlError?.code === "recovery-backup-recovery-required" ? 78 : 70
  process.stderr.write(invocation.json
    ? `${JSON.stringify(failure)}\n`
    : `${failure.problem.message}${source ? `; recovery point: ${source}` : ""}\n`)
}

function writeFailure(
  invocation: Invocation,
  code: string,
  message: string,
  exitCode: number,
): void {
  const failure = {
    ok: false,
    problem: {
      code,
      message,
    },
  }
  process.exitCode = exitCode
  process.stderr.write(invocation.json
    ? `${JSON.stringify(failure)}\n`
    : `${failure.problem.message}\n`)
}

async function status(invocation: Invocation) {
  let codexClient: RpcClient | undefined
  let claudeClient: RpcClient | undefined
  const cancellation = new AbortController()
  const deadline = setTimeout(() => {
    cancellation.abort()
    void codexClient?.close().catch(() => {})
    void claudeClient?.close().catch(() => {})
  }, probeTimeoutMs)
  try {
    codexClient = await RpcClient.connect(
      invocation.socketPath,
      controlPlaneRelease,
      cancellation.signal,
    )
  } catch (error) {
    clearTimeout(deadline)
    if (cancellation.signal.aborted) throw new Error("status-timeout")
    if (error instanceof ControlError && error.code === "service-unavailable") {
      return {
        ok: true,
        command: "status",
        service: { state: "stopped", release: null, rpc: null, epoch: null },
        targets: { codex: null, claude: null },
      } as const
    }
    throw error
  }
  try {
    claudeClient = await RpcClient.connect(
      invocation.socketPath,
      controlPlaneRelease,
      cancellation.signal,
    )
  } catch (error) {
    clearTimeout(deadline)
    await codexClient.close().catch(() => {})
    throw error
  }
  try {
    const [codex, claude] = await Promise.all([
      codexClient.request({ kind: "open-target", target: "codex" }),
      claudeClient.request({ kind: "open-target", target: "claude" }),
    ])
    if (codex.kind !== "target-view" || claude.kind !== "target-view") {
      throw new Error("unexpected-status-response")
    }
    const metadata = codexClient.serviceMetadata
    if (claudeClient.serviceMetadata.serviceEpoch !== metadata.serviceEpoch) {
      throw new Error("status-epoch-changed")
    }
    return {
      ok: true,
      command: "status",
      service: {
        state: "running",
        release: metadata.release,
        rpc: metadata.rpc,
        epoch: metadata.serviceEpoch,
      },
      targets: {
        codex: statusTarget(codex.view),
        claude: statusTarget(claude.view),
      },
    } as const
  } finally {
    clearTimeout(deadline)
    await Promise.all([
      codexClient.close().catch(() => {}),
      claudeClient.close().catch(() => {}),
    ])
  }
}

function statusTarget(view: TargetView): StatusTarget {
  return {
    mode: view.mode,
    takeover: view.takeover,
    managedConfiguration: view.managedConfiguration,
    recovery: view.recovery,
    routeHealth: view.routeHealth,
    problems: view.problems,
  }
}

function formatStatus(report: Awaited<ReturnType<typeof status>>): string {
  if (report.service.state === "stopped") return "Routing Service: stopped\n"
  const codex = report.targets.codex
  const claude = report.targets.claude
  if (!codex || !claude) return "Routing Service: running; Target status unavailable\n"
  return [
    `Routing Service: running (${report.service.release}, RPC ${report.service.rpc.major}.${report.service.rpc.minor})`,
    `Codex CLI: ${codex.mode}; takeover ${codex.takeover.state}`,
    `Claude Code: ${claude.mode}; takeover ${claude.takeover.state}`,
    "",
  ].join("\n")
}

async function doctor(invocation: Invocation) {
  const paths = effectivePaths(invocation)
  const checks: DoctorCheck[] = []
  checks.push(await bundleControlPlaneCheck(paths.bundle.controlPlane))
  checks.push(await bundleCheck(invocation.servicePath))
  for (const [id, path, kind] of [
    ["permissions.muxvia-home", paths.muxvia.home, "directory"],
    ["permissions.muxvia-state", paths.muxvia.state, "directory"],
    ["permissions.database", paths.muxvia.database, "file"],
    ["permissions.subscription-accounts", paths.muxvia.subscriptionAccounts, "file"],
    ["permissions.runtime", paths.muxvia.runtime, "directory"],
    ["permissions.control-socket", paths.muxvia.controlSocket, "socket"],
    ["permissions.service-lock", paths.muxvia.serviceLock, "file"],
    ["permissions.logs", paths.muxvia.logs, "directory"],
    ["permissions.backups", paths.muxvia.backups, "directory"],
    ["permissions.exports", paths.muxvia.exports, "shareable-directory"],
  ] as const) {
    checks.push(await permissionCheck(id, path, kind))
  }
  checks.push(homeCheck("codex", paths.targets.codex))
  checks.push(homeCheck("claude", paths.targets.claude))
  checks.push(await configurationHomeSymlinkCheck("codex", paths.targets.codex.configurationHome))
  checks.push(await managedFileSymlinkCheck("codex", paths.targets.codex.managedConfiguration))
  checks.push(await configurationHomeSymlinkCheck("claude", paths.targets.claude.configurationHome))
  checks.push(await managedFileSymlinkCheck("claude", paths.targets.claude.managedConfiguration))
  checks.push(await codexProfileShadowCheck(paths.targets.codex.managedConfiguration))
  checks.push(claudeSelectorShadowCheck())
  checks.push(await claudeFileShadowCheck(
    "shadow.claude-managed",
    process.platform === "darwin"
      ? "/Library/Application Support/ClaudeCode/managed-settings.json"
      : "/etc/claude-code/managed-settings.json",
  ))
  checks.push(await claudeFileShadowCheck("shadow.claude-project", join(process.cwd(), ".claude/settings.json")))
  checks.push(await claudeFileShadowCheck("shadow.claude-local", join(process.cwd(), ".claude/settings.local.json")))
  checks.push(await targetCompatibilityCheck("codex"))
  checks.push(await targetCompatibilityCheck("claude"))
  const failed = checks.filter((check) => check.status === "fail").length
  const warnings = checks.filter((check) => check.status === "warning").length
  return {
    ok: failed === 0,
    command: "doctor",
    summary: { passed: checks.length - failed - warnings, warnings, failed },
    checks,
    disclosures: {
      credentialStorage: {
        applicationEncryption: "none",
        filesystemPermissions: "best-effort-private",
        message: "Provider credentials and subscription refresh tokens are stored locally without application-level encryption.",
      },
      ...(process.platform === "darwin"
        ? {
            macos: {
              appleVerification: "not-established",
              message: "The initial macOS release may be unsigned and unnotarized; Gatekeeper may require explicit Operator approval.",
            },
          }
        : {}),
      runtimeShadows: {
        observable: "known configuration sources were inspected without modification",
        unobservable: "command-line overrides and already-running Target CLI state may still shadow Managed Configuration",
      },
    },
  }
}

async function bundleControlPlaneCheck(controlPlanePath: string): Promise<DoctorCheck> {
  try {
    return (await lstat(controlPlanePath)).isFile()
      ? { id: "bundle.control-plane", status: "pass", code: "present" }
      : { id: "bundle.control-plane", status: "fail", code: "unexpected-file-type" }
  } catch {
    return { id: "bundle.control-plane", status: "fail", code: "not-present" }
  }
}

async function bundleCheck(servicePath: string): Promise<DoctorCheck> {
  try {
    const metadata = await lstat(servicePath)
    if (!metadata.isFile() || (metadata.mode & 0o111) === 0) {
      return { id: "bundle.routing-service", status: "fail", code: "invalid-executable" }
    }
    const output = await runBounded(servicePath, ["--lifecycle-metadata"])
    if (output.exitCode !== 0 || output.stderr.length !== 0) {
      return { id: "bundle.routing-service", status: "fail", code: "metadata-probe-failed" }
    }
    const value = JSON.parse(output.stdout)
    if (
      value?.product !== "muxvia-routing"
      || value?.release !== routingServiceRelease
      || value?.rpc?.major !== 1
      || value?.rpc?.minor !== 0
      || Object.keys(value).length !== 3
      || Object.keys(value.rpc).length !== 2
    ) {
      return { id: "bundle.routing-service", status: "fail", code: "bundle-mismatch" }
    }
    return { id: "bundle.routing-service", status: "pass", code: "verified" }
  } catch {
    return { id: "bundle.routing-service", status: "fail", code: "metadata-probe-failed" }
  }
}

async function permissionCheck(
  id: string,
  path: string,
  kind: "directory" | "file" | "socket" | "shareable-directory",
): Promise<DoctorCheck> {
  try {
    const metadata = await lstat(path)
    const correctKind = kind === "directory" || kind === "shareable-directory"
      ? metadata.isDirectory()
      : kind === "file"
        ? metadata.isFile()
        : metadata.isSocket()
    if (!correctKind) return { id, status: "fail", code: "unexpected-file-type" }
    if (kind === "shareable-directory") {
      return { id, status: "pass", code: "shareable-artifacts" }
    }
    if ((metadata.mode & 0o077) !== 0) return { id, status: "fail", code: "permissions-too-open" }
    return { id, status: "pass", code: "private" }
  } catch (error) {
    if (isMissing(error)) return { id, status: "pass", code: "not-present" }
    return { id, status: "fail", code: "unreadable" }
  }
}

function homeCheck(target: "codex" | "claude", paths: TargetPaths): DoctorCheck {
  return paths.environmentHomeSupported
    ? { id: `home.${target}`, status: "pass", code: "default-configuration-home" }
    : { id: `home.${target}`, status: "warning", code: "unsupported-configuration-home" }
}

async function configurationHomeSymlinkCheck(
  target: "codex" | "claude",
  path: string,
): Promise<DoctorCheck> {
  const id = `symlink.${target}-configuration-home`
  try {
    const metadata = await lstat(path)
    if (!metadata.isSymbolicLink()) {
      return metadata.isDirectory()
        ? { id, status: "pass", code: "directory" }
        : { id, status: "fail", code: "unexpected-file-type" }
    }
    const resolved = await realpath(path)
    return (await lstat(resolved)).isDirectory()
      ? { id, status: "pass", code: "directory-symlink-canonicalized" }
      : { id, status: "fail", code: "broken-directory-symlink" }
  } catch (error) {
    if (isMissing(error)) return { id, status: "pass", code: "not-present" }
    return { id, status: "fail", code: "broken-directory-symlink" }
  }
}

async function managedFileSymlinkCheck(
  target: "codex" | "claude",
  path: string,
): Promise<DoctorCheck> {
  const id = `symlink.${target}-managed-configuration`
  try {
    const metadata = await lstat(path)
    if (metadata.isSymbolicLink()) return { id, status: "fail", code: "managed-file-symlink" }
    return metadata.isFile()
      ? { id, status: "pass", code: "regular-file" }
      : { id, status: "fail", code: "unexpected-file-type" }
  } catch (error) {
    if (isMissing(error)) return { id, status: "pass", code: "not-present" }
    return { id, status: "fail", code: "unreadable" }
  }
}

async function codexProfileShadowCheck(path: string): Promise<DoctorCheck> {
  const id = "shadow.codex-profile"
  try {
    const source = await readSmallText(path)
    return /^\s*profile\s*=/mu.test(source)
      ? { id, status: "warning", code: "shadowing-configuration" }
      : { id, status: "pass", code: "not-observed" }
  } catch (error) {
    if (isMissing(error)) return { id, status: "pass", code: "not-observed" }
    return { id, status: "fail", code: "configuration-unreadable" }
  }
}

function claudeSelectorShadowCheck(): DoctorCheck {
  const id = "shadow.claude-selector"
  const selectors = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
  ]
  const observed = selectors.some((name) => {
    const value = process.env[name]?.trim().toLowerCase()
    return value !== undefined && value !== "" && value !== "0" && value !== "false"
  })
  return observed
    ? { id, status: "warning", code: "shadowing-configuration" }
    : { id, status: "pass", code: "not-observed" }
}

async function claudeFileShadowCheck(id: string, path: string): Promise<DoctorCheck> {
  try {
    const value = JSON.parse(await readSmallText(path))
    const env = typeof value === "object" && value !== null && typeof value.env === "object" && value.env !== null
      ? value.env
      : {}
    const owned = ["ANTHROPIC_BASE_URL", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY", "ANTHROPIC_MODEL"]
    return owned.some((name) => Object.hasOwn(env, name))
      ? { id, status: "warning", code: "shadowing-configuration" }
      : { id, status: "pass", code: "not-observed" }
  } catch (error) {
    if (isMissing(error)) return { id, status: "pass", code: "not-observed" }
    return { id, status: "fail", code: "configuration-unreadable" }
  }
}

async function targetCompatibilityCheck(target: "codex" | "claude"): Promise<DoctorCheck> {
  const id = `compatibility.${target}`
  const executable = await findExecutable(target)
  if (!executable) return { id, status: "warning", code: "target-cli-not-found" }
  try {
    const [versionOutput, helpOutput] = await Promise.all([
      runBounded(executable, ["--version"]),
      runBounded(executable, ["--help"]),
    ])
    if (versionOutput.exitCode !== 0 || helpOutput.exitCode !== 0) {
      return { id, status: "fail", code: "incompatible-target-cli" }
    }
    const version = parseTargetVersion(target, versionOutput.stdout)
    const help = helpOutput.stdout.toLowerCase()
    const compatibleHelp = target === "codex"
      ? help.includes("usage:") && help.includes("codex") && help.includes("--config")
      : help.includes("usage:") && help.includes("claude") && help.includes("--settings") && help.includes("--model")
    if (!version || !compatibleHelp) return { id, status: "fail", code: "incompatible-target-cli" }
    const tested = target === "codex" ? version === "codex-cli 0.106.0" : version === "2.1.37 (Claude Code)"
    return tested
      ? { id, status: "pass", code: "tested", version }
      : { id, status: "warning", code: "unknown-compatible", version }
  } catch {
    return { id, status: "fail", code: "incompatible-target-cli" }
  }
}

function parseTargetVersion(target: "codex" | "claude", output: string): string | undefined {
  const lines = output.trimEnd().split("\n")
  if (lines.length !== 1) return undefined
  const version = lines[0]?.trim()
  if (!version) return undefined
  const token = target === "codex"
    ? version.startsWith("codex-cli ") ? version.slice("codex-cli ".length) : undefined
    : version.endsWith(" (Claude Code)") ? version.slice(0, -" (Claude Code)".length) : undefined
  if (!token || !/^\d+(?:\.\d+){1,}(?:[-+][0-9A-Za-z.-]+)?$/.test(token)) return undefined
  return version
}

async function findExecutable(name: string): Promise<string | undefined> {
  for (const directory of (process.env.PATH ?? "").split(":")) {
    if (!directory) continue
    const candidate = join(directory, name)
    try {
      const metadata = await lstat(candidate)
      if (metadata.isFile() && (metadata.mode & 0o111) !== 0) return await realpath(candidate)
    } catch {}
  }
  return undefined
}

async function runBounded(executable: string, args: string[]) {
  const proc = Bun.spawn([executable, ...args], {
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  })
  try {
    const [exitCode, stdout, stderr] = await Promise.all([
      Promise.race([
        proc.exited,
        Bun.sleep(probeTimeoutMs).then(() => {
          proc.kill("SIGKILL")
          throw new Error("probe-timeout")
        }),
      ]),
      readBounded(proc.stdout, probeOutputLimit),
      readBounded(proc.stderr, probeOutputLimit),
    ])
    return { exitCode, stdout, stderr }
  } finally {
    if (proc.exitCode === null) proc.kill("SIGKILL")
    await proc.exited.catch(() => {})
  }
}

async function readBounded(stream: ReadableStream<Uint8Array>, limit: number): Promise<string> {
  const reader = stream.getReader()
  const chunks: Uint8Array[] = []
  let length = 0
  while (true) {
    const { done, value } = await reader.read()
    if (done) break
    length += value.byteLength
    if (length > limit) throw new Error("probe-output-too-large")
    chunks.push(value)
  }
  return new TextDecoder("utf-8", { fatal: true }).decode(Buffer.concat(chunks.map((chunk) => Buffer.from(chunk))))
}

async function readSmallText(path: string): Promise<string> {
  const metadata = await lstat(path)
  if (metadata.size > 1_048_576) throw new Error("configuration-too-large")
  return await readFile(path, "utf8")
}

function isMissing(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT"
}

function formatDoctor(report: Awaited<ReturnType<typeof doctor>>): string {
  const lines = report.checks.map((check) => `${check.status.toUpperCase()} ${check.id}: ${check.code}`)
  lines.push(
    `Summary: ${report.summary.passed} passed, ${report.summary.warnings} warnings, ${report.summary.failed} failed`,
    report.disclosures.credentialStorage.message,
  )
  if (report.disclosures.macos) lines.push(report.disclosures.macos.message)
  return `${lines.join("\n")}\n`
}

async function startService(invocation: Invocation) {
  if (!isAbsolute(invocation.servicePath)) throw new Error("relative-service-path")
  const bundle = await bundleCheck(invocation.servicePath)
  if (bundle.status !== "pass") throw new Error("invalid-release-bundle")
  const current = await status(invocation)
  if (current.service.state === "running") {
    return {
      ok: true,
      command: "service-start",
      service: {
        state: "running",
        started: false,
        release: current.service.release,
        recoveredTakeovers: recoveredTakeovers(current),
      },
    } as const
  }

  const child = await spawnRoutingService(invocation)
  const deadline = Date.now() + probeTimeoutMs
  try {
    let report: Awaited<ReturnType<typeof status>> | undefined
    while (Date.now() < deadline) {
      const observed = await status(invocation)
      if (observed.service.state === "running") {
        report = observed
        break
      }
      await Bun.sleep(50)
    }
    if (!report || report.service.state !== "running") throw new Error("start-timeout")
    const recovered = recoveredTakeovers(report)
    if (!serviceLifecycleRequired(report)) {
      while (Date.now() < deadline) {
        if (!(await pathExists(invocation.socketPath))) {
          return {
            ok: true,
            command: "service-start",
            service: {
              state: "idle-exited",
              started: true,
              release: report.service.release,
              recoveredTakeovers: recovered,
            },
          } as const
        }
        await Bun.sleep(25)
      }
      throw new Error("idle-exit-timeout")
    }
    return {
      ok: true,
      command: "service-start",
      service: {
        state: "running",
        started: true,
        release: report.service.release,
        recoveredTakeovers: recovered,
      },
    } as const
  } catch (error) {
    child.kill("SIGTERM")
    throw error
  }
}

async function spawnRoutingService(invocation: Invocation): Promise<ChildProcess> {
  const child = spawn(
    invocation.servicePath,
    ["--home", dirname(dirname(invocation.socketPath))],
    { shell: false, detached: true, stdio: "ignore", env: process.env },
  )
  await new Promise<void>((resolveSpawn, rejectSpawn) => {
    child.once("spawn", resolveSpawn)
    child.once("error", rejectSpawn)
  })
  child.unref()
  return child
}

function recoveredTakeovers(report: Awaited<ReturnType<typeof status>>): Array<"codex" | "claude"> {
  const recovered: Array<"codex" | "claude"> = []
  for (const target of ["codex", "claude"] as const) {
    const view = report.targets[target]
    if (
      view?.takeover.state === "active"
      && view.managedConfiguration.state === "applied"
    ) {
      recovered.push(target)
    }
  }
  return recovered
}

function serviceLifecycleRequired(report: Awaited<ReturnType<typeof status>>): boolean {
  return [report.targets.codex, report.targets.claude].some((view) => (
    view?.mode === "takeover"
    || (view !== null
      && view !== undefined
      && ["pending", "recovery-required"].includes(view.recovery.state))
    || view?.managedConfiguration.state === "configuration-drift"
  ))
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await lstat(path)
    return true
  } catch (error) {
    if (isMissing(error)) return false
    throw error
  }
}

function formatServiceStart(report: Awaited<ReturnType<typeof startService>>): string {
  const takeovers = report.service.recoveredTakeovers.length
    ? report.service.recoveredTakeovers.join(", ")
    : "none"
  return `Routing Service: ${report.service.state}; recovered Target Takeovers: ${takeovers}\n`
}

async function stopServiceSafely(invocation: Invocation) {
  const current = await status(invocation)
  const targets = configuredTakeovers(current)
  if (current.service.state === "stopped") return safeStopResult([])
  if ([current.targets.codex, current.targets.claude].some((view) => (
    view !== null
    && (
      ["pending", "recovery-required"].includes(view.recovery.state)
      || view.managedConfiguration.state === "configuration-drift"
      || view.takeover.state === "unavailable"
    )
  ))) {
    throw new Error("safe-stop-blocked")
  }
  const sessions: TargetSession[] = []
  try {
    for (const target of targets) {
      const client = await RpcClient.connect(invocation.socketPath, controlPlaneRelease)
      try {
        sessions.push(await client.openTarget(
          target,
          target === "claude" ? claudePreflightContext(process.env) : undefined,
        ))
      } catch (error) {
        await client.close().catch(() => {})
        throw error
      }
    }
    await Promise.all(sessions.map((session) => session.act({ kind: "disable-takeover" })))
  } finally {
    await Promise.all(sessions.map((session) => session.close().catch(() => {})))
  }
  if (targets.length === 0) {
    const deadline = Date.now() + probeTimeoutMs
    while (await pathExists(invocation.socketPath)) {
      if (Date.now() >= deadline) throw new Error("safe-stop-timeout")
      await Bun.sleep(25)
    }
  } else {
    while (await pathExists(invocation.socketPath)) await Bun.sleep(25)
  }
  return safeStopResult(targets)
}

function configuredTakeovers(
  report: Awaited<ReturnType<typeof status>>,
): Array<"codex" | "claude"> {
  const configured: Array<"codex" | "claude"> = []
  if (report.targets.codex?.mode === "takeover") configured.push("codex")
  if (report.targets.claude?.mode === "takeover") configured.push("claude")
  return configured
}

function safeStopResult(disabledTakeovers: Array<"codex" | "claude">) {
  return {
    ok: true,
    command: "service-stop",
    mode: "safe",
    disabledTakeovers,
    managedConfiguration: "restored",
    streams: "drained",
    service: { state: "stopped" },
  } as const
}

async function forceStopService(invocation: Invocation) {
  const current = await status(invocation)
  if (current.service.state === "running") {
    const client = await RpcClient.connect(invocation.socketPath, controlPlaneRelease)
    const requestDeadline = setTimeout(() => {
      void client.close().catch(() => {})
    }, probeTimeoutMs)
    try {
      const result = await client.request({
        kind: "force-stop",
        acknowledgement: "managed-target-files-may-remain-pointed-at-dead-endpoint",
      })
      if (
        result.kind !== "force-stop-accepted"
        || result.warning !== "managed-target-files-may-remain-pointed-at-dead-endpoint"
      ) {
        throw new Error("unexpected-force-stop-response")
      }
    } finally {
      clearTimeout(requestDeadline)
      await client.close().catch(() => {})
    }
    const deadline = Date.now() + probeTimeoutMs
    while (await pathExists(invocation.socketPath)) {
      if (Date.now() >= deadline) throw new Error("force-stop-timeout")
      await Bun.sleep(25)
    }
  }
  return {
    ok: true,
    command: "service-stop",
    mode: "force",
    service: { state: "stopped" },
    warning: "Managed Target files may remain pointed at a dead endpoint.",
  } as const
}

function formatServiceStop(
  report: Awaited<ReturnType<typeof stopServiceSafely>> | Awaited<ReturnType<typeof forceStopService>>,
): string {
  if (report.mode === "force") {
    return `Routing Service force stopped. ${report.warning}\n`
  }
  const targets = report.disabledTakeovers.length ? report.disabledTakeovers.join(", ") : "none"
  return `Routing Service stopped safely; restored Target Takeovers: ${targets}; committed streams drained\n`
}

function flattenPaths(paths: PathReport): Array<[string, string | boolean]> {
  return [
    ["user-home", paths.userHome],
    ...Object.entries(paths.muxvia).map(([name, path]) => [`muxvia.${name}`, path] as [string, string]),
    ...Object.entries(paths.targets.codex).map(([name, path]) => [`target.codex.${name}`, path ?? "unset"] as [string, string | boolean]),
    ...Object.entries(paths.targets.claude).map(([name, path]) => [`target.claude.${name}`, path ?? "unset"] as [string, string | boolean]),
    ["bundle.control-plane", paths.bundle.controlPlane],
    ["bundle.routing-service", paths.bundle.routingService],
  ]
}
