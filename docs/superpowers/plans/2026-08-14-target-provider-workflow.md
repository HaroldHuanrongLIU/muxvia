# Target Provider Workflow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the complete T03 Codex Target Provider declaration workflow, including safe incomplete records, create/edit/reorder/duplicate/delete, one copy-on-create Preset, Model Discovery, Reachability Check, and an OpenCode-style TUI without implicit traffic changes.

**Architecture:** The Rust Routing Service remains the sole owner of provider state, credential identities, mutation receipts, and network inspection. Persistent mutations use revision-guarded Target actions and return complete Target Views; Model Discovery and Reachability use cancellable read-only RPC results that never carry a Target View. The Bun/Solid/OpenTUI Control Plane keeps all credential drafts and transient inspection state inside the focused Provider workflow.

**Tech Stack:** Rust 1.96, Tokio, rusqlite/tokio-rusqlite, reqwest/rustls, serde, Bun 1.3.14, TypeScript, Solid, OpenTUI 0.4.3, `@opentui/keymap` 0.4.3.

## Global Constraints

- T03 completes the Codex Target context only. Claude Target state and Anthropic authentication remain in #6.
- Provider declaration changes never change Current Target Provider, Serving Provider, Activated Snapshot, Target Takeover, Managed Configuration, Route Health, or routed traffic.
- A Provider name is required. Endpoint, model, and Credential Reference may be missing; a nonempty endpoint must satisfy the existing HTTPS-or-loopback-HTTP URL policy.
- Codex T03 fixes protocol/auth to OpenAI-compatible Responses with bearer authentication; do not introduce a generic provider SDK or arbitrary auth/header surface.
- Credential bytes never appear in Target Views, action receipts, read-only results, logs, Debug output, ordinary errors, activity entries, captured renderer frames, or test diagnostics.
- Receipt lookup occurs before raw action parsing. Every successful persistent mutation atomically commits declaration state, target management revision, view sequence, secret-free receipt, and complete Target View.
- Successful or failed read-only inspection never writes SQLite and never returns a Target View, so it cannot overwrite a newer session view.
- Model Discovery uses the pinned ordered candidates, bearer auth, 15-second per-candidate timeout, and 404/405-only fallback recorded in `docs/research/model-discovery-and-reachability-contracts.md`.
- Reachability uses unauthenticated headers-only `GET`, 8 seconds per attempt, one timeout-like retry, any HTTP status as reachable, and a strict separation from Route Health.
- Provider create/edit/duplicate/reorder/delete remain unavailable from the noninteractive CLI.
- The TUI retains the accepted OpenCode shell: named commands, centralized layered keymap, one overlay stack, no app bar/tabs/permanent navigation, and English/zh-CN key parity.
- Provider secrets remain transient masked editor state and are cleared on submit, cancellation, unmount, confirmed dirty exit, and superseded inspection.
- macOS arm64/x86-64 and Linux glibc arm64/x86-64 remain the supported matrix; no new native dependency is introduced.

---

### Task 1: Incomplete create/edit state and schema-v2 migration

**Files:**
- Create: `crates/routing-service/src/state/migrations.rs`
- Create: `crates/routing-service/src/state/providers.rs`
- Create: `crates/routing-service/tests/provider_declarations.rs`
- Modify: `crates/routing-service/src/state/schema.sql`
- Modify: `crates/routing-service/src/state/mod.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/domain/view.rs`
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `crates/routing-service/src/service/activate.rs`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/app-lifecycle.test.tsx`
- Modify: `packages/control-plane/test/responsive-shell.test.tsx`
- Modify: `packages/control-plane/test/target-session.test.ts`
- Modify: `packages/control-plane/test/fixtures/pty-control-plane.tsx`
- Modify: `crates/routing-service/tests/protocol_contract.rs`
- Modify: `protocol/control-v1.schema.json`
- Modify: `protocol/fixtures/initial-target-view.json`
- Modify: `protocol/fixtures/save-provider.json`
- Modify: existing Rust fixtures that insert `providers` or `provider_credentials`

**Interfaces:**
- Produces Rust `ProviderProtocol::OpenaiResponses`, `ProviderCompleteness`, `ProviderRequirement`, `ProviderProvenanceView`, `ProviderReferenceView`, `ProviderPresetView`, and the enriched `ProviderView` projection.
- Produces `CredentialEdit::{Keep, Remove, Replace { value }}` with a redacted `Debug` implementation.
- Produces `TargetAction::CreateProvider` and `TargetAction::UpdateProvider`.
- Produces schema version 2 with `providers.position`, `providers.provider_revision`, empty-string-capable declaration fields, `credentials`, and nullable `providers.credential_id`.
- Keeps `TargetSession.act()` and `ActionOutcome` unchanged for callers.

- [ ] **Step 1: Write protocol and projection RED tests**

Add literal Rust/TypeScript fixture assertions for this secret-free Provider shape:

```json
{
  "id": "00000000-0000-4000-8000-000000000101",
  "position": 0,
  "providerRevision": 1,
  "name": "Incomplete",
  "baseUrl": "",
  "model": "",
  "protocol": "openai-responses",
  "credential": "missing",
  "completeness": "incomplete",
  "missingFields": ["base-url", "model", "credential"],
  "provenance": null,
  "generated": false,
  "activeReferences": []
}
```

Add an `openai-api-responses` Target View Preset with base URL `https://api.openai.com/v1`, empty model, fixed protocol, and no credential field. Assert additive unknown wire fields remain accepted and `CredentialEdit::Replace` Debug never contains its sentinel.

