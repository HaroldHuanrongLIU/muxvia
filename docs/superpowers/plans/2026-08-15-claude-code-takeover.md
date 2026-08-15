# Claude Code Takeover Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a real, independent Claude Code Target: Operators can manage Claude Providers and enable Takeover, Claude Code can send authenticated Anthropic Messages and token-counting requests through its own loopback route, and every Claude state/configuration/runtime path remains isolated from Codex.

**Architecture:** Generalize the existing target orchestration only where target identity, transactional state, recovery ordering, session filtering, listener lifetime, and streaming mechanics are genuinely shared. Keep Codex TOML, Claude JSON, capability probes, Provider inspection authentication, Responses, and Messages behind native target adapters. A UDS session is permanently bound to one target; an immutable Activated Snapshot pins each model request.

**Tech Stack:** Rust 1.96, Tokio, Axum 0.8, Reqwest 0.13, rusqlite/tokio-rusqlite, serde/serde_json, secrecy, Bun 1.3.14, TypeScript, Solid, OpenTUI 0.4.3, `@opentui/keymap` 0.4.3.

## Global constraints

- T05 implements Claude Takeover only. Claude Direct Activation remains T06. Drift repair actions, Route Health evaluation/circuits, safe Takeover removal, Universal Providers, Failover, subscriptions, usage, import/export, backup, and distribution remain later tickets.
- Claude Provider authentication is explicit: `anthropic-api-key` or `anthropic-bearer`. The local Claude Routing Credential is a different secret and is always supplied inbound as `Authorization: Bearer`.
- Codex remains `openai-responses` plus `openai-bearer`. Names and URLs never infer target, protocol, authentication profile, or ownership.
- Every Provider, Credential Reference, route state, neutral Route Health projection, problem, snapshot, receipt, recovery intent, listener, Routing Credential, Current, Serving, revision, and view sequence is target-scoped.
- The only Claude managed settings are `env.ANTHROPIC_BASE_URL`, `env.ANTHROPIC_AUTH_TOKEN`, and `env.ANTHROPIC_MODEL`. Never own the complete `env`, top-level `model`, `ANTHROPIC_API_KEY`, provider selectors, `~/.claude.json`, login state, or credential stores.
- Active/unknown provider selectors, host-managed mode, observable higher-precedence shadows, unsupported homes, incompatible CLI, unsafe symlinks/identities, invalid JSON, Recovery Required, and Configuration Drift fail closed without clearing or repairing operator state.
- Messages and token-counting bodies are bounded to 32 MiB because the adapter owns top-level `model`. Responses, including SSE and upstream errors, stay streamed and byte-preserving.
- All persistent actions remain receipt-first. Replay never performs target work or publishes a duplicate view. The same UUID may be used independently by Codex and Claude.
- No secret, raw managed-state payload, raw backend message, or authorization header may appear in views, receipts, Debug, logs, activity entries, renderer frames, or test diagnostics.
- Tests use temporary user/Muxvia homes, fake CLIs, deterministic loopback upstreams, and scan-first fixed diagnostics. Never read or modify the Operator's real Claude or Codex files.

---

### Task 1: Schema-v4 target, protocol, authentication, and receipt contract

**Files:**
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `crates/routing-service/src/domain/activation.rs`
- Modify: `crates/routing-service/src/domain/provider.rs`
- Modify: `crates/routing-service/src/domain/view.rs`
- Modify: `crates/routing-service/src/state/schema.sql`
- Modify: `crates/routing-service/src/state/migrations.rs`
- Modify: `crates/routing-service/src/state/providers.rs`
- Modify: `crates/routing-service/src/state/recovery.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/tests/protocol_contract.rs`
- Modify: `crates/routing-service/tests/provider_declarations.rs`
- Modify: `crates/routing-service/tests/state_store.rs`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/test/protocol.test.ts`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/target-session.test.ts`
- Modify: `packages/control-plane/test/provider-inspection-ui.test.tsx`
- Modify: `packages/control-plane/test/provider-workflow.test.tsx`
- Modify: `packages/control-plane/test/responsive-shell.test.tsx`
- Modify: `protocol/control-v1.schema.json`
- Modify: `protocol/fixtures/*.json`

**Contract:** Add `Target::Claude`, `ProviderProtocol::AnthropicMessages`, and `ProviderAuthentication::{OpenaiBearer, AnthropicApiKey, AnthropicBearer}`. Providers, Presets, immutable snapshots, views, receipts, and fixtures carry the exact protocol/authentication projection. Add a target-isolated neutral Route Health projection with state `unobserved`; no T05 observation changes it.

