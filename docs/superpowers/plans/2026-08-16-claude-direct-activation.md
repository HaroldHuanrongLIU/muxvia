# Claude Code Direct Activation Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver safe Claude Code Direct Activation for both explicit Claude authentication profiles, with exact JSON ownership, T05-compatible recovery, transactional state, Control Plane workflow, and real-process proof that no model route starts.

**Architecture:** Deepen the existing Claude Target Configuration adapter and reuse the target-aware `ActivationService` transaction. Add one internal persisted ownership-version seam so T05 three-field Takeover/recovery state remains readable while new Claude activations use the four-field Direct contract. Keep JSON/authentication behavior target-native and keep receipt, recovery, final commit, and publication ordering shared.

**Tech Stack:** Rust 2024, Tokio, rusqlite/SQLite, serde/serde_json, existing Managed File seam, Bun 1.3.14, TypeScript, SolidJS/OpenTUI 0.4.3, real UDS/process/loopback test harnesses.

---

## Binding constraints

- Implement GitHub issue #7 and the approved design at `docs/superpowers/specs/2026-08-16-claude-direct-activation-design.md`.
- Direct supports both `anthropic-api-key` and `anthropic-bearer`. The active credential variable is set and the inactive one is absent.
- Only `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_MODEL`, `env.ANTHROPIC_AUTH_TOKEN`, and `env.ANTHROPIC_API_KEY` enter the T06 ownership contract. Never own the complete `env` object or settings document.
- Historical T05 Claude ownership remains three-field. Never infer the lost API-key value from a fingerprint and never reinterpret a legacy pending Recovery Intent as four-field.
- Direct never binds a listener, creates or reads a Routing Credential, starts a Model Server, or performs runtime handoff.
- Existing active Takeover is rejected; safe Takeover removal belongs to T10.
- Every post-credential assertion must scan first and collapse failure diagnostics to fixed secret-free text.
- Use `apply_patch` for edits, preserve unrelated work, and commit each task separately.

## File structure

- `crates/routing-service/src/state/schema.sql` and `state/migrations.rs`: schema-v6 internal managed-configuration version and transactional v5 migration.
- `crates/routing-service/src/state/store.rs`: load and atomically commit the internal ownership version with activation state.
- `crates/routing-service/src/state/recovery.rs`: retain typed legacy/current Claude recovery payload interpretation.
- `crates/routing-service/src/claude/config.rs`: Claude JSON ownership, Direct/Takeover desired states, typed observation, merge/verify/restore/reconcile behavior.
- `crates/routing-service/src/service/activate.rs`: target-aware mode branch and committed-state reconstruction without listener work for Direct.
- `packages/control-plane/src/commands/catalog.ts` and `ui/app.tsx`: expose the existing named Direct workflow to Claude without adding a second input path.
- `packages/control-plane/src/ui/provider-picker.tsx` and i18n catalogs: Claude picker capability and target-neutral localized Direct copy.
- Existing Rust, OpenTUI, and walking-skeleton test files remain the production-interface test surfaces; do not add a test-only product seam.

---

## Chunk 1: Persistence and Claude configuration adapter

### Task 1: Version Claude managed-configuration ownership without breaking T05

**Files:**
- Modify: `crates/routing-service/src/state/schema.sql`
- Modify: `crates/routing-service/src/state/migrations.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/state/recovery.rs`
- Modify: `crates/routing-service/src/claude/config.rs`
- Modify: `crates/routing-service/tests/provider_declarations.rs`
- Modify: `crates/routing-service/tests/state_store.rs`
- Modify: `crates/routing-service/tests/recovery.rs`
- Modify affected direct-SQL fixtures only where the new column changes an explicit projection or insertion.

**Interface:** Persist an opaque `managed_config_version` on `target_route_state`. Codex requires/commits version 1. Existing Claude state migrates as version 1; new Claude activation commits version 2. `ActivationPreparation` exposes the validated version, and `commit_activation_for` receives the version to update in its final transaction. No wire projection changes.

- [ ] **Step 1: Write fresh-schema and v5 migration RED tests**

Add assertions that:

- `SCHEMA_VERSION == 6`;
- fresh `target_route_state` has a checked, non-null `managed_config_version` initialized to 1;
- a real v5 database with inactive Claude state migrates to version 1 without changing existing rows, receipts, snapshots, recovery payload bytes, credentials, or Provider declarations;
- a real v5 committed Claude Takeover with an unrelated `ANTHROPIC_API_KEY` reopens as version 1;
- Claude with an unknown version and Codex with version 2 both return an authoritative target-scoped `recovery-required` result instead of a generic State error; and
- startup persists Recovery Required for only that invalid target, opens it control-only, and still resumes a clean peer target.

