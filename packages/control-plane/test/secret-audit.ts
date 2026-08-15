export type SecretSurfaceKind = "frame" | "action" | "activity" | "view" | "diagnostic"

function serializeSurface(value: unknown): string {
  try {
    const serialized = JSON.stringify(value, (_key, current) => current instanceof Error
      ? { name: current.name, message: current.message, cause: current.cause }
      : current)
    return serialized === undefined ? String(value) : serialized
  } catch {
    throw new Error("secret-surface-serialization-failed")
  }
}

export function auditSecretFreeSurface(
  kind: SecretSurfaceKind,
  value: unknown,
  secrets: readonly string[],
  label: string,
): void {
  const diagnostic = `secret-scan-failed:${label}-${kind}`
  let surface: string
  try {
    surface = serializeSurface(value)
  } catch {
    throw new Error(`secret-surface-serialization-failed:${label}-${kind}`)
  }
  if (secrets.some((secret) => secret.length > 0 && surface.includes(secret))) {
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
  kind: Exclude<SecretSurfaceKind, "frame" | "diagnostic">,
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
  let surface: string
  try {
    surface = serializeSurface(value)
  } catch {
    throw new Error(`controlled-secret-source-invalid:${label}`)
  }
  if (!secrets.every((secret) => secret.length > 0 && surface.includes(secret))) {
    throw new Error(`controlled-secret-source-missing:${label}`)
  }
}
