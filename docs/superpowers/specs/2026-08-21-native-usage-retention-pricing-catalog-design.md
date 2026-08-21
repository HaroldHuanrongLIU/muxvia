# Native Usage, Retention, and Pricing Catalog Design

Status: approved for T14 by issue #15 and ADRs 0036-0038 on 2026-08-21

Issue: [#15 — T14 Native Usage, retention, and Pricing Catalog](https://github.com/HaroldHuanrongLIU/muxvia/issues/15)

## Context

T14 completes the local usage lifecycle started by T13. It follows `CONTEXT.md`, ADR 0001, ADR 0010, ADR 0014, ADR 0015, ADR 0036, ADR 0037, ADR 0038, ADR 0040, ADR 0043, and the approved T13 Request Record design.

The Routing Service remains the sole owner of Native Usage import, SQLite state, retention, Daily Usage Rollups, the active Pricing Catalog, and Pricing Snapshot creation. The Control Plane invokes target-scoped usage operations over the private UDS and never reads Target CLI logs or the database directly.

## Scope

T14 delivers:

- incremental Native Usage Record import from the default Codex and Claude Configuration Homes;
- import when a Target session opens, on explicit refresh, and every 60 seconds while at least one Target Takeover already requires the Routing Service;
- one combined Target activity page containing Request Records, Native Usage Records, and retained Daily Usage Rollups;
- default 30-day detailed retention with an Operator-configurable 1–3,650 day period;
- transactional rollup and pruning of completed older local calendar days;
- one atomic clear of Request Records, retained upstream errors, Pricing Snapshots, Native Usage Records, Daily Usage Rollups, and import cursors;
- one explicit HTTPS GET of `https://models.dev/api.json`, normalized into a persisted active Pricing Catalog; and
- one-time first fill of unpriced Request and Native Usage Records without mutation of an existing Pricing Snapshot.

T14 does not add billing claims, provider quota projection, background catalog fetches, remote usage export, telemetry, transcript storage, or heuristic correlation between routed and native records.

## Binding Decisions

1. Native Usage Records and Request Records remain distinct. Native records contain a source-safe stable identifier, Target, model, observed time, normalized token counts, and an optional immutable Pricing Snapshot. They contain no transcript text, project path, session identifier, Provider identity, request body, response body, header, or retained error.
2. Import cursors key files by a one-way path fingerprint and retain only file modification time, byte length, and completed line count. A scan enumerates the Configuration Home again; no source path is persisted or projected.
3. Cursor advancement, all records parsed from the newly completed lines, and first-fill pricing commit in one SQLite transaction. A malformed or incomplete final line is not counted and is retried on the next scan.
4. Claude import accepts billable `assistant.message.usage` observations and deduplicates repeated snapshots by a stable hash of Target, session/message identity, and normalized usage. Codex import accepts `session_meta`, `turn_context`, and `event_msg/token_count`; it prefers exact `last_token_usage` and otherwise records the positive delta from the prior cumulative snapshot in that file.
5. Native and routed sources are both displayed with an explicit source label. T14 does not guess that two same-sized observations close in time are the same request; such a heuristic can silently discard real Direct usage.
6. A 60-second scan is an untracked child of the already-running Routing Service. It runs only when persisted route state says a Target Takeover is active, is cancelled during service shutdown, and is excluded from every idle-lifecycle count.
7. Retention keeps the current local day plus the configured period's remaining preceding detailed local dates (30 means the current date plus 29 preceding dates). Only strictly older completed dates are rolled up and pruned. A late-imported old Native Usage Record is additively merged into an existing Daily Usage Rollup before pruning.
8. A Daily Usage Rollup preserves separate Request and Native counts, successful and failed Request counts, all four token dimensions, priced and unpriced counts, estimated nano-USD cost, and latency observation count plus total latency. It never claims invoice accuracy.
9. Clear is one immediate SQLite transaction. Failure leaves details, errors, snapshots, rollups, and cursors unchanged. Retention configuration and the active Pricing Catalog remain.
10. The release-pinned Pricing Catalog is installed only when no active catalog exists. An explicit update downloads models.dev once, validates and normalizes the complete candidate before opening the replacement transaction, atomically replaces the active catalog, and backfills only still-unpriced details.
11. The models.dev adapter imports exact model IDs from the first-party `openai` and `anthropic` provider entries. Missing, tiered, non-token, duplicate, non-integral, or overflowing prices are rejected or skipped before state mutation. The catalog version is a SHA-256 digest of the downloaded bytes and the source is `models.dev`.
12. Request Recorder pricing reads the persisted active catalog for each completion. A catalog update therefore affects only completions admitted after replacement and the first permitted fill of still-unpriced records; frozen snapshots and rollups never change.
13. The only catalog network request is the Operator-invoked GET. It has no request body, query string, Target data, Native Usage content, Request Record content, identifiers, or credentials. Scans, startup, retention, list, and clear perform no network I/O.

## Product Invariants

1. Native Usage Records are target-isolated and immutable except for the single unpriced-to-priced transition represented by inserting their child Pricing Snapshot.
2. Existing Request and Native Pricing Snapshots are never updated or directly deleted. Atomic usage clear removes them only by cascading from their parent records.
3. Retention never prunes the current local calendar day and never deletes detail before its aggregate is durably merged into the corresponding Daily Usage Rollup.
4. Aggregate token, cost, latency, and count fields are checked for overflow; any failure rolls back the complete retention transaction.
5. Import is monotonic for append-only files and safe under unchanged-file rescans, file truncation, replacement, duplicate records, malformed completed lines, and incomplete final lines.
6. Stored/projected/logged/debug surfaces contain no transcript text, source paths, session identifiers, request bodies, response bodies, headers, credentials, or native log content.
7. Catalog fetch failure, invalid content, pricing overflow, import failure, and retention failure leave the prior active catalog and all prior usage state intact and report fixed secret-free diagnostics.
8. No pricing fetch or native usage content is transmitted as telemetry.

## Deep Module and Interface

The crate-internal Native Usage module is the deep module. Its interface has four operations: scan one Target, apply retention, clear all usage, and explicitly update the Pricing Catalog. Behind that interface it owns Configuration Home enumeration, target-native parsing, cursor recovery, source-safe identity, pricing, SQLite transactions, rollups, pruning, and fixed diagnostics.

The existing StateStore is the local-substitutable persistence seam. Tests use a real temporary Configuration Home and real SQLite through the same Native Usage interface as production; parser and SQL internals are not separate public seams.

The private UDS adds bounded activity listing plus explicit refresh, retention, clear, and catalog-update operations. TargetSession exposes those use cases without transport- or database-shaped methods. The activity overlay consumes the combined page and labels every entry by source.

## Confirmed Test Seams

1. Rust/TypeScript/JSON-schema protocol fixtures;
2. real SQLite fresh schema and v14-to-v15 migration;
3. the Native Usage module with real temporary Codex/Claude Configuration Homes and real SQLite;
4. real private UDS and TargetSession;
5. process lifecycle with a short test interval proving periodic scans occur during Takeover and never prevent final idle exit; and
6. an authenticated deterministic loopback models.dev adapter proving zero startup/background requests, one explicit bodyless GET, atomic replacement, and frozen snapshots.

Every source-bearing test plants controlled transcript/path/session/credential markers and scans SQLite, protocol frames, Debug output, logs, and fixed diagnostics before semantic assertions.
