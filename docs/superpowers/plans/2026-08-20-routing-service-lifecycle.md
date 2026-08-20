# Routing Service Lifecycle and Version Handover Implementation Plan

**Goal:** Complete safe Takeover disable, deterministic service lifetime, crash recovery evidence, and compatible sidecar replacement without introducing an operating-system startup service.

**Architecture:** Deepen the existing Routing Service process and model-runtime admission modules. A target action owns safe disable through the established recovery transaction. A process lifecycle coordinator owns candidate probe, drain, inherited service lock, and `exec` replacement. The Control Plane has one startup coordinator that compares release metadata and requests at most one compatible handover.

**Testing:** Strict TDD at the four approved public seams: real process/UDS, closed cross-language protocol, real model ingress, and Control Plane startup plus a real multi-process tracer.

## Global Constraints

- The Routing Service remains the sole SQLite owner.
- Major RPC incompatibility is rejected before state access or mutation.
- Response precedes exactly one push; receipt replay produces no second push or lifecycle effect.
- Safe disable restores and verifies Managed Configuration before committing the inactive route state.
- New model requests are rejected after drain reservation; accepted requests keep one pinned route plan and are never truncated after output commitment.
- A peer Target remains isolated throughout disable and drain.
- Candidate metadata probe is no-write and secret-free. T20 adds bundle integrity validation without changing the lifecycle interface.
- Handover uses one process image and one inherited exclusive lock; two processes never open SQLite concurrently.
- Failed probe or failed `exec` leaves the compatible old service available.
- No production fixed timeout truncates a committed stream.
- No OS startup service or new noninteractive CLI surface is added.

## Task 1 — Closed lifecycle protocol and release metadata

**Files:** protocol schema/fixtures, Rust protocol contract, TypeScript types/RpcClient tests.

1. Add exact fixtures for `disable-takeover`, `prepare-handover`, and handover outcomes.
2. RED: Rust and TypeScript reject the new closed discriminators and do not expose hello service release/epoch metadata to the startup coordinator.
3. GREEN: implement matching Rust/TS/Zod/schema types, fixed diagnostics, and secret scans.
4. Verify protocol contract suites and typecheck.

## Task 2 — Drain-aware model runtime

**Files:** model server/request routing and Codex/Claude model route tests.

1. RED at real loopback ingress: after drain begins, a new request is rejected while a held accepted SSE/body request completes byte-exactly; peer Target remains accepting.
2. GREEN: replace the binary idle reservation with a closed admission state and a drain handle that waits for accepted requests.
3. RED/GREEN cancellation and pre-commit failure release the accepted count exactly once.
4. Verify Codex/Claude ingress suites.

## Task 3 — Safe Target Takeover disable

**Files:** target action protocol, Activation/Reconciliation state seams, StateStore, control server, TUI command/localization only as required by the Operator action.

1. RED through real UDS for one Target: disable restores exact pre-Takeover bytes/mode, commits unmanaged state, clears Current/Serving/Snapshot/plan, responds before one push, and replays without a second effect.
2. GREEN using one target mutation gate, durable recovery intent, exact target-native restore, atomic state/receipt commit, and drain-only runtime.
3. RED/GREEN failpoints at intent, file restore, verify, final transaction, drain, and rollback verify.
4. RED/GREEN two-Target isolation and final-Takeover natural exit after response/push and accepted stream completion.
5. Add the minimal shared named Control Plane command and confirmation workflow; verify focus/localization/size behavior.

## Task 4 — Process lifecycle event module

**Files:** process runner, control server handle, lifecycle tests.

1. RED: lifecycle events currently collapse into one server completion and cannot distinguish idle, explicit shutdown, and handover.
2. GREEN: one internal lifecycle outcome from ControlServer to the process runner, with accepted sessions/actions/writers drained before completion.
3. Preserve existing detached Takeover, idle exit, pending action, startup recovery, and lock-collision tests.

## Task 5 — Candidate probe and inherited lock

**Files:** binary args/process module plus process tests.

1. RED: candidate metadata cannot be obtained without opening product state.
2. GREEN: hidden no-write metadata probe emits a closed product/release/RPC record and rejects ordinary test-only invocation rules correctly.
3. RED: ordinary `exec` drops the service lock and permits a competing database owner.
4. GREEN: clear close-on-exec only for the validated service lock, pass its descriptor through a hidden inherited-lock argument, and validate exact lock identity before SQLite open.
5. Add malformed descriptor, wrong-home, wrong-product, wrong-release, and wrong-major tests with zero database/socket/config mutation.

## Task 6 — Compatible handover and fallback

**Files:** lifecycle coordinator, process runner, control server, process tests.

1. RED multi-process tracer: compatible release replacement cannot happen while old service owns the lock.
2. GREEN: probe candidate, acknowledge, stop new admission, drain actions/writers/model requests, remove old UDS, and `exec` candidate with inherited lock.
3. Prove new epoch, same database/config/routes/credentials, stable ports, response continuity, and no duplicate listener.
4. RED/GREEN probe failure and injected `exec` failure keep/rebind the old compatible service and preserve an already committed stream.
5. Major incompatibility remains pre-mutation and never attempts handover.

## Task 7 — Control Plane startup coordination

**Files:** app startup, RpcClient metadata/lifecycle method, app lifecycle tests.

1. RED: two Target startup attempts can independently spawn or hand over.
2. GREEN: one startup coordinator negotiates once, reuses exact release, requests one compatible replacement, reconnects on new epoch, then opens all sessions.
3. RED/GREEN failed compatible handover reconnects to old service with a fixed diagnostic; major mismatch never spawns or mutates.
4. Preserve cancellation, renderer destruction, signal, and startup deadline behavior.

## Task 8 — Real end-to-end lifecycle tracer

**Files:** process lifecycle and walking skeleton tests; production only if an owning focused RED proves a defect.

1. Start a real service with both Target Takeovers and deterministic streaming upstreams.
2. Close the Control Plane and prove both routes remain available.
3. Crash the service and prove both routes fail closed; explicitly restart and prove clean resume plus stale health.
4. Begin committed streams, trigger compatible replacement, and prove exact completion followed by new-epoch routing.
5. Trigger failed replacement and prove the old epoch remains usable.
6. Disable one Takeover with peer isolation, then disable the final Takeover and require response/push, exact restore, stream drain, status-zero exit, and UDS/listener removal.
7. Assert no launchd/systemd/login-item artifacts and scan all wire/process/config/database diagnostics before semantics.

## Final Gates

- Focused protocol, model route, activation/reconciliation, process lifecycle, RpcClient, app lifecycle, renderer, and tracer suites.
- `cargo test --workspace -- --test-threads=1`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `bun run typecheck`
- `bun run verify`
- `bun install --frozen-lockfile` with no lockfile changes
- `git diff --check`
- self-review against #11, the approved design, CONTEXT, and relevant ADRs
- local commit only; no push or issue closure without explicit authorization
