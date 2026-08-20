# Device Authorization and Subscription Accounts Implementation Plan

**Goal:** Implement GitHub issue #12 with the pinned remote Device Authorization flow, private multi-account persistence, defaults, Provider bindings, Needs Reauthorization, and a terminal account workflow.

**Architecture:** Add an independent SubscriptionAccountSession. A crate-private Device Authorization module owns the fixed remote protocol and memory-only access tokens. A private account-file module owns atomic 0600 JSON. SQLite owns Provider bindings, recovery intents, catalog revisions, and receipts. T12 will consume the bindings for model routing.

**Tech stack:** Rust 2024, Tokio, reqwest/rustls, SQLite, serde/serde_json, Bun, TypeScript, Zod, SolidJS/OpenTUI, real UDS/loopback/process harnesses.

## Global Constraints

- Implement issue #12 and the approved T11 design.
- Test only at the four confirmed seams.
- Use vertical RED→GREEN slices; one behavior at a time.
- Keep every remote endpoint and identity fixed in production.
- Never expose remote device identity, authorization code, verifier, refresh/access token, JWT, or raw upstream body.
- Never generate a local verifier, listener, native-auth import, pasted-token input, or alternate login path.
- Provider bindings are persisted metadata only until T12.
- Use `apply_patch`, stage exact files, commit locally, and do not push.

## Task 1: Declare closed contracts and private storage

**Tracer bullet:** A valid current database migrates without changing existing fingerprints, one exact Subscription Account view/action fixture round-trips through Rust/Zod/JSON Schema, and one account document is atomically stored as 0600 without serializing an access token.

1. Add focused protocol and schema fixture REDs.
2. Add schema migration RED for account catalog, bindings, intents, and receipts.
3. Add private account-file REDs for create/reopen/replace/corruption/permissions/redacted Debug.
4. Implement only those declarations and storage operations.
5. Run focused protocol/state/storage, fmt, clippy, typecheck, and diff gates.
6. Commit `feat: declare subscription account contracts`.

## Task 2: Implement the pinned Device Authorization module

**Tracer bullet:** Against a deterministic loopback authority, start returns the exact public challenge, 403/404 remain Pending, 410 is Expired, and success exchanges the returned authorization code with the exact server verifier before storing the account.

1. RED exact fixed start request and response parsing, including numeric/string/missing interval.
2. GREEN start and final effective interval calculation.
3. RED pending/expired/unexpected status with bounded secret-free errors.
4. GREEN poll classification and private pending-flow state.
5. RED server verifier/code exchange, required refresh token, and identity precedence.
6. GREEN exchange, memory-only access token, and atomic account persistence.
7. RED cancellation and expiry cleanup; GREEN without remote revocation.
8. Commit `feat: authorize subscription accounts by device`.

## Task 3: Manage account lifecycle, defaults, and bindings

**Tracer bullet:** Two accounts can be added, one default-change preview lists only Follow Default Providers, confirmation changes their dynamic resolution, and deleting a Fixed account leaves a missing binding without substitution.

1. RED account catalog/open/add projection and stable ordering.
2. GREEN account state and session coordinator.
3. RED fixed/follow/none binding state and provider revision checks.
4. GREEN SQLite binding mutations and views.
5. RED default preview token/revision/stale/confirm behavior.
6. GREEN deterministic default changes and projections.
7. RED deletion with fixed dangling binding and default fallback.
8. GREEN deletion with no identity substitution.
9. RED permanent refresh rejection and same-identity reauthorization; GREEN persistent Needs Reauthorization and binding preservation.
10. Commit `feat: manage subscription account bindings`.

## Task 4: Expose the Subscription Account session over real UDS

**Tracer bullet:** Two real sessions observe response-before-one-push, receipt replay without push, cancellable polling, restart persistence, and target-independent lifecycle.

1. Open/close account session and initial view.
2. Start and cancellable poll operations.
3. Serialized account action queue with call-time deep capture/freeze.
4. Receipt-first replay, stale revision, malformed replay, response/push ordering, writer failure, reconnect, and shutdown.
5. Account-file recovery before UDS bind and Recovery Required behavior.
6. Commit `feat: expose subscription account session`.

## Task 5: Add the OpenTUI account workflow

**Tracer bullet:** From either Target, the same overlay starts authorization, displays and attempts to copy/open once, represents pending/expired/cancelled/success accurately, and manages default/bind/delete/reauthorize flows.

1. Global command/keymap and account catalog overlay.
2. Device challenge/poll/cancel/expiry/success overlay.
3. Best-effort clipboard/browser platform effects with failure guidance.
4. Default preview/confirm and affected Provider list.
5. Fixed/Follow Default picker and dangling status.
6. Delete and same-identity reauthorization.
7. Pending/stale/replay/target-switch/focus behavior.
8. English/Chinese parity and `1x1` through `121x30` rendering.
9. Scan-first controlled credential/account/config/backend/settings mutations.
10. Commit `feat: add subscription account workflow`.

## Task 6: Prove the complete workflow end to end

**Tracer:** With a temporary Muxvia Home, real binary, real UDS, real SQLite/account file, OpenTUI renderer, and deterministic Device Authority:

1. authorize two accounts across Pending then Success;
2. verify one copy/open attempt and browser failure does not stop polling;
3. bind Fixed and Follow Default Providers;
4. preview and change the default;
5. restart and prove identities/default/bindings/private permissions/no access token;
6. delete the fixed account and prove the explicit dangling binding;
7. inject permanent refresh rejection and prove durable Needs Reauthorization;
8. reauthorize the same identity and preserve every binding;
9. cancel/expire independent flows without revocation claims; and
10. close sessions and require natural status 0 and UDS removal.

Run focused tracer, full Rust workspace serialized, full Control Plane, `bun run verify`, fmt, clippy `-D warnings`, typecheck, frozen install, diff, lockfile, and clean-worktree gates. Commit `test: prove subscription accounts end to end`.

## Final Review

Review against the T11 branch base for both the approved design and repository standards. Fix every Critical, Important, and accepted Minor through focused RED→GREEN loops. Repeat all gates and leave the worktree clean without pushing.