- [ ] **Step 1: Write Rust/TypeScript wire RED tests**

Require exact additive literals:

```json
{
  "target": "claude",
  "protocol": "anthropic-messages",
  "authentication": "anthropic-api-key",
  "routeHealth": { "state": "unobserved" }
}
```

Assert invalid target/protocol/authentication combinations are rejected server-side, both Claude authentication profiles round-trip, unknown additive fields remain compatible, and no credential value is serializable through any projection.

- [ ] **Step 2: Run contract tests and verify RED**

```bash
cargo test -p muxvia-routing --test protocol_contract
bun test packages/control-plane/test/protocol.test.ts
```

Expected: failures name the missing Claude target, Messages protocol, authentication profile, and Route Health projection.

- [ ] **Step 3: Write fresh/v1/v2/v3 migration RED tests**

Build real pre-v4 SQLite fixtures containing Providers, shared Credential References, route state, snapshots, receipts, and pending recovery. Require opening through `StateStore::open` to:

- rebuild all target/protocol-constrained tables for `codex|claude`;
- preserve every legacy row as Codex;
- create a clean Claude route row with independent revision/sequence and `unobserved` health;
- add target to Action Receipt identity so `(target, action_id)` is unique;
- replace Recovery Intent's global action-ID uniqueness with `UNIQUE(target, action_id)` and target-qualified queries/upserts;
- preserve/rewrite legacy receipt projections inside the same immediate migration transaction;
- introduce the tagged recovery envelope, preserve its payloads, and migrate legacy payloads as Codex; and
- pass `PRAGMA foreign_key_check` before schema version 4 commits.

Replay a migrated Codex receipt through the malformed raw action boundary. Then use the same UUID for a new Claude action and prove it is applied rather than replayed. Insert simultaneous Codex and Claude Recovery Intents with the same action UUID and prove target-qualified reconciliation never crosses them.

- [ ] **Step 4: Run migration tests and verify RED**

```bash
cargo test -p muxvia-routing --test provider_declarations schema_v4 -- --nocapture
cargo test -p muxvia-routing --test state_store target_scoped_receipts -- --nocapture
```

Expected: schema/check/receipt failures occur before implementation.

- [ ] **Step 5: Implement schema v4 and typed projections**

Rebuild, copy, verify, and atomically swap the six target-constrained tables plus target-scoped receipts. Add explicit `authentication` to Providers/snapshots, validate exact target/protocol/authentication combinations, and ensure ordinary Claude creation/preset/duplicate/update preserve their server-owned combination. Keep Credential Reference reuse as identity/link reuse, never copied secret bytes.

- [ ] **Step 6: Run Task 1 verification**

```bash
cargo test -p muxvia-routing --test protocol_contract
cargo test -p muxvia-routing --test provider_declarations
cargo test -p muxvia-routing --test state_store
bun test packages/control-plane/test/protocol.test.ts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

- [ ] **Step 7: Commit Task 1**

```bash
git add crates/routing-service/src crates/routing-service/tests packages/control-plane/src/control/types.ts packages/control-plane/test protocol
git commit -m "feat: define target-isolated claude state"
```

---

### Task 2: Target-scoped store, UDS sessions, Provider workflow, and inspection

**Files:**
- Modify: `crates/routing-service/src/control/protocol.rs`
- Modify: `crates/routing-service/src/control/server.rs`
- Modify: `crates/routing-service/src/domain/view.rs`
- Modify: `crates/routing-service/src/service/provider_inspector.rs`
- Modify: `crates/routing-service/src/state/providers.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `crates/routing-service/tests/provider_declarations.rs`
- Modify: `crates/routing-service/tests/provider_lifecycle.rs`
- Modify: `crates/routing-service/tests/provider_duplication.rs`
- Modify: `crates/routing-service/tests/provider_inspection.rs`
- Modify: `packages/control-plane/src/control/rpc-client.ts`
- Modify: `packages/control-plane/src/control/target-session.ts`
- Modify: `packages/control-plane/src/control/types.ts`
- Modify: `packages/control-plane/test/control-socket.test.ts`
- Modify: `packages/control-plane/test/target-session.test.ts`
- Modify: `protocol/control-v1.schema.json`
- Modify: `protocol/fixtures/*.json`
- Create: `protocol/fixtures/claude-initial-target-view.json`

