import type { CommandDefinition, CommandScope, CommandTextKey } from "./types"

function command(
  id: CommandDefinition["id"],
  titleKey: CommandTextKey,
  slashName: string | null,
  bindings: readonly string[],
  scopes: readonly CommandScope[],
  palette: boolean,
): CommandDefinition {
  return { id, titleKey, descriptionKey: `${titleKey}.description`, slashName, bindings, scopes, palette }
}

export const commandCatalog = [
  command("command.palette.show", "command.palette", null, ["ctrl+p"], ["global"], true),
  command("target.codex.open", "command.target.codex", "codex", ["1"], ["home"], true),
  command("target.claude.open", "command.target.claude", "claude", ["2"], ["home"], true),
  command("target.home", "command.target.home", "home", ["escape"], ["codex", "claude"], true),
  command("target.sidebar.toggle", "command.target.sidebar", "sidebar", ["<leader>b"], ["codex", "claude"], true),
  command("provider.create", "command.provider.create", "provider", ["<leader>p"], ["codex", "claude", "provider-source-picker"], true),
  command("provider.list", "command.provider.list", "providers", ["<leader>l"], ["codex", "claude"], true),
  command("provider.edit", "command.provider.edit", null, ["return"], ["provider-picker", "provider-picker-claude"], false),
  command("provider.move-up", "command.provider.move-up", null, ["<leader>u"], ["provider-picker", "provider-picker-claude"], false),
  command("provider.move-down", "command.provider.move-down", null, ["<leader>n"], ["provider-picker", "provider-picker-claude"], false),
  command("provider.delete", "command.provider.delete", null, ["<leader>d"], ["provider-picker", "provider-picker-claude"], false),
  command("provider.activate.direct", "command.provider.activate.direct", null, ["<leader>a"], ["provider-picker"], false),
  command("provider.activate.takeover", "command.provider.activate.takeover", null, ["<leader>a"], ["provider-picker-claude"], false),
  command("provider.duplicate", "command.provider.duplicate", null, ["<leader>c"], ["provider-picker", "provider-picker-claude"], false),
  command("provider.reachability.check", "command.provider.reachability", null, ["<leader>t"], ["provider-picker", "provider-picker-claude"], false),
  command("provider.credential.remove", "command.provider.credential.remove", null, ["<leader>r"], ["editor"], false),
  command("provider.authentication.toggle", "command.provider.authentication.toggle", null, ["<leader>h"], ["editor"], false),
  command("provider.models.refresh", "command.provider.models.refresh", null, ["<leader>f"], ["editor"], false),
  command("provider.models.select", "command.provider.models.select", null, ["<leader>m"], ["editor", "provider-model-picker"], false),
  command("provider.save", "command.provider.save", null, ["return"], ["editor"], false),
  command("provider.cancel", "command.provider.cancel", null, ["escape"], ["editor"], false),
  command("provider.delete.confirm", "command.provider.delete.confirm", null, ["return", "y"], ["provider-delete-confirm"], false),
  command("provider.delete.cancel", "command.provider.delete.cancel", null, ["escape", "n"], ["provider-delete-confirm"], false),
  command("provider.credential.reuse", "command.provider.credential.reuse", null, ["return", "y"], ["provider-credential-confirm"], false),
  command("provider.credential.without", "command.provider.credential.without", null, ["n"], ["provider-credential-confirm"], false),
  command("provider.credential.confirmation.cancel", "command.provider.credential.confirmation.cancel", null, ["escape"], ["provider-credential-confirm"], false),
  command("provider.activate.takeover-confirm", "command.takeover.confirm", null, ["return", "y"], ["takeover-required-confirm"], false),
  command("provider.activate.takeover-cancel", "command.takeover.cancel", null, ["escape", "n"], ["takeover-required-confirm"], false),
  command("target.direct.apply", "command.direct.apply", "direct", ["<leader>d"], ["codex"], true),
  command("target.takeover.apply", "command.takeover.apply", "takeover", ["<leader>a"], ["codex", "claude"], true),
  command("app.exit.request", "command.app.exit", "quit", ["ctrl+c", "<leader>q"], ["global"], true),
  command("overlay.close", "command.overlay.close", null, ["escape"], ["overlay"], false),
  command("app.exit.confirm", "command.app.exit.confirm", null, ["return", "y"], ["confirm"], false),
  command("app.exit.cancel", "command.app.exit.cancel", null, ["escape", "n"], ["confirm"], false),
] as const satisfies readonly CommandDefinition[]

export function commandsForScope(scope: CommandScope): readonly CommandDefinition[] {
  return commandCatalog.filter((command) => command.scopes.includes(scope))
}

export function resolveSlash(input: string, scope: CommandScope): CommandDefinition["id"] | undefined {
  const match = input.trim().match(/^\/([^\s/]+)$/)
  if (!match) return undefined
  return commandsForScope(scope).find((command) => command.slashName === match[1])?.id
}