- [ ] **Step 2: Run the protocol tests and verify RED**

Run:

```bash
cargo test -p muxvia-routing --test protocol_contract
bun test packages/control-plane/test/protocol.test.ts
```

Expected: compilation/schema failures name the missing enriched Provider fields, Preset catalog, credential intent, and create/update actions.

- [ ] **Step 3: Write migration RED tests with a real v1 database**

In `provider_declarations.rs`, create a v1 SQLite file using the exact pre-T03 schema, insert two ordered Providers, one credential, Current state, and an Activated Snapshot, then open it through `StateStore::open`. Assert:

```rust
assert_eq!(view.providers.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
assert_eq!(view.providers[0].credential, CredentialPresence::Present);
assert_eq!(view.current_provider_id.as_deref(), Some(existing_provider_id));
assert_eq!(view.activated_snapshot.as_ref().map(|s| s.id), Some(existing_snapshot_id));
```

Also assert schema version `2`, unchanged Provider UUIDs, and the original credential bytes available only through the internal activation preparation seam.

- [ ] **Step 4: Run the migration test and verify RED**

Run:

```bash
cargo test -p muxvia-routing --test provider_declarations v1_database_migrates_provider_identity_order_credential_and_active_state -- --exact
```

Expected: FAIL because the current schema has no migration, explicit position, credential identity, or enriched projection.

- [ ] **Step 5: Implement the schema-v2 migration and final schema**

Implement `migrate(connection: &mut rusqlite::Connection)` with these exact rules:

```rust
pub const SCHEMA_VERSION: u32 = 2;

// v1 -> v2, inside one IMMEDIATE transaction:
// 1. Create credentials and providers_v2.
// 2. Copy providers in rowid order with zero-based position and revision 1.
// 3. Use the existing provider UUID as the migrated credential UUID.
// 4. Copy each bearer token once and point its provider at that credential.
// 5. Replace only the two old declaration tables.
// 6. Set metadata schema-version to 2 and commit.
```

The final `providers` table must keep non-null string `base_url` and `model` fields (empty means missing), plus fixed `protocol`, nullable `credential_id`, explicit `position`, `provider_revision`, `provenance_kind`, `provenance_key`, and `generated_owner_id`. Keep target constrained to Codex in T03. Enable foreign keys after migration and validate `PRAGMA foreign_key_check` before accepting the database.

- [ ] **Step 6: Implement enriched projection and typed wire contract**

Derive completeness from the row and credential reference. Project Preset catalog from one Rust constant:

```rust
pub const OPENAI_API_RESPONSES_PRESET_KEY: &str = "openai-api-responses";
pub const OPENAI_API_RESPONSES_BASE_URL: &str = "https://api.openai.com/v1";

pub struct ProviderView {
    pub id: Uuid,
    pub position: u32,
    pub provider_revision: u64,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub credential: CredentialPresence,
    pub completeness: ProviderCompleteness,
    pub missing_fields: Vec<ProviderRequirement>,
    pub provenance: Option<ProviderProvenanceView>,
    pub generated: bool,
    pub active_references: Vec<ProviderReferenceView>,
}
```

Do not expose credential UUIDs. Derive active references by joining Current and Activated Snapshot state. Keep the reference list deterministic: Current, Activated Snapshot, then future route-plan references.

- [ ] **Step 7: Write create/edit/incomplete RED tests**

Cover these real state transitions:

```rust
// Create with name only -> Applied, revision 1, Incomplete, three missing fields.
// Empty name -> invalid-provider and no receipt/revision change.
// Nonempty unsafe URL -> invalid-provider and no receipt/revision change.
// Update keeps UUID and increments only providerRevision/target revision.
// Credential Replace creates a new reference; Keep retains it; Remove detaches it.
// Replacing one shared reference never changes another Provider's secret.
// Replace collects the prior Credential Reference only when it becomes orphaned.
// An identical Keep update returns no-provider-change with no receipt or revision change.
// Editing an active Provider changes the declaration but not snapshot/config bytes.
// Concurrent same action ID yields one Applied and one Replayed result.
// Malformed replay returns the receipt before parsing the second payload.
```

- [ ] **Step 8: Run focused create/edit tests and verify RED**

Run:

```bash
cargo test -p muxvia-routing --test provider_declarations
```

Expected: the new state behavior fails because create/update and incomplete persistence are not implemented.

- [ ] **Step 9: Implement minimal create/update state actions**

