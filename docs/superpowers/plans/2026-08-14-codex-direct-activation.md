# Codex Direct Activation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver safe Codex Direct Activation: an Operator selects a complete Direct-compatible Provider, Muxvia writes only its approved Codex TOML fields, verifies and atomically commits Current plus an immutable Activated Snapshot, restores exact prior state on failure, and shows restart guidance without starting a model route.

**Architecture:** Extend the existing serialized `ActivationService` with explicit `ActivationMode::{Direct, Takeover}` and a mode-specific runtime preparation. Keep one receipt-first recovery transaction and one publication boundary. The Rust Routing Service remains authoritative for Provider routing requirements and file/database safety; the Bun/Solid/OpenTUI Control Plane dispatches named commands and never handles Provider credential bytes.

**Tech Stack:** Rust 1.96, Tokio, rusqlite/tokio-rusqlite, `toml_edit`, secrecy, serde, Bun 1.3.14, TypeScript, Solid, OpenTUI 0.4.3, `@opentui/keymap` 0.4.3.

## Global Constraints

- T04 changes the Codex Target only. Claude Direct Activation remains in #6.
- The only Direct-managed Codex fields are top-level `model`, top-level `model_provider`, and `model_providers.muxvia_codex.{name,base_url,wire_api,http_headers,supports_websockets}`.
- `auth.json` is never opened, parsed, rewritten, chmodded, or used as a credential source.
- Existing unrelated TOML values and semantic structure, prior owned-field absence, and file mode must survive apply/restore as already guaranteed by the codec.
- A directory symlink is canonicalized; the final managed file symlink, unsupported Configuration Home, reserved table collision, incomplete Provider, incompatible CLI, Recovery Required, stale revision, active Takeover, and Takeover-required Provider all fail before filesystem mutation.
- Ordinary T03 Provider creation defaults to `direct-compatible`. `routingRequirement` is server-owned and cannot be edited from the Provider form.
- Direct Activation creates no Routing Credential, binds no loopback port, starts no Model Server, and does not keep the Routing Service alive after the final Control Plane session closes.
- Success commits Current, clears Serving, creates a new immutable Activated Snapshot, leaves Takeover inactive, advances management revision/view sequence, commits the Recovery Intent and receipt, projects mode `direct`, and sets `restartRequired: true`.
- Direct-to-Direct and Direct-to-Takeover are supported. Takeover-to-Direct is rejected in T04; safe Takeover drain/removal belongs to T10.
- All persistent actions remain receipt-first. Replayed actions never write configuration or publish a duplicate Target View.
- Credential bytes never appear in views, receipts, problems, logs, Debug output, activity entries, renderer frames, or test failure diagnostics.
- Tests must use temporary `HOME`/Muxvia Home and fake CLI/upstream processes. Never inspect or mutate the Operator's real Codex files.

---

### Task 1: Schema-v3 routing requirement and Direct wire contract

