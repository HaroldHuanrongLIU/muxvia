# Configuration Drift, Shadowing, and Compatibility Reconciliation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Codex CLI and Claude Code one explicit, previewed, target-isolated workflow for compatibility classification and Configuration Drift resolution through Adopt, Reapply, or zero-in-flight Restore.

**Architecture:** Add a crate-internal Reconciliation Coordinator beside ActivationService. It owns ephemeral observation tokens, compatibility acknowledgement, durable intent/receipt ordering, rollback, and publication; existing Codex and Claude codecs remain the only target-native configuration authorities. Extend the existing target-scoped UDS and OpenTUI shell with one shared preview/apply workflow rather than target-specific action paths.

**Tech Stack:** Rust 2024, Tokio, rusqlite/SQLite, serde/serde_json, existing Managed File and model runtime seams, Bun 1.3.14, TypeScript, Zod, SolidJS/OpenTUI 0.4.3, real UDS/loopback/process test harnesses.

## Global Constraints

- Implement GitHub issue #8 and `docs/superpowers/specs/2026-08-16-configuration-drift-reconciliation-design.md`.
- Configuration Drift blocks ordinary Provider save, activation, synchronization, and restore writes only for the affected Target; preview and explicit reconciliation remain available.
- Unknown-compatible acknowledgement persists only for one exact Target/version pair and never changes the classification to tested.
- Adopt creates a new ordinary Target Provider, a new Credential Reference when required, and a new immutable Activated Snapshot; it never edits the prior Provider or credential.
- Reapply uses the committed Snapshot and bound Recovery Intent, never an editor draft.
- Restore requires zero in-flight requests; otherwise it returns `target-busy` with a complete no-mutation fingerprint. T10 retains normal drain-and-remove ownership.
- Preview is read-only. Apply re-observes revision, probe, shadows, canonical home, file identity, owned and unrelated semantics, Snapshot, and Recovery Intent.
- Observation tokens are opaque, ephemeral, target/strategy-bound, replaced by a newer preview for the same Target/strategy, and invalid after service restart.
- Existing requests remain pinned to their starting Snapshot. Known shadows are reported by closed source identity and never modified.
- Directory symlinks are canonicalized; managed-file symlinks remain blocked on macOS and Linux.
- All public, wire, and test diagnostics are scan-first and secret-free. Raw configuration, recovery payloads, credentials, Routing Credentials, and backend messages never enter a Target View or preview.
- Use `apply_patch`, preserve unrelated work, witness the exact RED before production edits, and commit every task separately.

## File Structure

- `crates/routing-service/src/service/reconcile.rs`: target-neutral preview/apply coordinator, bounded tokens, rollback, and publication.
- `crates/routing-service/src/service/reconciliation_adapter.rs`: crate-internal Codex/Claude enum adapter; no plugin ABI.
- `crates/routing-service/src/state/reconciliation.rs`: compatibility acknowledgement, preparation, durable intent, and final transaction.
- `crates/routing-service/src/state/schema.sql` and `state/migrations.rs`: schema-v8 storage.
- `crates/routing-service/src/control/protocol.rs` and `control/server.rs`: closed wire types and UDS dispatch.
- `crates/routing-service/src/codex/config.rs`, `claude/config.rs`, and probes: target-native observation and write/verify/restore.
- `packages/control-plane/src/control`: Zod parity and TargetSession API.
- `packages/control-plane/src/ui/reconciliation.tsx`: shared overlay workflow.
- Existing Rust, UDS, OpenTUI, and walking-skeleton tests remain the production-boundary surfaces.

---

## Chunk 1: Contract and target-native observation

### Task 1: Add schema-v8 reconciliation and compatibility contracts

