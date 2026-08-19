# Universal Providers and Transactional Synchronization Implementation Plan

**Goal:** Implement GitHub issue #9 with one independent Universal Provider catalog, protected Generated Target Providers, and explicit all-target transactional synchronization.

**Architecture:** Add a global UniversalProviderSession beside TargetSession. A crate-internal Provider Synchronization Coordinator owns cross-target eligibility, reference checks, credentials, one SQLite transaction, receipts, and authoritative publication. Existing Target Provider mutations remain the only path for Target Overlay edits and detached duplication.

**Tech stack:** Rust 2024, Tokio, rusqlite/SQLite, serde/serde_json, Bun, TypeScript, Zod, SolidJS/OpenTUI, real UDS/loopback/process harnesses.

## Global Constraints

- Implement #9 and `docs/superpowers/specs/2026-08-20-universal-provider-synchronization-design.md`.
- Test only at the four confirmed seams from the design.
- Work in vertical RED→GREEN slices; do not batch imagined tests ahead of implementation.
- Universal edits never synchronize implicitly.
- Provider Synchronization is per Universal Provider and atomic across all affected Targets.
- Never change Current, Serving, Activated Snapshot, Activated Route Plan, Managed Configuration, recovery, or runtime state.
- Preserve target isolation and acquire cross-target mutation gates in Codex-then-Claude order.
- Generated Universal-owned fields are server-enforced read-only; Target Overlay fields cannot replace them.
- All public and test diagnostics are scan-first and secret-free.
- Use `apply_patch`, keep commits task-scoped, and do not push.

## Task 1: Declare schema-v9 and closed catalog contracts

**Files:**

- `crates/routing-service/src/state/schema.sql`
- `crates/routing-service/src/state/migrations.rs`
- `crates/routing-service/src/control/protocol.rs`
- `crates/routing-service/tests/provider_declarations.rs`
- `crates/routing-service/tests/protocol_contract.rs`
- `crates/routing-service/tests/fixtures/state-schema-v8.sql`
- `protocol/control-v1.schema.json`
- new Universal Provider fixtures under `protocol/fixtures/`
- `packages/control-plane/src/control/types.ts`
- `packages/control-plane/test/protocol.test.ts`

**Tracer bullet:** An immutable valid v8 database migrates to v9 without changing any existing table fingerprint, and one exact `open-universal-providers` fixture round-trips through Rust, Zod, and JSON Schema.

1. Write focused Rust and TypeScript protocol RED tests for the catalog view, actions, outcome, and push discriminators.
2. Write v8→v9 migration RED tests with transaction rollback and foreign-key validation.
3. Implement only the closed declarations, tables, checks, and migration needed for those tests.
4. Run protocol, provider declaration, state-store, TypeScript contract, fmt, clippy, typecheck, and diff gates.
5. Commit `feat: declare universal provider contracts`.

## Task 2: Build the Universal Provider catalog CRUD module

**Files:**

- new `crates/routing-service/src/state/universal_providers.rs`
- `crates/routing-service/src/state/mod.rs`
- `crates/routing-service/src/state/store.rs`
- focused state and UDS tests

**Tracer bullet:** Through the catalog state interface, create a blank Universal Provider, reopen it, update it, duplicate it, and delete it with receipt-first replay and no Target Provider or runtime mutation.

Vertical slices:

1. Open empty catalog and stable Preset projection.
2. Create blank and Preset-based sources; prove submitted values win and provenance matches only stable key.
3. Update source and target settings with catalog/source revision guards and no-op rejection.
4. Duplicate declaration with explicit credential choice and detached identity.
5. Delete a source with no generated records.
6. Add one-time seed-key idempotency independent of UUID/name.

Run focused catalog/state gates after each slice, then full provider declaration/recovery regression, fmt/clippy/diff, and commit `feat: manage universal provider catalog`.

## Task 3: Materialize every enabled Target transactionally

**Files:**

- new `crates/routing-service/src/service/provider_synchronization.rs`
- `crates/routing-service/src/service/mod.rs`
- `crates/routing-service/src/state/universal_providers.rs`
- `crates/routing-service/src/state/store.rs`
- focused synchronization tests

**Tracer bullet:** One Universal Provider enabled for Codex and Claude creates both Generated Target Providers, fresh target-scoped credentials, one catalog receipt, and authoritative catalog/Target views in one transaction while protected runtime fingerprints remain byte-identical.

Vertical slices:

1. Codex-only create.
2. Both-target create.
3. Universal update causes pending state; explicit sync updates both while preserving Target Overlay.
4. Target disable causes pending removal; sync removes the unreferenced generated record.
5. Credential replacement rebinds generated records without mutating shared detached credentials.
6. Failpoints after each target insert/update/delete/credential/receipt/revision boundary prove complete rollback.
7. Drift, shadow, compatibility, Recovery Required, and stale-revision gates prove whole-sync rejection.
8. Held routed requests and listeners prove declaration-only behavior.