Put Provider transaction logic in `state/providers.rs`, not in the model transport. Normalize only a nonempty endpoint. Store a missing endpoint/model as an empty string so the existing wire field type remains compatible. Trim and reject a blank name. Credential behavior is explicit:

```rust
match edit {
    CredentialEdit::Keep => retain_existing_reference_for_update_only(),
    CredentialEdit::Remove => detach_and_collect_orphan(),
    CredentialEdit::Replace { value } if !value.trim().is_empty() => replace_reference_and_collect_prior_if_orphaned(value),
    _ => return invalid_provider(),
}
```

For create, `Keep` is invalid and `Remove` means no Credential Reference. A non-null `preset_key` must equal the one release-owned key; submitted draft values remain authoritative and the service records that non-owning provenance. The Control Plane performs the visible Preset-to-draft copy in Task 6.

- [ ] **Step 10: Preserve activation completeness and declaration immutability**

Update activation preparation to reject any missing endpoint/model/credential with `incomplete-provider` before a recovery intent or listener bind. Load the immutable snapshot from the saved declaration and credential identity. Existing active snapshot rows remain byte-for-byte independent from later declaration edits.

```rust
if base_url.is_empty() || model.is_empty() || credential.is_none() {
    return Ok(Err(failure("incomplete-provider", "Provider is missing or incomplete")));
}
```

- [ ] **Step 11: Run Task 1 verification**

Run:

```bash
cargo test -p muxvia-routing --test provider_declarations
cargo test -p muxvia-routing --test activation
cargo test -p muxvia-routing --test protocol_contract
bun test packages/control-plane/test/protocol.test.ts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

Expected: all commands exit 0; tests prove migration, incomplete persistence, edit semantics, activation blocking, replay, and secret-free projection.

- [ ] **Step 12: Commit Task 1**

```bash
git add crates/routing-service packages/control-plane/src/control/types.ts packages/control-plane/test/protocol.test.ts protocol
git commit -m "feat: persist incomplete provider declarations"
```

---

### Task 2: Inspect, reorder, and protected delete actions

**Files:**
- Create: `crates/routing-service/tests/provider_lifecycle.rs`
- Modify: `crates/routing-service/src/state/providers.rs`
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `crates/routing-service/src/service/activate.rs`
- Modify: `crates/routing-service/src/control/server.rs`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/test/control-socket.test.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`
- Modify: `crates/routing-service/tests/protocol_contract.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `protocol/control-v1.schema.json`
- Create: `protocol/fixtures/reorder-providers.json`
- Create: `protocol/fixtures/delete-provider.json`

**Interfaces:**
- Produces `TargetAction::ReorderProviders { provider_ids: Vec<Uuid> }`.
- Produces `TargetAction::DeleteProvider { provider_id: Uuid, provider_revision: u64 }`.
- Uses Task 1 `ProviderView.active_references`, receipts, target revision, and provider revision.

- [ ] **Step 1: Write reorder/delete RED tests**

Assert:

```rust
// Reorder [C, A, B] projects exactly [C, A, B].
// Duplicate, missing, or unknown IDs reject atomically with invalid-provider-order.
// Reorder changes target management revision and order, but not each Provider's configuration revision or any Current/Snapshot/config fields.
// Reorder to the existing order returns no-provider-change with no receipt/revision change.
// Delete an unreferenced Provider compacts positions and collects only an orphan credential.
// Delete one of two shared-reference Providers preserves the other credential.
// Delete Current or Activated Snapshot referenced Provider returns provider-referenced
// with the complete authoritative view and no mutation.
// Stale providerRevision and stale managementRevision both fail before delete.
// Replay and concurrent duplicate action ID return the one recorded outcome.
```

- [ ] **Step 2: Run lifecycle tests and verify RED**

Run:

```bash
cargo test -p muxvia-routing --test provider_lifecycle
```

Expected: missing action variants or unsupported-operation failures.

- [ ] **Step 3: Implement atomic reorder and delete**

Validate the exact full permutation before changing any position. Use a collision-free two-phase position update inside the same immediate transaction. Delete only ordinary, unreferenced Providers; compute blocking references from authoritative tables rather than trusting the client view. After delete, compact positions and delete the detached credential only when `NOT EXISTS` any remaining reference.

```rust
pub(super) fn reorder_providers(
    transaction: &Transaction<'_>,
    provider_ids: &[Uuid],
) -> Result<(), ProviderMutationError>;

pub(super) fn delete_provider(
    transaction: &Transaction<'_>,
    provider_id: Uuid,
    provider_revision: u64,
) -> Result<(), ProviderMutationError>;
```

Every success increments target management revision/view sequence once, writes one receipt, and publishes one complete view. Failures write no receipt.

- [ ] **Step 4: Extend real UDS contract tests**

Send raw reorder and delete actions through `ControlServer`. Assert receipt-first malformed replay, stale authoritative view, one push per applied action, no push for replay/failure, and no secret sentinel in raw encoded response frames.

Round-trip the reorder/delete fixtures through both Rust and TypeScript protocol contract tests and validate them against `control-v1.schema.json`.

- [ ] **Step 5: Run Task 2 verification**

Run:

```bash
cargo test -p muxvia-routing --test provider_lifecycle
cargo test -p muxvia-routing --test control_socket
bun test packages/control-plane/test/control-socket.test.ts packages/control-plane/test/protocol.test.ts
cargo test -p muxvia-routing --test protocol_contract
cargo test -p muxvia-routing --test activation
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