**Files:**
- Modify: `crates/routing-service/src/state/schema.sql`
- Modify: `crates/routing-service/src/state/migrations.rs`
- Create: `crates/routing-service/src/state/reconciliation.rs`
- Modify: `crates/routing-service/src/state/mod.rs`
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `protocol/control-v1.schema.json`
- Create: `protocol/fixtures/preview-reconciliation.json`
- Create: `protocol/fixtures/apply-reconciliation.json`
- Modify: `crates/routing-service/tests/protocol_contract.rs`
- Modify: `crates/routing-service/tests/provider_declarations.rs`
- Modify: `crates/routing-service/tests/state_store.rs`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`

**Interfaces:**

```rust
enum ReconciliationStrategy { Adopt, Reapply, Restore }
enum CompatibilityClassification { Tested, UnknownCompatible, Incompatible }
enum ReconciliationFieldState { Present, Absent, Unchanged, Changed }

struct CompatibilityView {
    version: String,
    classification: CompatibilityClassification,
    acknowledgement_required: bool,
}

enum ShadowSource {
    CodexProfile,
    ClaudeManaged,
    ClaudeShared,
    ClaudeProject,
    ClaudeLocal,
    ClaudeSelector(ClaudeBlockingSelector),
    ClaudeHostManaged,
}

enum ProviderEffect { CreateNew, KeepCurrent, ExitManaged }

struct ReconciliationPreview {
    observation_token: Uuid,
    target: Target,
    strategy: ReconciliationStrategy,
    management_revision: u64,
    compatibility: CompatibilityView,
    shadow_sources: Vec<ShadowSource>,
    changes: Vec<ReconciliationFieldChange>,
    provider_effect: ProviderEffect,
    restart_required: bool,
    unobservable_runtime_boundary: bool,
}
```

Extend `ControlOperation` with `PreviewReconciliation { target, strategy, claude_context }`, `ControlResponse` with `ReconciliationPreview { preview }`, and `TargetAction` with `Reconcile { strategy, observation_token, acknowledge_version }`. Matching TypeScript types use the same closed discriminators.

- [ ] **Step 1: Write Rust and TypeScript wire RED tests**

Add exact preview/apply fixtures. Assert closed strategy/classification/source/field enums, positive revision, UUID token, additive-field acceptance, arbitrary source/strategy rejection, and secret-bearing additive field removal before any Debug/JSON assertion.

- [ ] **Step 2: Run the protocol RED**

```bash
cargo test -p muxvia-routing --test protocol_contract reconciliation_ -- --nocapture
bun test packages/control-plane/test/protocol.test.ts --test-name-pattern "reconciliation"
```

Expected: FAIL because the variants and schemas do not exist.

- [ ] **Step 3: Write schema-v8 migration RED tests**

Create an immutable real v7 fixture and assert v7→v8 preserves every Provider, credential, snapshot, route, receipt, and recovery payload byte while adding:

```sql
CREATE TABLE target_compatibility (
  target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
  observed_version TEXT NOT NULL,
  classification TEXT NOT NULL CHECK (classification IN ('tested', 'unknown-compatible', 'incompatible')),
  acknowledged_version TEXT,
  CHECK (acknowledged_version IS NULL OR classification = 'unknown-compatible')
);

