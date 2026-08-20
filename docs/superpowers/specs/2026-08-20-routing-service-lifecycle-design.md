# Routing Service Lifecycle and Version Handover Design

Status: approved design captured for implementation review

Parent specification: [#1 — Muxvia v0.1](https://github.com/HaroldHuanrongLIU/muxvia/issues/1)

Implementation ticket: [#11 — T10 Routing Service lifecycle and version handover](https://github.com/HaroldHuanrongLIU/muxvia/issues/11)

## Goal

Make the Routing Service lifetime predictable across Control Plane exit, clean idle shutdown, crash recovery, safe Target Takeover removal, and a compatible sidecar replacement. Preserve every response stream whose output is already committed, fail closed before Managed Configuration mutation, and never install an operating-system startup service.

## Existing Foundation

T10 deepens the existing Routing Service rather than creating a second lifecycle layer. The current product already has:

- one detached Routing Service process and one exclusive Muxvia Home service lock;
- a private UDS control server with complete Target Views and receipt-first actions;
- target-local model listeners and persistent Takeover recovery;
- exit after the last accepted control session when no Takeover or pending recovery keeps the process alive;
- restart bootstrap that resumes exact committed Takeovers and projects drift or Recovery Required target-locally; and
- graceful Axum listener shutdown plus request admission accounting.

T10 adds the missing normal Takeover removal and release replacement contracts without weakening those invariants.

## Deep Modules and Interfaces

### Lifecycle Coordinator

The Routing Service process owns one lifecycle coordinator. Its small internal interface accepts only closed lifecycle intents:

```rust
enum LifecycleIntent {
    Idle,
    ExplicitShutdown,
    CompatibleHandover(ReplacementCandidate),
}
```

The coordinator hides candidate probing, service-lock inheritance, control admission, model admission, drain ordering, process replacement, and failure recovery. Activation and reconciliation do not learn process replacement details.

### Target Takeover Disable

The public target-scoped action is:

```text
disable-takeover
```

It is idempotent through the existing action receipt contract and remains revision guarded. It uses the same target mutation gate and target-native recovery material as Restore, but differs in runtime behavior: Restore rejects a busy Target; normal disable transitions the target runtime into drain-only mode and waits for already accepted requests.

### Compatible Replacement

The Control Plane compares its release to the service release returned by the hello acknowledgement. Exact matches reuse the current service. A different release with the same supported RPC major triggers one replacement request carrying the canonical absolute candidate executable and expected release metadata.

The candidate executable supports a no-write metadata probe that returns a closed record containing product identity, release, and RPC version. The old service validates this record before changing admission, configuration, SQLite, sockets, or process state. T20 later adds Release Bundle manifest and hash validation before the same lifecycle interface.

## Safe Disable Ordering

For one Target:

1. Check the receipt, expected management revision, recovery/drift gates, and exact active Takeover.
2. Acquire the target mutation gate.
3. Reserve the target runtime for draining. New requests receive a fixed local unavailable response; already accepted requests retain their pinned route plan.
4. Create a durable recovery intent and restore/verify the pre-Takeover Managed Configuration.
5. In one immediate SQLite transaction, commit mode `unmanaged`, clear Current, Serving, Activated Snapshot, active plan, and draft membership, advance revisions, persist the secret-free receipt, and expose the complete authoritative Target View.
6. Write the action response, then publish exactly one Target View.
7. Wait for all already accepted requests on that Target. Requests whose output is committed complete without truncation; pre-commit requests are also allowed to complete, which is safer than cancelling them and still satisfies the committed-stream guarantee.
8. Stop only that Target listener. A peer Target Takeover and its requests remain available.
9. If no Target Takeover, recovery work, control work, or accepted control session remains, remove the UDS and exit naturally with status zero. The final disable response/push must be delivered before session shutdown.

Any failure before the database commit restores exact Managed Configuration and releases drain admission. A rollback failure enters target-local Recovery Required. Any failure after commit is represented by the committed receipt and authoritative view; it is never reported as an uncommitted success.

## Request Drain Semantics

Model admission has three closed states per Target:

- `accepting`: new requests may enter;
- `draining`: new requests fail locally, while the accepted-count can only decrease; and
- `stopped`: no listener remains.

One request increments the accepted count only after Routing Credential validation and plan pinning. The count remains held through response-body completion or cancellation, not merely through upstream response headers. This makes the drain boundary coincide with observable stream lifetime.

No fixed production timeout truncates a committed stream. Tests use event-driven barriers and bounded harness deadlines only to detect a stuck implementation.

## Compatible Handover Ordering

Muxvia uses same-process `exec` replacement rather than two concurrent database owners:

1. Negotiate RPC before state access. A major mismatch closes the session with a fixed incompatibility problem and performs no handover attempt.
2. Canonicalize the absolute candidate path and execute its no-write metadata probe.
3. Require exact product identity, exact requested release, and a supported RPC major before accepting the handover.
4. Return a handover-accepted response to the initiating Control Plane.
5. Stop accepting new control sessions and target requests. Finish already accepted state mutations and response writes.
6. Drain accepted model requests on both Targets, then stop model listeners and remove the old UDS.
7. Preserve the exclusive service lock file descriptor across `exec` and replace the process image with the candidate using the same canonical Muxvia Home.
8. The replacement validates the inherited lock identity before opening SQLite, recovers pending work, resumes clean Takeovers, binds the UDS, creates a new service epoch, and becomes ready.

If probe validation fails, no admission or state changes. If `exec` fails, the old process retains the lock, rebinds its control server and committed Takeovers, restores accepting admission, and reports a fixed handover failure on reconnect. A failed handover never interrupts an already committed model stream and never leaves two SQLite owners.

## Control Plane Startup

Startup remains bounded and receives the private sidecar as an absolute path. One startup coordinator, not each Target session independently, owns replacement:

- connect and negotiate once;
- exact release: open sessions normally;
- compatible different release: request one handover, await disconnect, reconnect to a new service epoch, then open both Target sessions and the Universal Provider catalog;
- failed handover: reconnect to the compatible old release, keep it usable, and surface a fixed nonsecret diagnostic;
- incompatible major: do not mutate, spawn, or retry a handover through an unknown protocol.

The Control Plane never resolves the sidecar through `PATH` and never installs or manages launchd, systemd, login items, or automatic restart.

## Crash and Restart

Crash or reboot leaves Managed Configuration pointing at an unavailable loopback route, so native Target CLIs fail closed. No operating-system service restarts Muxvia. The next explicit Control Plane start launches the Routing Service, which:

- obtains the single Muxvia Home lock before opening SQLite;
- recovers pending activation/reconciliation/disable work;
- resumes each exact, non-drifted Takeover at its stable route endpoint;
- projects drift and Recovery Required target-locally without rewriting the file; and
- starts a fresh service epoch, leaving older Route Health stale and ineligible.

## Protocol and Diagnostics

Rust, TypeScript, Zod, JSON Schema, and exact fixtures evolve together. New lifecycle operations and results are closed discriminated unions. All candidate paths, subprocess output, Target Views, errors, and test diagnostics are scanned before semantic assertions. Ordinary output never includes credentials, Routing Credentials, Managed Configuration payloads, request bodies, response bodies, or inherited descriptor values.

## Test Seams

The confirmed seams are:

1. Real `muxvia-routing` processes with private UDS, temporary SQLite/configuration homes, and loopback model ingress in `process_lifecycle.rs`.
2. Real framed UDS plus Rust/TypeScript/schema fixtures for lifecycle negotiation and handover messages.
3. Real model ingress and deterministic upstreams for admission, cancellation, committed SSE/body drain, target isolation, and listener removal.
4. Control Plane startup through its production connector interface plus one real multi-process tracer covering release mismatch, successful handover, failed handover fallback, safe final disable, natural exit, and restart recovery.

Tests use real files and SQLite. The only adapters are true process boundaries: candidate metadata probes, deterministic upstream servers, test CLI executables, and event barriers. No internal lifecycle method-call assertions are accepted as evidence.

## Out of Scope

- launchd, systemd user units, login items, reboot auto-start, or crash auto-restart;
- Release Bundle hashes, signatures, Gatekeeper, package ownership, or installation-channel switching, which belong to T20–T24;
- noninteractive `muxvia service start|stop` presentation, which belongs to T19 and will consume this lifecycle interface;
- force stop, which belongs to the separately guarded T19 diagnostic CLI path;
- public plugin lifecycle interfaces, remote management, multiple Operators, or Windows service control; and
- truncating committed model streams to satisfy a fixed shutdown deadline.