Use boolean/fixed diagnostics for rows containing credentials or recovery JSON.

- [ ] **Step 2: Run the migration RED**

```bash
cargo test -p muxvia-routing --test provider_declarations schema_v6_ -- --nocapture
cargo test -p muxvia-routing --test state_store managed_config_version_ -- --nocapture
```

Expected: FAIL because the schema is v5 and the ownership column/interface does not exist.

- [ ] **Step 3: Implement the minimal schema-v6 migration**

Add the column with a closed check:

```sql
managed_config_version INTEGER NOT NULL DEFAULT 1
  CHECK (managed_config_version IN (1, 2))
```

Add a v5→v6 `IMMEDIATE` migration that only adds the column, updates metadata after success, runs `PRAGMA foreign_key_check`, and commits atomically. Extend every older-version chain through v6. Do not rewrite receipts or Recovery Intents because the public wire shape is unchanged.

- [ ] **Step 4: Write legacy recovery interpretation RED tests**

Persist exact T05 Claude payload JSON that lacks `ANTHROPIC_API_KEY` and an ownership-version field. Cover:

- before-state match marks the intent rolled back without touching an unrelated API-key value;
- desired-state match restores the original three owned values without touching the API-key value;
- changing only the API-key value changes the stored unrelated fingerprint and becomes a third state;
- a new version-2 payload serializes an explicit version and all four prior/desired values or absence; and
- Debug/error/panic text contains none of the injected Provider/API-key/Auth-token sentinels.

- [ ] **Step 5: Run the recovery RED**

```bash
cargo test -p muxvia-routing --test recovery claude_ownership_version_ -- --nocapture
```

Expected: FAIL because the typed payload does not distinguish the historical three-field contract from the new four-field contract.

- [ ] **Step 6: Implement typed legacy/current recovery payloads**

Introduce a small Claude-owned enum such as:

```rust
enum ClaudeConfigOwnership {
    LegacyThree,
    FourField,
}
```

New snapshots/desired states serialize version 2. Missing version deserializes as `LegacyThree`. Capture, unrelated projection, equality, write, verify, restore, and reconciliation must use the payload's original ownership. A legacy payload leaves `ANTHROPIC_API_KEY` in the unrelated semantic projection; a new payload captures it as an owned value. Keep custom redacted `Debug` implementations.

- [ ] **Step 7: Thread the persisted version through StateStore**

Select and validate `managed_config_version` in `prepare_activation_for`. Add it to `ActivationPreparation`. The only valid pairs are Codex/version 1 and Claude/version 1 or 2. Invalid pairs must produce/persist the same target-scoped Recovery Required state used for inconsistent committed activation state; they must not escape as a generic database error or block a clean peer target. Extend `commit_activation_for` so the final `UPDATE target_route_state` writes the requested version in the same transaction as Current, Snapshot, runtime, recovery, receipt, and view projection. Codex callers pass 1; new Claude callers will pass 2 in Task 3.

- [ ] **Step 8: Run Task 1 verification**

```bash
cargo test -p muxvia-routing --test provider_declarations
cargo test -p muxvia-routing --test state_store
cargo test -p muxvia-routing --test recovery
cargo test -p muxvia-routing --test activation
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Expected: all pass. Existing T05 Takeover and pending recovery behavior is byte/semantic compatible, and no Target View fixture changes.

- [ ] **Step 9: Commit Task 1**

```bash
git add crates/routing-service/src/state crates/routing-service/src/claude/config.rs crates/routing-service/tests
git commit -m "feat: version claude configuration ownership"
```

---

### Task 2: Encode four-field Claude Direct desired state and typed ownership

**Files:**
- Modify: `crates/routing-service/src/claude/config.rs`
- Modify: `crates/routing-service/src/claude/mod.rs`
- Modify: `crates/routing-service/tests/claude_config.rs`
- Modify: `crates/routing-service/tests/recovery.rs`

**Interface:** Keep one Claude codec interface with target-native constructors:

```rust
fn desired_direct(
    &self,
    model: &str,
    base_url: &str,
    authentication: ProviderAuthentication,
    provider_credential: &str,
) -> Result<DesiredClaudeState, ClaudeProblem>;