CREATE TABLE reconciliation_intents (
  action_id TEXT NOT NULL,
  target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
  strategy TEXT NOT NULL CHECK (strategy IN ('adopt', 'reapply', 'restore')),
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  created_revision INTEGER NOT NULL CHECK (created_revision >= 0),
  before_json TEXT NOT NULL,
  desired_json TEXT NOT NULL,
  PRIMARY KEY (target, action_id)
);
```

Also assert a failed migration rolls back metadata and tables, then reruns successfully.

- [ ] **Step 4: Run the migration RED**

```bash
cargo test -p muxvia-routing --test provider_declarations schema_v8_ -- --nocapture
cargo test -p muxvia-routing --test state_store reconciliation_ -- --nocapture
```

Expected: FAIL because `SCHEMA_VERSION` is 7.

- [ ] **Step 5: Implement declarations and migration**

Add custom redacted Debug implementations. Add an `IMMEDIATE` v7→v8 migration, update metadata only after success, run `PRAGMA foreign_key_check`, and extend every older chain through v8. TypeScript/Zod and JSON schema must drop unknown secret fields and keep all discriminators closed.

- [ ] **Step 6: Implement acknowledgement state**

Persist exact Target/version classification. Same unknown version plus matching `acknowledged_version` is acknowledged; a version change clears it; tested needs none; incompatible forbids acknowledgement. Corrupt classification or acknowledgement combinations fail closed with fixed diagnostics.

- [ ] **Step 7: Run Task 1 GREEN and commit**

```bash
cargo test -p muxvia-routing --test protocol_contract
cargo test -p muxvia-routing --test provider_declarations
cargo test -p muxvia-routing --test state_store
bun test packages/control-plane/test/protocol.test.ts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
git add crates/routing-service/src/state crates/routing-service/src/control/protocol.rs crates/routing-service/tests protocol packages/control-plane/src/control/types.ts packages/control-plane/test/protocol.test.ts
git commit -m "feat: declare reconciliation contracts"
```

Expected: all pass and the commit contains only Task 1 files.

---

### Task 2: Deepen Codex and Claude observation adapters

**Files:**
- Modify: `crates/routing-service/src/codex/config.rs`
- Modify: `crates/routing-service/src/codex/probe.rs`
- Modify: `crates/routing-service/src/claude/config.rs`
- Modify: `crates/routing-service/src/claude/probe.rs`
- Create: `crates/routing-service/src/service/reconciliation_adapter.rs`
- Modify: `crates/routing-service/src/service/mod.rs`
- Modify: `crates/routing-service/tests/codex_config.rs`
- Modify: `crates/routing-service/tests/claude_config.rs`
- Modify: `crates/routing-service/tests/recovery.rs`

**Interfaces:**

```rust
enum TargetReconciliationAdapter {
    Codex(CodexConfigCodec),
    Claude(ClaudeConfigCodec),
}

struct ReconciliationObservation {
    file_identity: FileIdentity,
    owned_fingerprint: String,
    unrelated_fingerprint: String,
    compatibility: ProbedCompatibility,
    shadows: Vec<ShadowSource>,
    changes: Vec<ReconciliationFieldChange>,
}

enum PreparedConfiguration {
    Codex { before: DesiredCodexState, desired: DesiredCodexState },
    Claude { before: DesiredClaudeState, desired: DesiredClaudeState },
}

enum ReconciliationContext {
    Codex,
    Claude(ClaudePreflightContext),
}
```

`observe(strategy, committed, context)` returns an observation plus prepared configuration or a stable problem. Only redacted `changes`, classification, and closed shadow identities can reach the preview.

- [ ] **Step 1: Write observation RED tests**

Table-drive both Targets and all strategies. Owned changes become only presence/change states; unrelated edits affect only the opaque race fingerprint; model/base URL/credential values never enter the preview. Cover Claude legacy-three/current-four ownership, Codex TOML decor, and sibling `auth.json` identity.

- [ ] **Step 2: Write shadow/symlink RED tests**

Cover canonical directory symlinks; managed-file symlinks; Claude managed/shared/local/project sources and six selector/host blockers; Codex profile/global collision sources. Assert the closed source identity and exact shadow file bytes/mode/mtime remain unchanged.

- [ ] **Step 3: Run the adapter RED**

```bash
cargo test -p muxvia-routing --test codex_config reconciliation_ -- --nocapture
cargo test -p muxvia-routing --test claude_config reconciliation_ -- --nocapture
```

Expected: FAIL because the shared adapter and redacted summary do not exist.

- [ ] **Step 4: Implement probe projection**

Keep `--version` and `--help` as the only subprocess arguments. Exact pinned versions become tested; capability-bearing others unknown-compatible; missing, contradictory, nonzero, or non-UTF8 surfaces incompatible. Store no raw stdout/stderr. Add a mutation test proving an exact version change invalidates acknowledgement.

- [ ] **Step 5: Implement strategy preparation**

```text
Adopt   => observed owned fields are desired; observed unrelated fingerprint is preserved
Reapply => committed Snapshot/recovery desired is desired; observed unrelated state is preserved
Restore => bound recovery before is desired using its historical ownership version
```

Outside explicit reconciliation, owned mismatch remains `configuration-drift`, observable higher-priority state remains `shadowing-configuration`, and incompatible probe remains `incompatible-target-cli`.

- [ ] **Step 6: Prove secret-safe diagnostics by mutation**

Temporarily remove one owned-field redaction in each adapter and witness the controlled scan fail before semantic comparison. Restore redaction and prove Debug, Display, serialized projection, panic text, and numeric byte signatures omit every sentinel.

- [ ] **Step 7: Run Task 2 GREEN and commit**

```bash
cargo test -p muxvia-routing --test codex_config
cargo test -p muxvia-routing --test claude_config
cargo test -p muxvia-routing --test recovery
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
git add crates/routing-service/src/codex crates/routing-service/src/claude crates/routing-service/src/service crates/routing-service/tests
git commit -m "feat: observe target configuration drift"
```

Expected: all pass.

---

## Chunk 2: Preview and transactional reconciliation

### Task 3: Implement read-only previews and bounded tokens

**Files:**
- Create: `crates/routing-service/src/service/reconcile.rs`
- Modify: `crates/routing-service/src/service/mod.rs`
- Modify: `crates/routing-service/src/control/server.rs`
- Modify: `crates/routing-service/src/lib.rs`
- Create: `crates/routing-service/tests/reconciliation.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`

**Interfaces:**

```rust
struct ReconciliationService {
    state: Arc<StateStore>,
    tokens: Mutex<HashMap<(Target, ReconciliationStrategy), ObservationRecord>>,
    codex: CodexRuntimeContext,
    claude: ClaudeRuntimeContext,
}