**Contract:** A connection opens exactly one target and all subsequent actions, inspections, refreshes, and pushes are bound to it. `open-target` carries a minimal secret-free Claude preflight context: observed `CLAUDE_CONFIG_DIR`, normalized selector/host-managed states, and cwd. It never carries the environment or secrets.

- [ ] **Step 1: Write target-isolation UDS RED tests**

Using two real UDS clients, require:

- Codex and Claude open independently and receive only their target's pushes;
- reopening the same target supports gap refresh;
- cross-target operations on an opened session fail before state/network work;
- the same action UUID is receipt-first and independent across targets;
- closing/cancelling one session does not affect the other;
- Claude context accepts the closed normalized `unknown-nonempty` selector state for read-only management but rejects invalid wire states, raw environment maps, and credentials; and
- shutdown aborts and awaits target-scoped inspection work without a slow-reader deadlock.

- [ ] **Step 2: Run UDS/session tests and verify RED**

```bash
cargo test -p muxvia-routing --test control_socket target_isolation -- --nocapture
bun test packages/control-plane/test/control-socket.test.ts packages/control-plane/test/target-session.test.ts
```

- [ ] **Step 3: Write Claude Provider workflow RED tests**

Run the complete T03 declaration contract against Claude: create incomplete/complete, edit, reorder, duplicate, delete blockers, Credential Reference reuse, preset copy, revision races, reconnect order, and no Current/Snapshot/Managed Configuration side effects. Require preset `anthropic-api-messages`, protocol `anthropic-messages`, authentication `anthropic-api-key`, and an explicit update path to `anthropic-bearer`.

- [ ] **Step 4: Write Claude inspection RED tests**

Use deterministic real loopback servers. Require same-origin `/models` candidates, bounded pagination, body/model caps, cancellation, and exact authentication headers:

```text
anthropic-api-key -> x-api-key + anthropic-version: 2023-06-01
anthropic-bearer  -> Authorization: Bearer + anthropic-version: 2023-06-01
```

Reachability stays unauthenticated/headers-only and cannot mutate Current, Serving, snapshots, Managed Configuration, or neutral Route Health.

Activation tests in Task 5 will prove that a normalized `unknown-nonempty` selector blocks Takeover before side effects rather than blocking the target session itself.

- [ ] **Step 5: Implement target-scoped store/RPC and Claude inspection**

Pass `Target` through every state query and transaction rather than cloning Codex methods. Keep one bounded response writer and per-session inspection cancellation map. Make `TargetSession` capture its target at construction and remove all hard-coded `"codex"` operation fields.

- [ ] **Step 6: Run Task 2 verification**

```bash
cargo test -p muxvia-routing --test control_socket
cargo test -p muxvia-routing --test provider_declarations
cargo test -p muxvia-routing --test provider_lifecycle
cargo test -p muxvia-routing --test provider_duplication
cargo test -p muxvia-routing --test provider_inspection
bun test packages/control-plane/test/control-socket.test.ts packages/control-plane/test/target-session.test.ts packages/control-plane/test/protocol.test.ts
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
git diff --check
```

- [ ] **Step 7: Commit Task 2**

```bash
git add crates/routing-service packages/control-plane/src/control packages/control-plane/test protocol
git commit -m "feat: add target-scoped claude control sessions"
```

---

### Task 3: Shared Managed File seam and Claude JSON configuration adapter

**Files:**
- Create: `crates/routing-service/src/config/mod.rs`
- Create: `crates/routing-service/src/config/managed_file.rs`
- Modify: `crates/routing-service/src/lib.rs`
- Modify: `crates/routing-service/src/codex/config.rs`
- Create: `crates/routing-service/src/claude/mod.rs`
- Create: `crates/routing-service/src/claude/config.rs`
- Create: `crates/routing-service/src/claude/probe.rs`
- Modify: `crates/routing-service/src/state/recovery.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/tests/codex_config.rs`
- Create: `crates/routing-service/tests/claude_config.rs`
- Modify: `crates/routing-service/tests/recovery.rs`
- Create: `crates/routing-service/tests/fixtures/claude/*.json`

**Contract:** Extract only atomic file safety from Codex. Parsing/ownership stays target-native. Claude owns three exact `env` members, preserves all unrelated semantic JSON and file mode, and records prior value/absence in a target-tagged recovery payload.

