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
  command("target.sidebar.toggle", "command.target.sidebar", "sidebar", ["<leader>b"], ["codex"], true),
  command("provider.create", "command.provider.create", "provider", ["<leader>p"], ["codex"], true),
  command("provider.save", "command.provider.save", null, ["return"], ["editor"], false),
  command("provider.cancel", "command.provider.cancel", null, ["escape"], ["editor"], false),
  command("target.takeover.apply", "command.takeover.apply", "takeover", ["<leader>a"], ["codex"], true),
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