fn desired_takeover_v2(
    &self,
    model: &str,
    base_url: &str,
    routing_credential: &str,
) -> DesiredClaudeState;
```

Expose a crate-internal typed observation `ManagedClaudeState::{Unmanaged, Direct, Takeover}` that requires caller-supplied committed expectations. Do not add a public ownership-inference helper.

- [ ] **Step 1: Write exact Bearer/API-key Direct RED tests**

From files containing both credential variables and unrelated nested JSON, assert:

- Bearer Direct writes base/model/Auth token and removes API key;
- API-key Direct writes base/model/API key and removes Auth token;
- all unrelated semantic JSON and the exact existing file mode remain unchanged;
- an absent file is private under restrictive umask;
- `restore` reinstates both credential values or prior absence exactly while preserving unrelated edits made after apply; and
- raw settings, credentials, and their byte/numeric signatures never appear in diagnostics.

- [ ] **Step 2: Run the Direct codec RED**

```bash
cargo test -p muxvia-routing --test claude_config direct_ -- --nocapture
```

Expected: FAIL because `desired_direct` and four-field ownership do not exist.

- [ ] **Step 3: Implement four-field capture/apply/restore**

For version 2, capture both credential fields. Construct desired state exactly:

```text
anthropic-bearer  => AUTH_TOKEN=credential, API_KEY=absent
anthropic-api-key => API_KEY=credential, AUTH_TOKEN=absent
```

Reject any other target/authentication pairing with a stable pre-write problem. `desired_takeover_v2` sets the Routing Credential in Auth token and makes API key absent. Preserve the version-1 constructor/interpretation only for committed legacy validation and recovery.

- [ ] **Step 4: Write typed-state and transition RED tests**

Prove:

- a caller-authorized exact Direct expectation yields `Direct`;
- an exact Takeover expectation yields `Takeover`;
- a matching file without a caller-supplied committed expectation observes exactly as `Unmanaged`, so a valid first activation may capture it as the before state without inferring prior Muxvia ownership;
- a caller-supplied committed Direct or Takeover expectation with any owned mismatch is a distinct drift/collision result that the activation layer maps to `recovery-required`;
- Direct Bearer→Direct API-key and API-key→Takeover change only approved fields;
- drift in any approved field or unrelated semantic tree is detected;
- first activation may capture existing approved values but does not call them managed; and
- legacy Takeover validation treats API key as unrelated, while a version-1→2 authorized transition captures it in the before state and can restore it.

- [ ] **Step 5: Run focused and regression GREEN**

```bash
cargo test -p muxvia-routing --test claude_config
cargo test -p muxvia-routing --test recovery
cargo test -p muxvia-routing --test codex_config
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Expected: all pass, including the existing Managed File adversarial tests and T05 legacy recovery tests.

- [ ] **Step 6: Commit Task 2**

```bash
git add crates/routing-service/src/claude crates/routing-service/tests/claude_config.rs crates/routing-service/tests/recovery.rs
git commit -m "feat: encode claude direct configuration"
```

---

## Chunk 2: Transactional activation and Control Plane

### Task 3: Activate Claude Providers directly through the shared transaction

