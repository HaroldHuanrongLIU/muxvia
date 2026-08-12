# Target CLI configuration contracts for Muxvia v1

Status: researched against first-party documentation and source on 2026-08-13.

This note defines the narrow integration seam Muxvia can safely rely on for Codex CLI and Claude Code. It does not treat either CLI's internal Rust/TypeScript types, prompts, model catalog, or undocumented traffic as a stable API.

Evidence labels used below:

- **Documented** — stated in current OpenAI or Anthropic product/API documentation.
- **Source-derived** — observed in a pinned first-party source snapshot, but not promised as a public compatibility contract.
- **Muxvia decision** — the conservative v1 behavior inferred from those facts and the accepted ADRs.

## Executive contract

| Concern | Codex CLI | Claude Code |
| --- | --- | --- |
| Supported home | Default `~/.codex` only | Default `~/.claude` only |
| Managed user file | `~/.codex/config.toml` | `~/.claude/settings.json` |
| Non-default-home signal | `CODEX_HOME` | `CLAUDE_CONFIG_DIR` |
| Takeover endpoint | Custom model provider `base_url = "http://127.0.0.1:<port>/v1"` | `env.ANTHROPIC_BASE_URL = "http://127.0.0.1:<port>"` |
| Target-facing protocol | OpenAI Responses over HTTP; SSE when streaming | Anthropic Messages over HTTP; SSE when streaming |
| Route authentication | Stable custom `http_headers` entry; exact Muxvia header is ours to define | `env.ANTHROPIC_AUTH_TOKEN`, sent as Bearer authorization |
| Model override | Top-level `model`, subject to higher layers | `env.ANTHROPIC_MODEL` or `model`, subject to `/model`, `--model`, and higher settings layers |
| Guaranteed effect of config edit | A newly started CLI process | A newly started CLI process |
| Hot route switch | Next request to the already-configured loopback endpoint | Next request to the already-configured loopback endpoint |

“Managed configuration” below means the individual keys Muxvia owns under ADR 0002. It does **not** mean Anthropic's organization-admin “managed settings” scope.

## Codex CLI

### Default files and effective precedence

