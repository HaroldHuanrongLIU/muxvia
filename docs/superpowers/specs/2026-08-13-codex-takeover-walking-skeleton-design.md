# Codex Takeover Walking Skeleton Design

Status: approved design captured for implementation review  
Parent specification: [#1 — Muxvia v0.1](https://github.com/HaroldHuanrongLIU/muxvia/issues/1)  
Implementation ticket: [#2 — T01 Codex Takeover walking skeleton](https://github.com/HaroldHuanrongLIU/muxvia/issues/2)

## Goal

Deliver the smallest complete Muxvia path: an Operator creates one Responses-compatible Codex Target Provider in the terminal Control Plane, applies Target Takeover, sends a Codex-shaped request through the authenticated local Routing Service to a deterministic fake upstream, and sees the resulting Current, Serving, service, configuration, and Activated Snapshot state in the TUI.

This is a walking skeleton, not a layer scaffold. Its value is that the same public seams, process boundary, persistence ownership, configuration transaction, and model transport used by later tickets already work end to end.

## Scope

T01 includes:

- a Bun, TypeScript, Solid, and OpenTUI Control Plane with the minimum screens needed to create and activate one Codex Target Provider;
- a separate Rust Routing Service;
- a real private Unix-domain management socket;
- a real SQLite database opened only by the Routing Service;
- field-owned writes to a real Codex `config.toml` under the default Configuration Home;
- an authenticated IPv4 loopback Responses HTTP/SSE ingress;
- a protocol-transparent HTTP upstream adapter and deterministic fake upstream;
- complete secret-free Target Views after provider creation, activation, and a routed request; and
- automated tests at pure, module, socket, HTTP, renderer, and cross-process seams.

T01 deliberately excludes the complete OpenCode-style shell, Claude Code, Direct Activation, provider update/delete/reorder, Universal Providers, drift reconciliation UI, failover, circuit breaking, Subscription Accounts, the Subscription Bridge, usage and pricing, import/export, backups, release bundles, service handover, and production-grade detached lifecycle management. Those remain in their approved tickets. The service is nevertheless a separate process and T01 must not introduce an in-process backend shortcut.

## System Shape

```text
┌──────────────────────────────┐
│ muxvia Control Plane         │
│ OpenTUI Target Session       │
│ get / act / subscribe        │
└──────────────┬───────────────┘
               │ private UDS
               │ length-prefixed JSON v1
┌──────────────▼───────────────┐
│ muxvia-routing               │
│                              │
│ target actions + projections│
│ SQLite + activation journal │
│ Codex configuration codec   │
│ Responses router            │
└───────┬──────────────┬───────┘
        │              │
        │ atomic TOML  │ authenticated loopback HTTP
        ▼              ▼
 ~/.codex/config.toml  /v1/responses ──► Provider upstream
```

The Routing Service is the central deep module. It owns all authoritative product state, persistence, configuration mutation, and request routing. The Control Plane owns only transient presentation state such as focus, field drafts, and overlay state.

For development and tests, the Control Plane receives the Routing Service executable as an absolute build artifact path; it never searches `PATH` for the private sidecar. It first attempts the private socket and starts the sidecar only when no service is available. Before opening SQLite, the Routing Service obtains an exclusive Muxvia Home service lock, so concurrent start attempts converge on one database owner. A second process exits without migration or mutation.

T01 implements the minimum lifetime implied by the process split: a service with an active Codex Target Takeover remains alive after the Control Plane disconnects, while a service with no takeover exits after its last control session and pending action finish. The cross-process test harness has an explicit test-only shutdown handle. Crash recovery, safe user-facing stop, signals, restart UX, and version handover remain T10.

## Repository Units

The implementation uses a small workspace rather than splitting every concern into a package:

- the root workspace owns common developer commands and cross-process tests;
- the Control Plane package owns OpenTUI rendering, Target Session state, form drafts, and the TypeScript RPC adapter;
- the Routing Service crate owns the service process, domain actions, SQLite, Codex configuration, UDS server, and model HTTP server;
- a language-neutral control-protocol schema and golden frames define the Rust/TypeScript wire contract; and
- integration fixtures own fake upstream behavior and temporary-home orchestration.

SQLite and the filesystem do not receive repository interfaces. Module tests use real temporary SQLite databases and real files. Only true external or process-bound dependencies receive ports: the UDS client boundary, the read-only Codex capability probe, and the upstream HTTP transport.

## Target Session Contract

The Control Plane consumes one use-case-oriented interface:

```ts
interface MuxviaControl {
  openTarget(target: "codex"): Promise<TargetSession>
}

interface TargetSession {
  get(): Readonly<TargetView>
  act(action: TargetAction): Promise<ActionOutcome>
  subscribe(listener: (next: TargetView) => void): () => void
  close(): Promise<void>
}
```

T01 implements two actions:

- `save-provider`, carrying name, normalized provider base URL, model, and provider API credential; and
- `activate-provider`, carrying the Provider identity and `mode: "takeover"`.

The session adapter automatically supplies a fresh action identifier and its latest target revision. Actions for a single session are serialized. A stale-revision response replaces the local view with the authoritative current view and tells the Operator to retry; it never silently retries an intent against changed state.

`TargetView` is the only product projection the TUI renders. T01 includes:

- target identity, a management revision, and a monotonically increasing view sequence;
- Routing Service epoch and running state;
- mode and Target Takeover state;
- secret-free Provider summaries;
- Current Target Provider;
- Serving Provider;
- Managed Configuration state and restart-required notice;
- Activated Snapshot identity, Provider, model, and epoch; and
- structured notices and actionable errors.

No Provider API credential, Routing Credential, upstream authorization header, configuration recovery payload, or secret-bearing raw configuration may appear in a Target View.

The management revision changes only when an Operator action changes authoritative declaration, activation, or recovery state. Runtime observations such as a Serving Provider update advance the view sequence but do not make an otherwise valid editor action stale. Pushed views carry both values; a sequence gap causes the client to request a fresh complete Target View rather than applying inferred patches.

## Management RPC

The production Target Session adapter crosses `~/.muxvia/run/control.sock`. The runtime directory is mode `0700`; the socket is mode `0600`. The Routing Service verifies that the peer operating-system user matches its own effective user on both macOS and Linux. If peer identity cannot be established, management access fails closed. A model Routing Credential is never a management credential.

Each frame is a four-byte unsigned big-endian byte length followed by UTF-8 JSON. T01 limits a frame to 1 MiB. An oversized, invalid UTF-8, invalid JSON, or schema-invalid frame is rejected without executing an action. Authentication or malformed-frame failures contain no submitted secret values.

The first client frame is a hello containing RPC major/minor and Control Plane release identity. The service replies with the selected minor, service release identity, service epoch, and frame limit. A major mismatch terminates the session before any state access or mutation. Unknown additive fields within the negotiated major version are ignored; an unknown operation returns a structured unsupported-operation error.

After negotiation, the protocol carries:

- an open-target request returning the initial complete Target View;
- an action request containing action ID, expected revision, target, and typed action;
- an action outcome containing the authoritative complete Target View; and
- pushed target-view events after committed state changes.

Every mutation requires an action ID and expected management revision. The Routing Service persists an action receipt containing the action ID, action kind, committed management revision, and secret-free outcome. Repeating an action ID returns the recorded outcome without inspecting a second payload or repeating external writes. The client must create a new action ID for a new intent.

## Persistent State

The minimal SQLite model stores:

- schema and product metadata;
- Target Provider declarations and a separate local credential record;
- one Codex Target Route State, including stable Routing Credential, loopback endpoint, Current Provider, Serving Provider, takeover state, management revision, view sequence, and Activated Snapshot reference;
- immutable Activated Snapshots;
- activation recovery intents with before and desired owned-field state;
- idempotent action receipts; and
- the durable product metadata from which the service creates a fresh in-memory service epoch at process start.

Provider API credentials and the Codex Routing Credential are persisted locally without application-level encryption, matching the accepted v0.1 trade-off. Database, runtime directories, and created private files receive best-effort restrictive Unix permissions. Secrets are excluded from ordinary debug formatting and structured logs.

The Rust service is the only process that opens or migrates this database. The TUI cannot read SQLite even for display or recovery.

## Provider Creation

T01 supports one ordinary, Responses-compatible Codex Target Provider shape:

- generated Muxvia UUID;
- Operator-visible name;
- normalized base URL ending at the provider API root, such as `https://example.test/v1`;
- model identifier; and
- Provider API credential.

Saving validates structural and safety requirements without contacting the model endpoint. The base URL must use HTTPS or loopback HTTP, contain no embedded user information, query, or fragment, and normalize trailing separators deterministically. Other plaintext HTTP endpoints are rejected in T01. The provider credential is a bearer token, is written to private storage, and is represented in the Target View only as present or missing.

The successful save increments the Codex target revision, persists a secret-free action receipt, and emits the complete new Target View. Saving does not activate the Provider or change Managed Configuration.

## Codex Managed Configuration

T01 manages the default `~/.codex/config.toml`, resolved from the child process's effective home. Tests set a temporary `HOME`, so neither tests nor demos touch the Operator's real files.

The codec uses a formatting-preserving TOML editor. It owns only:

- top-level `model`;
- top-level `model_provider`; and
- the exact fields in one persisted, collision-free Muxvia provider table: display name, loopback base URL, `wire_api = "responses"`, the static Routing Credential header, and `supports_websockets = false`.

All unrelated keys, provider tables, comments representable by the editor, and file permissions remain unchanged. The codec records both prior values and prior absence for every owned field.

Before activation, the service performs the T01 subset of the Target Compatibility Probe:

- require the canonical default Codex home;
- reject a symlinked managed file;
- parse the current TOML;
- reject collision with a pre-existing non-Muxvia reserved provider table; and
- execute only read-only `codex --version` and documented help inspection through an injected probe port.

T01 does not read `auth.json`, invoke a model, run experimental debug commands, or edit project/profile/CLI override layers. It reports that Managed Configuration is guaranteed only for newly started Codex processes.

## Takeover Activation Transaction

Activation is serialized per target and follows this order:

1. Revalidate action identity, expected revision, Provider completeness, Configuration Home, current file identity, and owned-field state.
2. Ensure the loopback listener is bound before changing Codex configuration. On first activation, choose an available IPv4 loopback port and persist it; later starts must bind that same port or fail closed.
3. Generate the Codex Routing Credential once if it does not exist and persist it as target state. Provider switches must not rotate it.
4. Build an immutable Activated Snapshot from the saved Provider declaration and credential reference.
5. Persist and commit a recovery intent containing the exact before state, desired owned-field state, file identity, and pre-action revision.
6. Atomically write the merged TOML in the same directory, preserving mode where possible, and reread it to verify every owned value and unrelated semantic content.
7. In one SQLite transaction, set Current Provider, Activated Snapshot, and Target Takeover; increment the revision; persist the action receipt; and mark the recovery intent committed.
8. Publish the complete Target View only after the database commit.

If any step through verification fails after the recovery intent is committed, the service restores the exact before state and verifies it before marking the intent rolled back. Current Provider, Activated Snapshot, takeover, and target revision remain unchanged. If restoration cannot be verified, the target enters Recovery Required and later managed writes fail closed.

At Routing Service startup, any pending recovery intent is reconciled before accepting managed actions: a file matching the before state is marked rolled back; a file matching only the uncommitted desired state is restored to before; any third state becomes Recovery Required. T01 implements this narrow startup recovery even though full service lifecycle behavior belongs to T10.

## Model Transport

The Codex provider table points to `http://127.0.0.1:<persisted-port>/v1`. T01 exposes `POST /v1/responses` only. WebSockets, Chat Completions, compact endpoints, and model discovery are not model-plane endpoints in this ticket.

The model server:

1. accepts only an IPv4 loopback-bound connection;
2. compares `X-Muxvia-Routing-Credential` in constant time with the persisted Codex credential;
3. rejects a missing or wrong credential with a generic local `401` before loading a Provider credential or contacting upstream;
4. loads and pins the immutable Activated Snapshot at request start;
5. joins the normalized upstream API root with `responses`;
6. removes hop-by-hop, host, content-length, incoming routing, and incoming upstream-authorization headers;
7. sets the selected Provider's upstream authorization and forwards supported remaining headers and the request body without parsing it into a reduced Muxvia schema; and
8. streams the upstream status, supported response headers, body bytes, and SSE event order back with cancellation propagation and backpressure.

No active snapshot produces `503`; an upstream connection failure before response commitment produces `502`. Upstream status codes and bodies are otherwise passed through. HTTP commitment occurs when the selected upstream response head is sent to Codex, because status and headers cannot be replaced afterward. A successful 2xx commitment updates Serving Provider, advances the view sequence, and emits a complete Target View without changing Current Provider, Activated Snapshot, or the management revision. T01 does not parse SSE events to create a later semantic commitment point; failover owns that behavior in T09.

T01 has one attempt and no failover. The upstream HTTP client is an internal port because the upstream is a true external dependency. Production uses a real streaming HTTP adapter; tests use both a real deterministic HTTP server and scripted failures.

## Minimal Control Plane Flow

T01 provides only the UI needed to prove the vertical slice:

1. open the Codex Target context;
2. open a Provider form;
3. enter name, API root, model, and API credential;
4. save the Provider and return to its secret-free summary;
5. invoke Apply Takeover and display progress from the action outcome;
6. show the resulting Mode, Current, Serving, service, Managed Configuration, restart notice, and Activated Snapshot; and
7. update Serving after the external request without a manual refresh.

The screen uses the accepted single-scale, cell-spaced, prompt-and-stream grammar, but T02 owns the full Home identity, command registry, slash-command parity, final dialogs, localization breadth, responsive sidebar, and PTY hardening. T01 must not introduce dashboard tabs or a permanent navigation sidebar that T02 would later need to remove.

## Error Semantics

Management errors are structured and stable enough for the TUI to render an action and recovery path. T01 distinguishes protocol mismatch, frame invalid, unauthorized peer, stale revision, invalid Provider, incomplete Provider, incompatible Target CLI, unsupported Configuration Home, configuration collision, configuration write failure, Recovery Required, no Activated Snapshot, and internal failure.

Errors do not echo API credentials, routing credentials, authorization headers, secret-bearing input objects, or raw recovery state. Unexpected internal errors receive an opaque correlation identifier. Logs may include Provider and action identifiers but never secret values.

No error path may report activation success before the database commit or report rollback success before the restored file is reread and verified.

## Testing Strategy

Tests assert behavior through the highest useful seam:

- protocol golden tests feed the same valid and invalid JSON frames to Rust and TypeScript implementations;
- pure domain tests cover revision checks, action-receipt idempotency, secret-free Target View projection, URL normalization, header policy, and Activated Snapshot immutability;
- configuration tests use real temporary TOML files to prove owned-field changes, unrelated-field preservation, comment preservation where supported, permission handling, symlink rejection, collision rejection, exact absence restoration, and startup recovery;
- SQLite module tests use a real temporary database and prove migrations, sole-writer behavior, action receipts, recovery intents, and transactional state changes;
- real UDS tests prove peer authorization, hello negotiation, major mismatch, the 1 MiB frame bound, malformed-frame rejection, stale revisions, idempotent retry, subscriptions, and secret-free views;
- real loopback tests prove bind address, credential rejection before upstream contact, request/header preservation, upstream authorization replacement, SSE byte ordering, cancellation, `502`/`503` behavior, and Serving updates;
- OpenTUI renderer tests drive the Provider form and Apply action through the Target Session interface and capture the resulting secret-free state; and
- one cross-process test starts a real Routing Service in a temporary home, connects through the TypeScript UDS adapter, creates and activates a Provider, inspects the written TOML, sends a Responses/SSE request to a real fake upstream, observes Serving in the TUI projection, and verifies that neither process touched the real home.

Every production behavior follows red-green-refactor. A test must be observed failing for the intended missing behavior before its minimum implementation is written. Configuration or generated metadata that cannot reasonably be test-first remains covered by the first consumer's failing integration test rather than receiving a standalone scaffold task.

## Demonstration and Completion Evidence

The T01 demonstration uses only temporary homes and a deterministic local upstream. It shows:

- Provider creation in the TUI;
- takeover activation and the exact managed Codex fragment;
- preservation of unrelated Codex configuration;
- rejection of an invalid Routing Credential without an upstream request;
- a valid Responses/SSE request and unchanged event order;
- Current and Serving state after success; and
- secret scans of Target Views, logs, and captured frames.

T01 is complete only when the full Rust suite, Bun suite, type checking, formatting/linting, cross-language protocol fixtures, real UDS integration, real loopback integration, OpenTUI renderer tests, and cross-process end-to-end test all pass from a fresh temporary home. A manual visual check supplements but never replaces those automated results.