- [ ] **Step 6: Commit Task 2**

```bash
git add crates/routing-service packages/control-plane/src/control/types.ts packages/control-plane/test/control-socket.test.ts protocol
git commit -m "feat: reorder and protect target providers"
```

---

### Task 3: Declaration-only duplicate and Preset semantics

**Files:**
- Create: `crates/routing-service/tests/provider_duplication.rs`
- Modify: `crates/routing-service/src/state/providers.rs`
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `crates/routing-service/src/service/activate.rs`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`
- Modify: `crates/routing-service/tests/protocol_contract.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `protocol/control-v1.schema.json`
- Create: `protocol/fixtures/duplicate-provider.json`

**Interfaces:**
- Produces `DuplicateCredential::{Without, ReuseSource, Replace { value }}` with redacted Debug.
- Produces `TargetAction::DuplicateProvider { source_provider_id, source_provider_revision, name, base_url, model, credential }` using that dedicated intent.
- Uses `CreateProvider.preset_key` from Task 1 and inserts a duplicate after its source.

- [ ] **Step 1: Write duplicate/Preset RED tests**

Use literal assertions:

```rust
// Duplicate source A -> new UUID, source position + 1, A's later rows shift once.
// Edited duplicate name/base/model are saved; source is unchanged.
// Source Preset provenance and other declaration-only metadata are copied server-side.
// Without -> credential missing; ReuseSource -> same internal credential UUID;
// Replace -> different internal credential UUID and secret.
// No Current, Serving, snapshot, reference, or runtime state transfers.
// Duplicate active source leaves only source referenced.
// Stale source revision, missing source, and malformed credential intent fail.
// A future Generated source duplicates into an ordinary detached Provider with ownership cleared.
// Create from openai-api-responses copies endpoint and records stable Preset provenance.
// Editing the catalog constant later cannot mutate the saved Provider fixture.
```

- [ ] **Step 2: Run duplicate tests and verify RED**

Run:

```bash
cargo test -p muxvia-routing --test provider_duplication
```

Expected: the missing duplicate action and insertion semantics fail.

- [ ] **Step 3: Implement duplicate and copy-on-create Preset**

Read the source and validate its revision in the mutation transaction. Create a new UUID, copy source provenance and other non-runtime declaration metadata server-side, apply the explicit editable fields, bind credential according to the chosen intent, shift positions after the source, and write one receipt/view. Never trust the client to reproduce provenance, and never copy active references or snapshot rows. If a future source is Generated, clear generated ownership and any ownership-implying provenance so the duplicate is an ordinary detached Provider.

```rust
let credential_id = match command.credential {
    DuplicateCredential::Without => None,
    DuplicateCredential::ReuseSource => source.credential_id,
    DuplicateCredential::Replace { value } => Some(insert_credential(transaction, value)?),
};
```

Extend `ActivationService::apply_raw` for the new action and round-trip the duplicate fixture through both Rust and TypeScript protocol contract tests plus `control-v1.schema.json`.

- [ ] **Step 4: Run Task 3 verification**

Run:

```bash
cargo test -p muxvia-routing --test provider_duplication
cargo test -p muxvia-routing --test provider_declarations
cargo test -p muxvia-routing --test provider_lifecycle
cargo test -p muxvia-routing --test control_socket
cargo test -p muxvia-routing --test protocol_contract
bun test packages/control-plane/test/protocol.test.ts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

- [ ] **Step 5: Commit Task 3**

```bash
git add crates/routing-service packages/control-plane/src/control/types.ts protocol
git commit -m "feat: duplicate provider declarations safely"
```

---

### Task 4: Cancellable Model Discovery and Reachability RPC

**Files:**
- Create: `crates/routing-service/src/service/provider_inspector.rs`
- Create: `crates/routing-service/tests/provider_inspection.rs`
- Modify: `crates/routing-service/src/service/mod.rs`
- Modify: `crates/routing-service/src/state/providers.rs`
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `crates/routing-service/src/control/server.rs`
- Modify: `crates/routing-service/src/control/framing.rs`
- Modify: `crates/routing-service/Cargo.toml`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/src/control/rpc-client.ts`
- Modify: `packages/control-plane/src/control/target-session.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`
- Modify: `packages/control-plane/test/control-socket.test.ts`
- Modify: `packages/control-plane/test/target-session.test.ts`
- Modify: `crates/routing-service/tests/protocol_contract.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `protocol/control-v1.schema.json`
- Create: `protocol/fixtures/discover-models.json`
- Create: `protocol/fixtures/check-reachability.json`
- Create: `protocol/fixtures/cancel-inspection.json`

**Interfaces:**
- Produces `DiscoverySource::{Saved { provider_id, provider_revision }, Draft { base_url, credential_source }}` with `DraftCredentialSource::{Missing, Ephemeral { value }, Saved { provider_id, provider_revision }}`. Automatic editor-open discovery uses `Saved`; explicit refresh uses `Draft`, including for unsaved Blank and Preset drafts.
- Produces `ControlOperation::DiscoverModels`, `ControlOperation::CheckReachability`, and `ClientFrame::Cancel { request_id }`.
- Produces `ControlResult::ModelDiscovery { result }` and `ControlResult::Reachability { result }`, neither containing a Target View.
- Produces `TargetSession.discoverModels(request, signal?)` and `TargetSession.checkReachability(providerId, providerRevision, signal?)`.

- [ ] **Step 1: Write pure candidate/parser RED tests**

Cover the exact candidate ordering from the research note for override, full inference URL, plain root, `/v1`, another `/vN`, and one member of every compatibility-suffix class. Assert same-origin fallback, no userinfo/query/fragment propagation, exact de-duplication, stable model sorting, empty-list success, malformed-entry failure, 256 KiB body cap, and 2,048 model cap.

- [ ] **Step 2: Run pure inspection tests and verify RED**

Run:

```bash
cargo test -p muxvia-routing --test provider_inspection
```

Expected: module/type compilation failures before production implementation.

- [ ] **Step 3: Implement bounded `ProviderInspector`**

Build one reqwest client with redirects disabled and environment proxies disabled. Stream response bytes into a bounded buffer instead of calling an unbounded body collector. Discovery behavior:

```text
2xx valid list -> success
404/405 -> next candidate
401/403 -> authentication-rejected
429 -> rate-limited
other status -> upstream-status
timeout/DNS/connect/TLS -> typed terminal category
```

Never return raw bodies/headers/library messages. Resolve saved credentials inside the Routing Service as `SecretString`. Ephemeral credentials exist only in the operation future and have redacted Debug.

Reachability sends no credential, uses exact normalized saved base URL, sends `Accept: */*` and `Accept-Encoding: identity`, reads headers only, returns status/TTFB/slow/retry count, retries once only for timeout, and treats every HTTP status as reachable.

- [ ] **Step 4: Write deterministic HTTP RED tests**

Use real loopback fake servers to prove:

```rust
// Bearer header reaches discovery server and never appears in result/error Debug.
// 404 then second candidate succeeds; 401/429/500/parse/timeout make one terminal path.
// A delayed body does not delay Reachability completion after headers.
// Reachability sends exact Accept */* and Accept-Encoding identity headers and no auth.
// 401 and 503 are reachable; timeout retries exactly once; connect failure does not.
// Before/after TargetView and database fingerprints are equal for every inspection.
```

- [ ] **Step 5: Run HTTP tests and verify RED, then GREEN**

Run:

```bash
cargo test -p muxvia-routing --test provider_inspection -- --nocapture
```

First expected: behavior failures name missing fallback, bounds, retry, or state isolation. After minimal implementation: all focused cases pass.

- [ ] **Step 6: Write RPC cancellation/concurrency RED tests**

Add one delayed discovery through a real UDS. While it is pending, send an ordinary `open-target` and assert the Target View response arrives before discovery completes. Cancel discovery by request ID and assert the fake upstream observes dropped work, no result is written, the UDS session remains usable, and shutdown drains no orphan task.

TypeScript tests must prove an aborted `discoverModels` rejects with `cancelled`, sends one cancel frame, removes the pending request, ignores any late response, and leaves `session.get()` at a newer pushed view. Add explicit refresh coverage for unsaved Blank and Preset drafts with no Provider identity.

Round-trip discovery, reachability, cancellation, and both read-only result variants through Rust and TypeScript protocol contract tests and validate every fixture against `control-v1.schema.json`.

- [ ] **Step 7: Run RPC tests and verify RED**

Run:

```bash
cargo test -p muxvia-routing --test control_socket
bun test packages/control-plane/test/control-socket.test.ts packages/control-plane/test/target-session.test.ts packages/control-plane/test/protocol.test.ts
```

Expected: current sequential session and request API cannot satisfy cancellation or read-only results.

- [ ] **Step 8: Implement concurrent read-only request handling**

After hello, serialize writes through one bounded response channel. Keep mutation handling revision-safe, but spawn only inspection operations in a session-owned `JoinSet`. Track inspection abort handles by request ID. Reap completed tasks promptly and remove their abort-map entries; `Cancel` removes and aborts only its read-only task; disconnect and server shutdown abort and await all inspection tasks. Responses remain correlated and may arrive out of order. Tests assert zero tracked inspection tasks after normal completion and cancellation.

Update the Bun RPC client with `request(operation, { signal }?)`. Abort sends one best-effort Cancel frame, rejects locally once, removes the pending entry, and ignores late responses. `TargetSession` does not enqueue inspections in the mutation chain and never installs a Target View from an inspection result.

```rust
match operation {
    ControlOperation::DiscoverModels { .. } | ControlOperation::CheckReachability { .. } => {
        let abort = inspections.spawn(inspect_and_send(operation, responses.clone()));
        inspection_aborts.insert(request_id, abort);
    }
    ControlOperation::Act { .. } => handle_mutation_inline(operation).await,
    ControlOperation::OpenTarget { .. } => send_current_view().await,
}
```

- [ ] **Step 9: Run Task 4 verification**

Run:

```bash
cargo test -p muxvia-routing --test provider_inspection
cargo test -p muxvia-routing --test control_socket
cargo test -p muxvia-routing --test protocol_contract
bun test packages/control-plane/test/protocol.test.ts packages/control-plane/test/control-socket.test.ts packages/control-plane/test/target-session.test.ts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

