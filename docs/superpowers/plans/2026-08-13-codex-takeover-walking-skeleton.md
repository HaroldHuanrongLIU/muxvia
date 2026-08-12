# Codex Takeover Walking Skeleton Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the smallest complete Muxvia path in which an Operator creates one Codex Target Provider in an OpenTUI control plane, applies authenticated Target Takeover, routes a real Responses/SSE request through a separate Rust service, and observes Current and Serving state.

**Architecture:** A Bun/TypeScript/Solid/OpenTUI Control Plane talks to a single Rust Routing Service over a private, same-user Unix-domain socket. The Routing Service alone owns SQLite, Codex managed configuration, the authenticated IPv4 loopback model listener, immutable Activated Snapshots, and runtime Target Views. Tests cross real file, socket, HTTP, renderer, and process boundaries while always using temporary homes.

**Tech Stack:** Bun 1.3.14; TypeScript 5.8.2; Solid 1.9.12; OpenTUI core/solid 0.4.3; Rust 1.96.1; Tokio 1.53.1; Axum 0.8.9; Reqwest 0.13.4; Tokio-Rusqlite 0.7.0/Rusqlite 0.37; TOML Edit 0.25.13; SQLite; Unix-domain sockets; GitHub Actions.

## Global Constraints

- T01 is a vertical walking skeleton, not a reusable layer scaffold; every task ends in observable behavior used by the final path.
- The Routing Service is a separate process and the only process that opens or migrates SQLite or mutates managed configuration.
- Management uses `~/.muxvia/run/control.sock`, directory mode `0700`, socket mode `0600`, a four-byte unsigned big-endian length prefix, UTF-8 JSON, and a hard `1_048_576`-byte frame limit.
- Management peer identity must match the service effective UID on both macOS and Linux; inability to determine peer identity fails closed.
- RPC version is major `1`, minor `0`; a major mismatch ends the connection before state access or mutation, while unknown additive fields are ignored.
- Every mutation carries a UUID action ID and expected management revision; replay returns the persisted secret-free outcome without inspecting or applying the second action payload.
- Runtime Serving observations increment only `viewSequence`; declaration, activation, and recovery mutations increment `managementRevision` and `viewSequence`.
- Target Views and ordinary errors/logs never contain Provider API credentials, Routing Credentials, authorization headers, raw recovery state, or secret-bearing configuration.
- The only accepted Provider base URLs are HTTPS or loopback HTTP; user info, query, fragment, and non-loopback plaintext HTTP are rejected, and trailing separators normalize deterministically.
- Codex managed configuration is only the default `~/.codex/config.toml`; reject symlinked managed files and non-Muxvia collision at reserved table `model_providers.muxvia_codex`.
- Managed Codex fields are top-level `model`, top-level `model_provider`, and reserved-provider fields `name`, `base_url`, `wire_api`, `http_headers`, and `supports_websockets`; preserve unrelated semantic content, comments supported by TOML Edit, and file mode.
- Activation binds `127.0.0.1:<stable-port>` before configuration mutation, persists a stable per-target Routing Credential, writes and verifies TOML atomically, then commits Current/Snapshot/Takeover/receipt in one SQLite transaction.
- The model plane exposes only `POST /v1/responses`; missing or wrong `X-Muxvia-Routing-Credential` returns generic `401` before loading the upstream secret or contacting upstream.
- Proxying is body-transparent and streaming; strip hop-by-hop, Host, Content-Length, incoming routing, and incoming Authorization headers, then set the selected Provider bearer credential.
- No Activated Snapshot returns `503`; pre-commit upstream connection failure returns `502`; other upstream status, headers, bytes, and SSE order pass through.
- Only a committed upstream `2xx` response head updates Serving and `viewSequence`; it never changes Current, Snapshot, or `managementRevision`.
- All automated and demonstration paths set a temporary `HOME`; no test or demo may read or write the Operator's real Muxvia or Codex homes.
- The TUI uses one monospace scale, cell spacing, a prompt/stream layout, and no dashboard tabs or permanent navigation sidebar; full OpenCode shell parity remains T02.
- Production Control Plane startup receives the Routing Service executable as an absolute path and never searches `PATH` for the private sidecar.
- Implement every production behavior with a witnessed red-green-refactor cycle. Generated lockfiles and configuration are covered by the first consumer's failing test rather than a standalone scaffold task.
- T01 excludes Claude, Direct Activation, provider update/delete/reorder, Universal Providers, failover, circuit breaking, subscription features, usage/pricing, import/export, backup/restore, update/release distribution, and production-grade lifecycle handover.

---

### Task 1: Cross-language control protocol and workspace

**Files:**
- Create: `package.json`
- Create: `bunfig.toml`
- Create: `packages/control-plane/package.json`
- Create: `packages/control-plane/tsconfig.json`
- Create: `packages/control-plane/src/control/types.ts`
- Create: `packages/control-plane/src/control/framing.ts`
- Create: `packages/control-plane/test/protocol.test.ts`
- Create: `Cargo.toml`
- Create: `crates/routing-service/Cargo.toml`
- Create: `crates/routing-service/src/lib.rs`
- Create: `crates/routing-service/src/control/mod.rs`
- Create: `crates/routing-service/src/control/protocol.rs`
- Create: `crates/routing-service/src/control/framing.rs`
- Create: `crates/routing-service/tests/protocol_contract.rs`
- Create: `protocol/control-v1.schema.json`
- Create: `protocol/fixtures/hello.json`
- Create: `protocol/fixtures/initial-target-view.json`
- Create: `protocol/fixtures/save-provider.json`
- Generate: `bun.lock`
- Generate: `Cargo.lock`

**Interfaces:**
- Produces TS `ClientFrame`, `ServerFrame`, `TargetView`, `TargetAction`, `ActionOutcome`, `ControlProblem`, `encodeFrame(value): Uint8Array`, and `FrameDecoder.push(chunk): unknown[]`.
- Produces Rust `ClientFrame`, `ServerFrame`, `TargetView`, `TargetAction`, `ActionOutcome`, `ControlProblem`, `read_frame`, and `write_frame` with wire names matching TypeScript exactly.
- Uses RPC `1.0`, frame limit `1_048_576`, target literal `codex`, and lower-kebab-case tagged variants.

- [ ] **Step 1: Create the minimum manifests and language-neutral contract fixture**

Pin the JavaScript package versions exactly:

```json
{
  "name": "muxvia",
  "private": true,
  "type": "module",
  "packageManager": "bun@1.3.14",
  "workspaces": ["packages/*"],
  "scripts": {
    "test:ts": "bun test packages/control-plane/test",
    "typecheck": "bunx tsc -p packages/control-plane/tsconfig.json --noEmit",
    "test:rust": "cargo test --workspace"
  }
}
```

The Control Plane package must pin `@opentui/core@0.4.3`, `@opentui/solid@0.4.3`, `solid-js@1.9.12`, `web-tree-sitter@0.25.10`, `zod@4.4.3`, `@tsconfig/bun@1.0.10`, `@types/bun@1.3.14`, and `typescript@5.8.2`. Configure both top-level and `[test]` preloads as `@opentui/solid/preload`; configure strict JSX with `jsxImportSource: "@opentui/solid"`.

Use Rust edition 2024 and exact dependency floors: `tokio=1.53.1`, `serde=1.0.229`, `serde_json=1.0.151`, `uuid=1.24.0`, `thiserror=2.0.20`, and `bytes=1.12.1`. Enable only features used by the task.

The canonical initial Target View fixture is:

```json
{
  "target": "codex",
  "managementRevision": 0,
  "viewSequence": 0,
  "service": { "epoch": "00000000-0000-4000-8000-000000000001", "state": "running" },
  "mode": "unmanaged",
  "takeover": { "state": "inactive", "endpoint": null },
  "providers": [],
  "currentProviderId": null,
  "servingProviderId": null,
  "managedConfiguration": { "state": "unmanaged", "path": null, "restartRequired": false },
  "activatedSnapshot": null,
  "problems": []
}
```

- [ ] **Step 2: Write failing Rust and TypeScript protocol tests**

Write table-driven tests that read all three JSON fixtures, decode them to the expected tagged type, encode them back to JSON values, and compare semantic equality. Add these focused framing cases in both languages:

```ts
test("decodes a frame split across arbitrary chunks", () => {
  const frame = encodeFrame({ type: "hello", rpc: { major: 1, minor: 0 }, release: "test" })
  const decoder = new FrameDecoder()
  expect(decoder.push(frame.subarray(0, 2))).toEqual([])
  expect(decoder.push(frame.subarray(2, 7))).toEqual([])
  expect(decoder.push(frame.subarray(7))).toEqual([
    { type: "hello", rpc: { major: 1, minor: 0 }, release: "test" },
  ])
})

test("rejects the advertised length before allocating an oversized body", () => {
  const decoder = new FrameDecoder()
  const prefix = new Uint8Array([0, 16, 0, 1])
  expect(() => decoder.push(prefix)).toThrow("frame-too-large")
})
```

Rust async tests must separately assert invalid UTF-8, invalid JSON, partial EOF, and big-endian encoding. Add a projection serialization assertion using unique strings `provider-secret-must-not-escape` and `routing-secret-must-not-escape`; neither may appear in serialized `TargetView` or `ControlProblem`.

- [ ] **Step 3: Run the focused tests and witness RED**

Run:

```bash
bun test packages/control-plane/test/protocol.test.ts
cargo test -p muxvia-routing --test protocol_contract
```

Expected: TypeScript fails because `src/control/types.ts`/`framing.ts` do not exist; Rust fails because `control::protocol`/`control::framing` do not exist.

- [ ] **Step 4: Implement the minimum shared wire contract**

Use these exact envelope shapes:

```ts
type ClientFrame =
  | { type: "hello"; rpc: { major: 1; minor: 0 }; release: string }
  | { type: "request"; requestId: string; operation: ControlOperation }

type ServerFrame =
  | { type: "hello-ack"; rpc: { major: 1; minor: 0 }; release: string; serviceEpoch: string; frameLimit: 1048576 }
  | { type: "response"; requestId: string; result: ControlResult }
  | { type: "error"; requestId: string | null; problem: ControlProblem; authoritativeView?: TargetView }
  | { type: "target-view"; view: TargetView }

type ControlOperation =
  | { kind: "open-target"; target: "codex" }
  | { kind: "act"; target: "codex"; actionId: string; expectedRevision: number; action: unknown }

type TargetAction =
  | { kind: "save-provider"; name: string; baseUrl: string; model: string; credential: string }
  | { kind: "activate-provider"; providerId: string; mode: "takeover" }
```

`act.action` stays untyped in the decoded envelope until the service has checked for an existing action receipt; `parseTargetAction` performs the later typed validation. Zod schemas and Serde must ignore unknown additive fields at RPC major 1. Neither side uses `deny_unknown_fields` on extensible envelopes.

Implement length parsing before allocation, explicit UTF-8 validation, JSON validation, and exact error codes `frame-too-large`, `invalid-utf8`, `invalid-json`, and `unexpected-eof`. No framing error includes payload bytes.

- [ ] **Step 5: Run focused and workspace tests, then commit**

Run:

```bash
bun install
bun test packages/control-plane/test/protocol.test.ts
bun run typecheck
cargo test -p muxvia-routing --test protocol_contract
cargo fmt --all -- --check
```

Expected: all protocol tests pass, type checking succeeds, and formatting is clean.

Commit:

```bash
git add package.json bunfig.toml bun.lock packages Cargo.toml Cargo.lock crates protocol
git commit -m "feat: define control protocol contract"
```

### Task 2: Authoritative provider state and idempotent actions

**Files:**
- Create: `crates/routing-service/src/home.rs`
- Create: `crates/routing-service/src/domain/mod.rs`
- Create: `crates/routing-service/src/domain/provider.rs`
- Create: `crates/routing-service/src/domain/view.rs`
- Create: `crates/routing-service/src/state/mod.rs`
- Create: `crates/routing-service/src/state/schema.sql`
- Create: `crates/routing-service/src/state/store.rs`
- Create: `crates/routing-service/tests/state_store.rs`
- Modify: `crates/routing-service/src/lib.rs`
- Modify: `crates/routing-service/Cargo.toml`

**Interfaces:**
- Consumes protocol `TargetView`, `ActionOutcome`, `ControlProblem`, and untyped action envelopes from Task 1.
- Produces `MuxviaHome::from_user_home(&Path)`, `normalize_provider_base_url(&str)`, and async `StateStore::{open,target_view,receipt,save_provider}`.
- `save_provider` accepts `SaveProviderCommand { action_id, expected_revision, name, base_url, model, credential }` and returns either `ActionOutcome` or `ActionFailure { problem, authoritative_view }`.

- [ ] **Step 1: Write failing state and domain tests against real temporary SQLite**