**Files:**
- Modify: `crates/routing-service/src/service/activate.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/state/recovery.rs`
- Modify: `crates/routing-service/tests/activation.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `crates/routing-service/tests/recovery.rs`
- Modify: `crates/routing-service/tests/process_lifecycle.rs`

**Interface:** Remove the temporary Claude-Direct rejection at the raw action boundary. Keep `activate-provider { mode: direct }` unchanged on the wire. The Direct branch accepts either target configuration adapter and always produces `ActivationRuntime::Direct`; only the Takeover branch may reserve/reuse a model runtime.

- [ ] **Step 1: Write production-boundary Claude Direct success RED tests**

Through `ActivationService::apply_raw` with real SQLite/settings/credential and deterministic Claude probe, assert both authentication profiles:

- return `Applied` with top-level mode `direct`, exact Current, null Serving, immutable Snapshot, applied managed path, and restart required;
- write the exact four approved settings;
- commit ownership version 2, one completed Recovery Intent, and one receipt;
- increment management revision/view sequence once and publish exactly one complete Claude view;
- never reach `BindListener`, `PersistRoutingCredential`, or `RuntimeHandoff`;
- leave route port and Routing Credential null, keep both model slots unchanged, and expose no endpoint; and
- replay a malformed same-action body before context/provider/file work and without publication.

- [ ] **Step 2: Run the success RED**

```bash
cargo test -p muxvia-routing --test activation claude_direct_ -- --nocapture
```

Expected: FAIL at the existing `unsupported-activation-mode`/Claude Direct rejection.

- [ ] **Step 3: Write transition and immutable-state RED tests**

Cover:

- Bearer Direct A→API-key Direct B and reverse profile replacement;
- Direct→Takeover writes version-2 Takeover and starts the route only after commit;
- version-1 committed Takeover hot-switch upgrades safely to version 2;
- a new service epoch resumes an exact version-1 committed Takeover without rewriting or newly owning its unrelated API-key value, while a version-2 Takeover requires API key absence;
- Takeover→Direct returns `takeover-active` before intent or file work;
- edits to the old/current Provider declaration do not redefine the committed managed expectation;
- stale final commit restores the previous Direct state and accepts a same-action retry; and
- clean Direct restart is control-only and exits after the last accepted session/pending action drains.

- [ ] **Step 4: Write fail-closed and fault-injection RED tests**

Use secret-safe file/DB/auth fingerprints and the activation observer. Require no mutation for:

- Routing-required and incomplete Providers;
- all six selector/host-managed blockers, inconsistent context, and nondefault home;
- managed/shared/local shadows, invalid JSON, file symlink/identity race, and committed drift;
- incompatible probe and stale revision.

Inject every post-intent boundary (`RecoveryIntent`, `AtomicConfigWrite`, `ConfigVerify`, `StateAndReceiptCommit`, and final revision pause/race). Exact before or desired restores; a third state/restore failure marks only Claude Recovery Required, publishes no Direct success, and leaves Codex unchanged.

The observer failpoint before `commit_activation_for` is not sufficient evidence for a SQLite failure. Through the existing separate fixture connection, install an ordinary per-fixture trigger in the temporary test database that raises `ABORT` on the exact Claude `target_route_state` activation update after the configuration write. Because the trigger belongs to the database schema rather than one connection, it exercises the private real `StateStore` connection without adding a test-only production seam. Require the real transaction error to roll back all database changes and restore configuration for:

- first Direct from unmanaged state;
- Direct Bearer→Direct API-key;
- Direct→Takeover, including candidate listener cleanup and restoration of the prior Direct authentication profile; and
- version-1 Takeover→version-2 Takeover upgrade, including restoration of the legacy unrelated API-key value and continued service by the old runtime.

Remove the trigger after each test through RAII/test cleanup; the whole database is already fixture-local. Trigger/error diagnostics must be fixed and secret-free.

- [ ] **Step 5: Implement the minimal target-native Direct branch**

Remove only the early Claude Direct rejection. In `preflight_configuration`, reconstruct the prior expectation from `preparation.managed_config_version`, prior immutable Snapshot, and optional route runtime:

```text
v1 + Takeover => validate legacy three-field expected state, then capture v2 before for an authorized upgrade
v2 + no runtime => validate Direct from snapshot authentication/base/model/credential
v2 + runtime    => validate four-field Takeover
inconsistent    => recovery-required
```

For `ActivationMode::Direct`, build either Codex or Claude desired state and return `ActivationRuntime::Direct` with no candidate handle. Pass ownership version 2 only for Claude commits and version 1 for Codex. Keep receipt checks, intent, apply, verify, commit, rollback, and publication shared.

- [ ] **Step 6: Verify real UDS receipt and target isolation**

Add a real control-socket action test that opens Claude with a valid context, applies Direct, observes response-before-one-push, replays the receipt, and proves no Codex push/state change. Assert the wire carries no provider credential or settings payload.

- [ ] **Step 7: Run Task 3 verification**

```bash
cargo test -p muxvia-routing --test activation
cargo test -p muxvia-routing --test recovery
cargo test -p muxvia-routing --test control_socket
cargo test -p muxvia-routing --test process_lifecycle -- --test-threads=1
cargo test -p muxvia-routing --test claude_config
cargo test -p muxvia-routing --test protocol_contract
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Expected: all pass. Run UDS/loopback suites with approved local-socket permission if the sandbox returns `EPERM`; do not alter product behavior to accommodate the sandbox.

- [ ] **Step 8: Commit Task 3**

```bash
git add crates/routing-service/src crates/routing-service/tests
git commit -m "feat: activate claude providers directly"
```

---

### Task 4: Expose Claude Direct in the existing named-command workflow

