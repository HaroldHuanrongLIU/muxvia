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
  command("provider.activate.direct", "command.provider.activate.direct", null, ["<leader>a"], ["provider-picker", "provider-picker-claude"], false),
  command("provider.activate.takeover", "command.provider.activate.takeover", null, ["<leader>o"], ["provider-picker-claude"], false),
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
  command("target.direct.apply", "command.direct.apply", "direct", ["<leader>d"], ["codex", "claude"], true),
  command("target.takeover.apply", "command.takeover.apply", "takeover", ["<leader>a"], ["codex", "claude"], true),
  command("target.takeover.disable", "command.takeover.disable", "disable-takeover", ["<leader>x"], ["codex", "claude"], true),
  command("target.takeover.disable-confirm", "command.takeover.disable.confirm", null, ["return", "y"], ["takeover-disable-confirm"], false),
  command("target.takeover.disable-cancel", "command.takeover.disable.cancel", null, ["escape", "n"], ["takeover-disable-confirm"], false),
  command("target.reconciliation.open", "command.reconciliation.open", "reconcile", ["<leader>r"], ["codex", "claude"], true),
  command("target.reconciliation.preview.adopt", "command.reconciliation.adopt", null, ["a"], ["reconciliation"], false),
  command("target.reconciliation.preview.reapply", "command.reconciliation.reapply", null, ["r"], ["reconciliation"], false),
  command("target.reconciliation.preview.restore", "command.reconciliation.restore", null, ["s"], ["reconciliation"], false),
  command("target.reconciliation.apply", "command.reconciliation.apply", null, ["return"], ["reconciliation"], false),
  command("target.reconciliation.cancel", "command.reconciliation.cancel", null, ["escape"], ["reconciliation"], false),
  command("activity.open", "command.activity.open", "activity", [], ["codex", "claude"], true),
  command("activity.select-previous", "command.activity.select-previous", null, ["up", "k"], ["activity"], false),
  command("activity.select-next", "command.activity.select-next", null, ["down", "j"], ["activity"], false),
  command("activity.inspect", "command.activity.inspect", null, ["return"], ["activity"], false),
  command("activity.more", "command.activity.more", null, ["m"], ["activity"], false),
  command("activity.cancel", "command.activity.cancel", null, ["escape"], ["activity"], false),
  command("route.open", "command.route.open", "route", ["<leader>f"], ["codex", "claude"], true),
  command("route.move-up", "command.route.move-up", null, ["u"], ["route-editor"], false),
  command("route.move-down", "command.route.move-down", null, ["n"], ["route-editor"], false),
  command("route.add-provider", "command.route.add-provider", null, ["a"], ["route-editor"], false),
  command("route.remove-provider", "command.route.remove-provider", null, ["x"], ["route-editor"], false),
  command("route.apply", "command.route.apply", null, ["return"], ["route-editor"], false),
  command("universal-provider.list", "command.universal-provider.list", "universal-providers", ["<leader>g"], ["codex", "claude"], true),
  command("universal-provider.create", "command.universal-provider.create", null, ["c"], ["universal-provider-picker"], false),
  command("universal-provider.edit", "command.universal-provider.edit", null, ["return"], ["universal-provider-picker"], false),
  command("universal-provider.duplicate", "command.universal-provider.duplicate", null, ["<leader>c"], ["universal-provider-picker"], false),
  command("universal-provider.delete", "command.universal-provider.delete", null, ["<leader>d"], ["universal-provider-picker"], false),
  command("universal-provider.synchronize", "command.universal-provider.synchronize", null, ["<leader>s"], ["universal-provider-picker"], false),
  command("universal-provider.save", "command.universal-provider.save", null, ["return"], ["universal-provider-editor"], false),
  command("universal-provider.cancel", "command.universal-provider.cancel", null, ["escape"], ["universal-provider-editor"], false),
  command("universal-provider.confirm", "command.universal-provider.confirm", null, ["return", "y"], ["universal-provider-confirm"], false),
  command("universal-provider.confirm.cancel", "command.universal-provider.confirm.cancel", null, ["escape", "n"], ["universal-provider-confirm"], false),
  command("subscription-account.list", "command.subscription-account.list", "accounts", ["<leader>i"], ["codex", "claude", "provider-picker-claude"], true),
  command("subscription-account.authorize", "command.subscription-account.authorize", null, ["a"], ["subscription-account-picker"], false),
  command("subscription-account.reauthorize", "command.subscription-account.reauthorize", null, ["r"], ["subscription-account-picker"], false),
  command("subscription-account.default", "command.subscription-account.default", null, ["s"], ["subscription-account-picker"], false),
  command("subscription-account.bind.fixed", "command.subscription-account.bind.fixed", null, ["f"], ["subscription-account-picker"], false),
  command("subscription-account.bind.follow-default", "command.subscription-account.bind.follow-default", null, ["l"], ["subscription-account-picker"], false),
  command("subscription-account.delete", "command.subscription-account.delete", null, ["<leader>d"], ["subscription-account-picker"], false),
  command("subscription-account.confirm", "command.subscription-account.confirm", null, ["return", "y"], ["subscription-account-picker"], false),
  command("subscription-account.cancel", "command.subscription-account.cancel", null, ["escape", "c"], ["subscription-account-picker"], false),
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