Add one test per behavior:

```rust
#[tokio::test]
async fn save_provider_persists_secret_separately_and_projects_no_secret() {
    let fixture = StoreFixture::new().await;
    let result = fixture.store.save_provider(SaveProviderCommand {
        action_id: fixed_uuid(10),
        expected_revision: 0,
        name: "Local test".into(),
        base_url: "http://127.0.0.1:4567/v1/".into(),
        model: "gpt-test".into(),
        credential: SecretString::from("provider-secret-must-not-escape"),
    }).await.unwrap();
    assert_eq!(result.status, ActionStatus::Applied);
    assert_eq!(result.view.management_revision, 1);
    assert_eq!(result.view.providers[0].base_url, "http://127.0.0.1:4567/v1");
    assert_eq!(result.view.providers[0].credential, CredentialPresence::Present);
    assert!(!serde_json::to_string(&result).unwrap().contains("provider-secret-must-not-escape"));
}
```

Also prove:

- replaying action ID `fixed_uuid(10)` with a different expected revision and a malformed second payload returns the recorded revision-1 outcome without a second Provider;
- a new action ID with expected revision `0` returns `stale-revision`, includes the authoritative revision-1 view, and changes no rows;
- HTTPS and `http://127.0.0.1`, `http://localhost`, and `http://[::1]` normalize; public HTTP, user info, query, and fragment reject with `invalid-provider`;
- empty name/model/credential returns `incomplete-provider` and does not consume the action ID;
- saving changes both revision and sequence from `0` to `1`; opening alone changes neither;
- database, credential-bearing database file, and created home directories receive owner-only permissions on Unix;
- no Target View/debug/error string includes either persisted secret.

- [ ] **Step 2: Run the state test and witness RED**

Run:

```bash
cargo test -p muxvia-routing --test state_store
```

Expected: compilation fails because `MuxviaHome`, provider validation, and `StateStore` are absent.

- [ ] **Step 3: Implement the minimum home, schema, and database actor**

Use `tokio-rusqlite=0.7.0` with its compatible `rusqlite=0.37` and `bundled` feature rather than introducing two Rusqlite versions. Add `secrecy=0.10.3`, `url=2.5.8`, and `libc=0.2.189`.

The initial schema must have these responsibilities, with foreign keys enabled:

```sql
CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE providers (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL
);
CREATE TABLE provider_credentials (
  provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
  bearer_token TEXT NOT NULL
);
CREATE TABLE target_route_state (
  target TEXT PRIMARY KEY CHECK (target = 'codex'),
  management_revision INTEGER NOT NULL,
  view_sequence INTEGER NOT NULL,
  current_provider_id TEXT,
  serving_provider_id TEXT,
  takeover_state TEXT NOT NULL,
  route_port INTEGER,
  routing_credential TEXT,
  activated_snapshot_id TEXT,
  recovery_state TEXT NOT NULL
);
CREATE TABLE action_receipts (
  action_id TEXT PRIMARY KEY,
  action_kind TEXT NOT NULL,
  committed_revision INTEGER NOT NULL,
  outcome_json TEXT NOT NULL
);
```

`StateStore` owns one dedicated SQLite thread, runs migrations only after file permissions are established, uses `PRAGMA foreign_keys=ON`, and uses `BEGIN IMMEDIATE` for mutation transactions. `save_provider` first looks up the action receipt by action ID, before parsing or validating the supplied second payload. For a new action ID it checks revision and fields, inserts declaration and credential separately, increments both counters, stores secret-free JSON outcome, and commits once.

Provider URLs normalize by removing empty trailing path separators while retaining the API-root path. `localhost`, `127.0.0.0/8`, and IPv6 loopback are loopback; all other plaintext hosts reject. Generated Provider IDs are UUID v4.

- [ ] **Step 4: Run tests, inspect secret boundaries, and commit**

Run:

```bash
cargo test -p muxvia-routing --test state_store
cargo test -p muxvia-routing
cargo fmt --all -- --check
cargo clippy -p muxvia-routing --all-targets -- -D warnings
```

Expected: state tests pass and Clippy is warning-free.

Commit:

```bash
git add Cargo.lock crates/routing-service
git commit -m "feat: persist codex provider state"
```

### Task 3: Private UDS control session

**Files:**
- Create: `crates/routing-service/src/control/server.rs`
- Create: `crates/routing-service/tests/control_socket.rs`
- Create: `packages/control-plane/src/control/rpc-client.ts`
- Create: `packages/control-plane/src/control/target-session.ts`
- Create: `packages/control-plane/test/target-session.test.ts`
- Create: `packages/control-plane/test/control-socket.test.ts`
- Modify: `crates/routing-service/src/control/mod.rs`
- Modify: `crates/routing-service/Cargo.toml`
- Modify: `packages/control-plane/src/control/types.ts`

**Interfaces:**
- Consumes `StateStore` and Task 1 framing/types.
- Produces Rust `ControlServer::bind(home, store, release)` and `ControlServerHandle::{socket_path,shutdown}`.
- Produces TS `RpcClient.connect(socketPath, release)`, `MuxviaControl.openTarget("codex")`, and `TargetSession { get, act, subscribe, close }`.
- The Target Session serializes actions, generates `crypto.randomUUID()` action IDs, supplies its current `managementRevision`, and never silently retries stale intent.

- [ ] **Step 1: Write failing real-socket and Target Session tests**

Rust real-UDS tests must prove:

```rust
#[tokio::test]
async fn major_mismatch_closes_before_opening_state() {
    let fixture = ControlFixture::start().await;
    let mut stream = UnixStream::connect(fixture.socket()).await.unwrap();
    write_json(&mut stream, json!({"type":"hello","rpc":{"major":2,"minor":0},"release":"test"})).await;
    let reply = read_json(&mut stream).await;
    assert_eq!(reply["problem"]["code"], "protocol-mismatch");
    assert!(read_frame(&mut stream).await.is_err());
    assert_eq!(fixture.store.target_view().await.unwrap().management_revision, 0);
}
```

Add cases for socket/runtime modes, same-UID authorization through a real connection, a pure `peer_uid_matches(peer, effective)` rejection, malformed/oversized frames executing no action, unknown operation, open-target, pushed complete views, stale revision, and replay with malformed second `action`.

TypeScript tests use a scripted local Unix server and assert:

```ts
test("a target session serializes actions and replaces stale state", async () => {
  const { session, server } = await openScriptedSession(initialView)
  const first = session.act({ kind: "save-provider", name: "P", baseUrl: "https://p.test/v1", model: "m", credential: "s" })
  const second = session.act({ kind: "activate-provider", providerId: "p", mode: "takeover" })
  expect(server.receivedActionCount()).toBe(1)
  server.replyApplied(viewAtRevision(1))
  await first
  expect(server.receivedActionCount()).toBe(2)
  server.replyStale(viewAtRevision(2))
  await expect(second).rejects.toMatchObject({ code: "stale-revision" })
  expect(session.get().managementRevision).toBe(2)
})
```

Also prove a pushed sequence gap triggers exactly one fresh `open-target`, subscriptions receive only complete increasing views, and `close()` removes listeners and closes the socket.

- [ ] **Step 2: Run focused tests and witness RED**

Run:

```bash
cargo test -p muxvia-routing --test control_socket
bun test packages/control-plane/test/target-session.test.ts packages/control-plane/test/control-socket.test.ts
```

Expected: both suites fail because the control server, RPC client, and Target Session do not exist.

- [ ] **Step 3: Implement same-user negotiation and request dispatch**

After `UnixListener::accept`, call Tokio `peer_cred()` and compare `uid()` to `unsafe { libc::geteuid() }`; a failed credential lookup or mismatch writes only generic `unauthorized-peer` when possible, then closes. Do not infer peer identity from socket ownership.

The first valid frame must be hello. Reply with negotiated `1.0`, service release, service epoch, and frame limit. Requests before hello, a second hello, and unsupported operations return structured problems. Dispatch `open-target` to `StateStore::target_view`.

For `act`, extract only `actionId` and check `StateStore::receipt` before calling `parseTargetAction`. A receipt returns status `replayed`; a new save action calls `save_provider`. Activate actions return `unsupported-operation` until Task 6. Broadcast the complete committed Target View to sessions that opened Codex; never broadcast a secret-bearing command.

The socket is created under a `0700` run directory and chmodded to `0600` after bind. Remove only a confirmed socket entry owned by the current home; reject a symlink or non-socket collision.

- [ ] **Step 4: Implement the TypeScript RPC adapter and session semantics**

Use `node:net` for Unix sockets and the Task 1 incremental frame decoder. Maintain a request-ID map, reject all pending calls if the socket closes, and bound inbound frames through the common decoder. Implement:

```ts
export interface MuxviaControl {
  openTarget(target: "codex"): Promise<TargetSession>
}

export interface TargetSession {
  get(): Readonly<TargetView>
  act(action: TargetAction): Promise<ActionOutcome>
  subscribe(listener: (next: TargetView) => void): () => void
  close(): Promise<void>
}
```

On a pushed view, accept `sequence === current + 1`; ignore duplicate/older sequence; on a gap, issue one `open-target` refresh and replace rather than infer patches. Action promises are chained so only one is in flight per session. A stale response always installs `authoritativeView` before rejecting with a retryable `ControlError`; it never creates a new action ID automatically.

- [ ] **Step 5: Run both language suites and commit**

Run:

```bash
cargo test -p muxvia-routing --test control_socket
bun test packages/control-plane/test/target-session.test.ts packages/control-plane/test/control-socket.test.ts
bun run typecheck
cargo clippy -p muxvia-routing --all-targets -- -D warnings
```

Expected: real UDS and Target Session tests pass with no warning output.

Commit:

```bash
git add Cargo.lock crates/routing-service packages/control-plane
git commit -m "feat: add private target control session"
```

### Task 4: Formatting-preserving Codex configuration and recovery journal

**Files:**
- Create: `crates/routing-service/src/codex/mod.rs`
- Create: `crates/routing-service/src/codex/probe.rs`
- Create: `crates/routing-service/src/codex/config.rs`
- Create: `crates/routing-service/src/state/recovery.rs`
- Create: `crates/routing-service/tests/codex_config.rs`
- Create: `crates/routing-service/tests/recovery.rs`
- Modify: `crates/routing-service/src/state/schema.sql`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/lib.rs`
- Modify: `crates/routing-service/Cargo.toml`

**Interfaces:**
- Produces `CodexProbe` with `probe(&Path) -> Result<CodexCapability, CodexProblem>` and production `CommandCodexProbe` that runs only `codex --version` and `codex --help`.
- Produces `CodexConfigCodec::{inspect,desired,atomic_apply,restore,verify,reconcile_pending}` over the default `HOME/.codex/config.toml`.
- Produces `OwnedCodexState` that records prior presence/value for every owned key, `FileIdentity`, and `RecoveryIntent` state `pending|committed|rolled-back|recovery-required`.
- No production code resolves a project/profile/custom Codex home in T01.

- [ ] **Step 1: Write failing real-file codec tests**

Use `tempfile::TempDir` and real TOML such as:

```toml
# keep this comment
approval_policy = "on-request"
model = "old-model"
model_provider = "old-provider"

[model_providers.existing]
name = "Existing"
base_url = "https://existing.test/v1"
wire_api = "responses"