**Files:**
- Modify: `packages/control-plane/src/commands/catalog.ts`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/ui/provider-picker.tsx` only if its capability interface needs a target-neutral adjustment
- Modify: `packages/control-plane/src/i18n/en.ts`
- Modify: `packages/control-plane/src/i18n/zh-cn.ts`
- Modify: `packages/control-plane/test/commands.test.tsx`
- Modify: `packages/control-plane/test/provider-workflow.test.tsx`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/localization.test.ts`
- Modify: `packages/control-plane/test/responsive-shell.test.tsx` only for visible Claude Direct status/copy assertions

**Interface:** Reuse the existing command IDs `target.direct.apply` and `provider.activate.direct`. Extend their scopes/capability to Claude. Do not add a Claude-specific command ID or keyboard handler.

The existing picker bindings would collide if both commands retained `<leader>a`. Preserve the established Direct picker binding `<leader>a` for Codex and Claude, and change the Claude-only `provider.activate.takeover` picker binding to `<leader>o` (takeOver). The route-level `target.takeover.apply` remains `<leader>a` because it is not active in the picker scope.

- [ ] **Step 1: Write named-command RED tests**

Using the real keymap provider and OpenTUI renderer, require `/direct`, route-level `<leader>d`, palette selection, and picker-level `<leader>a` to execute the exact existing Direct command identity against the Claude session. Require picker-level `<leader>o` to execute Takeover and prove neither key produces duplicate/binding-reject dispatch. Retain the Codex identity tests and prove overlays still suppress background commands.

- [ ] **Step 2: Run the command RED**

```bash
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/provider-workflow.test.tsx --test-name-pattern "Claude.*Direct|Direct.*Claude"
```

Expected: FAIL because Claude is excluded from both Direct command scopes and `allowDirect`.

- [ ] **Step 3: Write workflow and target-isolation RED tests**

Cover:

- Current Provider default, first-Provider fallback, and exact selected picker identity;
- local Incomplete rejection without an action;
- projected and authoritative `takeover-required` confirmation with only Takeover/cancel;
- pending label/action gating, success view install, restart guidance, stable failure localization, and picker overlay identity;
- switching to Codex while Claude Direct is pending cannot install Claude notices/activity/view into Codex;
- target return shows the committed Claude result once;
- double/scheduled activation dispatches once and cancel restores exact focus; and
- sentinel credentials never occur in captured frames, activities, view fixtures, or diagnostics.

- [ ] **Step 4: Implement minimal capability changes**

- Add `claude` to the existing Direct command scopes.
- Set Provider picker `allowDirect` for both real targets.
- Change only the Claude picker Takeover binding from `<leader>a` to `<leader>o`; keep route-level Takeover and existing Direct bindings unchanged.
- Remove the `target === "claude" && mode === "direct"` early return.
- Register `target.direct.apply` in the Claude route layer beside Takeover.
- Make existing Direct command description/restart text target-neutral or target-interpolated in both catalogs, and verify the presenter exposes the distinct Direct/Takeover picker bindings; do not duplicate the workflow.
- Preserve target-keyed pending state, origin overlay token, active-target checks, and centralized command dispatch.

- [ ] **Step 5: Run Task 4 verification**

```bash
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/overlays.test.tsx
bun test packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/app-render.test.tsx
bun test packages/control-plane/test/localization.test.ts packages/control-plane/test/responsive-shell.test.tsx
bun run typecheck
git diff --check
```

Expected: all pass with real renderer/keymap interactions and no new global keyboard listener.

- [ ] **Step 6: Commit Task 4**

```bash
git add packages/control-plane/src packages/control-plane/test
git commit -m "feat: expose claude direct activation"
```

---

## Chunk 3: Real-process tracer and final verification

### Task 5: Prove Claude Direct end to end and finish the branch

**Files:**
- Modify: `packages/control-plane/test/walking-skeleton.e2e.tsx`
- Modify: `tests/e2e/walking-skeleton.test.ts` only if the existing side-effect import or timeout must change for the new real tracer
- Modify: `.superpowers/sdd/2026-08-16-claude-direct-activation/task-5-report.md` as the local task report if that directory is ignored by repository policy
- Do not modify production, `process_lifecycle.rs`, fake upstreams, or CI unless the tracer first proves a real cross-layer product defect that an owning focused test reproduces.

**Contract:** Use the real Routing Service binary, real UDS/RpcClient/TargetSession, real OpenTUI renderer and named commands, SQLite, temporary HOME/Muxvia Home, fake read-only Claude executable, and real `settings.json`. Direct must remain control-only; no fake Messages upstream is needed for Direct success.

- [ ] **Step 1: Extend the real-process acceptance tracer**

