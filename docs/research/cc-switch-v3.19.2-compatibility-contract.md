# CC-Switch v3.19.2 compatibility and extraction contract

Status: pinned research note for Muxvia v0.1  
Baseline: CC-Switch `v3.19.2`, commit [`43eaf07355af145aebfee301801779e824d4c221`](https://github.com/farion1231/cc-switch/commit/43eaf07355af145aebfee301801779e824d4c221)  
Scope: Codex CLI, Claude Code, local routing, provider selection, and the Codex Subscription Bridge

## Executive contract

Muxvia may copy or adapt selected CC-Switch Rust code under MIT, but any distribution containing copied code or a substantial portion must retain CC-Switch's copyright and permission notice. Muxvia should therefore ship the upstream MIT text in its third-party notices and mark copied/adapted files with their provenance. The license expressly permits use, copy, modification, publication, distribution, sublicensing, and sale subject to that notice condition; it also supplies the upstream warranty disclaimer. ([CC-Switch license](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/LICENSE#L1-L21))

The compatibility baseline is behavioral, not architectural. Preserve externally observable routing and Subscription Bridge protocol behavior with golden/compatibility tests. Do not copy CC-Switch's Tauri process topology, configurable non-loopback listener, generally unauthenticated model routes, shared application database, or failover-induced mutation of the selected provider. Those are intentional Muxvia deviations required by ADRs 0005, 0006, 0010, 0012, and 0015–0019.

One important deviation must be named explicitly: CC-Switch changes its persisted current provider after a failover succeeds; Muxvia defines failover as changing only the Serving Provider while leaving the Current Target Provider unchanged. This is not compatible baseline behavior and needs a `Compatibility Deviation` record plus a regression test.

## Source integrity

The official annotated tag `v3.19.2` resolves to commit `43eaf07355af145aebfee301801779e824d4c221`; the commit itself is the authority when prose differs. ([tag](https://github.com/farion1231/cc-switch/tree/v3.19.2), [commit](https://github.com/farion1231/cc-switch/commit/43eaf07355af145aebfee301801779e824d4c221))

The sibling checkout at `cc-switch-tui/external/cc-switch` was not a valid baseline source during this audit: it was at `98ccde0050f33a1bc8b16b96287a0b6f582c5d12` and did not contain the pinned object. All source links below therefore address the immutable official commit directly. Future extraction must either fetch that exact object or vendor an archive whose digest and tree are recorded; it must not read the sibling checkout or a moving `main` branch implicitly.

## License, attribution, and reuse boundary

This is an engineering provenance rule, not a legal opinion.

### Direct code reuse

Direct copying, translation, or close adaptation of non-trivial implementation should be treated as CC-Switch-derived code. For every such file or extracted module:

1. record upstream repository, tag, commit, and original path in an extraction manifest;
2. retain `Copyright (c) 2025 Jason Young` and the full MIT permission/warranty text in a distributed third-party notice;
3. identify Muxvia modifications without implying upstream endorsement; and
4. keep compatibility tests tied to the pinned commit rather than to upstream `main`.

Good extraction candidates are protocol-pure or routing-pure Rust units: request/response transforms, SSE conversion/parsing, model mapping, header/auth construction, endpoint rewriting, body/content-encoding handling, and selected circuit-breaker logic. The source already separates provider adapters and transform modules from the Axum server, which gives a practical extraction seam. ([provider module map](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/mod.rs#L1-L65), [Codex-to-Anthropic selection](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex.rs#L162-L206))

### Behavioral reimplementation

Muxvia may instead implement the observed contract independently and use CC-Switch only as the pinned oracle for fixtures and golden tests. In that case, do not transplant source structure, comments, or implementation details. Still cite CC-Switch in the compatibility documentation, because Muxvia is making an explicit compatibility claim, and retain exact input/output fixtures with source-path and commit provenance.

### Do not reuse wholesale

Do not import the following as Muxvia architecture:

- Tauri commands, renderer hooks, tray/window lifecycle, or `AppHandle` event coupling;
- `AppState`/`ProxyService` composition, where the database and proxy live in the desktop application process ([AppState](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/store.rs#L1-L21));
- the CC-Switch database path, complete schema, migrations, application settings store, or backup format;
- the configurable listener/address surface;
- the baseline's current-provider side effects during failover; or
- baseline local-route authorization behavior.

## Management plane versus model transport

CC-Switch does not expose a standalone management server. Management calls are Tauri commands that receive application state and call `ProxyService`; start, stop, takeover, status, and configuration operations are all renderer-to-Tauri IPC. ([proxy commands](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/commands/proxy.rs#L1-L84)) Model traffic is separate HTTP traffic handled by an Axum/Hyper router. The HTTP surface includes health/status plus Claude Messages, OpenAI Chat Completions, OpenAI Responses/Compact, and namespaced routes. ([HTTP routes](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/server.rs#L291-L369))

Muxvia contract:

- Preserve the protocol-facing model HTTP behavior required by Codex CLI and Claude Code.
- Replace Tauri IPC with a versioned private Unix-domain management RPC owned by the Routing Service (ADR 0016).
- Never authorize management with a model Routing Credential (ADRs 0012, 0016, 0017).
- Keep TUI concerns out of extracted model transport code (ADR 0005).

This is a deliberate decomposition of the baseline, not a claim that CC-Switch already implements the Muxvia control boundary.

## Routing Service lifetime, binding, and authentication

### Baseline facts

The baseline proxy is an in-process `ProxyService`: `AppState` constructs it with the same `Arc<Database>` used by the rest of the desktop backend. ([AppState construction](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/store.rs#L5-L21)) The Tauri setup opens/migrates the database, creates `AppState`, and attaches the Tauri `AppHandle`. ([startup ownership](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/lib.rs#L525-L620)) On normal desktop exit it stops the proxy and restores taken-over live configuration; persisted takeover flags are used to start/take over again on the next application startup. ([exit cleanup](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/lib.rs#L1813-L1851), [startup restore](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/lib.rs#L1881-L1932)) It is therefore not an independently versioned, long-lived headless service.

The default listener is `127.0.0.1:15721`, but the address is data, not an enforced invariant. `ProxyServer::start` parses and binds the configured address, and takeover URL construction explicitly accounts for `0.0.0.0` and `::`, proving wildcard binding is supported. ([defaults](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/types.rs#L42-L55), [bind](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/server.rs#L94-L120), [wildcard handling](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/proxy.rs#L1476-L1512))

The general Claude Code and Codex handlers parse and route model requests without validating a local-client credential. Only the separately namespaced Claude Desktop gateway explicitly validates a generated bearer token; `/health` and `/status` are also unauthenticated. ([Claude/Codex handlers](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/handlers.rs#L52-L71), [Claude request path](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/handlers.rs#L117-L143), [Codex request paths](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/handlers.rs#L698-L774), [Claude Desktop bearer check](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/handlers.rs#L261-L286))

### Muxvia contract and deviations

- The Routing Service is a separate sidecar process whose lifetime is independent of the Control Plane (ADR 0005). Its RPC and binary upgrade contract must be versioned, with drain-before-handover semantics (ADR 0019); neither property comes from the baseline.
- Bind model HTTP listeners only to loopback and reject wildcard or non-loopback configured addresses rather than merely defaulting to loopback (ADR 0006).
- Authenticate every model route, including status-like model-plane endpoints exposed on the same listener. Use one stable Routing Credential per Target CLI and rotate/revoke by target (ADRs 0012 and 0017).
- Put management on the private Unix socket with a separate control credential or OS-local authorization (ADRs 0012 and 0016).

These changes must be covered by Muxvia security tests and recorded as Compatibility Deviations wherever a baseline fixture would otherwise accept an unauthenticated request or wildcard bind.

## Database ownership and state semantics

### Ownership

CC-Switch uses one `rusqlite::Connection` behind a mutex. The desktop backend opens `~/.cc-switch/cc-switch.db`, creates tables, migrates it, and shares that `Database` with both management code and the proxy. ([database implementation](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/database/mod.rs#L76-L166), [proxy state](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/server.rs#L32-L84))

Muxvia must create its own database, carry forward only required concepts/migrations, and never open or mutate CC-Switch's database (ADR 0010). Only the Routing Service opens and migrates the Muxvia database; the Control Plane goes through local RPC (ADR 0015). This preserves the baseline's useful single-writer effect while changing the process that owns it.

### Current provider

The baseline stores `is_current` on provider rows. `set_current_provider` clears all current rows for an application and sets one target inside a SQLite transaction. ([current DAO](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/database/dao/providers.rs#L111-L128), [transactional setter](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/database/dao/providers.rs#L290-L310)) Runtime selection can nevertheless prefer a device-local settings value and fall back to database `is_current`; the baseline explicitly describes the database value as a default for new devices. ([effective current](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/provider/mod.rs#L2534-L2548), [switch order](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/provider/mod.rs#L3136-L3146))

Muxvia must not reproduce that dual authority. The Routing Service owns exactly one Current Target Provider per Target CLI. The Control Plane and model transport read the same authoritative record through the Routing Service.

### Serving provider and failover

At request start, CC-Switch chooses either only the effective current provider or, when failover is enabled, the ordered failover queue. ([selection](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/provider_router.rs#L32-L108)) During an attempt it writes the attempted provider into in-memory status; after success it writes that provider into an in-memory per-app map. ([attempt/success status](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/forwarder.rs#L468-L506)) If the successful provider differs from the provider at request start, the baseline asynchronously invokes `FailoverSwitchManager`, which hot-switches the provider. ([failover trigger](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/forwarder.rs#L508-L527), [switch manager](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/failover_switch.rs#L74-L133)) The hot switch then commits both local settings and database current-provider state. ([hot-switch commit](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/proxy.rs#L2475-L2613))

Muxvia intentionally separates these meanings:

- Current Target Provider is Operator-selected and unchanged by failover.
- Serving Provider is the provider currently or most recently serving a routed request.
- A request is pinned to its Activated Route Plan epoch.

Therefore baseline failover-induced current mutation must not be copied. Add a compatibility test that demonstrates the upstream behavior and a Muxvia test that asserts the documented deviation: after fallback serves successfully, `current == original` and `serving == fallback`.

### Activation and takeover

Baseline normal switching is eager: it updates device-local current, updates database current, writes the selected provider to the live Target CLI files, and then best-effort reprojects MCP settings. These are ordered operations, not one atomic database-and-filesystem activation. ([normal switch](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/provider/mod.rs#L3087-L3146), [post-write semantics](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/provider/mod.rs#L3226-L3237)) Editing the effective current provider also writes live configuration immediately. ([current edit](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/provider/mod.rs#L2749-L2824))

Takeover has a useful safety ordering worth reproducing behaviorally: start the proxy, inspect/rebuild stale takeover state, create a restore backup, synchronize live credentials, write takeover configuration, and only then set the per-app `enabled` flag. During that activation window, the backup or live placeholders—not just `enabled`—signal takeover ownership. On disable, restore live configuration before deleting the backup and clearing `enabled`; stop the proxy only when no target remains taken over. ([takeover transaction ordering](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/proxy.rs#L736-L860), [disable ordering](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/services/proxy.rs#L863-L923))

Muxvia should preserve the crash-safety intent but not the eager activation model. Provider edits change declarative records only; explicit Apply/Activate creates an immutable Activated Snapshot/Activated Route Plan and commits Managed Configuration only after validation. That is a Muxvia domain deviation and should not be hidden behind CC-Switch-compatible naming.

## Codex Subscription Bridge contract

The bridge is compatibility work against undocumented interfaces, not an official OpenAI provider integration. CC-Switch itself calls the feature a reverse-engineered OAuth path and warns of terms/account/continuity risk. ([upstream risk notice](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/docs/release-notes/v3.13.0-en.md#L367-L379)) Muxvia public documentation must retain an equally clear statement (ADR 0018).

### Device Authorization

The baseline contract is:

1. POST the fixed client ID to `https://auth.openai.com/api/accounts/deviceauth/usercode` and display `https://auth.openai.com/codex/device` with the returned user code. ([endpoints and client identity](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L30-L58), [start request](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L256-L318))
2. Best-effort copy the code and best-effort open the remote verification URI; failure of either does not stop polling. Cancellation/unmount clears only local timers. ([renderer flow](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src/components/providers/forms/hooks/useManagedAuth.ts#L44-L82), [poll cancellation](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src/components/providers/forms/hooks/useManagedAuth.ts#L85-L126))
3. Poll the remote device-token endpoint with `device_auth_id` and `user_code`. HTTP 403/404 means pending; 410 means expired. Success returns both `authorization_code` and the server-supplied `code_verifier`. ([poll protocol](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L321-L388))
4. Exchange that code and verifier at the fixed OAuth token endpoint with the remote HTTPS redirect URI `https://auth.openai.com/deviceauth/callback`; do not start a local callback listener. ([exchange](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L424-L456))
5. Require a refresh token, derive account identity from token claims, keep access tokens only in memory, persist refresh tokens by account, and refresh access tokens before expiry. ([account/token handling](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L396-L421), [refresh behavior](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L494-L563))

The effective polling interval contains two safety additions in v3.19.2: backend parsing returns `max(server_interval, 1) + 3` seconds (or `8` seconds when absent), and the renderer then applies `max(returned_interval + 3, 8)`. Thus a normal server interval of 5 seconds is polled every 11 seconds. Preserve this effective behavior in compatibility tests unless it is intentionally declared a deviation. ([backend interval](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/codex_oauth_auth.rs#L900-L908), [renderer interval](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src/components/providers/forms/hooks/useManagedAuth.ts#L80-L83))

Muxvia must not import native Codex `auth.json`, accept pasted refresh tokens, or substitute local PKCE for this flow. Cancel means stop local polling only; it must not claim remote revocation (ADR 0018).

### Per-request bridge behavior

A provider may bind a specific managed Codex account; otherwise each request resolves the then-current default account. The forwarder obtains/refreshed the access token just before forwarding, adds bearer authorization and `ChatGPT-Account-Id`, and adds session identity headers only when the session ID came from the client rather than a generated fallback. ([account resolution](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/forwarder.rs#L1675-L1789), [session headers](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/forwarder.rs#L3208-L3228))

The real upstream is `https://chatgpt.com/backend-api/codex`; the bridge presents an Anthropic-facing provider and converts Anthropic Messages to OpenAI Responses. For this upstream it forces stateless reasoning continuity fields such as `store: false` and inclusion of encrypted reasoning content. ([upstream constant](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/mod.rs#L43-L48), [transform contract](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/transform_responses.rs#L284-L300)) The auth adapter also sends the baseline client identity pair `originator: codex_cli_rs` and `version: 0.144.1`; both are compatibility-sensitive. ([identity constants](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/claude.rs#L27-L32), [headers](https://github.com/farion1231/cc-switch/blob/43eaf07355af145aebfee301801779e824d4c221/src-tauri/src/proxy/providers/claude.rs#L900-L913))

Extract the device/token protocol, transform, header, identity, account-binding, and refresh behavior behind a Routing Service adapter. Do not copy the `AppHandle` lookup used by the baseline forwarder; inject an account/token resolver interface instead. Keep upstream endpoint and fingerprint values in compatibility fixtures so changes require an explicit baseline review.

## ADR reconciliation

| ADR | Result against v3.19.2 |
| --- | --- |
| 0005 — owned derived routing core | Compatible with MIT extraction. Headless sidecar and removal of Tauri lifecycle are intentional architectural deviations. |
| 0006 — local machine only | Baseline defaults to loopback but permits wildcard/non-loopback configuration. Muxvia must enforce loopback; deviation required. |
| 0009 — pinned baseline | Confirmed exact tag/commit. Fixed-source golden tests and an extraction manifest are required. |
| 0010 — independent database | Required deviation. Reuse selected concepts/migrations only; never open the CC-Switch database. |
| 0012 — authenticate all model routes | Baseline generally does not authenticate Codex/Claude local routes. Muxvia hardening deviation required. |
| 0015 — Routing Service owns DB | Baseline has a useful single shared connection but it is owned inside the Tauri backend. Move ownership to the sidecar and expose RPC only. |
| 0016 — management/model split | Baseline separates Tauri IPC from HTTP transport, but not through a standalone authenticated UDS. Preserve the conceptual split and replace both boundaries. |
| 0017 — credential per target | Not baseline behavior. Implement separate stable Codex/Claude Routing Credentials as a security deviation. |
| 0018 — pinned Subscription Bridge | Compatible if the device flow, effective polling interval, server-returned verifier, refresh, endpoint, conversion, request identity, dynamic default binding, and risk disclosure are fixed in tests. |
| 0019 — version and drain upgrades | No equivalent baseline contract. Separate-process version negotiation and stream draining are Muxvia-only requirements. |

There is no ADR that must be reversed. The one semantic conflict requiring special attention is baseline failover mutating current-provider state versus Muxvia's Current/Serving split; ADR 0009 permits this only when it is documented and tested as a Compatibility Deviation.

## Required implementation evidence

Before claiming CC-Switch v3.19.2 compatibility, Muxvia should have:

- an extraction manifest mapping every copied/adapted file to commit and original path;
- a distributed third-party notice containing the upstream MIT notice;
- golden tests for supported Codex/Claude request and streaming-response transforms;
- device-flow fixtures covering pending/expired/success, the 11-second default effective poll cadence, server-returned verifier, token refresh, fixed/default account binding, and cancellation without revocation;
- transport tests proving loopback-only binding and rejection of missing/wrong per-target Routing Credentials;
- RPC tests proving model credentials cannot perform management calls;
- state tests proving the Routing Service is the sole database opener/migrator;
- takeover tests for backup-before-write, activation failure rollback, and restore-before-backup-delete; and
- the explicit failover deviation test: fallback updates Serving Provider but not Current Target Provider.