[features]
web_search = true
```

Tests must prove one behavior each:

- desired write changes only owned keys to `model = "gpt-test"`, `model_provider = "muxvia_codex"`, and `[model_providers.muxvia_codex]` with `name = "Muxvia"`, `base_url = "http://127.0.0.1:43123/v1"`, `wire_api = "responses"`, `http_headers = { "X-Muxvia-Routing-Credential" = "route-secret" }`, and `supports_websockets = false`;
- the comment, `approval_policy`, existing provider, feature table, and pre-existing file mode remain;
- exact prior absence is restored by removing newly created owned keys/table while keeping unrelated edits absent from the recorded precondition out of T01's restore path;
- missing config creates parent `0700` and file `0600` on Unix;
- a symlinked config rejects before read/write;
- a pre-existing non-Muxvia `model_providers.muxvia_codex` rejects as `configuration-collision`;
- the probe receives only `--version` and `--help` and failure is `incompatible-target-cli`;
- verification compares all owned values plus the unrelated semantic TOML tree, never secrets in error text;
- same-directory atomic replacement plus parent-directory sync is used; an injected pre-rename identity change fails without overwriting the changed target.

- [ ] **Step 2: Write failing pending-recovery tests**

Extend the real SQLite schema with:

```sql
CREATE TABLE activation_recovery (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  action_id TEXT NOT NULL UNIQUE,
  config_path TEXT NOT NULL,
  file_identity_json TEXT NOT NULL,
  before_owned_json TEXT NOT NULL,
  desired_owned_json TEXT NOT NULL,
  state TEXT NOT NULL,
  created_revision INTEGER NOT NULL
);
```

Cover the startup decision table:

| Journal state | Current file | Result |
|---|---|---|
| pending | matches before | mark rolled-back |
| pending | matches desired only | restore exact before, verify, mark rolled-back |
| pending | third state | mark recovery-required, block managed writes |
| committed/rolled-back | any | no startup mutation |

Add an injected restore-write failure and assert the row and Target View both remain `recovery-required`; no test error contains either credential.

- [ ] **Step 3: Run focused tests and witness RED**

Run:

```bash
cargo test -p muxvia-routing --test codex_config
cargo test -p muxvia-routing --test recovery
```

Expected: tests fail because the Codex codec, probe port, and recovery repository are absent.

- [ ] **Step 4: Implement the codec, safe atomic write, probe, and recovery rules**

Add `toml_edit=0.25.13`, `tempfile=3.27.0`, and `async-trait=0.1.89`. Parse with `DocumentMut`; edit only the exact owned items. Capture an unrelated semantic projection by cloning the document and deleting owned keys/table before comparison.

Before each write use `symlink_metadata`, reject symlinks, and capture file identity (`dev`, `ino`, modified time, and length where available). Write a `NamedTempFile` in the destination directory, set old mode or `0600`, flush and `sync_all`, recheck the target identity/precondition, atomically persist/rename, reopen and verify, then sync the parent directory. Error types carry paths and correlation IDs but never rendered desired/before documents.

`CommandCodexProbe` receives an absolute Codex executable path from the caller and invokes only `["--version"]` and `["--help"]`. It rejects a nonzero status, recognizes the pinned tested version fixture as `tested`, and otherwise returns `unknown-compatible` with a warning when the documented general CLI/help surface is intact. T01 blocks only `incompatible` results; it does not invent undocumented `model_provider` help markers. Tests use an injected fake; no model or auth command exists.

Recovery rows store secrets because private recovery is sensitive state, but `Debug` and Target View project only intent ID/state. `reconcile_pending` completes before the control server accepts managed actions.

- [ ] **Step 5: Run tests and commit**

Run:

```bash
cargo test -p muxvia-routing --test codex_config --test recovery
cargo test -p muxvia-routing
cargo fmt --all -- --check
cargo clippy -p muxvia-routing --all-targets -- -D warnings
```

Expected: all codec/recovery cases pass and output is secret-free.

Commit:

```bash
git add Cargo.lock crates/routing-service
git commit -m "feat: manage codex configuration safely"
```

### Task 5: Authenticated streaming Responses route

**Files:**
- Create: `crates/routing-service/src/model/mod.rs`
- Create: `crates/routing-service/src/model/auth.rs`
- Create: `crates/routing-service/src/model/headers.rs`
- Create: `crates/routing-service/src/model/upstream.rs`
- Create: `crates/routing-service/src/model/server.rs`
- Create: `crates/routing-service/tests/model_route.rs`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/lib.rs`
- Modify: `crates/routing-service/Cargo.toml`

**Interfaces:**
- Consumes immutable snapshot lookup from `StateStore` and publishes `record_serving(snapshot_id)` after a committed upstream `2xx` response head.
- Produces `ModelServer::bind_reserved(ReservedListener, store, upstream)` and `ModelServerHandle::{endpoint,shutdown}`.
- Produces internal `UpstreamTransport::send(UpstreamRequest) -> UpstreamResponse`; production `ReqwestUpstream` streams request and response bodies without buffering.
- `ReservedListener` is an already-bound `127.0.0.1` Tokio TCP listener and is later consumed by activation.

- [ ] **Step 1: Write failing header/auth pure tests**

Assert the incoming set:

```text
Connection: keep-alive, X-Remove-Me
X-Remove-Me: secret-hop
Keep-Alive: timeout=5
Host: 127.0.0.1
Content-Length: 123
Authorization: Bearer incoming
X-Muxvia-Routing-Credential: local-secret
OpenAI-Beta: responses=v1
X-Correlation-ID: abc
```

becomes only supported end-to-end headers plus `Authorization: Bearer provider-secret`; the routing credential and dynamic Connection-nominated header never reach upstream. Apply the same RFC hop-by-hop and dynamic Connection-token stripping to response headers.

For credential comparison, generate a fixed-length ASCII credential. Copy the candidate into a same-length buffer, combine a valid-length flag with `subtle::ConstantTimeEq`, and return the same generic `401` for missing, malformed, and wrong values.

- [ ] **Step 2: Write failing real-loopback integration tests**

Start a deterministic Axum fake upstream on `127.0.0.1:0` that records call count, method/path/headers, and emits three delayed byte chunks:

```text
data: {"type":"response.created"}\n\n
data: {"type":"response.output_text.delta","delta":"hello"}\n\n
data: [DONE]\n\n
```

Tests must prove:

- listener local address is IPv4 loopback and only `POST /v1/responses` exists;
- missing/wrong routing credentials return `401` and fake-upstream call count remains zero;
- no Activated Snapshot returns `503` without loading provider credentials;
- upstream connect failure returns `502` before commitment;
- `/api/v1/` normalizes to upstream `/api/v1/responses`, without `Url::join` dropping `v1`;
- request bytes and supported headers reach upstream; auth/routing/hop-by-hop headers do not;
- upstream `429` status/body pass through and do not update Serving;
- upstream `200` status, response headers, total bytes, and SSE event order pass through; cancellation drops the upstream body future;
- after the `200` head, Serving becomes the snapshot Provider and only `viewSequence` increments;
- the model server never binds `0.0.0.0`, `::`, or non-loopback.

- [ ] **Step 3: Run focused tests and witness RED**

Run:

```bash
cargo test -p muxvia-routing --test model_route
```

Expected: compilation fails because model auth, header policy, transport, and server are absent.

- [ ] **Step 4: Implement the single-attempt streaming route**

Add `axum=0.8.9` with `default-features=false, features=["http1","tokio"]`, `reqwest=0.13.4` with `default-features=false, features=["rustls","stream"]`, `subtle=2.6.1`, and `futures-util=0.3.34`. Configure the Reqwest client with redirects disabled and proxy disabled; do not enable decompression and do not call `error_for_status`.

The handler receives `Request<axum::body::Body>`, authenticates before snapshot/secret load, pins one immutable snapshot, creates a Reqwest body from `into_data_stream()`, and creates the Axum response body from `bytes_stream()`. Do not use Axum `Sse`, `collect`, `.bytes()`, or a detached streaming task. Downstream drop must drop the Reqwest response body naturally.

