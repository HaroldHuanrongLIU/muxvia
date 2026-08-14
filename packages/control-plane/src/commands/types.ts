export type CommandId =
  | "command.palette.show"
  | "target.codex.open"
  | "target.claude.open"
  | "target.home"
  | "target.sidebar.toggle"
  | "provider.create"
  | "provider.list"
  | "provider.edit"
  | "provider.move-up"
  | "provider.move-down"
  | "provider.delete"
  | "provider.duplicate"
  | "provider.models.refresh"
  | "provider.models.select"
  | "provider.reachability.check"
  | "provider.credential.remove"
  | "provider.credential.reuse"
  | "provider.credential.without"
  | "provider.credential.confirmation.cancel"
  | "provider.save"
  | "provider.cancel"
  | "provider.delete.confirm"
  | "provider.delete.cancel"
  | "target.takeover.apply"
  | "app.exit.request"
  | "overlay.close"
  | "app.exit.confirm"
  | "app.exit.cancel"

export type CommandScope = "global" | "home" | "codex" | "claude" | "editor" | "provider-picker" | "provider-source-picker" | "provider-model-picker" | "provider-credential-confirm" | "provider-delete-confirm" | "overlay" | "confirm"

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