Run synchronization plus activation/reconciliation/recovery/runtime regressions and commit `feat: synchronize universal providers transactionally`.

## Task 4: Enforce Generated Target Provider ownership and lifecycle

**Files:**

- `crates/routing-service/src/state/providers.rs`
- `crates/routing-service/src/domain/view.rs`
- protocol and TypeScript Provider view types
- provider lifecycle/duplication/control UDS tests

**Tracer bullet:** A Generated Target Provider rejects a Universal-owned edit and independent delete, accepts a Target Overlay edit, and duplicates into an ordinary detached Provider with an explicitly chosen Credential Reference.

Vertical slices:

1. Closed ownership projection in Target View.
2. Read-only name/Base URL/Credential/protocol enforcement.
3. Model/authentication/routing-requirement overlay edit updates the canonical target setting and generated declaration atomically.
4. Independent delete rejection.
5. Detached duplication clears generated ownership and never copies runtime state.
6. Current/snapshot/route-plan reference discovery lists every deterministic blocker.
7. Target disable and Universal delete blocker behavior; unreferenced delete cascades atomically.

Run provider, activation, reconciliation, and protocol gates; commit `feat: protect generated provider lifecycles`.

## Task 5: Expose the independent catalog session over real UDS

**Files:**

- `crates/routing-service/src/control/server.rs`
- `crates/routing-service/src/control/protocol.rs`
- catalog service/state modules
- `packages/control-plane/src/control/rpc-client.ts`
- `packages/control-plane/src/control/target-session.ts` or a new catalog-session module
- UDS and TargetSession tests

**Tracer bullet:** Two real catalog sessions observe response-before-one-push, replay without push, stale revision replacement, and target-view publication after a cross-target synchronization.

1. Open/close catalog session and authoritative initial view.
2. Serialized catalog action queue with call-time deep capture/freeze.
3. Receipt-first replay and raw malformed replay.
4. Catalog and affected Target response/publication ordering.
5. Writer failure, reconnect, close, and shutdown behavior.
6. Concurrent catalog/Target mutations and fixed cross-target lock order.

Run full control-socket, TargetSession, process-lifecycle, typecheck, clippy, and diff gates; commit `feat: expose universal provider session`.

## Task 6: Add the shared OpenTUI workflow

**Files:**

- command catalog/keymap
- `packages/control-plane/src/ui/app.tsx`
- new Universal Provider picker/editor/confirmation modules
- generated Provider editor changes
- English and Simplified Chinese catalogs
- provider workflow, renderer, localization, responsive, and audit tests

**Tracer bullet:** From either Target context, `/universal-providers` opens the same catalog, creates a Preset draft, edits both Target settings, synchronizes, and renders protected generated fields in each Target provider flow.

1. Command scope and one overlay stack.
2. List/inspect/create/edit/duplicate/delete catalog workflow.
3. Stable Preset copy and credential handling.
4. Target enablement, overlay editing, pending/current sync status, and explicit synchronize confirmation.
5. Generated read-only fields, overlay edit, detached duplication, and reference blockers.
6. Pending/stale/replay/target-switch/focus behavior.
7. English/Chinese parity and `1x1` through `121x30` rendering.
8. Scan-first controlled secret mutations on every frame/action/view/error surface.

Run the exact owning suites, full Control Plane, typecheck, and diff gates; commit `feat: add universal provider workflow`.

## Task 7: Prove the complete workflow end to end

**Files:**

- `packages/control-plane/test/walking-skeleton.e2e.tsx`
- only owning product files if a real cross-layer defect is reproduced first

**Tracer:** With a temporary Muxvia Home, both target homes, real binary, real UDS, real SQLite, OpenTUI renderer, and deterministic upstreams:

1. create a Preset-based Universal Provider and enable both Targets;
2. synchronize and inspect both Generated Target Providers;
3. activate immutable snapshots, then edit/synchronize declarations and prove traffic/config stay pinned;
4. edit a Target Overlay and duplicate one generated record into an ordinary detached Provider;
5. prove referenced disable/delete lists every blocker;
6. release references, disable/resynchronize, and delete the source;
7. restart between phases and prove durable identities, revisions, receipts, and pending/current state;
8. close all sessions and require natural status 0 and UDS removal.

Run focused tracer, full Rust workspace serialized, full Control Plane, `bun run verify`, fmt, clippy `-D warnings`, typecheck, frozen install, diff, lockfile, and clean-worktree checks. Commit `test: prove universal provider synchronization end to end`.

## Final Review

Run independent Spec and Standards reviews against the T08 branch base. Fix every Critical, Important, and accepted Minor through focused RED→GREEN loops. Repeat full gates, update the ignored local report, and leave the worktree clean without pushing.
