# Claude Code Takeover Design

**Issue:** [#6 — T05 Claude Code Takeover and Messages ingress](https://github.com/HaroldHuanrongLIU/muxvia/issues/6)

**Status:** Approved ticket with the remaining Provider-authentication checkpoint resolved on 2026-08-15: Claude Target Providers explicitly choose `anthropic-api-key` or `anthropic-bearer`.

## Goal

Make Claude Code a real, independent Target CLI context. The Operator can manage Claude Target Providers, enable Target Takeover, and route Anthropic Messages and token-counting traffic through a Claude-only authenticated loopback endpoint while Codex state and traffic remain unchanged.

## Scope

T05 delivers:

- a real `claude` target in RPC, persistence, subscriptions, recovery, activation, runtime, and the Control Plane;
- Claude Target Provider CRUD, ordering, duplication, Model Discovery, and Reachability with the T03 invariants;
- Claude Target Takeover using only approved `settings.json` paths;
- independent Claude Current, Serving, Activated Snapshot, Routing Credential, listener, recovery state, and view sequencing;
- Anthropic Messages and token-counting ingress, including SSE, tools, compatible unknown fields, upstream errors, headers, and cancellation;
- startup recovery and detached Routing Service lifetime while either target has an active takeover; and
- real-process tests proving the complete Claude path and Codex/Claude isolation.

T05 does not deliver Claude Direct Activation, Takeover removal, Drift reconciliation actions, Route Health evaluation/circuit breaking beyond a neutral target-isolated projection, Universal Providers, Failover Chains, Subscription Accounts, the Subscription Bridge, Request Records, usage, import/export, backup/restore, or release distribution. Those remain in their approved later tickets.

## Alternatives considered

### 1. Duplicate the Codex implementation for Claude

This is initially direct, but it duplicates recovery ordering, filesystem race handling, target state, lifecycle, and streaming behavior. The two copies would drift in exactly the security-sensitive paths that must remain equivalent. Rejected.

### 2. Normalize Responses and Messages into one generic LLM schema

This appears reusable, but it would make unknown fields, tools, Anthropic beta/version behavior, upstream errors, and SSE event evolution depend on Muxvia's invented schema. It also anticipates the later Subscription Bridge. Rejected.

### 3. Share target orchestration and transport mechanics; keep target adapters native

This is the selected design. Target identity, state transactions, recovery ordering, listener lifetime, streaming mechanics, and Control Plane workflows become target-aware. Codex TOML, Claude JSON, capability probes, routing authentication, upstream authentication, and protocol paths remain target-specific adapters.

## Module and seam design

The existing target-scoped RPC remains the external interface:

```text
TargetSession(target)
        │ private bounded UDS RPC
        ▼
Target-scoped coordinator
        ├── target-scoped StateStore transactions
        ├── Target Configuration seam
        │     ├── Codex TOML adapter
        │     └── Claude JSON adapter
        └── Target Model Endpoint
              ├── Codex Responses adapter
              └── Claude Messages adapter
```

Callers continue to learn `openTarget`, `get`, `act`, inspections, and subscriptions. SQLite, recovery payloads, file identities, configuration formats, credentials, listener handles, and model protocol details stay behind that interface.

### Shared Managed File seam

Codex and Claude are now two real adapters for one internal filesystem seam. The shared module owns regular-file reads, canonical parent-directory handles, no-follow checks, identity comparison, same-directory temporary files, mode preservation, no-replace/exchange commit, directory durability, rollback verification, and safe displaced-file retention.

The seam does not parse TOML or JSON. Codex retains TOML ownership semantics; Claude retains JSON ownership semantics. Existing adversarial Codex tests move to or continue exercising the shared filesystem behavior before Claude uses it.

### Target Configuration adapters

Each adapter provides target-specific behavior for:

- capability probing and preflight;
- observing the currently committed managed state;
- building the desired Takeover state from an immutable Activated Snapshot and route endpoint;
- atomically applying and verifying owned fields;
- restoring exact prior values or absence; and
- reconciling a pending Recovery Intent on startup.

The activation coordinator owns ordering, receipts, final revision checks, publication, and rollback state. It must not know TOML tables, JSON paths, or target-specific credentials.

## Target identity and persistence

`Target` has exact wire values `codex` and `claude`. Every Provider, Credential Reference, Target Route State, Target Problem, Activated Snapshot, Action Receipt, and Recovery Intent is associated with one target.

Schema v4 transactionally rebuilds the tables whose existing `CHECK` constraints allow only Codex. Existing v3 rows, secrets, file identities, snapshots, pending recovery, and historical receipt projections migrate as Codex data. The migration creates a clean Claude Target Route State and validates foreign keys before commit.

Action receipts and Recovery Intents become target-scoped. Receipt identity and recovery action identity are both `(target, action_id)`, so reusing the same UUID against another target cannot replay an outcome, authoritative view, or recovery row from the first target. Provider IDs and snapshot IDs remain globally unique UUIDs.

Provider declarations gain:

- protocol `openai-responses` or `anthropic-messages`; and
- authentication profile `openai-bearer`, `anthropic-api-key`, or `anthropic-bearer`.

Codex Providers remain `openai-responses` plus `openai-bearer`. Claude Providers remain `anthropic-messages` plus one of the two explicitly selected Claude profiles. The server validates these target/protocol/authentication combinations; names and URLs never infer them.

Claude receives a copy-on-create `anthropic-api-messages` preset using the official Messages base path and `anthropic-api-key`. The Provider editor can explicitly switch a Claude Provider between API-key and Bearer authentication. Provider views and receipts expose only the profile enum and credential presence, never secret bytes.

Provider ordering, revision guards, Incomplete Provider rules, explicit Credential Reference reuse, declaration-only edits, Model Discovery, and Reachability retain the T03 behavior independently for each target.

Each target also projects its own neutral Route Health state. T05 initializes Claude Route Health to `unobserved` and never derives a circuit state from Model Discovery, Reachability, configuration probes, or editor activity. Reachability cannot mutate Route Health. T09 owns request-derived health evaluation, circuit transitions, and stale-epoch behavior; T05 only establishes the target-isolated neutral projection required for that later behavior.

## Target-scoped RPC and Control Plane context

A UDS connection opens exactly one target. Reopening the same target is allowed for gap refresh; operations naming another target are rejected before work. Target View pushes are filtered to the opened target.

`TargetSession` captures its target once and uses it for actions, inspections, refresh, and push filtering. The Control Plane opens independent Codex and Claude sessions over independent RPC clients. Closing one target session never closes the other.

Some preflight facts belong to the current Control Plane invocation rather than an older detached Routing Service process. `open-target` therefore carries a small, secret-free configuration context instead of a copied environment:

- the observed `CLAUDE_CONFIG_DIR`, if set;
- normalized states for the five documented Claude provider selectors;
- whether host-managed provider mode is observable; and
- the current working directory used to locate observable shared/local project settings.

The action uses the context attached to its target session. A normalized `unknown-nonempty` selector value is allowed on the read-only session but blocks Takeover before side effects; only invalid wire states are rejected at session open. The complete environment is never serialized or stored. Independently launched Claude flags, `/model`, resumed transcript state, and environment remain explicitly unobservable and are not falsely reported as controlled.

## Claude Provider inspection

Claude Model Discovery uses the saved endpoint/credential once when an existing editor mounts, or the explicit draft only after Operator refresh. It never starts merely because the Operator types.

The inspector derives same-origin `/models` candidates from the normalized Messages base path, follows bounded Anthropic pagination, uses the Provider's explicit authentication profile, injects the fixed documented `anthropic-version: 2023-06-01`, enforces the existing body/model caps, and preserves cancellation. API-key Providers send `x-api-key`; Bearer Providers send `Authorization: Bearer`; both send the version header on every pagination request. Discovery remains advisory and never mutates Current, Serving, Managed Configuration, Activated Snapshot, or Route Health.

Reachability remains an unauthenticated headers-only observation and does not become routing evidence.

## Claude Managed Configuration

The only owned `~/.claude/settings.json` paths are:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:<claude-port>",
    "ANTHROPIC_AUTH_TOKEN": "<claude-routing-credential>",
    "ANTHROPIC_MODEL": "<activated-snapshot-model>"
  }
}
```

Muxvia never owns the complete `env` object, top-level `model`, `ANTHROPIC_API_KEY`, Provider selectors, `~/.claude.json`, Claude application authorization state, or OS credential stores.

The JSON adapter requires an object root and an object `env` when present. It records each owned value or prior absence, preserves all unrelated semantic values and file mode, and writes atomically through the Managed File seam. JSON whitespace is not a compatibility promise.

The Configuration Home directory may be a symlink and is canonicalized. A symlinked `settings.json`, a nondefault `CLAUDE_CONFIG_DIR`, invalid JSON, unsafe file identity change, incompatible capability, or observed shadow fails before a write.

Preflight detects these provider-mode variables in settings and the normalized Control Plane context:

- `CLAUDE_CODE_USE_BEDROCK`
- `CLAUDE_CODE_USE_VERTEX`
- `CLAUDE_CODE_USE_FOUNDRY`
- `CLAUDE_CODE_USE_MANTLE`
- `CLAUDE_CODE_USE_ANTHROPIC_AWS`
- `CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST`

`1` and `true` are active; `0`, `false`, and empty are inactive/unset according to the documented setting behavior. Any other non-empty value fails closed. Muxvia reports these selectors and never clears them.

Observable managed, shared-project, and local-project settings that override an owned path block Takeover with the source identified. Unobservable invocation state is disclosed but cannot be used to claim guaranteed effective routing.

The read-only probe uses the installed `claude` version/help surfaces and never performs inference, login, Model Discovery, or a repair-capable interactive command. Tested versions proceed, unknown-compatible versions proceed with a persisted secret-free warning, and incompatible versions fail before intent/listener/credential/file mutation.

Every successful write sets `restartRequired: true`. Muxvia does not promise hot reload for a running Claude Code process.

## Activation, recovery, and runtime isolation

Each target has its own serialized activation gate and model-runtime slot. Claude Takeover executes:

1. receipt-first replay lookup for the Claude target;
2. revision, Provider completeness, protocol/authentication, home, selector, shadow, capability, and managed-state validation;
3. exact Claude loopback listener reservation;
4. reuse or generation of the stable Claude Routing Credential;
5. immutable Claude Activated Snapshot creation;
6. durable Claude Recovery Intent insertion;
7. JSON apply and reread verification;
8. final revision check and one database transaction for Current, Snapshot, Takeover, receipt, and committed recovery; and
9. one complete Claude Target View publication.

The reserved listener, newly generated Routing Credential, and Activated Snapshot are provisional activation-owned candidates until the final commit. The listener does not serve, the credential and snapshot are not persisted, and dropping the activation scope releases them. Every failure between listener reservation and durable intent insertion releases the listener and candidate secrets without changing persisted state or configuration. Fault injection covers each boundary between reservation, credential generation, snapshot construction, and intent insertion.

Any failure after intent insertion but before the final database transaction commits compares the file against exact before/desired states, restores and verifies when needed, and commits rolled-back state. A third state or unverifiable rollback marks only Claude as Recovery Required. The reserved listener and runtime are fully ready before the commit, making the final ownership handoff an infallible move. Once the database transaction commits, its successful receipt and configuration are authoritative and are never rolled back because a subscriber disappeared or publication failed; clients can refresh the committed view, and replay remains side-effect free. Codex actions and a healthy Codex route remain available.

Claude Direct Activation is rejected at the server action boundary before capability probing, intent insertion, listener reservation, credential/snapshot construction, database mutation, or file access. T06 owns that mode.

Claude Provider switching during Takeover reuses the Claude endpoint and Routing Credential, creates a new immutable snapshot, and affects only the next new Claude request. An in-flight stream retains its starting snapshot. The two targets never share listener ports, Routing Credentials, Current, Serving, snapshots, recovery, or view sequence.

On process startup, each clean committed takeover is reconciled and bootstrapped independently. If the committed Claude takeover's owned settings no longer match the committed snapshot, startup records target-scoped Configuration Drift, performs no write, and starts no Claude listener; T07 owns Adopt/Reapply/Restore actions. A target in Recovery Required or Configuration Drift remains control-only while the other target may resume. The Routing Service stays alive while either target has a committed takeover or unresolved recovery/drift state and exits only when neither target needs routing or control-only recovery and the final control work/session is drained.

## Claude Messages endpoint

The Claude listener binds only IPv4 loopback and exposes:

- `POST /v1/messages`, including query strings such as `?beta=true`; and
- `POST /v1/messages/count_tokens`.

It does not expose WebSockets, Chat Completions, Responses, compact endpoints, or gateway Model Discovery.

Before reading the body, snapshot secrets, or upstream state, the listener validates exactly one `Authorization: Bearer <Claude Routing Credential>`. Missing, wrong, duplicate, malformed, short, long, non-ASCII, and Codex cross-target credentials return the same generic 401 and make zero upstream calls. The Codex endpoint likewise rejects the Claude credential.

The inbound Routing Credential and inbound `x-api-key` are consumed, never forwarded. The Activated Snapshot's explicit authentication profile injects either:

- `x-api-key: <Provider credential>` for `anthropic-api-key`; or
- `Authorization: Bearer <Provider credential>` for `anthropic-bearer`.

The adapter removes hop-by-hop, framing, host, and Connection-nominated headers. It preserves `anthropic-version`, all `anthropic-beta` values, correlation headers, future `anthropic-*`/`x-claude-code-*` end-to-end headers, the request query, and other safe headers.

The request is pinned to one immutable snapshot at start. The adapter owns the top-level `model` value so a hot switch can route the next request to the new snapshot model. It preserves messages, system order, tools, tool choice, thinking, metadata, context/output configuration, and compatible unknown JSON fields without introducing a generic LLM schema.

Owning the top-level model requires bounded request parsing. Messages and token-counting bodies are buffered to at most 32 MiB after content decoding, matching Anthropic's documented endpoint limit. Identity and gzip/x-gzip request bodies are accepted; gzip is decoded with the same 32 MiB output bound, then forwarded as rebuilt identity JSON with stale entity/framing headers removed. Unsupported or stacked request encodings return a fixed local 415 with zero upstream calls. A declared, encoded, or decoded body over the limit returns a fixed local 413 with zero upstream calls. Invalid JSON or a non-object top level returns a fixed local 400 with zero upstream calls. A valid object has its top-level `model` inserted or replaced from the pinned snapshot while every other compatible field is preserved. Downstream cancellation while buffering stops the read and makes no upstream call. Automatic Reqwest decompression stays disabled; response bodies, including compressed non-stream bodies, compressed errors, and SSE, preserve Content-Encoding and stream bytes without buffering.

Non-stream status, safe headers, body, and non-2xx upstream error payloads pass through. SSE bytes are forwarded with backpressure without parsing, buffering, event reordering, or wrapping mid-stream errors. Downstream disconnect drops request upload or the upstream response stream and never starts a detached pump.

Serving changes only after a successful Claude 2xx response head is obtained. The transaction records the Provider from the request's pinned snapshot, advances only Claude's view sequence, and publishes only the Claude Target View. Observation failure never changes an already committed upstream response.

## Control Plane behavior

Claude no longer renders an unavailable preview. Home opens a real Claude context using the same OpenCode-style Target View, Provider overlays, activity stream, prompt, responsive folding, and contextual sidebar as Codex.

Provider CRUD, Model Discovery, Reachability, and Takeover commands are available in both target scopes. Claude does not expose `/direct` or Direct picker actions in T05. Command availability, editor state, selected Provider, pending actions, overlays, notices, and activity entries are bound to the originating target/session so switching context cannot dispatch against another target.

All visible Claude strings use the English and Simplified-Chinese catalogs. Restart guidance names Claude Code. Known backend codes map to stable localized messages; backend message text and secrets never render.

## Error and security rules

- No credential, settings snapshot, raw recovery payload, inbound/outbound authorization header, or raw backend message appears in Target Views, receipts, Debug, logs, activity entries, test diagnostics, or renderer frames.
- Configuration/preflight failures happen before intent, listener, Routing Credential, snapshot, or file mutation wherever the failure is knowable before mutation.
- Provider credentials and Routing Credentials remain separate secrets with separate storage and header placement.
- Model-plane credentials never authorize UDS management.
- The implementation does not use Provider names or URLs to infer protocol, authentication, ownership, or target.
- Tests use temporary user/Muxvia homes, fake CLIs, deterministic loopback upstreams, and fixed redacted failure diagnostics. The Operator's real Claude/Codex files are never opened.

## Verification strategy

### Pure and storage tests

- schema-v3 to v4 migration, receipt migration, pending recovery migration, and foreign-key rollback;
- target-scoped Provider CRUD, ordering, idempotency, revision, Current/Serving, problems, snapshots, and recovery;
- protocol/authentication combination validation and target-specific Presets;
- JSON merge/restore, prior absence, invalid shapes, selectors, shadows, mode preservation, symlinks, identity races, restrictive umask, and secret-safe diagnostics;
- Claude probe tested/unknown-compatible/incompatible behavior.

### Real UDS and loopback tests

- one-target-per-session enforcement and target-filtered pushes;
- independent sessions, same action UUID isolation, inspection cancellation, and gap refresh;
- independent listeners and cross-target credential rejection before upstream;
- API-key/Bearer upstream injection and credential stripping;
- Messages/count_tokens tools, unknown fields, query, headers, errors, SSE order, and cancellation;
- pinned snapshots under hot switch and target-only Serving publication;
- startup reconciliation, Recovery Required isolation, detached lifetime, and two-listener shutdown.

### OpenTUI and process tests

- real Claude Provider workflows, command identity, focus/overlay behavior, localization, and extreme terminal sizes;
- temporary `settings.json` exact semantic and mode assertions with unrelated settings preserved;
- no mutation of Codex configuration/state during Claude operations;
- Control Plane exit while Claude routing continues;
- service restart restoring the same Claude endpoint/credential/snapshot; and
- scan-first secret checks across RPC frames, views, receipts, renderer frames, process output, configuration diagnostics, and captured upstream requests.

## Compatibility evidence

Golden fixtures record whether their oracle is current official Claude/Anthropic documentation or CC-Switch v3.19.2 commit `43eaf07355af145aebfee301801779e824d4c221`, plus source URL/path, retrieval date, behavior proved, and fixture hash. Muxvia explicitly records deviations where the baseline lacks local route authentication, lacks token counting, or rewrites version/beta/error behavior that Muxvia must preserve.