Explicitly append `/responses` to a normalized base-path string. Strip static hop-by-hop headers and names listed in `Connection` on both directions. Request policy additionally strips Host, Content-Length, incoming Authorization, and `X-Muxvia-Routing-Credential`; response policy strips Content-Length because transfer framing is regenerated.

Only after the upstream response head is available and selected status is `2xx`, call `record_serving`. That transaction changes Serving if needed, increments only `view_sequence`, and broadcasts the resulting complete view. The body continues streaming after that commitment point.

- [ ] **Step 5: Run focused and crate suites, then commit**

Run:

```bash
cargo test -p muxvia-routing --test model_route
cargo test -p muxvia-routing
cargo fmt --all -- --check
cargo clippy -p muxvia-routing --all-targets -- -D warnings
```

Expected: all loopback/auth/streaming/state tests pass.

Commit:

```bash
git add Cargo.lock crates/routing-service
git commit -m "feat: proxy authenticated codex responses"
```

### Task 6: Transactional Takeover activation

**Files:**
- Create: `crates/routing-service/src/domain/activation.rs`
- Create: `crates/routing-service/src/service/mod.rs`
- Create: `crates/routing-service/src/service/activate.rs`
- Create: `crates/routing-service/tests/activation.rs`
- Modify: `crates/routing-service/src/control/server.rs`
- Modify: `crates/routing-service/src/model/server.rs`
- Modify: `crates/routing-service/src/state/schema.sql`
- Modify: `crates/routing-service/src/state/store.rs`
- Modify: `crates/routing-service/src/lib.rs`

**Interfaces:**
- Consumes Task 4 `CodexConfigCodec`, Task 5 already-bound `ReservedListener`, and existing `StateStore`/control dispatch.
- Produces `ActivationService::activate(ActivateProviderCommand) -> Result<ActionOutcome, ActionFailure>`.
- Produces immutable `ActivatedSnapshot { id, target, provider_id, base_url, model, provider_credential, epoch }`; snapshots are never updated in place.
- `activate-provider` becomes a supported UDS action and emits one complete post-commit Target View.

- [ ] **Step 1: Write failing activation success tests**

Use a temporary HOME, real SQLite, real config file, injected successful `CodexProbe`, and an actual reserved loopback listener. Verify this order with failpoints recorded by a test observer:

```text
validate -> bind-listener -> persist-routing-credential -> snapshot -> recovery-intent
-> atomic-config-write -> config-verify -> state-and-receipt-commit -> publish-view
```

Assert after success:

- one immutable snapshot contains the saved Provider declaration and secret reference at activation time;
- stable route port and fixed-length Routing Credential are stored once;
- `config.toml` points to `http://127.0.0.1:<port>/v1` and sets the static routing header;
- Current is the Provider, Serving is still null, mode/takeover are active, Managed Configuration is applied with `restartRequired: true`;
- management revision and sequence each advance exactly once from the saved Provider view;
- the recovery row is committed in the same state transaction as Current/Snapshot/Takeover/action receipt;
- replay with the same action ID returns the recorded view without probing, binding, or touching the config again;
- a later Provider declaration mutation cannot alter the immutable snapshot loaded by a routed request.

- [ ] **Step 2: Write failing rollback and fail-closed tests**

Inject failure independently after recovery-intent commit at atomic write, verification, and final database commit. For each, assert the exact before owned state is restored and reread, Current/Snapshot/Takeover/revision stay unchanged, and no success view is published.

Inject a restore verification failure and assert `recovery-required`, future managed writes return `recovery-required`, and no success/rollback wording appears. Also prove:

- incomplete Provider, stale revision, unsupported home, symlink, collision, and failed probe do not create a snapshot or recovery intent;
- first activation reserves an available IPv4 loopback port before write;
- a persisted port that cannot be rebound fails before configuration mutation;
- activation of a different Provider reuses the same port and Routing Credential;
- a second service epoch may load the snapshot but cannot silently select a new port.

- [ ] **Step 3: Run the activation test and witness RED**

Run:

```bash
cargo test -p muxvia-routing --test activation
```

Expected: compilation fails because `ActivationService` and snapshot/recovery commit functions do not exist.

- [ ] **Step 4: Implement the serialized activation transaction**

Guard activation with one per-target async mutex. Before parsing a new action, check the receipt by action ID. Then validate revision/Provider/home/file/probe, bind the first or persisted port to `127.0.0.1`, use `getrandom=0.4.3` to generate a 32-byte operating-system random credential encoded as 64 lowercase hex characters only if absent, create the immutable snapshot, and persist the pending recovery intent in its own committed transaction.

Apply and verify the file through Task 4. In one subsequent `BEGIN IMMEDIATE` transaction insert the snapshot, set Current/Snapshot/Takeover/port/routing credential, increment both counters once, mark recovery committed, and write the secret-free receipt. Publish only after commit.

On any post-intent failure, call exact restore and verify before marking rolled-back. A restore/verify failure marks both journal and Target View recovery state as required. Error mapping is stable: `stale-revision`, `incomplete-provider`, `incompatible-target-cli`, `unsupported-configuration-home`, `configuration-collision`, `configuration-write-failed`, `recovery-required`, or opaque `internal-failure` with correlation ID.

Pass the successfully reserved listener to `ModelServer`; do not close and rebind the port between config write and server start. For an already running listener, reuse its handle.

- [ ] **Step 5: Wire activation into UDS and commit**

Dispatch the typed activate action to `ActivationService`. The response and push both contain complete secret-free views; subscription deduplication may suppress a duplicate identical sequence at the client.

Run:

```bash
cargo test -p muxvia-routing --test activation --test control_socket --test model_route
cargo test -p muxvia-routing
cargo fmt --all -- --check
cargo clippy -p muxvia-routing --all-targets -- -D warnings
```

Expected: success, replay, rollback, recovery-required, and UDS activation paths pass.

Commit:

```bash
git add crates/routing-service
git commit -m "feat: activate codex takeover transactionally"
```

### Task 7: Minimal OpenCode-style Control Plane flow

