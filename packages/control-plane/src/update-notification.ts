import { constants } from "node:fs"
import { chmod, mkdir, open, readFile, rename, rm } from "node:fs/promises"
import { join } from "node:path"
import { z } from "zod"

export const PUBLIC_RELEASE_MANIFEST_URL =
  "https://github.com/HaroldHuanrongLIU/muxvia/releases/latest/download/muxvia-latest.json"

const intervalMilliseconds = 24 * 60 * 60 * 1_000
const requestTimeoutMilliseconds = 2_000

const publicManifestSchema = z.object({
  schemaVersion: z.literal(1),
  product: z.literal("muxvia"),
  release: z.string(),
}).passthrough()

const stateSchema = z.object({
  schemaVersion: z.literal(1),
  lastAttemptAt: z.number().int().nonnegative(),
  latestRelease: z.string().optional(),
}).strict()

type Fetch = (input: string, init: RequestInit) => Promise<Response>

export interface UpdateNotice {
  release: string
}
export interface UpdateCheckOptions {
  currentRelease: string
  muxviaHome: string
  environment?: Record<string, string | undefined>
  now?: () => number
  fetch?: Fetch
}

function parseVersion(value: string): [number, number, number] | undefined {
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(value)
  if (!match) return undefined
  const version = match.slice(1).map(Number) as [number, number, number]
  return version.every(Number.isSafeInteger) ? version : undefined
}

function newer(candidate: string | undefined, current: string): boolean {
  const left = candidate ? parseVersion(candidate) : undefined
  const right = parseVersion(current)
  if (!left || !right) return false
  for (let index = 0; index < left.length; index += 1) {
    if (left[index]! !== right[index]!) return left[index]! > right[index]!
  }
  return false
}

async function readState(path: string): Promise<z.infer<typeof stateSchema> | undefined> {
  try {
    return stateSchema.parse(JSON.parse(await readFile(path, "utf8")))
  } catch {
    return undefined
  }
}

async function writePrivateJson(path: string, value: unknown): Promise<void> {
  const temporary = `${path}.tmp-${process.pid}-${crypto.randomUUID()}`
  const handle = await open(temporary, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600)
  try {
    await handle.writeFile(`${JSON.stringify(value)}\n`)
    await handle.sync()
  } finally {
    await handle.close()
  }
  await rename(temporary, path)
  await chmod(path, 0o600)
}

export async function checkForUpdate(options: UpdateCheckOptions): Promise<UpdateNotice | undefined> {
  const environment = options.environment ?? process.env
  if (environment.MUXVIA_UPDATE_CHECK === "0") return undefined
  const now = options.now?.() ?? Date.now()
  const stateDirectory = join(options.muxviaHome, "state")
  const statePath = join(stateDirectory, "update-check.json")
  const lockPath = join(stateDirectory, "update-check.lock")
  await mkdir(stateDirectory, { recursive: true, mode: 0o700 })
  await chmod(stateDirectory, 0o700)

  const cached = await readState(statePath)
  if (cached && now - cached.lastAttemptAt < intervalMilliseconds) {
    return newer(cached.latestRelease, options.currentRelease)
      ? { release: cached.latestRelease! }
      : undefined
  }

  let lock
  try {
    lock = await open(lockPath, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600)
  } catch (error) {
    if (typeof error === "object" && error !== null && "code" in error && error.code === "EEXIST") {
      return undefined
    }
    return undefined
  }

  try {
    const current = await readState(statePath)
    if (current && now - current.lastAttemptAt < intervalMilliseconds) {
      return newer(current.latestRelease, options.currentRelease)
        ? { release: current.latestRelease! }
        : undefined
    }
    await writePrivateJson(statePath, { schemaVersion: 1, lastAttemptAt: now })
    try {
      const response = await (options.fetch ?? fetch)(PUBLIC_RELEASE_MANIFEST_URL, {
        method: "GET",
        headers: { accept: "application/json" },
        redirect: "follow",
        signal: AbortSignal.timeout(requestTimeoutMilliseconds),
      })
      if (!response.ok) return undefined
      const manifest = publicManifestSchema.parse(await response.json())
      const latestRelease = parseVersion(manifest.release) ? manifest.release : undefined
      await writePrivateJson(statePath, {
        schemaVersion: 1,
        lastAttemptAt: now,
        ...(latestRelease ? { latestRelease } : {}),
      })
      return newer(latestRelease, options.currentRelease) ? { release: latestRelease! } : undefined
    } catch {
      return undefined
    }
  } finally {
    await lock.close().catch(() => {})
    await rm(lockPath, { force: true }).catch(() => {})
  }
}