- [ ] **Step 1: Characterize shared filesystem behavior before extraction**

Keep or move the existing Codex adversarial tests for canonical parent handles, final-file no-follow, identity races, no-replace/exchange rollback, restrictive umask, directory sync, and retained displaced artifacts. Run them green before refactoring.

- [ ] **Step 2: Write Claude JSON codec RED tests**

Cover absent file, unrelated objects/arrays/scalars, existing `env`, owned prior absence/value, file mode, restrictive umask, Configuration Home directory symlink, final-file symlink rejection, invalid JSON, non-object root/env, identity races, exact restore, unrelated edits after apply, third-state drift, and Debug/error redaction.

Require only:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:<port>",
    "ANTHROPIC_AUTH_TOKEN": "<routing credential>",
    "ANTHROPIC_MODEL": "<snapshot model>"
  }
}
```

- [ ] **Step 3: Write selector/shadow/probe RED tests**

Require all five provider selectors and host-managed mode to block without mutation when active or unknown-nonempty. Require `0`, `false`, and documented empty settings values not to block. Cover nondefault `CLAUDE_CONFIG_DIR`, observable managed/shared/local settings shadows, unsupported homes, tested/unknown-compatible/incompatible fake Claude versions, and the stated limits for CLI flags, `/model`, resumed state, and external environment.

- [ ] **Step 4: Implement Managed File and Claude adapters**

Move no parsing logic into `managed_file.rs`. Refactor Codex to use the shared file transaction unchanged, then implement Claude JSON observation/apply/restore/reconcile and a read-only capability probe. Use a tagged target-specific recovery state; never serialize or log routing/provider secrets.

- [ ] **Step 5: Run Task 3 verification**

```bash
cargo test -p muxvia-routing --test codex_config
cargo test -p muxvia-routing --test claude_config
cargo test -p muxvia-routing --test recovery
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

- [ ] **Step 6: Commit Task 3**

```bash
git add crates/routing-service/src/config crates/routing-service/src/claude crates/routing-service/src/codex/config.rs crates/routing-service/src/lib.rs crates/routing-service/src/state crates/routing-service/tests
git commit -m "feat: manage claude settings safely"
```

---

### Task 4: Authenticated Anthropic Messages model endpoint

**Files:**
- Modify: `crates/routing-service/src/model/mod.rs`
- Modify: `crates/routing-service/src/model/server.rs`
- Modify: `crates/routing-service/src/model/auth.rs`
- Modify: `crates/routing-service/src/model/headers.rs`
- Modify: `crates/routing-service/src/model/upstream.rs`
- Create: `crates/routing-service/src/model/messages.rs`
- Modify: `crates/routing-service/Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/tests/model_route.rs`
- Create: `crates/routing-service/tests/claude_model_route.rs`
- Create: `crates/routing-service/tests/fixtures/claude/messages-*.json`
- Create: `crates/routing-service/tests/fixtures/claude/messages-*.sse`
- Create: `crates/routing-service/tests/fixtures/claude/manifest.json`

**Contract:** Model server mechanics are shared, but Responses and Messages remain native endpoint adapters. A Claude request validates the Claude Routing Credential before body/state/provider-secret work, pins one Claude snapshot, owns only top-level model, and streams the upstream response.

- [ ] **Step 1: Write authentication and path RED tests**

Bind real loopback servers and require `POST /v1/messages`, `/v1/messages?beta=true`, and `/v1/messages/count_tokens`. Missing/wrong/short/long/non-ASCII/malformed/duplicate credentials and the Codex credential all return one generic 401 with zero upstream calls and no Provider-secret read. Claude credential against Codex behaves identically.

- [ ] **Step 2: Write bounded-body, compression, and upstream-auth RED tests**

Require Content-Length, encoded, decoded, or streamed bodies over 32 MiB to return fixed 413, invalid/non-object JSON to return fixed 400, cancellation during buffering to drop work, and valid objects to insert/replace only top-level `model`. Identity and gzip/x-gzip requests are accepted; decode is output-bounded, rebuilt upstream JSON removes Content-Encoding/Content-Length, and unsupported/stacked encodings return fixed 415 with zero upstream calls. Reqwest automatic decompression stays off: gzip non-stream responses and gzip upstream errors preserve Content-Encoding and exact compressed bytes. Assert exact API-key versus Bearer upstream injection, removal of inbound routing/auth credentials, and no secret in diagnostics.