**Files:**
- Modify: `crates/routing-service/src/state/schema.sql`
- Modify: `crates/routing-service/src/state/migrations.rs`
- Modify: `crates/routing-service/src/state/providers.rs`
- Modify: `crates/routing-service/src/domain/view.rs`
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `crates/routing-service/tests/provider_declarations.rs`
- Modify: `crates/routing-service/tests/protocol_contract.rs`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`
- Modify: `protocol/control-v1.schema.json`
- Modify: `protocol/fixtures/initial-target-view.json`
- Modify: `protocol/fixtures/save-provider.json`
- Modify: other checked-in Target View fixtures that contain Provider projections

**Interfaces:**
- Rename `TakeoverMode` to `ActivationMode` and expose exact wire literals `direct` and `takeover`.
- Add `ProviderRoutingRequirement::{DirectCompatible, TakeoverRequired}` with exact wire literals `direct-compatible` and `takeover-required`.
- Add `routing_requirement TEXT NOT NULL` to Provider storage and `routingRequirement` to `ProviderView`.
- Set schema version to 3 and upgrade stored v2 action receipt JSON inside the migration transaction.

- [ ] **Step 1: Write Rust and TypeScript protocol RED tests**

Require both activation modes and both routing requirements. A projected ordinary Provider must contain:

```json
{
  "id": "00000000-0000-4000-8000-000000000101",
  "position": 0,
  "providerRevision": 1,
  "name": "Direct Provider",
  "baseUrl": "https://provider.example/v1",
  "model": "model-a",
  "protocol": "openai-responses",
  "routingRequirement": "direct-compatible",
  "credential": "present",
  "completeness": "complete",
  "missingFields": [],
  "provenance": null,
  "generated": false,
  "activeReferences": []
}
```

Assert `activate-provider` accepts `mode: "direct"`, continues accepting `mode: "takeover"`, rejects unknown modes, and preserves additive unknown envelope fields.

- [ ] **Step 2: Run protocol tests and verify RED**

```bash
cargo test -p muxvia-routing --test protocol_contract
bun test packages/control-plane/test/protocol.test.ts
```

Expected: compilation/schema failures name missing `ActivationMode`, `ProviderRoutingRequirement`, `routingRequirement`, and the `direct` discriminator.

- [ ] **Step 3: Write a real schema-v2 migration RED test**

Create a v2 SQLite file with one ordinary Provider, one Current Provider, one Activated Snapshot, and one historical action receipt. Open it through `StateStore::open` and assert:

```rust
assert_eq!(schema_version, "3");
assert_eq!(provider.routing_requirement, ProviderRoutingRequirement::DirectCompatible);
assert_eq!(store.receipt(old_action_id).await?.unwrap().view.providers[0].routing_requirement,
           ProviderRoutingRequirement::DirectCompatible);
```

Then replay the old action ID through the raw production action boundary with a malformed replacement body and assert `Replayed`, proving receipt lookup still precedes parsing after migration.

- [ ] **Step 4: Run the migration test and verify RED**

```bash
cargo test -p muxvia-routing --test provider_declarations v2_database_migrates_routing_requirement_and_historical_receipts -- --exact
```

Expected: FAIL because schema version 2 has no routing requirement and historical receipts do not contain the new required projection field.

- [ ] **Step 5: Implement schema v3 and receipt upgrade**

Within one `BEGIN IMMEDIATE` migration:

```sql
ALTER TABLE providers
ADD COLUMN routing_requirement TEXT NOT NULL DEFAULT 'direct-compatible';

UPDATE metadata SET value = '3' WHERE key = 'schema-version';
```

Read, deserialize through the legacy-v2 receipt shape, enrich each Provider with `direct-compatible`, and rewrite each `outcome_json` before commit. Do not use Provider names, URLs, or protocol strings to infer ownership. Validate `PRAGMA foreign_key_check` after migration as before.

- [ ] **Step 6: Implement typed projection and create defaults**

Add the server-owned enum to Rust and Zod/TypeScript. Ordinary create, update, duplicate, Preset copy, and migration produce or preserve `DirectCompatible`; no client mutation action accepts this field. Keep deterministic Provider ordering and existing receipt semantics unchanged.

Update `project_target_view` mode rules:

```rust
let mode = match (takeover_state.as_str(), activated_snapshot_id.as_ref()) {
    ("active", _) => "takeover",
    (_, Some(_)) => "direct",
    _ => "unmanaged",
};
```

For `direct`, Managed Configuration is `applied`, carries the persisted path, and has `restartRequired: true`.

- [ ] **Step 7: Run Task 1 verification**

```bash
cargo test -p muxvia-routing --test protocol_contract
cargo test -p muxvia-routing --test provider_declarations
bun test packages/control-plane/test/protocol.test.ts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

Expected: all commands exit 0; migration replay is receipt-first and all projections contain exact additive wire values.

- [ ] **Step 8: Commit Task 1**

