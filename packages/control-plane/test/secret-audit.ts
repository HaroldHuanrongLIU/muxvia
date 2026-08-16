export type SecretSurfaceKind = "frame" | "action" | "activity" | "view" | "preview" | "error" | "timeout" | "diagnostic"

interface SecretScan {
  matched: boolean[]
  seen: WeakSet<object>
  secrets: readonly string[]
}

function scanText(value: string, scan: SecretScan): void {
  for (let index = 0; index < scan.secrets.length; index += 1) {
    const secret = scan.secrets[index]!
    if (secret.length > 0 && value.includes(secret)) {
      scan.matched[index] = true
    }
  }
}

function scanSurface(value: unknown, scan: SecretScan): void {
  if (typeof value === "string") {
    scanText(value, scan)
    return
  }
  if (typeof value === "bigint" || typeof value === "number" || typeof value === "boolean") {
    scanText(String(value), scan)
    return
  }
  if (typeof value === "symbol") {
    scanText(value.description ?? "", scan)
    return
  }
  if ((typeof value !== "object" || value === null) && typeof value !== "function") {
    return
  }

  const object = value as object
  if (scan.seen.has(object)) {
    return
  }
  scan.seen.add(object)

  try {
    if (object instanceof Error) {
      scanSurface(object.name, scan)
      scanSurface(object.message, scan)
      scanSurface(object.stack, scan)
      scanSurface((object as Error & { cause?: unknown }).cause, scan)
      if (object instanceof AggregateError) {
        scanSurface(object.errors, scan)
      }
    }
  } catch {
    throw new Error("secret-surface-uninspectable")
  }

  let keys: PropertyKey[]
  try {
    keys = Reflect.ownKeys(object)
  } catch {
    throw new Error("secret-surface-uninspectable")
  }

  for (const key of keys) {
    scanText(typeof key === "symbol" ? key.description ?? "" : String(key), scan)
    let descriptor: PropertyDescriptor | undefined
    try {
      descriptor = Reflect.getOwnPropertyDescriptor(object, key)
    } catch {
      throw new Error("secret-surface-uninspectable")
    }
    if (descriptor === undefined) {
      continue
    }
    if (!("value" in descriptor)) {
      throw new Error("secret-surface-uninspectable")
    }
    scanSurface(descriptor.value, scan)
  }

  try {
    if (object instanceof Map) {
      for (const [key, entry] of object) {
        scanSurface(key, scan)
        scanSurface(entry, scan)
      }
    } else if (object instanceof Set) {
      for (const entry of object) {
        scanSurface(entry, scan)
      }
    }
  } catch {
    throw new Error("secret-surface-uninspectable")
  }
}

function scanSecrets(value: unknown, secrets: readonly string[]): boolean[] {
  const scan = {
    matched: secrets.map(() => false),
    seen: new WeakSet<object>(),
    secrets,
  }
  scanSurface(value, scan)
  return scan.matched
}

export function auditSecretFreeSurface(
  kind: SecretSurfaceKind,
  value: unknown,
  secrets: readonly string[],
  label: string,
): void {
  const diagnostic = `secret-scan-failed:${label}-${kind}`
  try {
    if (scanSecrets(value, secrets).some(Boolean)) {
      throw new Error(diagnostic)
    }
  } catch {
    throw new Error(diagnostic)
  }
}

export const auditSecretFreeFrame = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("frame", value, secrets, label)

export const auditSecretFreeActions = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("action", value, secrets, label)

export const auditSecretFreeActivities = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("activity", value, secrets, label)

export const auditSecretFreeView = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("view", value, secrets, label)

export const auditSecretFreePreview = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("preview", value, secrets, label)

export const auditSecretFreeError = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("error", value, secrets, label)

export const auditSecretFreeTimeout = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("timeout", value, secrets, label)

export const auditSecretFreeDiagnostic = (
  value: unknown,
  secrets: readonly string[],
  label: string,
) => auditSecretFreeSurface("diagnostic", value, secrets, label)

export async function waitForSecretFreeFrame(
  setup: { waitForFrame: (predicate: (frame: string) => boolean) => Promise<string> },
  predicate: (frame: string) => boolean,
  secrets: readonly string[],
  label: string,
): Promise<string> {
  const scanFailure = `secret-scan-failed:${label}-frame`
  try {
    const frame = await setup.waitForFrame((current) => {
      auditSecretFreeFrame(current, secrets, label)
      return predicate(current)
    })
    auditSecretFreeFrame(frame, secrets, label)
    return frame
  } catch (error) {
    const message = error instanceof Error ? error.message : ""
    throw new Error(message === scanFailure ? scanFailure : `renderer-wait-failed:${label}`)
  }
}

export async function waitForSecretFreeCondition(
  setup: { waitFor: (predicate: () => boolean) => Promise<void> },
  predicate: () => boolean,
  audit: () => void,
  scanFailure: string,
  label: string,
): Promise<void> {
  try {
    await setup.waitFor(() => {
      audit()
      return predicate()
    })
  } catch (error) {
    const message = error instanceof Error ? error.message : ""
    throw new Error(message === scanFailure ? scanFailure : `condition-wait-failed:${label}`)
  }
}

export function assertSecretFreeStructured<T>(
  kind: Exclude<SecretSurfaceKind, "frame" | "diagnostic" | "error" | "timeout">,
  value: T,
  secrets: readonly string[],
  label: string,
  assertion: (safeValue: T) => void,
): void {
  const scanFailure = `secret-scan-failed:${label}-${kind}`
  try {
    auditSecretFreeSurface(kind, value, secrets, label)
    assertion(value)
  } catch (error) {
    const message = error instanceof Error ? error.message : ""
    throw new Error(message === scanFailure ? scanFailure : `structured-assertion-failed:${label}-${kind}`)
  }
}

export function assertControlledSecretSource(
  value: unknown,
  secrets: readonly string[],
  label: string,
): void {
  let matched: boolean[]
  try {
    matched = scanSecrets(value, secrets)
  } catch {
    throw new Error(`controlled-secret-source-invalid:${label}`)
  }
  if (!secrets.every((secret, index) => secret.length > 0 && matched[index])) {
    throw new Error(`controlled-secret-source-missing:${label}`)
  }
}