- [ ] **Step 3: Write forwarding/streaming RED tests**

Use golden fixtures for system blocks, tools/tool_choice, thinking, metadata, context/output fields, compatible unknown fields, repeated `anthropic-beta`, version/correlation/future headers, query preservation, token counting, request/response compression behavior, non-2xx status/body, SSE byte order, delayed first/last chunks, request upload, downstream cancellation, and hop-by-hop/Connection-nominated stripping.

The fixture manifest records for every fixture: oracle type, official source URL or pinned CC-Switch path/commit, retrieval date, exact behavior proved, file hash, and any Muxvia compatibility deviation.

Assert Serving publishes only after a Claude 2xx response head, uses the request-pinned snapshot under a concurrent switch, advances only Claude view sequence, and cannot alter an already committed upstream response if observation fails.

- [ ] **Step 4: Implement native Messages adapter**

Keep shared credential validation/header sanitation/stream ownership small. Do not introduce a generic LLM request schema. Parse one bounded JSON object solely to set `model`; serialize it once upstream. Stream response bytes directly without detached pumps.

- [ ] **Step 5: Run Task 4 verification**

```bash
cargo test -p muxvia-routing --test model_route
cargo test -p muxvia-routing --test claude_model_route
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

- [ ] **Step 6: Commit Task 4**

```bash
git add Cargo.lock crates/routing-service/Cargo.toml crates/routing-service/src/model crates/routing-service/src/state/store.rs crates/routing-service/tests
git commit -m "feat: proxy authenticated claude messages"
```

---

### Task 5: Transactional Claude Takeover, recovery, and dual runtime lifecycle

**Files:**
- Modify: `crates/routing-service/src/service/activate.rs`
- Modify: `crates/routing-service/src/service/process.rs`
- Modify: `crates/routing-service/src/control/server.rs`
- Modify: `crates/routing-service/src/main.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/state/recovery.rs`
- Modify: `crates/routing-service/src/domain/activation.rs`
- Modify: `crates/routing-service/src/domain/view.rs`
- Modify: `crates/routing-service/src/model/server.rs`
- Modify: `crates/routing-service/tests/activation.rs`
- Modify: `crates/routing-service/tests/recovery.rs`
- Modify: `crates/routing-service/tests/control_socket.rs`
- Modify: `crates/routing-service/tests/process_lifecycle.rs`

**Contract:** Activation has one target-aware coordinator with target-native configuration/model adapters, independent per-target gates and runtime slots, and one receipt/publication owner. Provisional listener/credential/snapshot candidates are not serving or persisted before final commit.

- [ ] **Step 1: Write Claude activation RED tests**

Through `ActivationService::apply_raw`, require one complete Claude Provider to:

- validate receipt/revision/provider/protocol/auth/home/selectors/shadows/probe/managed state before side effects;
- reserve a distinct IPv4 loopback port and stable Claude Routing Credential;
- create an immutable Claude snapshot containing protocol/authentication/provider credential;
- insert recovery intent before settings mutation;
- apply/reread settings and atomically commit Current/Snapshot/Takeover/receipt/recovery;
- publish exactly one complete Claude view; and
- leave every Codex projection, database fingerprint, file, listener, credential, and sequence unchanged.

Also dispatch `target=claude, mode=direct` through the raw production action boundary and require a stable rejection before probe, intent, listener, credential/snapshot candidate, database, or file work.

- [ ] **Step 2: Write transition/fault/recovery RED tests**

Cover Claude Provider hot switch with endpoint/credential reuse and next-request snapshot pinning. Inject failure after listener reservation, credential generation, snapshot creation, intent insertion, atomic write, verification, and final revision check. Pre-intent failures release all provisional resources; post-intent/pre-commit failures exact-restore or mark only Claude Recovery Required. Same-action retry remains receipt-first.

Separately inject subscriber disappearance/publication failure after a successful DB commit. The Applied receipt, settings, Current, snapshot, and runtime stay committed; no rollback or duplicate publication occurs, and a later refresh/replay returns the authoritative committed view.

- [ ] **Step 3: Write startup/drift/lifecycle RED tests**

Across process epochs require clean Codex/Claude takeovers to bind exact independent endpoints, one-target Recovery Required or Configuration Drift to stay control-only with no write/listener while the other resumes, occupied persisted ports to fail closed before UDS, and service lifetime to continue while either committed takeover or unresolved recovery/drift exists. Explicit shutdown drains both model servers and UDS sessions.

- [ ] **Step 4: Implement target-aware activation and runtime ownership**

Factor only orchestration ordering and resource ownership. Dispatch to the Codex or Claude configuration adapter and Responses or Messages runtime. Store per-target activation mutexes/runtime slots. Ensure replay and Model Serving publication have exactly one owner.

- [ ] **Step 5: Run Task 5 verification**

```bash
cargo test -p muxvia-routing --test activation
cargo test -p muxvia-routing --test recovery
cargo test -p muxvia-routing --test control_socket
cargo test -p muxvia-routing --test process_lifecycle -- --test-threads=1
cargo test -p muxvia-routing --test model_route
cargo test -p muxvia-routing --test claude_model_route
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

