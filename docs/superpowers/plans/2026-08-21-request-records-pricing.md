# Request Records and Immutable Pricing Snapshots Implementation Plan

**Goal:** Deliver GitHub issue #14 / T13 through the approved Request Records and Pricing Snapshots design.

**Architecture:** Add one crate-internal Request Recorder behind the existing Codex/Claude model handlers. It observes one pinned routed request, forwards the target-native response unchanged, and writes one immutable Request Record plus an optional Pricing Snapshot through the Routing Service-owned StateStore. Add bounded read-only Request History operations to the private UDS and one shared `/activity` overlay. Do not add Native Usage, retention, rollups, clear, catalog fetch, or telemetry.

**Method:** Each task is one vertical RED → minimal GREEN slice. Tests cross only the approved seams in the design.

## Task 1: Closed contracts and schema v13

**Seams:** Rust/TypeScript/JSON-schema fixtures; real SQLite migration.

- Add closed Request Record summary/detail, usage, outcome, price, cursor, list, and inspect contracts.
- Add `request_records` and `pricing_snapshots` tables with target/outcome/size/immutability constraints.
- Migrate v12 to v13 atomically without changing existing Target, Provider, account, route, recovery, receipt, or secret state.
- RED malformed discriminators, wrong-target details, oversized payload rows, mutable snapshots, and failed migration residue.
- GREEN fresh schema, immutable v12 fixture migration, protocol fixtures, TypeScript parsing, and secret-safe diagnostics.
- Final-review hardening advances v13 to v14 solely to prevent direct Pricing Snapshot deletion while preserving parent Request Record cascade deletion; a real v13 database fixture proves the atomic migration.

## Task 2: Deterministic Pricing Snapshot module

**Seam:** real StateStore with an injected immutable catalog.

- Add fixed-point catalog types, checked calculation, exact-model lookup, and release-pinned catalog loading.
- Insert one nonzero Pricing Snapshot atomically with its Request Record.
- Backfill only unpriced exact-model records and freeze the first successful result.
- Prove later catalog changes do not alter frozen rows, zero/unknown prices remain unpriced, and overflow fails without partial mutation.

## Task 3: Codex Request Recorder tracer

**Seam:** authenticated real loopback Codex Responses route.

- Reserve bounded completion capacity after plan pinning and before routing.
- Record Provider/model/epoch, target-native JSON/SSE usage, latency, success, non-2xx, semantic failure, transport exhaustion, stream failure, and cancellation.
- Forward exact bytes/order/backpressure and preserve failover, Serving, health, and active-request lifecycle.
- Prove successful bodies/headers never reach SQLite and failed payload is sanitized, exactly capped at 65,536 bytes, and explicitly truncated.

## Task 4: Claude and Subscription Bridge recording

**Seam:** authenticated real loopback Claude Messages and Subscription Bridge routes.

- Add target-native Claude usage extraction without a public generic schema.
- Observe Subscription Bridge Responses before conversion while preserving exact Anthropic output.
- Cover API-key/bearer Providers, Bridge success/failure/incomplete, native-to-Bridge failover, count_tokens, cancellation, and both Target isolation.
- Prove subscription access/refresh tokens, Provider credentials, Routing Credentials, request bodies, and headers never enter records or diagnostics.

## Task 5: Request History over real UDS

**Seam:** real private UDS and TargetSession.

- Implement newest-first bounded list with opaque cursor and no payload bytes.
- Implement exact Target-bound failed-record detail with sanitized payload, truncation, and sensitivity warning.
- Capture returned values deeply in TargetSession and validate response kind/Target/cursor/record identity.
- Cover cancellation, malformed frames, frame bounds, target isolation, reconnect, missing record, and secret-free fixed diagnostics.

## Task 6: OpenTUI `/activity` workflow

**Seam:** real TargetSession fakes and OpenTUI renderer/keymap.

- Add the shared `activity.open` command and modal for Codex and Claude.
- Render Provider, model, usage, latency, outcome, and explicitly estimated or unpriced cost.
- Load failed detail only on selection and show the localized sensitivity/truncation warning.
- Preserve target/session/generation binding, cancellation, focus restoration, English/zh-CN parity, and `1x1`, `2x2`, `20x5`, `40x10`, `80x24`, and `121x30` rendering.
- Add controlled credential/config/backend/settings/payload mutations and scan raw frames before semantic assertions.

## Task 7: Real-process tracer and final verification

**Seam:** real `muxvia`, `muxvia-routing`, UDS, SQLite, OpenTUI, Codex/Claude listeners, and deterministic upstreams.

- Route priced and unpriced successful requests, a fallback request, a failed payload over 64 KiB, and a cancelled stream for both Targets where applicable.
- Reopen and restart, page history, inspect one failed record, and prove frozen pricing and historical plan/provider identity.
- Prove no successful body/header or controlled secret exists in SQLite, protocol frames, renderer frames, logs, or diagnostics.
- Close sessions, require natural status 0 and UDS removal, and verify accepted recorder completions are drained.

## Final gates

- `cargo test --workspace -- --test-threads=1`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `bun run typecheck`
- `bun run verify`
- `bun install --frozen-lockfile` with no dependency or lockfile changes
- `git diff --check`
- independent Spec and Standards review before the final local commit