**Documented.** `CODEX_HOME` defaults to `~/.codex` and is the root for configuration, authentication, logs, sessions, and skills. User configuration is `~/.codex/config.toml`; authentication is separately cached in `~/.codex/auth.json` or the OS credential store. Muxvia must not edit the authentication store. [`CODEX_HOME` and environment variables](https://developers.openai.com/codex/config-reference/#environment-variables), [authentication storage](https://developers.openai.com/codex/auth/)

The documented configuration precedence, highest first, is:

1. CLI flags and `--config` overrides.
2. Trusted project `.codex/config.toml` layers, from project root toward the current directory.
3. The selected profile layer in the Codex home.
4. User `~/.codex/config.toml`.
5. System configuration such as `/etc/codex/config.toml` on Unix.
6. Built-in defaults.

Current Codex security rules ignore provider/authentication/profile keys in project configuration. In particular, a project file cannot replace `openai_base_url`, `model_provider`, or `model_providers`; a project may still affect allowed keys such as `model`. CLI flags/`--config` and the selected profile remain real shadows above the user file. [Basic configuration and precedence](https://developers.openai.com/codex/config-basic/), [advanced configuration and project restrictions](https://developers.openai.com/codex/config-advanced/)

**Muxvia decision.** Reject activation when `CODEX_HOME` resolves away from the canonical default home. Read all known effective layers before writing. Report a higher-layer value as a shadow; never edit it. Treat unknown CLI launch flags as an unverifiable external shadow and say that global routing cannot be guaranteed for such a process.

### Stable configuration seam

**Documented.** A custom provider is selected with top-level `model_provider`; the model is selected with top-level `model`. A `[model_providers.<id>]` table supports `base_url`, `wire_api`, authentication, static or environment-derived HTTP headers, and other transport controls. The only currently supported `wire_api` value is `responses`. `openai_base_url` is a separate shortcut for overriding the built-in OpenAI provider; a named custom provider is the clearer ownership boundary for Muxvia. [Advanced provider configuration](https://developers.openai.com/codex/config-advanced/#custom-model-providers), [configuration reference](https://developers.openai.com/codex/config-reference/)

A takeover fragment should have this shape (illustrative values only):

```toml
model = "<activated-route-model>"
model_provider = "muxvia"

[model_providers.muxvia]
name = "Muxvia"
base_url = "http://127.0.0.1:<port>/v1"
wire_api = "responses"
http_headers = { X-Muxvia-Routing-Credential = "<generated-target-credential>" }
supports_websockets = false
```

**Muxvia decision.** Before first use, reject a pre-existing non-Muxvia provider named `muxvia` (or choose a collision-free reserved ID and persist it). Own only the two selected top-level keys and the exact generated provider-table entries. Preserve unrelated provider entries and comments as far as the TOML editor permits. Record field-level prior values, including absence.

Static `http_headers` is a documented way to inject a generated routing credential without controlling the target process environment. `experimental_bearer_token` is explicitly unstable and should not be used. Command-backed provider authentication is documented and avoids an inline secret, but it would require a separately installed credential-helper command and therefore expands the v1 seam. [`http_headers` and provider authentication](https://developers.openai.com/codex/config-reference/)

**Open decision.** The first-party contract does not define Muxvia's header name. V1 can use a private header such as `X-Muxvia-Routing-Credential`, stored with the same local-file protections as other Muxvia-managed secrets, or add a credential helper in a later ADR. The router must redact it from logs and diagnostics.

### Request protocol required at the loopback seam

**Documented.** A custom Codex provider speaks the OpenAI Responses API. A streamed Responses request sets `stream: true` and returns server-sent events. The request is not merely `{model, prompt}`: Responses supports structured `input`, instructions, tools, reasoning controls, and evolving optional fields. Muxvia must accept and validate the complete public Responses shape needed by supported Codex versions rather than invent a narrow prompt API. [Create a response](https://developers.openai.com/api/reference/resources/responses/methods/create), [streaming Responses](https://developers.openai.com/api/docs/guides/streaming-responses)

**Source-derived.** At OpenAI Codex commit [`95aada11`](https://github.com/openai/codex/tree/95aada11c4150e4ba28d6279c50f0995c1d93e5a), the first-party Responses proxy documents `base_url = "http://127.0.0.1:<port>/v1"`, `wire_api = "responses"`, and accepts `POST /v1/responses`. It forwards request headers while replacing authorization. This pins the present path behavior; it is not a promise that Codex will never add companion endpoints or fields. [Pinned proxy README](https://github.com/openai/codex/blob/95aada11c4150e4ba28d6279c50f0995c1d93e5a/codex-rs/responses-api-proxy/README.md)

**Muxvia decision.** Advertise `supports_websockets = false` and support HTTP plus SSE only in v1. Preserve unknown supported request fields, ordering-sensitive tool data, relevant headers, SSE event ordering, and upstream error bodies across the routing seam. Version-gate any semantic translation. Do not depend on Codex's private Rust request structs or system prompts.

### Capability probing and runtime effects

**Documented.** The public CLI exposes version/help surfaces and diagnostic commands; experimental `codex debug` commands are not a compatibility contract. [Codex CLI command reference](https://developers.openai.com/codex/cli/reference/)

**Muxvia decision.** Before the first write:

1. Resolve the executable that the control plane will describe to the user.
2. Run `codex --version` and inspect `codex --help`/relevant subcommand help without making a model request.
3. Parse the current TOML, resolve the documented layers, check the default home, provider-ID collision, symlinks, and known shadows.
4. Classify the installed version against a tested compatibility matrix as tested, unknown-compatible, or incompatible.
5. Do not use `codex debug models`, invoke provider discovery, read `auth.json`, or send a real inference request as a capability probe.

There is no documented, stable, machine-readable dry-run command that proves an arbitrary future provider fragment and full request protocol without starting a session. An unknown version therefore cannot be promoted to “tested” by version output alone. A later opt-in loopback smoke test may be designed, but it must be non-forwarding, use a fake credential, and account for session-state writes.

No general public guarantee says a running Codex process hot-reloads all configuration. Muxvia therefore guarantees managed-file changes only for a newly started Codex process. Once that process already targets Muxvia's stable loopback URL, a server-side Activated Snapshot switch applies to its **next model request**; it must not retarget an in-flight stream.

## Claude Code

### Default files and effective precedence

**Documented.** User settings live at `~/.claude/settings.json`. Shared project settings are `.claude/settings.json`; local uncommitted project settings are `.claude/settings.local.json`. `CLAUDE_CONFIG_DIR` relocates paths normally rooted at `~/.claude`. `~/.claude.json` is separate application state and must not be treated as the settings file. [Settings scopes](https://code.claude.com/docs/en/settings), [Claude directory reference](https://code.claude.com/docs/en/claude-directory)

The documented settings precedence, highest first, is:

1. Organization managed settings.
2. Command-line arguments, including an explicit `--settings` source.
3. Local project settings.
4. Shared project settings.
5. User settings.

Settings merge across scopes; scalar values use the highest-precedence value and `env` merges per environment-variable key. `/status` shows active settings sources, but it is not a reliable machine-readable per-key provenance API. [`settings.json` precedence](https://code.claude.com/docs/en/settings), [CLI reference](https://code.claude.com/docs/en/cli-reference)

**Muxvia decision.** Reject activation when `CLAUDE_CONFIG_DIR` resolves away from the canonical default. Inspect known managed, project, local, and user sources; do not edit higher scopes. Treat `--settings`, `--model`, `/model`, and unknown inherited launch state as shadows. Do not edit OAuth/application-state files or credential stores.

### Stable configuration seam

**Documented.** `ANTHROPIC_BASE_URL` routes Claude Code through a gateway. `ANTHROPIC_AUTH_TOKEN` is sent as an `Authorization: Bearer ...` value; `ANTHROPIC_API_KEY` is sent as `X-Api-Key`. Values under the settings file's `env` object override inherited shell values per key. Authentication/provider selection has its own precedence, with cloud-provider modes and explicit credentials able to change the route. [Environment variables](https://code.claude.com/docs/en/env-vars), [authentication precedence](https://code.claude.com/docs/en/authentication)

A takeover fragment should merge these owned keys into the existing `env` object:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:<port>",
    "ANTHROPIC_AUTH_TOKEN": "<generated-target-credential>",
    "ANTHROPIC_MODEL": "<activated-route-model>"
  }
}
```

`/model` has highest interactive priority, followed by `--model`, `ANTHROPIC_MODEL`, and the `model` setting. Resumed sessions can retain model state from their transcript. [Claude Code model configuration](https://code.claude.com/docs/en/model-config)

**Muxvia decision.** Own only these `env` keys (and any explicitly approved provider-selector keys found necessary by the compatibility probe), recording each prior value or absence. A higher-scope value for any owned key blocks guaranteed takeover. Cloud provider selectors such as Bedrock, Vertex, or Foundry modes must be detected: if active, Claude Code may not speak the Anthropic HTTP seam at all. Do not silently clear them without adding those exact keys to the managed/recovery contract.

Direct activation uses the same field-level mechanism with the selected upstream base URL, credential form, and model. Muxvia must never overwrite the complete `env` object or settings file.

### Request protocol required at the loopback seam

**Documented.** A Claude Code LLM gateway must expose Anthropic Messages endpoints `POST /v1/messages` and `POST /v1/messages/count_tokens`, and preserve `anthropic-version` and `anthropic-beta` headers. Claude Code also sends correlation metadata such as `X-Claude-Code-Session-Id`. With gateway model discovery explicitly enabled, it requests `/v1/models` at startup; discovery is off by default and is not needed for Muxvia's provider editor. [Claude Code LLM gateway](https://code.claude.com/docs/en/llm-gateway)

Messages requests include, among other fields, `model`, `messages`, `max_tokens`, system content, and tools. With `stream: true`, the response is SSE with message, content-block, delta, ping, and stop events. Anthropic's versioning policy permits compatible additions, including new optional fields and enum values. [Messages API](https://platform.claude.com/docs/en/api/messages/create), [streaming Messages](https://platform.claude.com/docs/en/build-with-claude/streaming), [API versioning](https://platform.claude.com/docs/en/api/versioning)

**Muxvia decision.** Implement a forward-compatible Anthropic Messages ingress, including token counting and SSE, and validate the Muxvia bearer credential before any upstream call. Preserve version/beta headers and unknown fields that the selected translator supports. Do not depend on Claude Code's unpublished prompts, tool schemas beyond the public wire payload, or terminal UI behavior.

Anthropic explicitly frames Claude Code gateways as routing to Claude models and does not support using this mechanism to route Claude Code to non-Claude models. Any Muxvia translation to a non-Claude provider is therefore a Muxvia compatibility layer, not an Anthropic-supported configuration. It needs explicit per-target compatibility tests and must not be represented as first-party-supported behavior. [Gateway limitations](https://code.claude.com/docs/en/llm-gateway)

### Capability probing and runtime effects

**Documented.** `claude --version`, CLI help, and `claude doctor` are public diagnostic surfaces. `claude doctor` performs read-only installation/configuration diagnostics; interactive `/doctor` can offer repairs and is not the unattended probe. [CLI reference](https://code.claude.com/docs/en/cli-reference), [troubleshooting](https://code.claude.com/docs/en/troubleshooting)

**Muxvia decision.** Before the first write:

1. Run `claude --version`, inspect public help, and run only read-only diagnostics.
2. Parse JSON and enumerate known settings sources, per-key `env` shadows, default-home status, symlinks, provider mode, and model overrides.
3. Classify the installed version using the tested compatibility matrix.
4. Do not launch an inference, `/model`, model discovery, login, or any repair-capable interactive command.

No documented dry-run command proves that a candidate settings fragment will yield the desired Messages traffic. Unknown versions therefore remain unknown-compatible until explicitly tested.

Claude Code watches settings files and reloads most changed keys in a running session. Its `env` values are re-applied when settings change, but removing an `env` key does not unset that variable in the existing process; model selection is read at startup unless changed with `/model`. Consequently ADR 0045's new-process guarantee remains correct and deliberately conservative. A new process is required after activation, restore, or removal for a deterministic result. A running process already using Muxvia's unchanged loopback base URL can receive a server-side route change on its next request. [Settings reload behavior](https://code.claude.com/docs/en/settings), [`env` lifecycle](https://code.claude.com/docs/en/env-vars), [model lifecycle](https://code.claude.com/docs/en/model-config)

## Write, drift, restore, and shadowing algorithm

The target adapters should expose the same small semantic operation despite different file formats:

1. Build an immutable candidate from the Activated Snapshot: owned key paths, new values, observed prior values, canonical file identity, file hash/revision, CLI version, and detected shadows.
2. Capability-probe before the first write. Block incompatible versions and non-default homes; require the established checkpoint for unknown-compatible versions.
3. Immediately before writing, re-read and compare the owned paths plus file identity. Any divergence is drift.
4. Write a field-level merge atomically. Never replace the whole TOML/JSON document and never touch authentication/application-state files.
5. Re-read and verify the owned paths. Only then commit activation/recovery state.
6. On failure, restore the exact prior owned values/absence from the recovery snapshot and verify again.
7. On later save, activation, synchronization, or restore, block on drift until Adopt, Reapply, or Restore resolves it.

The process environment and invocation flags of independently launched target CLIs are not globally observable. Muxvia can guarantee the managed default-file state; it must qualify runtime claims when the user launches a CLI with an overriding profile, flag, alternate settings file, cloud-provider mode, or resumed model state.

## ADR reconciliation

| ADR | Result of current first-party contracts |
| --- | --- |
| [0002](../adr/0002-own-only-managed-configuration.md) | Confirmed. Both targets support narrow key-level ownership. Codex needs selected top-level keys plus one provider table; Claude needs individual `env` entries. Whole-file replacement would corrupt unrelated user configuration. |
| [0012](../adr/0012-authenticate-all-local-model-routes.md) | Implementable. Codex has stable custom headers; Claude has `ANTHROPIC_AUTH_TOKEN`. The exact Codex Muxvia header name and inline-secret versus helper choice remain a Muxvia contract decision. |
| [0021](../adr/0021-restore-database-and-managed-configuration-together.md) | Confirmed. Absence is a meaningful prior value. Restore must include selected provider/model pointers and only the generated provider or `env` entries, then verify. |
| [0023](../adr/0023-retain-direct-and-takeover-activation-modes.md) | Confirmed at the configuration seam. Takeover fixes the CLI on a loopback base URL; direct activation writes the corresponding upstream values. Claude non-Claude routing remains outside Anthropic support. |
| [0025](../adr/0025-activate-immutable-provider-snapshots-explicitly.md) | Confirmed. The observed target-file identity/hash and prior owned values belong in the recovery snapshot before an atomic merge. |
| [0026](../adr/0026-reconcile-drift-before-managed-writes.md) | Confirmed. Higher-layer shadows and changes to owned paths are distinct: drift is reconciled; an unowned higher-precedence shadow is reported and never rewritten. |
| [0027](../adr/0027-discover-models-non-blockingly.md) | Confirmed. Target CLI model pickers/debug catalogs are not control-plane APIs. Continue provider-native, explicit asynchronous discovery; Claude gateway `/v1/models` is opt-in startup behavior, not a replacement. |
| [0039](../adr/0039-support-only-default-target-configuration-homes.md) | Confirmed, with a nuance: Codex project files currently cannot shadow provider/base/auth keys, although they can shadow allowed keys such as `model`. Codex CLI/profile overrides and Claude managed/CLI/project/local settings remain shadows. |
| [0045](../adr/0045-probe-target-capabilities-and-bound-runtime-effects.md) | Confirmed. Claude often hot-reloads values, but removal and model behavior make new-process-only the sound guarantee. Codex exposes no general hot-reload guarantee. Server-side switches still affect the next request, not an in-flight stream. |

## Checkpoints and unresolved compatibility work

The research exposes no contradiction requiring the accepted ADRs to be reversed. These points do require an implementation decision or versioned test evidence:

1. **Codex route credential representation.** Adopt a named private static header for v1, or explicitly expand scope to ship a command-backed credential helper. Do not use the experimental bearer-token field.
2. **Compatibility matrix bounds.** Record the first and latest tested Codex/Claude versions and fixtures; the public docs do not specify a minimum CLI version for Muxvia's complete seam.
3. **Claude provider-mode ownership.** Decide whether takeover merely blocks Bedrock/Vertex/Foundry selectors or owns and restores those exact selector keys. Blocking is the smaller v1 contract.
4. **Non-Claude models through Claude Code.** This is explicitly outside Anthropic's supported gateway use. Each translated model family needs a tested compatibility claim and a user-visible limitation.
5. **Forward protocol fixtures.** Pin golden HTTP/SSE fixtures for Responses, Messages, token counting, errors, tool use, cancellation, and new/unknown fields. Source-derived Codex paths and all translator behavior must be version-gated.
6. **Unobservable launch shadows.** No file scan can prove that an independently launched process lacks CLI overrides or resumed-session model state. The UI and diagnostic CLI must state this boundary rather than promising universal takeover.