- [ ] **Step 6: Commit Task 5**

```bash
git add crates/routing-service/src crates/routing-service/tests
git commit -m "feat: activate claude takeover independently"
```

---

### Task 6: Real Claude Control Plane context

**Files:**
- Modify: `packages/control-plane/src/app.tsx`
- Modify: `packages/control-plane/src/control/rpc-client.ts`
- Modify: `packages/control-plane/src/control/target-session.ts`
- Modify: `packages/control-plane/src/ui/app.tsx`
- Modify: `packages/control-plane/src/ui/claude-context.tsx`
- Modify: `packages/control-plane/src/ui/target-view.tsx`
- Modify: `packages/control-plane/src/ui/target-sidebar.tsx`
- Modify: `packages/control-plane/src/ui/provider-picker.tsx`
- Modify: `packages/control-plane/src/ui/provider-form.tsx`
- Modify: `packages/control-plane/src/ui/provider-model-picker.tsx`
- Modify: `packages/control-plane/src/commands/catalog.ts`
- Modify: `packages/control-plane/src/i18n/en.ts`
- Modify: `packages/control-plane/src/i18n/zh-cn.ts`
- Modify: `packages/control-plane/test/app-lifecycle.test.tsx`
- Modify: `packages/control-plane/test/app-render.test.tsx`
- Modify: `packages/control-plane/test/provider-workflow.test.tsx`
- Modify: `packages/control-plane/test/provider-inspection-ui.test.tsx`
- Modify: `packages/control-plane/test/responsive-shell.test.tsx`
- Modify: `packages/control-plane/test/localization.test.ts`
- Modify: `packages/control-plane/test/commands.test.tsx`

**Contract:** The process opens independent target sessions and renders Claude through the real OpenCode-style target shell. Shared UI components receive target/session/view explicitly. Claude offers Provider CRUD, Discovery, Reachability, and Takeover; it never offers Direct in T05.

- [ ] **Step 1: Write dual-session lifecycle RED tests**

Require startup to open Codex and Claude over independent RPC clients, preserve target-specific pushes/gap refresh, cancel both bounded connections on signals, close both exactly once on normal/error paths, and never start two Routing Services. One failed target session yields a stable target-specific unavailable state without corrupting the other.

- [ ] **Step 2: Write Claude renderer workflow RED tests**

Using real OpenTUI/keymap renderers, drive Home to Claude, create/edit/reorder/duplicate/delete Providers, choose API-key/Bearer authentication, run automatic/explicit Discovery and Reachability, apply Takeover, switch Providers, and render Current/Serving/endpoint/config/restart/recovery/neutral-health state. Assert all commands dispatch against Claude only.

- [ ] **Step 3: Write modal/focus/localization/security RED tests**

Cover pending activation gating, exact overlay identity, cancel focus restore, target switching during async work, stale completion suppression, unknown/incomplete/stable errors, English/zh-CN labels, 1x1/120/121-column behavior, and absence of Claude Direct actions. Every post-credential frame/assertion is scan-first with fixed diagnostics.

- [ ] **Step 4: Implement real Claude context**

Replace the static preview with shared target components parameterized by target/session/view and target capabilities. Keep selected Provider, dirty form, pending actions, overlays, notices, activities, and command layers owned by the originating target. Do not duplicate the complete Codex UI tree.

- [ ] **Step 5: Run Task 6 verification**

```bash
bun test packages/control-plane/test/app-lifecycle.test.tsx packages/control-plane/test/app-render.test.tsx
bun test packages/control-plane/test/provider-workflow.test.tsx packages/control-plane/test/provider-inspection-ui.test.tsx
bun test packages/control-plane/test/commands.test.tsx packages/control-plane/test/overlays.test.tsx
bun test packages/control-plane/test/responsive-shell.test.tsx packages/control-plane/test/localization.test.ts
bun run typecheck
git diff --check
```