async fn preview(
    &self,
    target: Target,
    strategy: ReconciliationStrategy,
    context: ReconciliationContext,
) -> Result<ReconciliationPreview, ControlProblem>;
```

A newer preview replaces the token for the same Target/strategy, limiting the registry to six records. Service construction always starts with an empty registry.

- [ ] **Step 1: Write preview no-mutation RED**

Fingerprint every SQLite table, both Target files, credentials, receipts, runtime slots, subscriptions, and published views. Preview every strategy/Target and require byte/semantic equality, zero listener changes, and zero pushes.

- [ ] **Step 2: Write token-binding RED**

Preview, then separately mutate Target revision, strategy, exact version, acknowledgement requirement, shadow, canonical home, file identity, owned semantics, unrelated semantics, Snapshot ID, Recovery Intent ID, and service epoch. Apply must return `stale-reconciliation-preview` before intent/file/runtime work.

- [ ] **Step 3: Run the preview RED**

```bash
cargo test -p muxvia-routing --test reconciliation preview_ -- --nocapture
cargo test -p muxvia-routing --test control_socket reconciliation_preview_ -- --nocapture
```

Expected: FAIL because preview dispatch and token storage do not exist.

- [ ] **Step 4: Implement bounded preview storage**

Use `(Target, ReconciliationStrategy)` as key and random UUID as opaque token. Never serialize `ObservationRecord`. Apply always re-observes through the adapter; token lookup alone cannot authorize a write.

- [ ] **Step 5: Wire read-only UDS dispatch**

Track preview like inspection work: disconnect/shutdown aborts and awaits it, the bounded writer cannot starve shutdown, and no Target View push is emitted.

- [ ] **Step 6: Run Task 3 GREEN and commit**

```bash
cargo test -p muxvia-routing --test reconciliation preview_
cargo test -p muxvia-routing --test control_socket reconciliation_preview_
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
git add crates/routing-service/src/service crates/routing-service/src/control/server.rs crates/routing-service/src/lib.rs crates/routing-service/tests
git commit -m "feat: preview target reconciliation"
```

Expected: all pass.

---

### Task 4: Apply Adopt, Reapply, and zero-in-flight Restore

**Files:**
- Modify: `crates/routing-service/src/service/reconcile.rs`
- Modify: `crates/routing-service/src/state/reconciliation.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/domain/view.rs`
- Modify: `crates/routing-service/src/model/server.rs`
- Modify: `crates/routing-service/tests/reconciliation.rs`
- Modify: `crates/routing-service/tests/activation.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `crates/routing-service/tests/process_lifecycle.rs`

**Interfaces:**

