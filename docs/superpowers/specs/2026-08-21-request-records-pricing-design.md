# Request Records and Immutable Pricing Snapshots Design

Status: approved for T13 on 2026-08-21

Issue: [#14 — T13 Request Records and immutable Pricing Snapshots](https://github.com/HaroldHuanrongLIU/muxvia/issues/14)

## Context

T13 records routed activity without turning Muxvia into a transcript store. It follows `CONTEXT.md`, ADR 0001, ADR 0005, ADR 0008, ADR 0010, ADR 0015, ADR 0030, ADR 0037, ADR 0038, ADR 0040, and ADR 0043.

The Routing Service remains the sole owner of routed-request observation, SQLite state, pricing calculation, and secret redaction. The Control Plane reads target-scoped, secret-free summaries and an explicitly selected failed-record detail over the private UDS. It never opens product storage.

## Scope

T13 delivers:

- one Request Record for each authenticated model request that enters a pinned Activated Route Plan;
- final Provider, model, plan epoch, usage, latency, outcome, and HTTP status when available;
- successful-response observation without persisting request bodies, response bodies, or headers;
- at most 65,536 bytes of a sanitized final upstream error payload, an explicit truncation marker, and a sensitivity warning;
- immutable Pricing Snapshots for nonzero estimates produced by the release-pinned Pricing Catalog;
- a one-time StateStore backfill operation for previously unpriced records;
- target-scoped pagination and failed-record inspection over the private UDS;
- a shared Codex/Claude `/activity` overlay that labels costs as estimates.

T13 does not add Native Usage Records, the 60-second native scan, configurable retention, Daily Usage Rollups, atomic clear, models.dev catalog updates, import/export, billing claims, or telemetry. Those remain T14 or later work.

## Binding Decisions

1. A Request Record represents one inbound Target request, not one Provider attempt. Failover attempts remain visible through Route Health; the record names the final attempted or serving Provider and the pinned Activated Route Plan epoch.
2. Requests rejected before Routing Credential authentication or before a plan is pinned are not Request Records. Once a plan is pinned, every terminal outcome is recorded, including route exhaustion, transport failure, upstream failure, semantic failure, downstream cancellation, and success.
3. Successful request bodies, response bodies, and headers are never persisted. The recorder may hold bounded target-native parser state in memory while the response is live and discards it at completion.
4. Only a failed request may persist payload bytes. The retained value is the final upstream error payload after known request credentials and tokens are replaced; it is capped at exactly 65,536 bytes and records whether bytes were omitted.
5. A streaming response is finalized only when its body completes, errors, or is dropped. Downstream drop records cancellation and drops the upstream stream without starting a detached upstream pump.
6. Usage remains target-native internally and is normalized to input, cached-input, cache-creation, and output token counts. Codex Responses and Claude Messages parsers remain separate; no generic public LLM schema is introduced.
7. Pricing uses integer fixed point only. Unit prices are nano-USD per million tokens, multipliers are parts per million, and estimated cost is nano-USD. Calculation uses checked wide intermediates and deterministic half-up rounding.
8. A nonzero estimate and its Pricing Snapshot are inserted in one transaction. A Pricing Snapshot stores catalog version and source, source model, unit prices, multipliers, pricing time, and the frozen estimate.
9. An unpriced Request Record may receive a Pricing Snapshot exactly once when a later catalog contains its exact source model. Existing Pricing Snapshots are never recalculated or updated.
10. The production catalog is release-pinned data. T13 adds no background or explicit remote fetch. T14 may replace the active catalog and invoke the same one-time backfill interface.

## Product Invariants

1. Request Records are target-isolated and immutable except for the single permitted transition from unpriced to priced.
2. Historical Provider edits, Current changes, new Activated Route Plans, failover, and catalog changes never rewrite recorded Provider, model, plan epoch, usage, outcome, latency, or an existing Pricing Snapshot.
3. Every nonzero estimated cost has exactly one Pricing Snapshot. A record without a snapshot reports no estimate, never a zero-price claim.
4. Request and response headers never enter Request Record storage, wire projections, logs, diagnostics, or test failure output.
5. Successful request and response bodies never enter SQLite, protocol frames, Target Views, renderer state, or logs.
6. Retained failed payloads are private diagnostics. Every public display warns that they may contain sensitive request material even after designated-secret redaction.
7. Provider credentials, Routing Credentials, subscription access or refresh tokens, and designated secret headers are replaced before failed payload persistence and before any projection.
8. Recording failure never changes routing commitment, Current, Serving, Route Health, Managed Configuration, or the downstream response. It is reported through fixed internal diagnostics and remains observable in owning tests.
9. Listing records is read-only, bounded, newest-first, and uses an opaque stable cursor. Failed payload bytes are excluded from list pages and loaded only by explicit record inspection.
10. Muxvia sends no Request Record, usage, error payload, Pricing Snapshot, or catalog data off the local machine.

## Domain and Storage Model

### Request Record

Each row contains:

- monotonically increasing local sequence and UUID;
- Target, Activated Route Plan identity and epoch;
- final Provider identity and immutable display name when an attempt exists;
- source model and target-native protocol;
- start and finish time plus total latency;
- closed outcome and optional HTTP status;
- normalized usage counts plus an explicit usage-observed flag;
- sanitized failed payload bytes, presence, byte count, and truncation state; and
- no request body, successful response body, request headers, or response headers.

The closed outcomes are `success`, `upstream-error`, `semantic-error`, `transport-error`, `route-unavailable`, `cancelled`, and `stream-error`.

### Pricing Snapshot

Each priced Request Record has at most one child row containing:

- catalog version and source;
- exact source model;
- input and output nano-USD-per-million unit prices;
- cache-read and cache-creation parts-per-million multipliers;
- pricing time; and
- frozen estimated nano-USD cost.

SQLite rejects Pricing Snapshot updates. Deletion remains possible only through the later atomic usage-clear operation by cascading from its Request Record.

## Deep Modules and Interfaces

### Request Recorder

The crate-internal Request Recorder is the deep module shared by Codex Responses and Claude Messages handlers. Its small interface starts one recording from a pinned route context and returns a response-stream wrapper. The implementation owns:

- bounded completion capacity reserved before the response is returned;
- target-native usage and terminal-event observation;
- failed-payload capture and secret replacement;
- cancellation/drop finalization;
- deterministic pricing calculation;
- SQLite insertion through the StateStore; and
- shutdown draining so accepted completions are not abandoned.

The Request Router supplies pinned plan/member facts and final attempt classification. Target-native handlers supply no storage details.

### Request History

The private UDS adds two read-only inspection operations:

- list one Target's Request Records with a bounded limit and opaque cursor;
- inspect one Target-bound failed record to obtain its sanitized payload detail and sensitivity warning.

The StateStore also owns one crate-internal one-time pricing-backfill interface. T13 tests that interface with a real catalog and SQLite; a public catalog-update command remains T14.

## Recording and Pricing Ordering

1. Authenticate the Routing Credential and pin one Activated Route Plan.
2. Capture request start time and reserve recorder completion capacity before upstream routing.
3. Route normally; Route Health and Serving semantics remain unchanged.
4. Return a response body wrapped by the Request Recorder.
5. Observe target-native usage and terminal state while forwarding exact bytes with backpressure.
6. On completion, stream error, or drop, finalize one immutable completion in memory.
7. Replace designated secrets, truncate failed payload to 65,536 bytes, and discard every successful body/header surface.
8. Match the exact source model against the active release-pinned catalog.
9. Insert the Request Record and optional nonzero Pricing Snapshot in one SQLite transaction.

A recorder queue that cannot reserve capacity fails closed before returning an unrecorded routed response. Shutdown stops admission, drains active response streams, drains accepted recorder completions, and only then releases the listener task.

## Control Plane

The shared target-scoped command is `activity.open`, available as `/activity` and through the palette. It opens a modal rather than permanent navigation.

The list shows newest-first:

- completion time;
- Provider and model;
- input/output usage;
- total latency;
- outcome; and
- estimated cost or `unpriced`.

Costs are always labelled estimates. Selecting a failed row loads its detail through the inspection operation and shows the truncation state plus a localized warning that the retained payload may contain sensitive request material. The payload is never placed in the ordinary Target View or ephemeral activity feed.

The overlay is target/session/generation bound, cancellable while loading, restores focus, and renders at every supported terminal size in English and Simplified Chinese.

## Stable Problems

- `request-history-unavailable`
- `request-record-not-found`
- `invalid-request-history-cursor`
- `request-recording-unavailable`
- existing connection, state, target, and frame-limit problems

Messages never contain payload bytes, credentials, headers, database values, or request content.

## Confirmed Test Seams

1. Rust/TypeScript/JSON-schema protocol fixtures plus a real UDS TargetSession;
2. real SQLite fresh-schema and v12-to-v13 migration, immutable snapshots, and one-time backfill;
3. authenticated real loopback Codex Responses and Claude Messages routes with deterministic streaming, errors, cancellation, and failover;
4. real TargetSession through the OpenTUI renderer/keymap;
5. real `muxvia` plus `muxvia-routing`, UDS, SQLite, both loopback Targets, and deterministic upstreams.

Every secret-bearing test scans raw/debug/serialized surfaces before semantic assertions and uses fixed diagnostics. Controlled mutations prove successful bodies or headers cannot reach storage, payload truncation is exact, Pricing Snapshots cannot be rewritten, and renderer failures do not print retained payloads.

