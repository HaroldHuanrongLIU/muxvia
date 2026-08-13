export type CommandId =
  | "command.palette.show"
  | "target.codex.open"
  | "target.claude.open"
  | "target.home"
  | "target.sidebar.toggle"
  | "provider.create"
  | "provider.save"
  | "provider.cancel"
  | "target.takeover.apply"
  | "app.exit.request"
  | "overlay.close"
  | "app.exit.confirm"
  | "app.exit.cancel"

export type CommandScope = "global" | "home" | "codex" | "claude" | "editor" | "overlay" | "confirm"

export type CommandTextKey = `command.${string}`

export interface CommandDefinition {
  id: CommandId
  titleKey: CommandTextKey
  descriptionKey: CommandTextKey
  slashName: string | null
  bindings: readonly string[]
  scopes: readonly CommandScope[]
  palette: boolean
}