```bash
git add crates/routing-service packages/control-plane/src/control/types.ts packages/control-plane/test/protocol.test.ts protocol
git commit -m "feat: define direct activation contract"
```

---

### Task 2: Direct Codex desired state and managed-state inspection

**Files:**
- Modify: `crates/routing-service/src/codex/config.rs`
- Modify: `crates/routing-service/tests/codex_config.rs`
- Modify: `crates/routing-service/tests/recovery.rs`

**Interfaces:**
- Rename the current route-producing `desired` method to `desired_takeover`.
- Add `desired_direct(model, base_url, provider_credential)`.
- Add an internal typed managed-state projection that distinguishes `Unmanaged`, `Direct`, and `Takeover` without exposing secrets.

- [ ] **Step 1: Write direct desired-state RED tests**

Given unrelated comments, tables, arrays, and mode `0640`, applying Direct state must yield these exact managed semantics:

```toml
model = "model-a"
model_provider = "muxvia_codex"

[model_providers.muxvia_codex]
name = "Muxvia Direct"
base_url = "https://provider.example/api/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer provider-secret" }
supports_websockets = false
```

Assert unrelated semantic nodes remain equal, file mode remains `0640` even under restrictive umask, the input base path is not rewritten with `Url::join`, and Debug/errors never contain `provider-secret`.

- [ ] **Step 2: Write managed-state and restore RED tests**

Cover:

- unmanaged config with no owned fields;
- exact currently applied Direct state;
- exact currently applied Takeover state;
- forged/reserved `muxvia_codex` table rejection;
- table-shaped owned keys rejection;
- final-file symlink rejection and Configuration Home directory symlink canonicalization;
- prior absence restoration;
- exact prior item/decor restoration after Direct apply;
- unrelated changes made after apply survive restore;
- third-state owned or unrelated drift becomes `recovery-required`;
- a sibling `auth.json` sentinel retains bytes, mode, size, and mtime through apply, restore, and recovery reconciliation.

- [ ] **Step 3: Run focused codec tests and verify RED**

```bash
cargo test -p muxvia-routing --test codex_config direct_ -- --nocapture
cargo test -p muxvia-routing --test recovery direct_ -- --nocapture
```

Expected: compilation/behavior failures name missing Direct desired-state and mode-aware inspection interfaces.

- [ ] **Step 4: Implement explicit Direct and Takeover desired states**

Keep one owned-field merge engine, but build exact mode-specific `DesiredCodexState` values. Direct places the Provider bearer token only in the owned static Authorization header. Takeover retains the loopback base URL and Muxvia Routing Credential header. Never consult `auth.json`.

The inspection result must carry enough typed information for activation to verify an existing Direct configuration before Direct-to-Direct or Direct-to-Takeover transition. It must not infer ownership from matching user-provided values; only an exact previously committed Muxvia managed state is owned.

- [ ] **Step 5: Run Task 2 verification**

```bash
cargo test -p muxvia-routing --test codex_config
cargo test -p muxvia-routing --test recovery
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Expected: all commands exit 0; Direct/Takeover codecs remain distinct, recoverable, mode-preserving, and secret-safe.

- [ ] **Step 6: Commit Task 2**

```bash
git add crates/routing-service/src/codex/config.rs crates/routing-service/tests/codex_config.rs crates/routing-service/tests/recovery.rs
git commit -m "feat: encode direct codex configuration"
```

---

### Task 3: Transactional Direct Activation backend

**Files:**
- Modify: `crates/routing-service/src/service/activate.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/state/recovery.rs`
- Modify: `crates/routing-service/src/domain/view.rs`
- Modify: `crates/routing-service/src/control/server.rs`
- Modify: `crates/routing-service/tests/activation.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `crates/routing-service/tests/recovery.rs`
- Create: `protocol/fixtures/activate-provider.json`

**Interfaces:**
- `ActivationPreparation` carries `routing_requirement`, declaration data, optional prior route runtime, and enough committed managed-state data to verify ownership.
- Final commit accepts a typed runtime:

```rust
enum ActivationRuntime {
    Direct,
    Takeover { route_port: u16, routing_credential: SecretString },
}
```

- Stable Direct failures include `takeover-required` and `takeover-active`.

- [ ] **Step 1: Write Direct success RED tests through the production raw boundary**

Use real temporary SQLite/configuration files and invoke `ActivationService::apply_raw`. Assert one Direct action:

- returns `Applied`, mode `direct`, exact Current Provider, Serving `null`, new immutable snapshot, Applied Managed Configuration, and restart required;
- writes exact Direct owned fields;
- commits one action receipt and completed Recovery Intent;
- advances management revision and view sequence once and publishes exactly one complete view;
- never reaches `BindListener` or `PersistRoutingCredential` observer steps;
- leaves `route_port` and `routing_credential` null, has no `ModelServerHandle`, and exposes no loopback endpoint;
- replays a malformed second body with the same action ID without a write or publication.

- [ ] **Step 2: Run the focused Direct success test and verify RED**

```bash
cargo test -p muxvia-routing --test activation direct_activation_commits_config_snapshot_and_receipt_without_model_route -- --exact
```

Expected: FAIL because `direct` is not dispatched and activation always starts Takeover runtime.

- [ ] **Step 3: Write pre-mutation validation RED tests**

For every failure, fingerprint Provider/route/recovery/receipt SQLite rows plus `config.toml` and `auth.json`, then assert equality:

- Takeover-required Provider → `takeover-required`;
- active Takeover → `takeover-active`;
- incomplete Provider → `incomplete-provider`;
- stale management revision → `stale-revision`;
- Recovery Required → `recovery-required`;
- nondefault Configuration Home → `unsupported-configuration-home`;
- incompatible probe → `incompatible-target-cli`;
- reserved table collision or final-file symlink → existing stable Codex safety problem.

Also assert no Recovery Intent, listener, route credential, snapshot, receipt, or publication exists for the failed action.

- [ ] **Step 4: Write transition and rollback RED tests**

Cover:

- Direct A → Direct B: verifies A-owned fields, creates a new B snapshot, preserves A snapshot row, and keeps no route runtime;
- Direct → Takeover: verifies Direct ownership, then uses the existing listener/routing path and commits Takeover;
- Takeover → Direct: fails `takeover-active` before mutation;
- save-provider racing the final commit: final stale detection restores exact prior config and does not commit Current/snapshot/receipt;
- `AtomicConfigWrite`, `ConfigVerify`, and `FinalCommit` failpoints restore/confirm exact prior state;
- restore verification failure marks Recovery Required and does not advertise Direct;
- retry of a rolled-back unreceipted action ID succeeds without recovery-row collision;
- process restart after committed Direct opens only the control socket and does not bootstrap the Model Server.

- [ ] **Step 5: Implement the minimal mode-specific activation pipeline**

Keep receipt checks, gate, Provider/CLI/config validation, snapshot creation, Recovery Intent insertion, atomic write, reread verification, final revision recheck, and publication shared. Branch only at runtime-specific points:

```rust
match command.mode {
    ActivationMode::Direct => {
        reject_takeover_required_or_active();
        desired = codec.desired_direct(...);
        runtime = ActivationRuntime::Direct;
    }
    ActivationMode::Takeover => {
        desired = codec.desired_takeover(...);
        runtime = reserve_or_reuse_model_route(...).await?;
    }
}
```

For `Direct`, the final immediate transaction must set `takeover_state = 'inactive'`, null `route_port`, null `routing_credential`, null Serving, Current to the selected Provider, and persist the managed path. Do not stop an existing active Takeover in this task; reject it before intent.

Keep publication owned by the newly Applied commit only. Replay, validation failure, rollback, and recovery-required transitions must not emit a false success view.

- [ ] **Step 6: Run Task 3 verification**

