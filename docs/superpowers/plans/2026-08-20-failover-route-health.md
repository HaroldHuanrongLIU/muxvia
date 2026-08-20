# Failover Chain, Route Health, and Immutable Route Epochs Implementation Plan

**Goal:** Deliver GitHub issue #10 / T09 through the approved failover-route-health design.

**Architecture:** Keep target actions and complete views on the existing private UDS. Add one crate-internal Route Plan Coordinator for draft/apply persistence and one crate-internal Request Router for pinned-plan attempts, target-native response commitment, Serving updates, and passive health. Keep circuit eligibility memory-only per Routing Service epoch and persist only secret-free health observations.

**Method:** Each task is one vertical RED → minimal GREEN slice. Tests cross only the approved public seams from the design. Do not add a generic LLM schema, storage repository interface, routing DSL, configurable policy UI, or background probes.

## Task 1: Closed contracts and schema v10

**Seams:** Rust/TypeScript protocol fixtures; real SQLite migration.

- Add closed draft/apply Target actions and complete Target View projections for draft, active plan, plan members, per-Provider health, and Target summary health.
- Add schema tables for failover drafts/members, activated route plans/members, and Provider health observations; add the active plan foreign key to Target route state.
- Migrate v9 to v10 atomically. Existing activated snapshots become one-member drafts/plans when Current exists; unmanaged Targets remain without plans.
- Extend active Provider references with authoritative plan membership.
- RED: fixtures/schema reject missing or malformed discriminators, duplicate/invalid state, and migration loses either Target state.
- GREEN: Rust/TS/schema fixtures and v9→v10/fresh schema tests pass with secret-free projections.

## Task 2: Draft save through real UDS

**Seam:** `TargetSession.act(save-failover-draft)` over a real UDS connection.

- Add receipt-first, revision-guarded draft replacement.
- Validate nonempty unique Target-local IDs, exact Provider revisions, Current-first order, completeness, and generated synchronization.
- Prove draft save changes no active plan, snapshots, Current, Serving, Managed Configuration, recovery, listener, or held request.
- Cover stale Target/draft/Provider revisions, malformed/replayed payloads, target isolation, response-before-one-push, reconnect, and scan-first diagnostics.

## Task 3: Immutable Apply through real UDS

**Seam:** `TargetSession.act(apply-failover-chain)` with real SQLite and active listeners.

- Implement the Route Plan Coordinator and immutable member snapshots.
- Share the existing Target mutation gate and live managed-write eligibility checks.
- Commit plan, active pointer, receipt, revision/view sequence, and complete authoritative view in one immediate transaction.
- Add failpoints after each insert/update and prove exact rollback.
- Prove historical plans remain immutable, active references block deletion/synchronization removal, and replay never republishes.

## Task 4: Activation creates a one-member plan

**Seam:** existing real UDS Direct/Takeover activation actions.

- Extend final activation transactions to create a one-member immutable plan and reset the draft to the new Current Provider.
- Preserve recovery ordering and exact rollback.
- Prove Direct/Takeover, Codex/Claude, restart/bootstrap, same-action replay, failpoints, and configuration fingerprints remain correct.

## Task 5: Pinned plan request router and pre-commit failover

**Seam:** authenticated real loopback Codex Responses and Claude Messages routes.

- Add a crate-internal Request Router that captures one plan at request start.
- Attempt each eligible member at most once in order with bounded replayable request input.
- Implement the approved retryable/nonretryable classification.
- Keep target-native URL, authentication, header, request, and response adapters.
- Prove priority-first failback, Current-versus-Serving, wrong/cross-target credential rejection, target isolation, and plan changes during concurrent requests.

## Task 6: Responses and Messages semantic commitment

**Seam:** real HTTP/SSE streams with versioned deterministic fixtures.

- Buffer bounded lifecycle-only start events.
- Retry early failure/error terminals before productive output.
- Commit on the first productive or valid nonfailure terminal event and replay exact bytes/order.
- Never retry after commitment; cancellation drops the pinned attempt without detached work.
- Cover compressed errors, invalid/missing content type, byte/time priming bounds, downstream backpressure, and secret-free failures.

## Task 7: Route Health and epoch-local circuits

**Seam:** routed requests plus complete Target View push/reopen/restart.

- Implement fixed circuit constants, one half-open permit, and target/provider keyed runtime state.
- Persist request-derived health observations transactionally with Serving/view-sequence updates.
- Project per-Provider current/stale/unobserved health and Target plan summary.
- On service restart, reset eligibility and mark old observations stale until current-epoch evidence exists.
- Prove Reachability, Discovery, declaration edits, synchronization, and reconciliation never change health.

## Task 8: TargetSession and OpenTUI route workflow

**Seam:** real TargetSession fakes plus OpenTUI renderer/keymap.

- Add deep-captured draft/apply actions and closed schemas.
- Add shared Codex/Claude route overlay with add/remove/move/apply commands.
- Show draft-versus-active state, Current-first invariant, plan epoch, Current/Serving, member completeness/synchronization, and health.
- Preserve target/session/generation binding, pending nondismissibility, duplicate suppression, focus restoration, exact stale recovery, and English/zh-CN parity.
- Cover `1x1`, `2x2`, `20x5`, `40x10`, `80x24`, and `121x30`.

## Task 9: Real-process tracer and final verification

**Seam:** real `muxvia` + `muxvia-routing`, UDS, SQLite, OpenTUI, two loopback Target listeners, and deterministic upstreams.

- Apply chains for both Targets.
- Prove first-provider failure, fallback Serving, next-request failback, and unchanged Current.
- Hold requests across a concurrent Apply and prove exact plan-epoch pinning.
- Restart the service and prove historical health is stale while eligibility resets.
- Prove Reachability does not change health and both Targets remain isolated.
- Close sessions, require natural status 0 and UDS removal, and scan all public surfaces before semantic assertions.

## Final gates

- `cargo test --workspace -- --test-threads=1`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `bun run typecheck`
- `bun run verify`
- frozen dependency install with no lockfile changes
- `git diff --check`
- independent Spec and quality review before the final local commit