**Files:**
- Create: `packages/control-plane/src/theme.ts`
- Create: `packages/control-plane/src/ui/app.tsx`
- Create: `packages/control-plane/src/ui/target-view.tsx`
- Create: `packages/control-plane/src/ui/provider-form.tsx`
- Create: `packages/control-plane/src/app.tsx`
- Create: `packages/control-plane/src/index.tsx`
- Create: `packages/control-plane/test/app-render.test.tsx`
- Create: `packages/control-plane/test/app-lifecycle.test.tsx`
- Modify: `packages/control-plane/package.json`
- Modify: `packages/control-plane/src/control/target-session.ts`

**Interfaces:**
- Consumes only `TargetSession`; UI components never import SQLite, TOML, socket, or HTTP modules.
- Produces `App(props: { session: TargetSession })`, `run({ servicePath, socketPath, release })`, and production executable entrypoint.
- `run` connects first, spawns only an absolute `servicePath` when the socket is unavailable, reconnects with a bounded readiness deadline, and destroys the renderer on every exit.

- [ ] **Step 1: Write failing renderer tests through a real in-memory Target Session**

Use `testRender(() => <App session={fakeSession} />, { width: 80, height: 24, useThread: false })`. Destroy every renderer in `finally`/`afterEach`. The initial frame must show one scrollable Codex context with these labels and no app bar/tabs/sidebar:

```text
MUXVIA
Codex
Mode       Unmanaged
Current    —
Serving    —
Service    Running
Config     Unmanaged
Snapshot   —

[p] provider   [a] apply takeover   [q] quit
```

Do not assert every trailing space; assert ordered meaningful lines and absence of `Overview`, `Providers | Routing`, and secret sentinel strings.

Drive the full flow using `mockInput`:

1. press `p` to open the Provider form;
2. type name, base URL, model, and credential, advancing with Tab;
3. press Enter to save and wait for the fake session action;
4. assert the form closes and only `Credential  Present` appears;
5. press `a`, assert activate action uses the visible Provider ID and mode `takeover`;
6. push an active view, then a Serving view, and assert live rerender without refresh.

Add invalid save/stale action tests that display structured retry/action text and install the authoritative view. Add a 40×12 resize assertion that remains usable without a permanent navigation column.

- [ ] **Step 2: Write failing lifecycle and sidecar-path tests**

Mock `createCliRenderer` with `createTestRenderer({ width: 80, height: 24, useThread: false })` and inject connector/spawner ports. Prove:

- an available socket never spawns a process;
- unavailable socket plus absolute service path spawns exactly that path with `--home <temporary-home>` and no shell;
- a relative service path rejects before spawn;
- readiness timeout surfaces `service-unavailable` and destroys renderer;
- Ctrl+C, `q`, thrown render error, and session close each call `session.close()` and idempotent `renderer.destroy()` exactly once;
- no branch calls `process.exit()` or searches `PATH`.

- [ ] **Step 3: Run focused renderer tests and witness RED**

Run:

```bash
bun test packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
```

Expected: tests fail because `App` and `run` do not exist.

- [ ] **Step 4: Implement the minimal prompt/stream UI**

Use OpenCode dark tokens exactly for T01:

```ts
export const theme = {
  background: "#0a0a0a",
  panel: "#141414",
  element: "#1e1e1e",
  text: "#eeeeee",
  muted: "#808080",
  primary: "#fab283",
  error: "#e06c75",
  warning: "#f5a742",
  success: "#7fd88f",
}
```

Render one full-screen background, two-cell horizontal padding, a single-scale wordmark/title, continuous Target status/activity stream, and a bottom action prompt with only a left primary rail. Do not create an app bar, horizontal tabs, dashboard cards, permanent navigation, boxed keycaps, shadows, blur, or `Cmd+K`.

The Provider form is one focused panel with four labeled inputs. Credential draft remains only in component state, displays masked glyphs, is cleared on unmount/save/error, and is never copied into notices. Disable save/apply while the corresponding action is pending. T01 keyboard surface is only `p`, `a`, Tab/Shift+Tab, Enter, Esc, `q`, and Ctrl+C; the centralized command palette belongs to T02.

Subscribe in Solid `onMount`, update one `createSignal<TargetView>`, and unsubscribe in `onCleanup`. Target View is the sole product projection.

- [ ] **Step 5: Implement production renderer lifecycle and commit**

Create the renderer explicitly:

```ts
const renderer = await createCliRenderer({
  exitOnCtrlC: false,
  useKittyKeyboard: {},
  autoFocus: false,
})
const destroyed = new Promise<void>((resolve) => renderer.once("destroy", resolve))
try {
  await render(() => <App session={session} />, renderer)
  await destroyed
} finally {
  await session.close()
  if (!renderer.isDestroyed) renderer.destroy()
}
```

Use the default alternate-screen resolution; do not set `split-footer` and do not call `renderer.start()`. Clear any title before destroy. Implement a bounded connect/start/reconnect loop against an injected clock rather than arbitrary test sleeps.

Run:

```bash
bun test packages/control-plane/test/app-render.test.tsx packages/control-plane/test/app-lifecycle.test.tsx
bun test packages/control-plane/test
bun run typecheck
```

Expected: renderer flow, resize, secret absence, cleanup, and sidecar-path tests pass.

Commit:

```bash
git add bun.lock packages/control-plane
git commit -m "feat: add codex takeover terminal flow"
```

### Task 8: Service composition, process lifetime, and full walking-skeleton proof

**Files:**
- Create: `crates/routing-service/src/service/process.rs`
- Create: `crates/routing-service/src/main.rs`
- Create: `crates/routing-service/tests/process_lifecycle.rs`
- Create: `tests/e2e/fixtures/fake-codex`
- Create: `tests/e2e/fake-upstream.ts`
- Create: `tests/e2e/walking-skeleton.test.ts`
- Create: `scripts/verify-t01.sh`
- Create: `.github/workflows/ci.yml`
- Modify: `package.json`
- Modify: `README.md`
- Modify: `.gitignore`
- Modify: `crates/routing-service/Cargo.toml`

**Interfaces:**
- Consumes every prior public seam; adds no second state/configuration path.
- Produces `muxvia-routing --home <absolute-path> [--test-shutdown-file <absolute-path>]` and `bun run packages/control-plane/src/index.tsx --service <absolute-path> --socket <absolute-path>`.
- One exclusive service lock is acquired before SQLite open/migration and held for process lifetime.
- Production lifetime: active takeover survives zero control sessions; after the service has accepted at least one control session, zero takeover exits after its last session and pending action. A freshly spawned service remains available for its first bounded connection attempt. Test-only shutdown is available only in test builds/integration invocation.

- [ ] **Step 1: Write failing process-lifecycle tests**

