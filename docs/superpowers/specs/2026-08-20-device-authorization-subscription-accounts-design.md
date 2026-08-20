# Device Authorization and Subscription Accounts Design

Status: approved for T11 on 2026-08-20

Issue: [#12 — T11 Device Authorization and Subscription Accounts](https://github.com/HaroldHuanrongLIU/muxvia/issues/12)

## Context

T11 lets the Operator authorize and manage multiple Codex Subscription Accounts before the Subscription Bridge consumes them in T12. The design follows `CONTEXT.md`, ADR 0007, ADR 0018, ADR 0034, ADR 0035, ADR 0044, ADR 0046, and the source-derived CC-Switch v3.19.2 compatibility contract.

The Routing Service owns the fixed remote protocol, refresh-token persistence, account identities, defaults, and Provider bindings. The Control Plane owns terminal effects and presentation. Refresh tokens, authorization codes, server-returned verifiers, and access tokens never cross the control protocol.

This compatibility behavior depends on undocumented interfaces and is not an officially supported OpenAI integration.

## Scope

T11 delivers:

- the pinned remote Device Authorization start, poll, code exchange, and refresh behavior;
- multiple Subscription Accounts with one default and persistent Needs Reauthorization state;
- an independent Subscription Account session over the control socket;
- Provider bindings as Fixed account identity or Follow Default metadata;
- default-change preview listing every affected Follow Default Provider;
- deletion that retains dangling Fixed bindings without identity substitution;
- reauthorization that requires and preserves the original account identity;
- an OpenTUI account workflow with best-effort clipboard/browser effects; and
- real-process, private-file, restart, cancellation, and secret-redaction evidence.

T11 does not add the Claude Code Subscription Bridge model route, Messages-to-Responses conversion, request identity headers, account resolution on model requests, Provider-level failover using account failures, native Codex auth import, pasted refresh tokens, local callbacks, or alternate PKCE. Those runtime behaviors remain T12.

## Binding Decisions

1. Subscription Accounts live behind an independent global `SubscriptionAccountSession`, not a TargetSession.
2. The Routing Service owns remote protocol state. The public challenge contains only a generated flow identity, user code, fixed verification URL, expiry, and final effective polling interval.
3. The Control Plane attempts to copy the user code and open the fixed HTTPS verification URL. Either failure is nonfatal. It owns the polling loop and local cancelled state.
4. A poll crosses the control seam using only the generated flow identity. The remote device identity, authorization code, and server-returned verifier remain private to the Routing Service.
5. Provider bindings are optional SQLite metadata: `Fixed(account identity)` or `Follow Default`. T11 persists and displays them; T12 is the first ticket that consumes them for routed requests.
6. Deleting an account does not rewrite Fixed bindings. A dangling binding is projected explicitly and never resolves to the default account.
7. Reauthorization names the account being repaired. The returned token identity must match exactly before the refresh token, metadata, or status is replaced.
8. The separate account file retains the compatible v1 accounts/default shape and adds a closed authorization state. Refresh tokens remain private; access tokens remain memory-only.

## Compatibility Contract

Production uses the pinned constants:

- client identity `app_EMoamEEZ73f0CkXaXp7hrann`;
- `POST https://auth.openai.com/api/accounts/deviceauth/usercode`;
- `POST https://auth.openai.com/api/accounts/deviceauth/token`;
- `POST https://auth.openai.com/oauth/token`;
- verification URL `https://auth.openai.com/codex/device`;
- redirect URI `https://auth.openai.com/deviceauth/callback`; and
- User-Agent `cc-switch-codex-oauth`.

The start request sends only the fixed client ID. The poll request sends the private remote device identity and user code. HTTP 403 and 404 are Pending; 410 is Expired. A successful poll must contain both `authorization_code` and server-returned `code_verifier`. Exchange uses that exact verifier and requires a refresh token. There is no locally generated verifier or callback listener.

The final effective polling interval preserves both baseline safety additions: backend parsing computes `max(server interval, 1) + 3` seconds, or 8 seconds when absent; the client interval then computes `max(backend interval + 3, 8)`. A normal server interval of 5 seconds is therefore 11 seconds.

Identity is derived from the ID-token claims using the baseline precedence and is never accepted from a client. Access tokens are cached only in memory and treated as expiring 60 seconds before their reported expiry. HTTP 401/403 from refresh permanently marks the account Needs Reauthorization; transient failures do not change persistent identity or authorization state.

## Deep Modules and Interfaces

### Subscription Account Session

The external seam is one global session:

```text
MuxviaControl.openSubscriptionAccounts() -> SubscriptionAccountSession

SubscriptionAccountSession.get()
SubscriptionAccountSession.startAuthorization(accountId?)
SubscriptionAccountSession.pollAuthorization(flowId, signal?)
SubscriptionAccountSession.previewDefault(accountId)
SubscriptionAccountSession.act(action)
SubscriptionAccountSession.subscribe(listener)
SubscriptionAccountSession.close()
```

`startAuthorization()` without an account identity creates a new-account flow. Supplying one starts reauthorization. `pollAuthorization()` returns Pending, Expired, or Authorized and is cancellable at the request seam. Cancellation does not mutate the remote authorization or claim revocation.

Closed account actions are set default, bind Provider Fixed, bind Provider Follow Default, delete account, and confirm a default change from an exact preview. Session actions use action UUIDs, exact catalog revision, call-time deep capture, receipt-first replay, response-before-one-push, and authoritative replacement.

### Device Authorization module

A crate-private Device Authorization module hides remote requests, response parsing, effective interval calculation, pending-flow memory, identity extraction, code exchange, access-token cache, and refresh coordination. Its small interface accepts start/poll/refresh inputs and returns typed secret-owning results.

Production HTTP and deterministic loopback authority are the two adapters at its internal transport seam. Endpoints are fixed in production and injectable only through crate-private construction.

### Subscription Account Store

The account store owns one private JSON file beneath Muxvia Home. It validates the complete document before replacement, writes a new 0600 file in the same directory, flushes, atomically renames, and reasserts private permissions. Debug and errors expose only fixed categories.

The compatible document contains version 1, a map keyed by account identity, account identity, optional email, refresh token, authenticated time, authorization state, and optional default account identity. Missing state in a compatible legacy document defaults to Authorized. Unknown versions or mismatched map/record identities fail closed.

Provider bindings and account action receipts remain in SQLite because they belong to Provider declarations and control idempotency, not the credential file. Account file mutations use a durable SQLite intent with before/desired fingerprints so restart completes or restores the file before the control socket opens.

## Provider Bindings

Every existing Target Provider may carry zero or one Subscription Account binding as inert metadata in T11:

- Fixed stores an account identity and projects `available`, `needs-reauthorization`, or `missing`;
- Follow Default stores no account identity and projects the current default identity/status dynamically; and
- no binding projects `none`.

This metadata does not replace ordinary Provider credentials or change activation/routing in T11. T12 will introduce the account-backed Provider declaration and consume only this closed binding.

Changing the default first returns a revision-bound preview listing every Follow Default binding in deterministic Target/Provider order and its old/new resolved identity. Confirm applies only when the catalog and preview are still current. Fixed bindings are never included.

Deleting an account is allowed. If it was default, the deterministic fallback is the most recently authenticated remaining account, then lexical identity. Fixed bindings remain and project missing. Follow Default bindings resolve the new default or no account.

## Transaction and Recovery Ordering

Every action follows:

1. check an existing receipt before parsing the raw action;
2. acquire the account catalog mutation gate;
3. recheck receipt and catalog revision;
4. parse and validate account/provider identities and any default preview token;
5. for file mutations, persist a recovery intent containing secret-free fingerprints and private before/desired payloads;
6. atomically replace and verify the private account file;
7. commit SQLite binding/receipt/catalog state and mark the recovery intent committed;
8. write the response; and
9. publish at most one newer account view.

Failure after file replacement restores and verifies the exact prior file. An unverifiable third state fails closed as account Recovery Required and blocks later account writes without blocking unrelated Target reads or routing.

## Control Plane

One global command opens the Subscription Accounts overlay from either Target context. The overlay lists redacted identity/email, default, authorization state, and binding counts. It supports add, reauthorize, set-default preview/confirm, bind, and delete.

The Device Authorization overlay displays the user code and verification URL, attempts clipboard and browser effects once, then polls at the returned final interval. Pending remains visible. Expired offers restart. Authorized returns to the catalog. Cancel aborts the in-flight poll and timer, records a local Cancelled activity, and makes no revocation claim.

Clipboard and browser launching sit behind a Control Plane platform-effects seam. Production uses OSC 52 clipboard output and the platform opener (`open` on macOS, `xdg-open` on Linux). Failures are represented only as nonfatal fixed guidance.

Pending account mutations make the originating overlay nondismissible and suppress duplicate dispatch. Async results remain bound to the originating account session/generation. English and Simplified Chinese share one key set and render at every supported size.

## Stable Problems

- `device-authorization-pending`
- `device-authorization-expired`
- `device-authorization-failed`
- `device-authorization-identity-mismatch`
- `subscription-account-not-found`
- `subscription-account-needs-reauthorization`
- `stale-subscription-catalog-revision`
- `stale-default-account-preview`
- `invalid-subscription-binding`
- `subscription-account-recovery-required`
- existing `state-store-error`, `invalid-response`, `cancelled`, and `connection-closed`

Remote bodies, device identities, authorization codes, verifiers, refresh tokens, access tokens, JWTs, Provider credentials, SQLite values, and raw account-file content never enter public messages, views, receipts, activities, logs, or Debug output.

## Confirmed Testing Seams

1. Rust/TypeScript/JSON-schema protocol fixtures and a real UDS Subscription Account session.
2. A deterministic loopback Device Authority plus real private account JSON and real SQLite.
3. SubscriptionAccountSession through the real OpenTUI renderer and platform-effects adapter.
4. A real-process walking tracer using real UDS, SQLite, private files, renderer, restart, and permission checks.

Tests cover exact baseline requests and intervals; pending/expired/success/cancel; server verifier use; identity extraction; atomic 0600 persistence; no access-token persistence; default preview; Fixed/Follow Default/dangling behavior; permanent refresh rejection; same-identity reauthorization; restart recovery; response/push/replay ordering; and scan-first controlled secret mutations.
