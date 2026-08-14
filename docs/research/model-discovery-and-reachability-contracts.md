# Model Discovery and Reachability Check contracts

Status: researched for T03 on 2026-08-14

Compatibility baseline: CC-Switch `v3.19.2`, commit [`43eaf07355af145aebfee301801779e824d4c221`](https://github.com/farion1231/cc-switch/commit/43eaf07355af145aebfee301801779e824d4c221)

Evidence labels in this note:

- **Documented** — an official provider or target-CLI document states the behavior.
- **Source-derived** — the pinned CC-Switch source implements the behavior.
- **Muxvia contract** — the behavior T03 should expose, including named hardening or compatibility decisions.

## Executive contract

Model Discovery is an authenticated, read-only retrieval of model identifiers. It may update only transient discovery state; it must not select a model, modify a Target Provider, change the Current Target Provider, write Managed Configuration, activate a route plan, or change Route Health. Opening an editor starts one asynchronous discovery against the last-saved endpoint and credential, while manual entry remains available ([ADR 0027](../adr/0027-discover-models-non-blockingly.md), [domain definition](../../CONTEXT.md#model-discovery)).

A Reachability Check is a separate, Operator-initiated, unauthenticated network probe. It answers only whether the saved endpoint returned response headers. It must never become a synthetic model request or feed passive Route Health, failover eligibility, circuit state, Current Target Provider, or Serving Provider ([domain definitions](../../CONTEXT.md#reachability-check), [pinned separation](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/stream_check.rs#L1-L17)).

The two operations therefore have intentionally different meanings:

| Operation | Credentials | Success | Timeout | Permitted state effect |
| --- | --- | --- | --- | --- |
| Model Discovery | Provider credential, using the provider's declared auth profile | Valid model-list response | 15 seconds per candidate | Transient result/error only |
| Reachability Check | None | Any HTTP response, including 4xx/5xx | 8 seconds per attempt; one timeout-like retry | Separate observation/result only |
| Route Health | Real routed-request credentials and traffic | Determined by routing/failure policy | Routing policy | Passive Route Health and circuit state |

## 1. Pinned CC-Switch Model Discovery

### 1.1 Generic OpenAI-compatible candidate construction

**Source-derived.** `build_models_url_candidates` trims surrounding whitespace and trailing slashes, de-duplicates while preserving first occurrence, and returns at most three candidates. Its exact branch order is ([implementation](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L129-L203)):

1. A nonblank `models_url_override` is the sole candidate, exactly as trimmed.
2. With `is_full_url = true`, the function returns one derived candidate:
   - if the raw string contains `/v1/`, replace everything from the first such occurrence onward with `/v1/models`;
   - otherwise remove the last slash-delimited segment and append `/v1/models`;
   - fail if neither derivation produces a root that looks like it contains a scheme and authority.
3. For an ordinary base whose final segment is lowercase `v` followed only by one or more ASCII digits, first append `/models`. If that segment is not exactly `v1`, next append `/v1/models` to the same base.
4. For any other ordinary base, append `/v1/models`.
5. If the base ends with one of these exact, case-sensitive compatibility suffixes, also strip it and append `/v1/models`, then `/models`, to the remaining root:

   ```text
   /api/claudecode  /api/anthropic  /apps/anthropic
   /api/coding      /claudecode     /anthropic
   /step_plan       /coding         /claude
   ```

The version-segment and suffix checks are raw string operations, not URL-semantic operations ([helpers](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L216-L236)). Consequently a query, fragment, userinfo, unusual path, or encoded separator can produce surprising candidates.

**Muxvia contract.** Preserve that ordering for valid HTTP(S) endpoint paths, but construct candidates through parsed URLs: reject userinfo and fragments, do not copy a query into a derived candidate, and require every derived fallback to retain the original origin. A models-endpoint override is a single explicit target and may have a different origin only when that complete URL was saved deliberately. Do not infer or probe further hosts after an error.

### 1.2 Request, authentication, and timeout

**Source-derived.** Every generic candidate receives:

- `GET`;
- `Authorization: Bearer <api_key>`;
- a 15-second per-request timeout; and
- the saved custom `User-Agent`, if it parses as a header value.

The command accepts `base_url`, raw `api_key`, `is_full_url`, an exact `models_url`, and a custom user agent; an invalid custom user agent is silently omitted ([command boundary](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/commands/model_fetch.rs#L90-L114), [request construction](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L51-L89)). Because the timeout is per candidate, three slow 404/405 responses can consume almost 45 seconds; a timeout itself is terminal and does not advance to the next candidate.

**Muxvia contract.** The Control Plane passes a Provider identity/config revision and a Credential Reference, not secret bytes returned from storage. The Routing Service resolves the reference, snapshots the saved endpoint/auth inputs for the operation, attaches credentials only after validating the candidate, and supports cancellation when the editor closes or a newer discovery supersedes it. Automatic editor-open discovery always uses last-saved values as required by ADR 0027.

Auth is a declared protocol property, not an endpoint heuristic:

| Provider protocol/auth profile | Discovery headers |
| --- | --- |
| OpenAI and explicitly OpenAI-compatible API key | `Authorization: Bearer <credential>` |
| Anthropic direct API key | `x-api-key: <credential>` and `anthropic-version: 2023-06-01` |
| Anthropic OAuth/workload token | `Authorization: Bearer <credential>` and `anthropic-version: 2023-06-01` |
| Anthropic-format gateway | The gateway's saved inference auth mode, plus its explicitly saved custom headers |
| Managed Codex Subscription | Dedicated adapter in section 1.4; never the generic provider path |

OpenAI documents bearer authentication for `GET /v1/models` and a top-level `data` list ([OpenAI Models API](https://platform.openai.com/docs/api-reference/models/object?lang=curl)). Anthropic documents `GET /v1/models` with `x-api-key` and `anthropic-version` for direct API-key calls, while its API overview also permits a bearer workload-identity token as the alternative auth mode ([Anthropic Models API](https://platform.claude.com/docs/en/api/models/list), [Anthropic API overview](https://platform.claude.com/docs/en/api/overview)). Claude Code gateway discovery likewise uses the same auth as inference: `ANTHROPIC_AUTH_TOKEN` as bearer, otherwise `ANTHROPIC_API_KEY` as `x-api-key`, plus configured custom headers ([Claude Code gateway discovery](https://code.claude.com/docs/en/llm-gateway#model-selection)).

CC-Switch's unconditional bearer header is therefore the exact generic baseline but not a valid universal provider contract. Muxvia must not send both auth headers unless a dedicated adapter explicitly requires both.

### 1.3 Response parsing and fallback

**Source-derived.** The generic parser expects a JSON object with optional `data`; each element must contain string `id` and may contain string `owned_by`. Unknown fields are ignored. A missing or `null` `data` succeeds with an empty list, a malformed entry fails the whole parse, IDs are sorted lexicographically, and duplicates are retained ([response types and parse](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L20-L30), [success path](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L91-L110)).

Fallback is deliberately narrow ([status handling](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L84-L127)):

| Result from a candidate | Pinned behavior | T03 behavior |
| --- | --- | --- |
| 2xx + valid JSON | Return parsed list immediately | Same |
| 2xx + invalid JSON/schema | Terminal parse error | Same |
| 404 or 405 | Record error and try next candidate | Same |
| 401 or 403 | Terminal HTTP error | Terminal `authentication-rejected` |
| 429 | Terminal HTTP error | Terminal `rate-limited` |
| Other 4xx or any 5xx | Terminal HTTP error | Terminal `upstream-status` |
| DNS/connect/TLS/timeout/send failure | Terminal request error | Terminal typed network error |
| All candidates return 404/405 | Error containing the last response | `endpoint-unsupported`, with no raw body |

This rule prevents credential-bearing discovery from turning into broad endpoint fishing. In particular, a timeout, TLS failure, 401, 429, 5xx, or invalid successful response must not silently try a different path.

**Documented.** OpenAI's shape matches the baseline fields. Anthropic also returns `data`, but its list is paginated: the default page size is 20, the maximum is 1,000, and `has_more`/`last_id` drive `after_id` pagination ([Anthropic Models API](https://platform.claude.com/docs/en/api/models/list)).

**Muxvia contract.** A protocol-specific parser must:

- preserve each nonblank model `id` exactly;
- accept optional `owned_by` (OpenAI-compatible) or `display_name` (Anthropic) as display metadata;
- ignore additive unknown fields;
- represent a valid empty list as success, distinct from parse failure;
- sort deterministically and de-duplicate exact IDs; and
- for Anthropic-native discovery, request up to 1,000 entries per page and follow `has_more` using `last_id`, with an operation-wide page/item cap and repeated-cursor rejection.

Discovery output is advisory at a point in time. It must not delete a manually entered model or imply that an absent model is unavailable forever. Claude Code's own gateway discovery is opt-in, filters model IDs for its picker, caches results, and falls back on failure; that is useful target-CLI behavior but not Muxvia's provider-editor API contract ([Claude Code gateway discovery](https://code.claude.com/docs/en/llm-gateway#model-selection)).

### 1.4 Managed Codex Subscription exception

**Source-derived, undocumented upstream.** CC-Switch does not use `/v1/models` for a managed ChatGPT account. It calls the fixed, undocumented URL `https://chatgpt.com/backend-api/codex/models?client_version=<CC-Switch package version>` with bearer access token, `originator: cc-switch`, `chatgpt-account-id`, and a 15-second timeout ([request](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/codex_oauth_models.rs#L1-L40)). The command resolves either the requested account or the current default account and obtains a valid access token immediately before the call ([account resolution](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/commands/codex_oauth.rs#L62-L90)).

Its permissive parser accepts a top-level array, `data`, `models`, or `items`; `models` may also be an object map. Entries may be strings or objects, the ID is selected from `slug`, `id`, `model`, or `name`, owner metadata accepts several aliases, malformed entries are skipped, and IDs are sorted and de-duplicated ([parser](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/codex_oauth_models.rs#L45-L117)).

**Muxvia contract.** Keep this behind the Subscription Bridge adapter and its risk disclosure; it is compatibility with an undocumented interface, not an official OpenAI Models API ([ADR 0018](../adr/0018-pin-the-subscription-bridge-to-derived-cc-switch-behavior.md)). Do not reuse this endpoint, parser, account header, or identity for ordinary API-key providers.

## 2. Error and redaction boundary

### Pinned behavior

CC-Switch caps an HTTP error body at 512 Unicode characters and returns it in the user-facing error; request and parse errors also embed the underlying library error string ([generic errors](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L84-L127), [truncation](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/model_fetch.rs#L205-L213)). The managed Codex path has the same 512-character body boundary ([managed errors](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/codex_oauth_models.rs#L31-L40), [managed truncation](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/codex_oauth_models.rs#L119-L127)).

Candidate debug logs use a URL redactor that removes userinfo, query, and fragment, then replaces exact known secrets only when they are at least eight characters. When no known secret is available, a separate helper can reduce the URL to origin only ([redaction implementation](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/lib.rs#L97-L215)). The returned error strings do not pass through that URL redactor. Thus the 512-character cap is a size boundary, not a secrecy boundary.

### Muxvia contract

Model Discovery and Reachability Check are management operations, not routed requests. They must not create a Request Record or inherit the retained upstream-error allowance in [ADR 0008](../adr/0008-retain-upstream-error-payloads.md) and [ADR 0037](../adr/0037-bound-and-clear-local-usage-data.md).

- Never return, log, or persist authorization headers, credential bytes, raw response bodies, response headers, or parser excerpts.
- Return a stable category plus safe metadata: HTTP status when present, attempt/candidate count, elapsed time, and a sanitized endpoint identifier.
- Sanitize logged endpoints by removing userinfo, query, and fragment. Prefer origin only unless a normalized path is needed for diagnostics and has been checked against all known secrets.
- Resolve Credential References and build auth headers only inside the Routing Service. The Control Plane receives no reconstructed credential.
- Keep detailed network-library errors in a protected diagnostic channel only after URL/secret sanitization; the normal result uses categories such as `invalid-endpoint`, `authentication-rejected`, `endpoint-unsupported`, `rate-limited`, `upstream-status`, `timeout`, `dns`, `connect`, `tls`, `cancelled`, and `malformed-response`.
- A cancellation caused by editor closure or supersession is not a provider failure and must not be displayed or recorded as one.

## 3. Minimal Reachability Check

### Pinned algorithm

**Source-derived.** The minimal probe is `GET` against the exact trimmed saved base URL. It sends no authentication and no model request; its only fixed headers are `Accept: */*` and `Accept-Encoding: identity`, plus the saved custom `User-Agent` if valid. It calls `send()` and never reads the body, so elapsed time is response-header TTFB ([probe](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/stream_check.rs#L193-L223)).

The defaults are an 8-second timeout per attempt, one retry, and a 6,000 ms slow/degraded threshold. Only timeout/abort-like failures are retried, immediately and without delay; DNS failure and connection refusal are terminal ([configuration and retry](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/stream_check.rs#L38-L60), [retry loop](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/stream_check.rs#L84-L130)). Any received status, including 401, 403, 404, 429, 500, and 503, is reachable; a slow response may be labeled degraded but remains successful ([result construction](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/stream_check.rs#L225-L279), [status tests](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/stream_check.rs#L387-L454)).

Official/OAuth providers with no saved probe target are skipped rather than guessed. The baseline can resolve a dynamic Copilot endpoint, but otherwise it probes the stored provider base and explicitly does not derive a concrete messages/completions path ([base resolution](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/stream_check.rs#L159-L190), [command behavior](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/commands/stream_check.rs#L16-L44)).

### T03 operation contract

1. Accept an Operator command for one saved Provider/config revision. Reject a missing or non-HTTP(S) target; do not guess an official endpoint.
2. Snapshot and normalize the saved base URL. Do not accept credential material and do not add provider custom auth headers. A custom user agent is the only provider-specific optional header.
3. Send `GET` with `Accept: */*` and `Accept-Encoding: identity`; stop after response headers and drop the body unread.
4. Apply 8 seconds per attempt. Retry once only for a typed timeout or local abort-like transport condition that was not Operator cancellation. Do not retry DNS, connect refusal, TLS, or an HTTP response.
5. Return `reachable`, HTTP status, TTFB milliseconds, timestamp, and retry count. Optionally return a separate `slow` flag at `TTFB > 6000 ms`; do not name it Route Health or Provider Health.
6. On cancellation, abort the in-flight request and return `cancelled` without retry.
7. If history is required, store only a separate Reachability observation containing Provider ID/config revision, redacted endpoint identity, status/category, TTFB, timestamp, and retry count. Do not store a response body or headers.

### Separation invariants

Neither success nor failure may:

- change Route Health, breaker counters, route eligibility, or the current service epoch;
- select or reorder a Failover Chain;
- change Current Target Provider, Serving Provider, Activated Snapshot, or Activated Route Plan;
- write Managed Configuration or trigger Target Takeover;
- create a routed Request Record or usage record; or
- prove that authentication, model availability, or inference works.

CC-Switch saves reachability results to a separate log while explicitly keeping them away from its failover circuit ([single-check command](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/commands/stream_check.rs#L1-L44)). Muxvia may keep equivalent separate history, but `Route Health` remains the passive assessment derived only from real routed requests ([domain definition](../../CONTEXT.md#route-health), [ADR 0031](../adr/0031-reset-route-eligibility-per-service-epoch.md)).

## 4. Verification matrix

T03 tests should pin at least these observable cases:

- exact candidate order for override, full inference URL, plain root, `/v1`, another `/vN`, and each compatibility-suffix class;
- stable de-duplication and same-origin fallback construction;
- fallback on 404/405 only, with no second request for 2xx parse failure, 401/403, 429, 5xx, TLS, DNS, connect, or timeout;
- OpenAI bearer, Anthropic API-key, Anthropic bearer-token, and saved gateway-header profiles without cross-contamination;
- successful empty `data`, unknown fields, missing optional metadata, duplicate IDs, malformed entries, and deterministic ordering;
- bounded Anthropic pagination, including repeated cursors and cancellation;
- error output/log capture proving secrets, auth headers, query, fragment, userinfo, and raw bodies are absent;
- reachability for 200/401/404/429/503, headers-only completion, one timeout retry, no retry for other failures, and cancellation;
- before/after snapshots proving both operations leave Current/Serving provider, Managed Configuration, Activated state, Route Health, circuit counters, and Request Records unchanged; and
- stale-result suppression when an editor closes, its saved config revision changes, or a newer discovery wins.

## 5. True design ambiguities requiring a decision

1. **Explicit refresh inputs.** ADR 0027 fixes automatic discovery to last-saved endpoint and credential and says unsaved edits require explicit refresh, but it does not specify whether that refresh probes the draft endpoint/credential or still uses the persisted snapshot. If drafts are allowed, the RPC needs a deliberate ephemeral-secret boundary; it must not silently persist the draft or place raw secret bytes in logs.
2. **Auth profile representation.** The pinned generic path assumes bearer auth, while official Anthropic requires a declared API-key or bearer-token mode plus `anthropic-version`. T03 needs a schema-level auth/protocol discriminator and a rule for allowed custom discovery headers; URL-based inference is not sufficient.
3. **Anthropic completeness bound.** The official Models API is paginated, but neither ADR 0027 nor the pinned generic baseline defines whether discovery promises all pages. This note recommends bounded exhaustive pagination; the page/item cap and partial-result behavior still need a product decision.
4. **Managed Codex discovery identity.** The pinned adapter sends `originator: cc-switch` and the CC-Switch package version to an undocumented endpoint. ADR 0018 pins Subscription Bridge compatibility, but it does not explicitly decide whether Muxvia must impersonate that discovery identity or send its own. Either choice must be recorded and fixture-tested; changing it is a compatibility decision, not an ordinary refactor.

## Appendix: v3.19.2 Codex Provider Presets and catalogs

The pinned Codex preset catalog lives in the frontend constant `src/config/codexProviderPresets.ts`. Each entry can contain display/partner metadata, auth/config templates, endpoint candidates, API format, and an optional local `modelCatalog`; the array begins with `OpenAI Official` and then release-specific third-party entries ([type and catalog helper](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src/config/codexProviderPresets.ts#L13-L110), [preset array](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src/config/codexProviderPresets.ts#L112-L165)). The form assigns transient IDs from array indexes and copies the selected preset's auth, config, catalog, reasoning, routing, and format values into the editable form ([index identity](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src/components/providers/forms/ProviderForm.tsx#L685-L718), [copy into form](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src/components/providers/forms/ProviderForm.tsx#L1795-L1820)). A separate backend seed table duplicates only the official Codex entry so startup can create the stable `codex-official` record ([official seeds](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/database/dao/providers_seed.rs#L1-L63)).

The pinned file is authoritative evidence of what v3.19.2 shipped, but its exact third-party names, ordering, endpoints, model IDs, partner metadata, and capability/catalog values are not a stable product or provider contract. The index-derived UI IDs and copy-on-selection behavior provide no upstream stability guarantee, and the values are not official provider documentation. Muxvia should treat any imported snapshot as versioned seed data with its own stable keys and provenance. Creating from a preset copies values into an ordinary Provider; later preset changes must not mutate that record, matching Muxvia's copy-on-create [Provider Preset definition](../../CONTEXT.md#provider-preset).