- [ ] **Step 10: Commit Task 4**

```bash
git add crates/routing-service packages/control-plane/src/control packages/control-plane/test protocol
git commit -m "feat: inspect provider connectivity safely"
```

---

### Task 5: Provider list, inspect, create/edit, reorder, and delete TUI

**Files:**
- Create: `packages/control-plane/src/ui/provider-picker.tsx`
- Create: `packages/control-plane/src/ui/provider-delete-confirmation.tsx`
- Create: `packages/control-plane/test/provider-workflow.test.tsx`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/ui/provider-form.tsx`
- Modify: `packages/control-plane/src/commands/catalog.ts`
- Modify: `packages/control-plane/src/commands/types.ts`
- Modify: `packages/control-plane/src/i18n/en.ts`
- Modify: `packages/control-plane/src/i18n/zh-cn.ts`
- Modify: `packages/control-plane/test/localization.test.ts`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/commands.test.tsx`
- Modify: `packages/control-plane/test/overlays.test.tsx`

**Interfaces:**
- Produces `<ProviderPicker>` as a top-only OpenCode-style overlay with selected Provider detail and named action handlers.
- Extends `<ProviderForm>` with `mode`, `initialDraft`, `credentialPresence`, explicit credential intent, and Provider form result for create/update.
- Adds named commands `provider.list`, `provider.edit`, `provider.move-up`, `provider.move-down`, `provider.delete`, and `provider.credential.remove`.

- [ ] **Step 1: Write renderer RED tests for list/inspect**

Render three Providers and assert `/providers` opens one overlay with exact order, selected detail, complete/incomplete label, Preset/ordinary provenance, generated state, Credential Reference presence, and Current/Activated Snapshot references. Assert secrets and credential UUIDs never render.

Navigate Up/Down through the focused picker; Enter opens edit. Escape restores the Codex action prompt focus. At `1x1`, `2x2`, `20x5`, `40x10`, `80x24`, and `121x30`, render without exceptions or permanent navigation.

- [ ] **Step 2: Run focused UI tests and verify RED**

Run:

```bash
bun test packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/commands.test.tsx
```

Expected: missing Provider picker/commands and create-only editor behavior.

- [ ] **Step 3: Implement Provider picker and edit form**

Use the existing overlay provider and keymap. Background layers remain disabled while the picker is open. Picker-local navigation may use the focused input's `onKeyDown`; Provider actions remain named keymap commands. Do not add renderer-global `useKeyboard` handlers.

Edit initializes normal fields from the selected Provider but never reconstructs a saved credential. Blank credential input means Keep until the Operator explicitly removes or types a replacement. Create uses Remove initially. Save dispatches create/update, clears the transient credential synchronously before awaiting, and retains normal dirty fields after a failed action.

```ts
export interface ProviderFormProps {
  mode: "create" | "edit" | "duplicate"
  initialDraft: ProviderDraft
  credentialPresence: "present" | "missing"
  onSave(result: ProviderFormResult): Promise<boolean>
}
```

- [ ] **Step 4: Write reorder/delete RED tests**

Prove move up/down dispatches the complete exact identity permutation and updates the rendered order only after the action outcome. Boundary moves dispatch nothing. Delete requires a confirmation overlay. Unreferenced delete removes the row; referenced delete leaves the picker open and renders every localized active reference from the authoritative view.

- [ ] **Step 5: Implement reorder/delete UI**

Keep selection on the moved Provider after a successful reorder. Disable repeated mutation submission while pending. Delete confirmation has distinct confirm/cancel named commands and no competing generic Escape layer. All errors use stable problem-code localization rather than server `message` text.

```ts
const moveSelected = (delta: -1 | 1) => {
  const nextIds = moveIdentity(view().providers.map(({ id }) => id), selectedId(), delta)
  if (nextIds) void session.act({ kind: "reorder-providers", providerIds: nextIds })
}
```

- [ ] **Step 6: Run Task 5 verification**

Run:

```bash
bun test packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/app-render.test.tsx packages/control-plane/test/commands.test.tsx packages/control-plane/test/overlays.test.tsx packages/control-plane/test/localization.test.ts
bun run typecheck
! rg -n "provider-secret-must-not-escape|routing-secret-must-not-escape" packages/control-plane/src
git diff --check
```

Expected: all tests pass; the production sentinel scan has no matches.

- [ ] **Step 7: Commit Task 5**

```bash
git add packages/control-plane
git commit -m "feat: manage provider declarations in the tui"
```

---

### Task 6: Duplicate, Preset, discovery, and Reachability TUI

**Files:**
- Create: `packages/control-plane/src/ui/provider-source-picker.tsx`
- Create: `packages/control-plane/src/ui/provider-credential-confirmation.tsx`
- Create: `packages/control-plane/src/ui/provider-model-picker.tsx`
- Create: `packages/control-plane/test/provider-inspection-ui.test.tsx`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/ui/provider-picker.tsx`
- Modify: `packages/control-plane/src/ui/provider-form.tsx`
- Modify: `packages/control-plane/src/commands/catalog.ts`
- Modify: `packages/control-plane/src/commands/types.ts`
- Modify: `packages/control-plane/src/i18n/en.ts`
- Modify: `packages/control-plane/src/i18n/zh-cn.ts`
- Modify: `packages/control-plane/test/provider-workflow.test.tsx`
- Modify: `packages/control-plane/test/commands.test.tsx`
- Modify: `packages/control-plane/test/overlays.test.tsx`
- Modify: `packages/control-plane/test/localization.test.ts`

**Interfaces:**
- Adds named commands `provider.duplicate`, `provider.models.refresh`, `provider.models.select`, `provider.reachability.check`, `provider.credential.reuse`, `provider.credential.without`, and `provider.credential.confirmation.cancel`.
- Uses Task 4 `TargetSession.discoverModels()` and `checkReachability()` with one editor-owned `AbortController` per current discovery.
- Uses Task 3 duplicate action and Task 1 Target View Preset catalog.

- [ ] **Step 1: Write duplicate/Preset UI RED tests**

Assert Provider create offers `Blank` and localized `OpenAI API (Responses)`. Choosing the Preset opens an editable draft with the exact endpoint, blank model/credential, and no request. Saving projects Preset provenance.

Duplicate opens the Credential Reference confirmation only when the source has one. Confirm reuse, decline, and cancel are distinct. The resulting editor is prefilled with source declaration and localized `<source> Copy`; save sends the source ID/revision and selected explicit credential intent. The new row appears immediately after the source.

- [ ] **Step 2: Run duplicate/Preset tests and verify RED**

Run:

```bash
bun test packages/control-plane/test/provider-workflow.test.tsx --test-name-pattern "Preset|duplicate"
```

Expected: missing source picker, confirmation, and duplicate UI.

- [ ] **Step 3: Implement source and credential-confirmation overlays**

Use lazy overlay render functions so every overlay's keymap layer disposes exactly once. Preset values are copied into ordinary editor signals. Reuse confirmation exposes only Credential Reference presence, never identity or bytes, and all three choices use named commands rather than a renderer-global keyboard handler. A typed replacement credential overrides reuse before save.

```ts
type DuplicateCredentialChoice =
  | { kind: "without" }
  | { kind: "reuse-source" }
  | { kind: "replace"; value: string }
