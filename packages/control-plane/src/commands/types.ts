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
  | "provider.activate.direct"
  | "provider.activate.takeover"
  | "provider.duplicate"
  | "provider.models.refresh"
  | "provider.models.select"
  | "provider.reachability.check"
  | "provider.credential.remove"
  | "provider.authentication.toggle"
  | "provider.credential.reuse"
  | "provider.credential.without"
  | "provider.credential.confirmation.cancel"
  | "provider.save"
  | "provider.cancel"
  | "provider.delete.confirm"
  | "provider.delete.cancel"
  | "provider.activate.takeover-confirm"
  | "provider.activate.takeover-cancel"
  | "target.direct.apply"
  | "target.takeover.apply"
  | "target.reconciliation.open"
  | "target.reconciliation.preview.adopt"
  | "target.reconciliation.preview.reapply"
  | "target.reconciliation.preview.restore"
  | "target.reconciliation.apply"
  | "target.reconciliation.cancel"
  | "universal-provider.list"
  | "universal-provider.create"
  | "universal-provider.edit"
  | "universal-provider.duplicate"
  | "universal-provider.delete"
  | "universal-provider.synchronize"
  | "universal-provider.save"
  | "universal-provider.cancel"
  | "universal-provider.confirm"
  | "universal-provider.confirm.cancel"
  | "app.exit.request"
  | "overlay.close"
  | "app.exit.confirm"
  | "app.exit.cancel"

export type CommandScope = "global" | "home" | "codex" | "claude" | "editor" | "provider-picker" | "provider-picker-claude" | "provider-source-picker" | "provider-model-picker" | "provider-credential-confirm" | "provider-delete-confirm" | "takeover-required-confirm" | "reconciliation" | "universal-provider-picker" | "universal-provider-editor" | "universal-provider-confirm" | "overlay" | "confirm"

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