In one temporary isolated environment:

1. seed unrelated settings, both prior credential variables, restrictive file mode, and trap files outside the explicit home;
2. start the real Routing Service and open real Codex/Claude sessions;
3. create complete Claude Bearer and API-key Providers through the real TUI workflow;
4. Direct Activate Bearer through `/direct` or the picker and assert response/push/restart state;
5. Direct Activate API-key through the other named path and assert exact replacement;
6. close every first-epoch TargetSession/RpcClient, wait for the real service to exit naturally with status 0 after its pending-action/session drain, and require UDS removal and no TCP listener;
7. start a second service epoch, reconnect, and prove Current/Snapshot/settings persist without a model listener;
8. inject a controlled failed activation/final-commit case through an existing deterministic product failpoint only if that seam is already available to real-process tests; otherwise leave fault injection to Task 3; and
9. close every second-epoch session and again require natural status-0 idle exit, UDS removal, and no TCP listener. Explicit shutdown/SIGKILL is failure cleanup only and cannot satisfy the successful-path assertion.

Before each semantic assertion, scan raw/captured surfaces and throw only fixed diagnostic codes.

- [ ] **Step 2: Run the tracer and classify the first result**

```bash
bun test ./packages/control-plane/test/walking-skeleton.e2e.tsx --test-name-pattern "Claude Direct"
```

Expected: this is post-implementation acceptance coverage, so it may be GREEN on its first run. If it fails, classify the failure as product, harness, or environment before changing code. Do not manufacture a test-only RED or alter production when Tasks 1–4 already satisfy the real cross-layer contract.

- [ ] **Step 3: Add mutation-sensitive security and ownership checks**

Prove fixed-diagnostic detection for:

- credential in a Target View, receipt, action error, activity, renderer frame, process output, or disallowed SQLite/recovery/settings location;
- API key and Bearer token simultaneously present after either successful Direct mode;
- unrelated JSON or file mode mutation;
- an unexpected TCP listener owned by the Routing Service process;
- a legacy recovery payload incorrectly treating API key as owned; and
- a late audit failure after an earlier functional failure.

Reuse the existing ordered final-audit accumulator, process-output drain, raw frame scanner, SQLite cell scanner, settings semantic audit, and platform-specific listener observer. Do not add boolean-only projections that discard the evidence before scanning.

- [ ] **Step 4: Run focused cross-layer verification**

```bash
bun test ./packages/control-plane/test/walking-skeleton.e2e.tsx
bun test ./tests/e2e/walking-skeleton.test.ts
cargo test -p muxvia-routing --test activation
cargo test -p muxvia-routing --test recovery
cargo test -p muxvia-routing --test claude_config
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/app-render.test.tsx
bun run typecheck
git diff --check
```

Expected: all pass; use approved UDS/loopback permission where required.

- [ ] **Step 5: Run the complete repository gate**

```bash
bun install --frozen-lockfile
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bun run typecheck
bun run verify
git diff --check
git status --short
```

Expected: every command exits 0, lockfiles remain unchanged, and only intentional T06 files are modified. Record exact test counts and any sandbox-only permission rerun in the report.

- [ ] **Step 6: Request two-axis final review**

Run independent Standards and Spec reviews of the full T06 base-to-head diff. Fix every genuine Critical/Important issue with a focused RED→GREEN cycle, rerun the affected suite, and repeat review until approved. Treat refactor-only judgments as advisory unless they affect the issue contract.

- [ ] **Step 7: Commit Task 5 and any reviewed fixes**

```bash
git add packages/control-plane/test/walking-skeleton.e2e.tsx tests/e2e/walking-skeleton.test.ts
git commit -m "test: prove claude direct activation end to end"
```

Use additional narrowly scoped `fix:` commits only when review exposes a real defect. Do not push or merge without explicit Operator approval.

---

## Completion criteria

- Both Claude authentication profiles Direct Activate through the same public action and named-command identities.
- T05 three-field Takeover and pending recovery remain safe and exact after schema-v6 migration.
- New Direct/Takeover writes use deterministic four-field ownership and exact rollback.
- All fail-closed conditions perform no managed write or provisional runtime work.
- Direct commits no listener, port, Routing Credential, or Serving state and permits idle service exit.
- Control Plane behavior is target-keyed, localized, overlay-safe, and secret-free.
- Real-process and mutation-sensitive tests prove settings, SQLite, recovery, RPC, renderer, process, and listener boundaries.
- Full verification and independent Standards/Spec review are green before branch handoff.