```rust
async fn apply(
    &self,
    target: Target,
    action_id: Uuid,
    expected_revision: u64,
    strategy: ReconciliationStrategy,
    observation_token: Uuid,
    acknowledge_version: Option<String>,
) -> Result<ActionOutcome, ActionFailure>;

fn active_request_count(&self) -> usize;
```

- [ ] **Step 1: Write Adopt RED tests**

For Codex and both Claude authentication shapes, externally drift Managed Configuration then apply an exact preview. Require a new ordinary Provider, new credential identity when needed, immutable Snapshot, Current selection, recovery binding, revision +1, one receipt, response-before-one-push, and unchanged old Provider/credential/snapshot.

- [ ] **Step 2: Write Reapply RED tests**

Drift every owned field and unrelated tree. Reapply restores committed owned values, preserves latest unrelated JSON/TOML and mode, clears drift, preserves Provider/Snapshot identities, and publishes once. An unrelated change after preview is stale rather than overwritten.

- [ ] **Step 3: Write Restore/target-busy RED tests**

With zero requests, Restore exact-restores the versioned before payload, stops only that Target listener, exits managed state, clears the applied route/current Snapshot projection, retains Provider/credential history, and leaves the peer live. With one held real request it returns `target-busy` with zero mutation; the request completes on its starting Snapshot.

- [ ] **Step 4: Run strategy RED tests**

```bash
cargo test -p muxvia-routing --test reconciliation adopt_ -- --nocapture
cargo test -p muxvia-routing --test reconciliation reapply_ -- --nocapture
cargo test -p muxvia-routing --test reconciliation restore_ -- --nocapture
```

Expected: FAIL because apply is unsupported.

- [ ] **Step 5: Implement write gates**

Tested proceeds; exact acknowledged unknown-compatible proceeds; unacknowledged returns `compatibility-acknowledgement-required`; incompatible returns `incompatible-target-cli`. Drift/incompatible blocks ordinary save, activation, synchronization, and ordinary restore for only that Target. Opening, preview, inspection, discovery, and reachability remain read-only available.

- [ ] **Step 6: Implement durable transaction ordering**

Check receipt, revision, token, and full re-observation; reject busy Restore; insert a pending intent; atomically write/verify; then commit strategy-specific Provider/Credential/Snapshot/route/problem/acknowledgement/intent/receipt state in one immediate transaction. Respond before exactly one push. Replay consumes no token and does no work.

- [ ] **Step 7: Add failpoint/race cycles**

Cover after-intent, atomic-write, verify, credential insert, Provider insert, Snapshot insert, final revision, listener stop, and final transaction. Pre-intent failures are no-mutation. Post-intent failures exact-restore/roll back. Restore verification failure marks only the target Recovery Required and rewrites receipt outcome for replay consistency.

- [ ] **Step 8: Run Task 4 GREEN and commit**

```bash
cargo test -p muxvia-routing --test reconciliation
cargo test -p muxvia-routing --test activation
cargo test -p muxvia-routing --test control_socket
cargo test -p muxvia-routing --test process_lifecycle
cargo test -p muxvia-routing --test recovery
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
git add crates/routing-service/src crates/routing-service/tests
git commit -m "feat: reconcile target configuration"
```

Expected: all pass with real UDS/loopback permission.

## Chunk 3: Control Plane and full-stack proof

### Task 5: Expose reconciliation through TargetSession