- [ ] **Step 6: Commit Task 6**

```bash
git add packages/control-plane/src packages/control-plane/test
git commit -m "feat: add real claude control plane context"
```

---

### Task 7: Real-process Claude tracer and final security proof

**Files:**
- Create: `tests/e2e/fixtures/fake-claude`
- Modify: `tests/e2e/fake-upstream.ts`
- Modify: `packages/control-plane/test/walking-skeleton.e2e.tsx`
- Modify: `tests/e2e/walking-skeleton.test.ts`
- Modify: `crates/routing-service/tests/process_lifecycle.rs` only if the real tracer proves a production lifecycle defect

**Contract:** Prove the complete user-visible and network path with the real Routing Service binary, real UDS/RpcClient/TargetSession, real OpenTUI renderer, temporary Claude/Codex homes, deterministic upstream, SQLite, process restart, and no test-only product seam.

- [ ] **Step 1: Extend fake Claude and upstream fixtures**

The fake CLI exposes deterministic version/help behavior and never accesses real homes. The upstream captures scan-first safe projections for API-key/Bearer Messages, token counting, query/header/unknown-field behavior, errors, and delayed SSE; every handler is included in an event-driven quiescence barrier.

- [ ] **Step 2: Write the real tracer RED**

Drive named commands through these observable steps:

1. establish an active Codex Takeover baseline, fingerprint its endpoint/credential/snapshot/configuration, then open Claude and create an API-key Provider;
2. prove automatic/explicit Model Discovery and Reachability leave DB/config/neutral health unchanged;
3. enable Claude Takeover and verify only three settings fields, exact file mode, snapshot, route credential, and restart guidance;
4. send wrong, Codex, and correct Claude credentials and prove pre-upstream isolation;
5. send Messages, tools/unknown fields, count_tokens, errors, query, and delayed SSE;
6. switch to a Bearer Provider during an in-flight stream and prove old/new snapshot pinning plus independent Serving;
7. keep the active Codex configuration/state/network fingerprints unchanged while both routes remain authenticated and serving;
8. close the Control Plane and prove both active routes continue;
9. restart the service and recover exact independent endpoints/credentials/snapshots; and
10. explicitly drain shutdown and prove UDS plus both model listeners close.

- [ ] **Step 3: Add scan-first and restrictive-environment proofs**

Separate audit surfaces explicitly:

- zero-secret observations: decoded RPC frames, Target Views, receipts, activities, every native frame, and drained process output must contain no Provider or Routing Credential; and
- authorized secret-bearing state: Claude settings, SQLite credential columns, and captured upstream requests may contain only the exact credential in its approved target/path/header location, never in another target or field.

Create redacted safe projections before assertions, use fixed diagnostics, and add controlled mutation tests proving a misplaced secret fails without printing it. Run the real tracer in an isolated restrictive-umask child with explicit file modes and HOME/CLAUDE_CONFIG_DIR/CODEX_HOME traps outside the temporary target home.

- [ ] **Step 4: Run focused and full verification**

```bash
bun test ./packages/control-plane/test/walking-skeleton.e2e.tsx
bun test ./tests/e2e/walking-skeleton.test.ts
cargo test --workspace
bun install --frozen-lockfile
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
bun run typecheck
bun run verify
git diff --check
```

Expected: all commands exit 0; macOS and Linux CI exercise the same real process/UDS/loopback contracts.

- [ ] **Step 5: Commit Task 7**

```bash
git add tests/e2e packages/control-plane/test/walking-skeleton.e2e.tsx crates/routing-service/tests/process_lifecycle.rs
git commit -m "test: prove claude takeover end to end"
```

---

## Completion gate

- [ ] Request an independent Standards and Spec review of the complete T05 diff against Issue #6 and this plan.
- [ ] Resolve every Critical/Important finding with a witnessed RED and fresh GREEN; resolve Minor findings that affect correctness, security, determinism, or maintainability.
- [ ] Run `bun run verify` after the final review fix and confirm a clean worktree.
- [ ] Update the ignored SDD task reports with exact RED/GREEN evidence and commit hashes.
- [ ] Present the completed branch for the user's chosen local merge/push workflow; do not merge or push without that instruction.