```

- [ ] **Step 4: Write discovery/reachability UI RED tests**

Use a real `TargetSession` test double that records projected secret-free inspection metadata and keeps raw credential sentinels only in its ephemeral call closure. Assert:

```text
edit existing -> exactly one saved discovery call
create/Preset -> zero automatic calls
typing endpoint/credential/model -> zero new calls
explicit refresh -> one draft call with current endpoint and intended credential source
new refresh aborts old; close aborts current; late result cannot change a newer draft
failed discovery -> manual model input and save remain enabled
model picker selection writes the exact model ID into the draft
saved Provider Reachability -> exact saved ID/revision, status/TTFB/slow display
Reachability never changes the Target View or activity into Route Health
```

Capture every frame before, during, after failure, and after cancellation; no credential sentinel may occur.

- [ ] **Step 5: Run inspection UI tests and verify RED**

Run:

```bash
bun test packages/control-plane/test/provider-inspection-ui.test.tsx
```

Expected: missing inspection methods and editor transient-state behavior.

- [ ] **Step 6: Implement editor inspection state**

On existing-editor mount, snapshot Provider ID/revision and start one saved discovery in `onMount`. Keep an editor generation plus AbortController. Explicit refresh aborts the previous controller and uses current draft. Catch `cancelled` silently; localize every other stable category. Suggestions never replace manual model text until the Operator selects one.

Reachability runs from selected saved Provider detail and renders only reachable/unreachable, HTTP status when present, TTFB, retry count, and slow label. Do not append a Route Health activity or mutate the Provider.

```ts
onMount(() => {
  if (props.mode === "edit") void discover({ kind: "saved", providerId, providerRevision })
})
onCleanup(() => discoveryAbort?.abort())
```

- [ ] **Step 7: Run Task 6 verification**

Run:

```bash
bun test packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/provider-inspection-ui.test.tsx packages/control-plane/test/app-render.test.tsx packages/control-plane/test/commands.test.tsx packages/control-plane/test/overlays.test.tsx packages/control-plane/test/localization.test.ts packages/control-plane/test/responsive-shell.test.tsx
bun run typecheck
! rg -n "provider-secret-must-not-escape|routing-secret-must-not-escape" packages/control-plane/src
git diff --check
```

- [ ] **Step 8: Commit Task 6**

```bash
git add packages/control-plane
git commit -m "feat: add provider presets and inspection flow"
```

---

### Task 7: Real-process tracer, security regression, and release checks

**Files:**
- Modify: `packages/control-plane/test/walking-skeleton.e2e.tsx`
- Modify: `tests/e2e/fake-upstream.ts`
- Modify: `tests/e2e/walking-skeleton.test.ts`
- Modify: `crates/routing-service/tests/process_lifecycle.rs` only if the real tracer exposes a service-lifecycle contract gap
- Modify: `.github/workflows/ci.yml` only if a new test command is not already included by `bun run verify`

**Interfaces:**
- Uses the production Routing Service binary, production UDS framing, production TargetSession, OpenTUI test renderer, temporary Muxvia/Codex homes, and deterministic loopback HTTP.
- Does not add test-only production flags beyond the existing guarded E2E mechanism.

- [ ] **Step 1: Write the extended real-process RED tracer**

Drive named commands through the real renderer and prove this sequence:

```text
1. Create a name-only Incomplete Provider and observe three missing fields.
2. Edit it with endpoint/model/credential and save; automatic discovery uses saved state only after reopening.
3. Explicit discovery receives deterministic model IDs; select one and save.
4. Create from the safe Preset without any network request while typing.
5. Duplicate the complete Provider twice: once without and once with explicit Credential Reference reuse.
6. Reorder the Providers and verify the exact persisted order after reconnect.
7. Apply existing Takeover to the original Provider, edit its declaration, and prove config bytes plus routed upstream remain pinned to the immutable snapshot.
8. Reachability reports a deterministic 401 as reachable without authentication or state change.
9. Active delete is rejected; inactive duplicate delete succeeds without breaking the shared credential.
10. Close the Control Plane and prove the active takeover still serves a second authenticated request.
```

Fingerprint the temporary DB projections and Managed Configuration before/after every read-only inspection. Scan decoded inbound server RPC frames, receipts, Target Views, activities, rendered frames, and process output for provider/routing secret sentinels. Separately assert that the fake upstream receives the Provider credential only in its expected Authorization header and never receives the Routing Credential; outgoing credential-bearing management requests are not falsely classified as leaks.

- [ ] **Step 2: Run the real tracer as a cross-layer acceptance check**

Run:

```bash
bun test packages/control-plane/test/walking-skeleton.e2e.tsx tests/e2e/walking-skeleton.test.ts
```

Expected: the tracer passes because Tasks 1–6 were each driven by a focused RED test. If it exposes a cross-layer defect, reproduce that defect with a new failing focused test in the owning task's test file before changing production code; do not weaken secret scans, replace real processes with mocks, add sleeps, or bypass named commands.

- [ ] **Step 3: Run focused cross-layer verification**

Run:

```bash
cargo test --workspace
bun test packages/control-plane/test
bun test tests/e2e
bun run typecheck
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

- [ ] **Step 4: Run the repository completion gate**

Run:

```bash
bun install --frozen-lockfile
cargo build --workspace
bun run verify
```

Expected: Rust and Bun suites, PTY tests, process tests, E2E, formatting, Clippy, and type checking all exit 0 on the feature branch.

- [ ] **Step 5: Commit Task 7**

```bash
git add packages/control-plane/test tests/e2e crates/routing-service/tests .github/workflows/ci.yml
git commit -m "test: prove target provider workflow end to end"
```

---

## Whole-branch acceptance checklist

- [ ] Every GitHub #4 acceptance criterion maps to at least one mutation-sensitive test above.
- [ ] Existing T01 takeover/config/SSE/Serving and T02 shell/PTY/localization behavior remains green.
- [ ] Provider declaration mutations never alter Current/Serving/Snapshot/Managed Configuration implicitly.
- [ ] Incomplete save succeeds while activation fails before side effects.
- [ ] Duplicate credential reuse is explicit and reference-based; no secret bytes are copied as runtime state.
- [ ] Preset creation is copy-on-create with stable key and no affiliate/native-auth behavior.
- [ ] Discovery is one-shot on saved edit, explicit for drafts, cancellable, bounded, and manual-entry-safe.
- [ ] Reachability matches Q102 and does not change Route Health or persistent state.
- [ ] Target Views expose completeness, provenance, generated status, and active references without secret values.
- [ ] Full macOS/Linux CI is green before closing #4.
