# Claude Code Codex Subscription Bridge Design

Status: approved for T12 on 2026-08-21

Issue: [#13 — T12 Claude Code Codex Subscription Bridge](https://github.com/HaroldHuanrongLIU/muxvia/issues/13)

## Context

T12 consumes the Subscription Accounts and Provider bindings introduced by T11. It exposes ChatGPT Subscription access as a Claude Code Target Provider while Claude Code remains connected to a local Takeover listener and speaks Anthropic Messages.

The compatibility behavior is derived from CC-Switch v3.19.2 commit `43eaf07355af145aebfee301801779e824d4c221`, as recorded by ADR 0009, ADR 0018, and `docs/research/cc-switch-v3.19.2-compatibility-contract.md`. The integration uses an undocumented ChatGPT Codex endpoint and is not an officially supported OpenAI or Anthropic integration. Muxvia does not claim full CC-Switch compatibility.

## Scope

T12 delivers:

- one Claude-only Codex Subscription Bridge Target Provider preset;
- credentialless Provider declarations backed by a required Fixed or Follow Default Subscription Account binding;
- per-request binding and account resolution with memory-only access tokens and the T11 refresh behavior;
- the pinned internal endpoint, Codex client identity headers, and client-provided session identity headers;
- bounded Anthropic Messages to OpenAI Responses request conversion;
- streaming Responses to Anthropic Messages event conversion, including text, tools, usage, completion, and errors;
- Provider-level failover for missing, dangling, Needs Reauthorization, refresh-failed, and retryable upstream account failures;
- source-provenance golden fixtures and real listener evidence for the explicitly claimed model family;
- Target Provider and account workflow presentation for the preset and its binding state; and
- public English and Simplified Chinese risk disclosure.

T12 does not add native Codex CLI authentication import, pasted refresh tokens, alternate OAuth, account substitution, account-level failover, arbitrary Responses gateways, Chat Completions, FAST mode, quota display, model discovery through the undocumented endpoint, multimodal/PDF conversion, WebSearch emulation, or full CC-Switch parity.

## Binding Decisions

1. The Bridge is a Claude Target Provider only. Its closed declaration is `anthropic-messages` plus `codex-subscription`, a fixed internal base URL, no static Provider credential, and `takeover-required`.
2. The preset key is `codex-subscription-bridge`. The fixed production base is `https://chatgpt.com/backend-api/codex`; the form may display it but cannot redirect account tokens to another origin.
3. A Bridge Provider is complete only when it has one T11 Subscription Account binding. The bound account does not have to be currently usable for activation or Failover Chain application; an unusable account is a request-time Provider failure so later Providers remain eligible.
4. The active route plan pins Provider identity and declaration. The binding row is read when the request is pinned. Fixed then resolves only that exact account. Follow Default resolves the current default on every attempted request. Neither path substitutes another identity.
5. Access-token acquisition occurs only after the Provider member is admitted for an attempt. Skipped circuit members do not refresh accounts. Refresh rotation and Needs Reauthorization remain owned by T11.
6. The model listener receives a narrow account resolver installed before committed Takeover bootstrap. It does not read the private account file, call the Device Authority, or understand refresh persistence directly.
7. A private Subscription Bridge adapter owns the endpoint, headers, transformations, streaming state, and bounded error mapping. The generic router owns only attempt ordering, health, retryable status classification, and observation persistence.
8. T12 publicly claims the pinned guide's `gpt-5.6` and `gpt-5.6-luna` family names only for the versioned text, tool-use, and streaming fixtures proven here. Other model strings are passed through but are unverified and not advertised as compatible.
9. `/v1/messages/count_tokens` is a named Compatibility Deviation. The pinned bridge has no source-derived count-tokens mapping, so Muxvia returns a fixed local `501` without contacting an account or upstream.

## Provider Declaration and Persistence

`ProviderAuthentication` gains the closed value `codex-subscription`. It is valid only for `Target::Claude` with `ProviderProtocol::AnthropicMessages`. The declaration requires:

- the exact fixed base URL;
- `takeover-required`;
- no `credential_id`; and
- one `subscription_provider_bindings` row for the same Target and Provider.

Existing Claude Anthropic declarations continue to require a static credential. Existing Codex and Universal Provider declarations are unchanged. The Bridge is deliberately not a Universal Provider preset because its account binding, endpoint, credential ownership, and Target applicability are not universal.

Schema v12 widens only the closed authentication checks and makes an activated route member credential reference nullable for this one declaration. Activated snapshots keep their existing private credential column but store an empty value for the credentialless Bridge; all validation treats that value as valid only for `codex-subscription`. Migration preserves every existing table fingerprint and fails closed on a Bridge-shaped row that violates the declaration.

The Target Provider form exposes the preset, fixes the endpoint/authentication/routing requirement, hides static credential entry, and directs the Operator to the existing Subscription Accounts binding workflow. Update and duplicate operations cannot turn a Bridge Provider into a credential-bearing or redirectable declaration.

## Per-Request Account Resolution

The pinned request plan carries the closed binding:

```text
Fixed(account identity)
FollowDefault
```

For Fixed, the resolver requests a token only for the stored identity. Missing and Needs Reauthorization are failures for that Provider member. For Follow Default, the resolver rereads the private account document, obtains the current default identity, and then requests that exact account's token. No default, missing default, Needs Reauthorization, refresh failure, and identity mismatch are member failures.

The resolver returns only `{ account identity, access token }` in a redacted private value. The token is injected directly into the outbound request and is never copied into SQLite, route plans, snapshots, recovery payloads, receipts, views, logs, Debug output, or the control protocol.

Resolver failures advance to the next eligible Provider without an upstream call. Upstream `401`, `403`, `404`, `408`, `409`, `429`, `451`, and retryable `5xx` retain the existing Provider failover policy. A dangling Fixed binding never consults the default account.

## Pinned Request Conversion

For `/v1/messages`, the adapter sends `POST https://chatgpt.com/backend-api/codex/responses` and derives a Responses body from one bounded JSON object:

- `model` is replaced with the Provider's configured model;
- string or text-array `system` content becomes `instructions`;
- Anthropic user/assistant text becomes Responses input messages using `input_text`/`output_text`;
- assistant `tool_use` becomes top-level `function_call` with canonical JSON arguments;
- user `tool_result` becomes top-level `function_call_output`;
- Anthropic tools become Responses `function` tools with name, description, and input schema;
- tool choice maps `any` to `required`, preserves `auto`/`none`, and maps a named tool to a named function choice;
- `store` is always `false`;
- `include` is always `["reasoning.encrypted_content"]`;
- `stream` is always `true`;
- missing `instructions`, `tools`, and `parallel_tool_calls` default to `""`, `[]`, and `false`;
- `max_output_tokens`, `temperature`, and `top_p` are absent; and
- unknown Anthropic top-level fields are not forwarded.

The outbound headers are closed:

- `Authorization: Bearer <memory-only access token>`;
- `ChatGPT-Account-Id: <resolved identity>`;
- `originator: codex_cli_rs`;
- `version: 0.144.1`;
- `Content-Type: application/json`; and
- when a client-provided session is found, `session_id`, `x-client-request-id`, and `x-codex-window-id: <session>:0`.

Session extraction follows the pinned order: a `_session_` suffix in `metadata.user_id`, then `metadata.session_id`, then raw `metadata.user_id`, then inbound `x-session-id`. Muxvia never invents a session identity.

## Streaming Response Conversion

The adapter incrementally parses bounded `data:` SSE events without buffering the entire successful response and emits Anthropic SSE frames in order:

- `response.created` starts the message;
- output-text events open, append to, and close text content blocks;
- function-call item and argument events open, append JSON input, and close tool-use blocks;
- reasoning summary/text events map to thinking deltas when present;
- completed/incomplete events emit final usage, the source-derived stop reason, `message_delta`, and `message_stop`;
- failed/error events emit a fixed Anthropic error event without raw upstream bodies; and
- malformed, oversized, contradictory, or incomplete streams terminate with a fixed bridge error.

The adapter does not spawn a detached conversion task. Dropping the client response body drops the converter and upstream stream, so cancellation propagates by ownership. Non-success upstream bodies are bounded, scanned, discarded, and represented by a fixed Anthropic-compatible error while preserving the upstream status for Provider failover.

## Compatibility Deviations

The public documentation and fixtures name these deviations:

- no `/messages/count_tokens` bridge;
- no image, PDF, or other multimodal conversion claim;
- no WebSearch emulation;
- no FAST/service-tier toggle;
- no quota, model-catalog, prompt-cache-key, or usage-price projection;
- no CC-Switch model alias/role mapping beyond the Provider's single configured model;
- no claim for arbitrary model strings; and
- stricter bounded JSON/SSE parsing and secret-free fixed diagnostics.

Each deviation has a regression test. Unsupported features fail locally or remain unclaimed rather than silently approximating the baseline.

## Control Plane and Public Documentation

The Claude Target Provider picker identifies the Bridge as requiring Takeover and a Subscription Account binding. The form discloses that it uses an undocumented ChatGPT Codex interface, consumes the selected account's shared subscription quota, may stop working without notice, and may be subject to the Operator's applicable terms. It does not use OpenAI or Anthropic logos or imply endorsement.

The existing Subscription Accounts overlay remains the only place to select Fixed or Follow Default. A Bridge Provider with no binding is visibly incomplete. A dangling or Needs Reauthorization binding remains explicit. English and Simplified Chinese use the same closed localization keys and render at every supported terminal size.

The public documentation repeats the endpoint/support/continuity/account/quota/terms disclosures, the tested model names and capabilities, and every Compatibility Deviation.

## Stable Failure Categories

- `incomplete-provider` for a Bridge declaration without a binding;
- `unsupported-activation-mode` for Direct;
- `subscription-account-unavailable` for missing/default/refresh/transient resolver failure;
- `subscription-account-needs-reauthorization` for the exact persistent account state;
- `subscription-bridge-invalid-request` for unsupported or malformed Messages input;
- `subscription-bridge-invalid-response` for malformed/contradictory/oversized upstream data; and
- existing retryable Provider status, route unavailable, state, and recovery categories.

No category includes an account email, identity, token, Provider credential, request body, response body, tool arguments, session identity, or numeric byte rendering of those values.

## Confirmed Testing Seams

1. Pinned-source golden fixtures with manifest provenance at the exact CC-Switch commit.
2. Rust codec tests for request, headers, streaming text/tools/errors, cancellation, size limits, and named deviations.
3. Real SQLite plus deterministic account resolver and loopback upstream through the real Claude model listener and Provider failover chain.
4. Real UDS Target/Subscription Account sessions and Target Provider actions.
5. TargetSession/OpenTUI provider and account workflows with real renderer state.
6. A real-process walking tracer using real UDS, SQLite, private account file, renderer, restart, listener, and deterministic Device Authority/upstream.

Tests scan every secret-bearing Debug/serialized/frame/render surface before semantic assertions and use fixed diagnostics. Golden fixtures contain synthetic sentinels only.