```bash
cargo test -p muxvia-routing --test activation
cargo test -p muxvia-routing --test recovery
cargo test -p muxvia-routing --test control_socket
cargo test -p muxvia-routing --test protocol_contract
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Expected: all commands exit 0; the tests prove the complete Direct transaction, transitions, recovery, receipt, service restart, and no-listener boundary.

- [ ] **Step 7: Commit Task 3**

```bash
git add crates/routing-service protocol
git commit -m "feat: activate codex providers directly"
```

---

### Task 4: OpenCode-style Direct Activation workflow

**Files:**
- Modify: `packages/control-plane/src/commands/types.ts`
- Modify: `packages/control-plane/src/commands/catalog.ts`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/ui/provider-picker.tsx`
- Create: `packages/control-plane/src/ui/takeover-required-confirm.tsx`
- Modify: `packages/control-plane/src/i18n/en.ts`
- Modify: `packages/control-plane/src/i18n/zh-cn.ts`
- Modify: `packages/control-plane/test/commands.test.tsx`
- Modify: `packages/control-plane/test/provider-workflow.test.tsx`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/localization.test.ts`

**Interfaces:**
- `target.direct.apply`: slash `/direct`, Codex scope, binding `<leader>d`.
- `provider.activate.direct`: Provider-picker scope, binding `<leader>a`.
- `provider.activate.takeover-confirm` and `provider.activate.takeover-cancel`: modal-only named commands.

- [ ] **Step 1: Write named-command and rendering RED tests**

Through the real keymap provider and OpenTUI test renderer, prove:

- `/direct` and `<leader>d` dispatch the identical `target.direct.apply` command;
- target Direct selects Current or the first Provider when Current is absent;
- Provider-picker activation uses the selected row's exact Provider ID, not Current/first;
- a complete Direct-compatible Provider dispatches `activate-provider` with `mode: "direct"`;
- successful returned view is installed, one localized activity is appended, and restart guidance is visible;
- no Provider yields localized `activity.provider.required` and no action;
- incomplete and authoritative backend failures preserve authoritative view and show localized stable problems;
- all captured frames and recorded actions exclude credential sentinels.

- [ ] **Step 2: Run focused UI tests and verify RED**

```bash
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/app-render.test.tsx
```

Expected: failures name missing Direct commands, selected-row handler, and localized workflow.

- [ ] **Step 3: Write Takeover-required modal RED tests**

Project a complete Provider with `routingRequirement: "takeover-required"`. Assert Direct request opens a single modal whose only product actions are localized Enable Takeover and Cancel. Enter/`y` dispatches `mode: "takeover"` for that exact Provider; Escape/`n` closes without action. Background route/provider commands are disabled while open, `onClose` runs exactly once, focus restores to the Provider picker, and stale/double confirmation cannot dispatch twice.

Also make the backend return `takeover-required` for a view that initially projected `direct-compatible`; assert the same confirmation opens from the authoritative failure without retaining an error object or secret.

- [ ] **Step 4: Implement the minimal command workflow**

Share one `activateProvider(providerId, mode)` action helper that installs only the authoritative outcome/failure view and appends localized activity at that action seam. Resolve default selection before dispatch. Keep the Provider-picker handler separate so overlay scope does not fall through to the Target command.

Use the existing overlay stack with its disposal-scoped command registration. The confirmation must carry only Provider ID/name and never a Provider object containing credential-related draft state.

- [ ] **Step 5: Add complete English and zh-CN strings**

Add command titles/descriptions, Direct success, restart guidance reuse, Takeover-required prompt/actions, `takeover-required`, and `takeover-active`. Assert both catalogs have identical keys and no raw backend message is rendered.

- [ ] **Step 6: Run Task 4 verification**

```bash
bun test packages/control-plane/test/commands.test.tsx
bun test packages/control-plane/test/provider-workflow.test.tsx
bun test packages/control-plane/test/app-render.test.tsx
bun test packages/control-plane/test/localization.test.ts
bun run typecheck
git diff --check
```

Expected: all commands exit 0; named command identity, modal priority/focus, exact Provider selection, authoritative view installation, localization, and secret-free frames pass.

- [ ] **Step 7: Commit Task 4**

```bash
git add packages/control-plane/src packages/control-plane/test
git commit -m "feat: add codex direct activation flow"
```

---

### Task 5: Real-process Direct Activation tracer and repository verification

**Files:**
- Modify: `packages/control-plane/test/walking-skeleton.e2e.tsx`

- [ ] **Step 1: Extend the real-process tracer and verify RED**

In a temporary root, create:

- temporary `HOME` and Muxvia Home;
- real `.codex/config.toml` with unrelated comments/tables and known mode;
- sibling `.codex/auth.json` with sentinel bytes and known mode/size/mtime;
- fake Codex executable that reports a tested-compatible version;
- one complete Direct-compatible Provider through the real TUI/UDS action path.

Drive the accepted shell with named commands: Home → Codex → Provider selection → Direct Activation. Assert:

1. the returned/rendered Target View is `direct`, Current is exact, Serving is null, snapshot matches the saved declaration, Managed Configuration is Applied, and restart guidance is visible;
2. the file contains only exact Direct-owned values while unrelated TOML and mode remain intact;
3. `auth.json` fingerprint is unchanged;
4. SQLite contains no route port or Routing Credential and the committed snapshot is immutable;
5. the former model endpoint is absent/unreachable and no loopback listener was created for Direct;
6. closing the TUI/TargetSession lets the Routing Service exit and removes the control socket;
7. restarting the service/TUI projects the same Direct Current/snapshot/config state without a Model Server;
8. RPC frames, receipts, Target Views, activity projections, renderer frames, process output, and captured requests contain no credential sentinel.

Run:

```bash
bun test packages/control-plane/test/walking-skeleton.e2e.tsx --test-name-pattern "direct activation"
```

Expected: FAIL until all Direct layers are wired end to end.

- [ ] **Step 2: Fix only tracer-exposed cross-layer defects**

For any real failure, add a focused owning-layer RED test first, implement the smallest production fix, rerun the owning test, then rerun the tracer. Do not add test-only runtime flags or weaken the existing scan-first redaction harness.

- [ ] **Step 3: Run focused cross-layer verification**

```bash
cargo test -p muxvia-routing --test protocol_contract
cargo test -p muxvia-routing --test provider_declarations
cargo test -p muxvia-routing --test codex_config
cargo test -p muxvia-routing --test activation
cargo test -p muxvia-routing --test recovery
cargo test -p muxvia-routing --test control_socket
bun test packages/control-plane/test/protocol.test.ts packages/control-plane/test/target-session.test.ts packages/control-plane/test/control-socket.test.ts
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/app-render.test.tsx packages/control-plane/test/localization.test.ts
bun test packages/control-plane/test/walking-skeleton.e2e.tsx
```

Expected: every command exits 0, including the original Takeover walking skeleton and T03 Provider workflow regressions.

- [ ] **Step 4: Run repository verification**

```bash
bun install --frozen-lockfile
bun run verify
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

Expected: all commands exit 0 on macOS locally; the existing CI matrix supplies Linux glibc verification.

- [ ] **Step 5: Commit Task 5**

```bash
git add packages/control-plane/test tests/e2e
git commit -m "test: prove codex direct activation end to end"
```

---

## Final Review and Handoff

- [ ] Run an independent Spec review against GitHub issue #5, this plan, the approved design, and relevant ADRs.
- [ ] Run an independent Standards review against `AGENTS.md`, `CONTEXT.md`, repository agent docs, and existing module boundaries.
- [ ] Address every Critical/Important finding with a new RED/GREEN cycle; rerun the mutation-sensitive owning test and full verification.
- [ ] Confirm `git status --short`, `git diff --check`, commit history, and branch base.
- [ ] Use `superpowers:finishing-a-development-branch` to offer local merge, push/PR, keep branch, or discard options. Do not push or merge without the Operator's explicit choice.
