import type { ClaudePreflightContext } from "./types"

export function claudePreflightContext(
  environment: NodeJS.ProcessEnv,
  cwd = process.cwd(),
): ClaudePreflightContext {
  const selectorNames = [
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
  ] as const
  const normalized = selectorNames.map((name) => normalizeSelector(environment[name]))
  const environmentBlockingSelector = selectorNames.find((_, index) => (
    normalized[index] === "enabled" || normalized[index] === "unknown-nonempty"
  ))
  const selectorState = normalized.includes("enabled")
    ? "enabled"
    : normalized.includes("unknown-nonempty")
      ? "unknown-nonempty"
      : normalized.every((state) => state === "unset") ? "unset" : "disabled"
  const hostManagedState = (() => {
    const state = normalizeSelector(environment.CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST)
    if (state === "enabled") return "managed" as const
    if (state === "unknown-nonempty") return "unknown" as const
    return "unmanaged" as const
  })()
  const blockingSelector = environmentBlockingSelector
    ?? (hostManagedState === "unmanaged" ? null : "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST")
  return {
    claudeConfigDir: environment.CLAUDE_CONFIG_DIR ?? null,
    selectorState,
    blockingSelector,
    hostManagedState,
    cwd,
  }
}

function normalizeSelector(value: string | undefined): "unset" | "disabled" | "enabled" | "unknown-nonempty" {
  if (value === undefined || value === "") return "unset"
  if (value === "0" || value === "false") return "disabled"
  if (value === "1" || value === "true") return "enabled"
  return "unknown-nonempty"
}