**Files:**
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/src/control/target-session.ts`
- Modify: `packages/control-plane/src/control/rpc-client.ts`
- Modify: `packages/control-plane/test/target-session.test.ts`
- Modify: `packages/control-plane/test/control-socket.test.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`

**Interfaces:**

```ts
interface TargetSession {
  previewReconciliation(
    strategy: ReconciliationStrategy,
    signal?: AbortSignal,
  ): Promise<ReconciliationPreview>
  applyReconciliation(input: {
    strategy: ReconciliationStrategy
    observationToken: string
    acknowledgeVersion?: string
  }): Promise<ActionOutcome>
}
```

Preview is not serialized behind mutations and supports AbortSignal. Apply uses the existing serialized `act` queue, fresh action UUID, and current management revision.

- [ ] **Step 1: Write TargetSession RED tests**

Prove exact target/context capture, abort, response-kind validation, apply serialization, authoritative stale-view replacement, close rejection, preview immutability, and Claude context retention across a reconciliation push gap.

- [ ] **Step 2: Run the RED**

```bash
bun test packages/control-plane/test/target-session.test.ts packages/control-plane/test/control-socket.test.ts --test-name-pattern "reconciliation"
```

Expected: FAIL because the methods and request kinds do not exist.

- [ ] **Step 3: Implement minimal session methods**

Reuse `RpcTransport.request` and AbortSignal handling. Do not keep tokens globally; return the immutable preview and pass only its token through the action.

- [ ] **Step 4: Run Task 5 GREEN and commit**

```bash
bun test packages/control-plane/test/protocol.test.ts packages/control-plane/test/control-socket.test.ts packages/control-plane/test/target-session.test.ts
bun run typecheck
git diff --check
git add packages/control-plane/src/control packages/control-plane/test/protocol.test.ts packages/control-plane/test/control-socket.test.ts packages/control-plane/test/target-session.test.ts
git commit -m "feat: expose reconciliation session API"
```

Expected: all pass.

---

### Task 6: Add the shared OpenTUI workflow

**Files:**
- Create: `packages/control-plane/src/ui/reconciliation.tsx`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/commands/catalog.ts`
- Modify: `packages/control-plane/src/i18n/en.ts`
- Modify: `packages/control-plane/src/i18n/zh-cn.ts`
- Modify: `packages/control-plane/src/i18n/index.ts`
- Modify: `packages/control-plane/test/commands.test.tsx`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/provider-workflow.test.tsx`
- Modify: `packages/control-plane/test/localization.test.ts`
- Modify: `packages/control-plane/test/responsive-shell.test.tsx`
- Modify: `packages/control-plane/test/secret-audit.ts`
- Modify: `packages/control-plane/test/secret-audit.test.ts`

**Interfaces:**

```text
target.reconciliation.open
target.reconciliation.preview.adopt
target.reconciliation.preview.reapply
target.reconciliation.preview.restore
target.reconciliation.apply
target.reconciliation.cancel
```

One modal serves both Targets, captures origin Target/session before awaiting, owns one preview generation, and closes only its exact overlay token.

- [ ] **Step 1: Write command/renderer RED tests**

Cover both Targets: open, strategy selection, preview, unknown-compatible acknowledgement, apply, cancel, stale preview, pending nondismissibility, duplicate suppression, target switching, exact focus restoration, one activity, and restart guidance. Prove no second global keyboard listener.

- [ ] **Step 2: Write gating RED tests**

Drift disables Provider save/activation only for the affected Target while inspection/discovery/reachability remain. Incompatible is read-only; unknown-compatible requires exact acknowledgement; version change reopens it; healthy peer remains fully usable.

- [ ] **Step 3: Write localization/size RED tests**

Require English/Chinese parity and exact copy for compatibility, shadows, unobservable boundary, field states, strategies, `target-busy`, stale preview, restart, and acknowledgement. Render at `1x1`, `2x2`, `20x5`, `40x10`, `80x24`, and `121x30`.

- [ ] **Step 4: Run the UI RED**

```bash
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/app-render.test.tsx packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/localization.test.ts packages/control-plane/test/responsive-shell.test.tsx --test-name-pattern "Reconciliation|reconciliation|drift|compatibility"
```

Expected: FAIL because the commands and modal do not exist.

- [ ] **Step 5: Implement target-local modal state**

Use disposal-scoped command layers and exact overlay identity. Keep preview/pending/error keyed by Target. Capture `originTarget`, `originSession`, and generation before await. Pending makes the modal nondismissible and disables background layers. Stale results affect only the matching origin modal and never auto-retry.

- [ ] **Step 6: Harden scan-first assertions**

Every frame predicate, action, activity, view, preview, caught error, timeout, and diagnostic scans controlled Provider/config/backend/settings sentinels before semantics and collapses to fixed labels. Mutation-test Error stack/custom/AggregateError, opposite branches, missing source, and secret-bearing preview extensions.

- [ ] **Step 7: Run Task 6 GREEN and commit**

```bash
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/overlays.test.tsx packages/control-plane/test/app-render.test.tsx packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/localization.test.ts packages/control-plane/test/responsive-shell.test.tsx packages/control-plane/test/secret-audit.test.ts
bun run typecheck
git diff --check
git add packages/control-plane/src packages/control-plane/test
git commit -m "feat: add target reconciliation workflow"
```

Expected: all pass.

---

### Task 7: Prove both Targets through the real-process tracer

**Files:**
- Modify: `packages/control-plane/test/walking-skeleton.e2e.tsx`
- Modify: `packages/control-plane/test/fixtures/pty-control-plane.tsx` only if the existing fixture needs the named entry point.
- Do not modify production unless the tracer proves a product defect and an owning focused RED is added first.

**Interfaces:** Consumes the real binary, UDS RpcClient/TargetSession, OpenTUI renderer, SQLite, native files, deterministic loopback servers, listener observer, and ordered security finalizer.

- [ ] **Step 1: Extend controlled security mutations**

Reject secrets misplaced into preview, Target View, outcome, receipt, activity, native frame, process output, SQLite text, recovery payload, target settings, or upstream requests. Fixed scanner diagnostics override earlier functional failures and every finalizer step still runs.

- [ ] **Step 2: Write the real tracer RED**

For Codex and Claude: activate; externally drift owned+unrelated fields; prove target-local blocking and peer/existing request continuity; Reapply; drift and Adopt new immutable identities; restart and prove same-version acknowledgement; change fake version and require acknowledgement; hold a routed request and prove `target-busy`; release, re-preview, Restore; verify only target listener stops; close sessions and require natural status 0 plus UDS removal.

Use event-driven barriers for every RPC result, push, upstream request, held request, listener transition, exit, and output drain. Never use fixed sleeps.

- [ ] **Step 3: Run the tracer RED**

```bash
cargo build --workspace
bun test ./packages/control-plane/test/walking-skeleton.e2e.tsx --test-name-pattern "real processes reconcile Codex and Claude configuration drift"
```

Expected: FAIL at the first unsupported reconciliation boundary. Classify and fix harness setup failures without production edits.

- [ ] **Step 4: Fix only proven cross-layer defects**

For each real defect, add an owning Rust/UDS/OpenTUI RED, witness it, implement the smallest production fix, rerun the owning suite, then return to the tracer. Do not add a test-only production seam or widen T10 scope.

- [ ] **Step 5: Run full verification**

```bash
cargo test --workspace -- --test-threads=1
bun test ./packages/control-plane/test/walking-skeleton.e2e.tsx
bun test ./tests/e2e/walking-skeleton.test.ts
bun install --frozen-lockfile
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
bun run verify
git diff --check
```

Expected: all pass with approved UDS/loopback permission; lockfiles remain unchanged.

- [ ] **Step 6: Report and commit**

Write ignored `.superpowers/sdd/2026-08-16-configuration-drift-reconciliation/task-7-report.md` with exact RED classifications, GREEN commands/counts, security surfaces, deviations, commit hash, and concerns. Then:

```bash
git add packages/control-plane/test/walking-skeleton.e2e.tsx packages/control-plane/test/fixtures/pty-control-plane.tsx
git commit -m "test: prove configuration reconciliation end to end"
```

---

## Final Review and Integration Gate

- [ ] Run whole-branch Spec review against issue #8, the approved design, ADR 0026, ADR 0045, and task reports.
- [ ] Run whole-branch Standards review against `AGENTS.md`, `CONTEXT.md`, domain, protocol/security, and diagnostic requirements.
- [ ] Address verified findings only through focused RED→GREEN and rerun owning suites.
- [ ] Run fresh `bun run verify`, fmt, Clippy with warnings denied, typecheck, and diff check after the last fix.
- [ ] Confirm clean worktree, preserve ignored reports, and use `superpowers:finishing-a-development-branch`. Do not push without explicit authorization.