Start real service processes against one temporary Muxvia Home and assert:

```rust
#[tokio::test]
async fn second_service_exits_before_opening_the_database() {
    let first = ProcessFixture::start().await;
    let before = file_fingerprint(first.database_path());
    let second = ProcessFixture::command(first.home()).output().await.unwrap();
    assert_eq!(second.status.code(), Some(73));
    assert_eq!(file_fingerprint(first.database_path()), before);
}
```

Also prove runtime dir/database permissions, stale safe socket cleanup, a freshly spawned service waiting for its first bounded control connection, active takeover surviving the last UDS disconnect, no-takeover service exiting only after an accepted last control session and pending action, and the explicit test shutdown draining listeners before exit. A lock collision must happen before SQLite migrations or mutation.

- [ ] **Step 2: Write the failing cross-process Bun test**

The test must create one temporary root and set child `HOME` to `<root>/home`. It starts the real Rust binary and deterministic fake upstream, then uses the production TypeScript RPC adapter and rendered `App` to:

1. create Provider `Fixture Provider` at the fake upstream `/v1`, model `gpt-test`, credential `provider-secret-must-not-escape`;
2. activate Takeover with injected fake Codex capability probe executable;
3. inspect `<temp HOME>/.codex/config.toml` for the exact managed fields while verifying an unrelated comment/key survived;
4. extract only the loopback endpoint for the client and use the Routing Credential from the private config header to send a real chunked `POST /v1/responses`;
5. first send a wrong credential and assert `401` plus zero upstream calls;
6. send the valid request and assert status, SSE byte order, upstream replacement auth, and request preservation;
7. observe Serving through `TargetSession.subscribe` and in a captured OpenTUI frame without manual refresh;
8. disconnect the TUI/session, send a second valid model request, and prove active takeover keeps the service alive;
9. scan captured frames, RPC frames, normal stdout/stderr, and Target Views for both secret sentinels; and
10. assert the Operator's actual home paths were neither read nor modified by comparing pre/post non-recursive sentinel fingerprints outside the temp root.

The fake Codex executable accepts only `--version` and `--help`; any other argument exits nonzero so the test detects an accidental model/auth invocation.

- [ ] **Step 3: Run process and E2E tests and witness RED**

Run:

```bash
cargo test -p muxvia-routing --test process_lifecycle
bun test tests/e2e/walking-skeleton.test.ts
```

Expected: failures because the binary composition, lock/lifetime manager, fixtures, and full process harness do not exist.

- [ ] **Step 4: Compose the Routing Service without bypasses**

Add `clap=4.6.6` with derive and `fs2=0.4.3`. Resolve all home paths from the required absolute `--home` in integration mode or effective HOME in normal mode. Create the Muxvia Home, acquire `service.lock` with `try_lock_exclusive`, and retain the open lock file for process lifetime before `StateStore::open`.

Startup order is exact:

```text
resolve/validate home -> create restrictive dirs -> acquire service lock
-> open/migrate SQLite -> reconcile pending recovery -> bind persisted model port when takeover active
-> start model server -> bind private UDS -> accept control sessions
```

If takeover is inactive, do not start a model listener until activation reserves it. Track whether a control session has ever been accepted, active sessions, and pending actions. Only after at least one accepted session, when active sessions and pending actions both reach zero and takeover is inactive, close UDS and exit cleanly. When takeover is active, control disconnect has no effect on model serving. The test-only shutdown trigger closes accept loops, waits for in-flight tasks, removes its own socket, and exits without altering managed configuration.

Use structured logs that include only target/provider/action IDs and opaque correlation IDs. Redact command payloads, headers, recovery values, and secret wrapper Debug output.

- [ ] **Step 5: Add one verification entrypoint and CI matrix**

`scripts/verify-t01.sh` must be an executable POSIX shell script that runs, in order:

```bash
set -eu
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bun run typecheck
bun test
```

Root `package.json` adds `"test": "bun test"` and `"verify": "./scripts/verify-t01.sh"`.

GitHub Actions runs on `macos-latest` and `ubuntu-latest`, installs Bun `1.3.14` and Rust stable, restores only dependency caches, runs `bun install --frozen-lockfile`, then `bun run verify`. No job receives repository secrets. Linux and macOS both execute real UDS peer-credential and loopback tests.

README adds only T01-local build/test/demo instructions, explicitly labels the project pre-release, states that tests use a temporary HOME, and lists the T01 exclusions rather than promising future features.

- [ ] **Step 6: Run fresh full verification, inspect the diff, and commit**

Build the service first so the E2E test receives an absolute artifact path:

```bash
cargo build -p muxvia-routing
bun install --frozen-lockfile
bun run verify
git diff --check
git status --short
```

Expected: Rust format/Clippy/tests, TypeScript typecheck/tests, renderer tests, real UDS/loopback/process tests, and the cross-process walking skeleton all pass with zero failures and no warnings; `git diff --check` reports nothing.

Manually run the temporary-home demo once, never against the real home:

```bash
MUXVIA_DEMO_ROOT="$(mktemp -d)"
HOME="$MUXVIA_DEMO_ROOT/home" bun run packages/control-plane/src/index.tsx \
  --service "$(pwd)/target/debug/muxvia-routing" \
  --socket "$MUXVIA_DEMO_ROOT/home/.muxvia/run/control.sock"
```

Visually verify only the approved prompt/stream shell, Provider form, active state, and no dashboard navigation. Exit, then remove only the printed temporary demo directory after confirming its path is under the system temporary directory.

Commit:

```bash
git add .github .gitignore Cargo.lock README.md package.json scripts tests crates/routing-service
git commit -m "feat: prove codex takeover end to end"
```

## Completion Checklist

- [ ] Re-read the approved design and map every included behavior to Tasks 1–8; record any missing item as a failing test before changing production code.
- [ ] Confirm every T01 exclusion stayed absent from public types, UI, database schema, and command surface.
- [ ] Run `bun run verify` from a fresh checkout/worktree with no pre-existing build output.
- [ ] Confirm both protocol implementations consume the same committed fixtures and the E2E path uses the production adapters rather than test-only state/configuration shortcuts.
- [ ] Confirm secret sentinel scans cover serialized views/problems, stored action receipts, captured RPC frames, renderer frames, and normal logs.
- [ ] Confirm no test resolves the real Operator HOME and no production code lets the Control Plane open SQLite directly.
- [ ] Confirm the branch contains focused commits for every task and no unrelated changes.
