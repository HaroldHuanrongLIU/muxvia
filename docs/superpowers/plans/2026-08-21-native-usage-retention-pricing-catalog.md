# Native Usage, Retention, and Pricing Catalog Implementation Plan

**Goal:** Deliver GitHub issue #15 / T14 through the approved Native Usage, retention, and Pricing Catalog design.

**Architecture:** Put file discovery, target-native parsing, cursor commits, active catalog replacement, retention, rollups, and clear behind one crate-internal Native Usage module backed by the Routing Service-owned StateStore. Extend the existing target-scoped activity seam rather than exposing storage details.

**Method:** Each task is a vertical RED → minimal GREEN slice. Tests cross only the confirmed seams in the design.

## Task 1: Closed contracts and schema v15

- RED protocol fixtures and real v14 migration for Native Usage Records, their immutable Pricing Snapshots, cursor privacy, retention settings, active Pricing Catalog, and Daily Usage Rollups.
- GREEN the closed Rust/TypeScript/JSON-schema contracts and atomic v15 migration.
- Commit and report the schema/contract boundary before behavior work.

## Task 2: Native Usage import

- RED real Codex rollout and Claude project JSONL fixtures for first scan, append, unchanged rescan, incomplete final line, malformed completed line, truncation/replacement, source identity, and target isolation.
- GREEN one scan operation that atomically inserts details/snapshots and advances source-safe cursors.
- RED then GREEN combined activity pagination through Request, Native, and rollup entries.

## Task 3: Retention, Daily Usage Rollups, and clear

- RED default/configured retention across completed local dates, late old imports, exact aggregate preservation, overflow rollback, and current-day protection.
- GREEN additive rollup plus pruning in one transaction.
- RED clear failpoint and complete surface scan; GREEN one atomic clear preserving settings/catalog.

## Task 4: Explicit Pricing Catalog update

- RED deterministic loopback models.dev responses for bodyless explicit GET, invalid/duplicate/tiered/overflowing data, active-catalog persistence, request/native first fill, and frozen snapshots.
- GREEN normalized atomic catalog replacement and Request Recorder use of the active catalog.
- Prove startup, scan, retention, list, and clear issue no network request.

## Task 5: Control Plane and lifecycle

- RED real UDS/TargetSession startup and explicit refresh, retention, clear, catalog update, cancellation, target binding, response-kind validation, and secret-safe fixed problems.
- GREEN target-scoped use-case methods and combined activity rendering with explicit source labels.
- RED process test with a short injected interval; GREEN a periodic scan gated only by active Target Takeover and excluded from idle lifecycle accounting.

## Final gates

- `cargo test --workspace -- --test-threads=1`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `bun run typecheck`
- `bun run verify`
- `bun install --frozen-lockfile` with no dependency or lockfile changes
- `git diff --check`
- Spec and Standards self-review before the final local commit
