# Codex Direct Activation Design

Date: 2026-08-14

Implementation ticket: [#5 — T04 Codex Direct Activation](https://github.com/HaroldHuanrongLIU/muxvia/issues/5)

## Goal

Let the Operator explicitly apply one complete, Direct-compatible Codex Target Provider to the default Codex Configuration Home. The operation must write only Muxvia-owned TOML fields, commit the Current Target Provider and immutable Activated Snapshot only after verified configuration success, recover the exact prior state on failure, and tell the Operator to restart already-running Codex processes.

## Scope

T04 adds Direct Activation to the existing Codex Target context. It includes:

- an explicit Direct activation mode in the versioned control protocol;
- a persisted Provider routing requirement that distinguishes Direct-compatible records from Takeover-required records;
- a Muxvia-owned direct Codex provider table containing the selected upstream base URL, Responses protocol, model, and static bearer Authorization header;
- the same recovery-intent, atomic-write, reread-verification, receipt-first, immutable-snapshot, and fail-closed guarantees already used by Codex Takeover;
- OpenCode-style named commands and a Takeover-or-cancel confirmation for a Takeover-required Provider; and
- a real-process tracer proving configuration, state, restart guidance, isolation from `auth.json`, and no model listener.

T04 does not implement safe removal of an already-active Target Takeover, Configuration Drift reconciliation, Shadowing Configuration UX, Claude Code, Universal Providers, Failover Chains, or general service handover. Direct Activation is rejected while a Codex Target Takeover is active; safe Takeover removal and drain remain in T10.

## Chosen Approach

Extend the existing `ActivationService` with an explicit `ActivationMode::{Direct, Takeover}` and mode-specific preparation/runtime values while retaining one serialized activation gate and one transaction/recovery pipeline.

Two alternatives were rejected:

1. A separate Direct activation service would duplicate receipt lookup, compatibility probing, recovery journaling, rollback, final revision checks, and publication ordering.
2. A generic multi-target activation framework would anticipate Claude behavior before its JSON configuration and Messages ingress contracts exist.

The chosen design keeps shared safety mechanics in one place while leaving Codex-specific desired-state construction explicit.

## Provider Routing Requirement

Provider schema version 3 adds a server-owned routing requirement:

- `direct-compatible` for ordinary OpenAI-compatible Responses Providers created by T03;
- `takeover-required` for future bridge or transformation Providers.

The field is projected as `routingRequirement` but is not editable in the ordinary Provider form. Existing Providers migrate to `direct-compatible`. Stored action receipts are upgraded in the same migration so receipt-first replay remains valid after opening a schema-v2 database.

The Routing Service is authoritative: a Direct activation request for a Takeover-required Provider fails before recovery intent or filesystem mutation with `takeover-required`. The Control Plane may preemptively show the same confirmation from the projected field, but it must still handle the authoritative failure.

## Managed Codex Configuration

Muxvia continues to own only:

- top-level `model`;
- top-level `model_provider`; and
- `model_providers.muxvia_codex.{name,base_url,wire_api,http_headers,supports_websockets}`.

Direct desired state is:

```toml
model = "<Provider model>"
model_provider = "muxvia_codex"

[model_providers.muxvia_codex]
name = "Muxvia Direct"
base_url = "<Provider base URL>"
wire_api = "responses"
http_headers = { Authorization = "Bearer <Provider credential>" }
supports_websockets = false
```

The credential is already accepted local plaintext state in v0.1 and is written only to the private managed configuration and recovery data. It must never appear in Target Views, receipts, logs, ordinary errors, activity entries, captured renderer frames, or test diagnostics.

The codec retains exact prior owned values and prior absence while preserving unrelated TOML semantics, comments representable by `toml_edit`, and existing file mode. A directory symlink is canonicalized; a managed-file symlink and a pre-existing `muxvia_codex` table not matching the currently applied Muxvia state are rejected. `auth.json` is neither opened nor modified.

## Activation State Model

The projected Codex mode is:

- `unmanaged` when no Activated Snapshot exists and Takeover is inactive;
- `direct` when an Activated Snapshot exists and Takeover is inactive;
- `takeover` when Takeover is active.

Direct commit sets Current Target Provider, clears Serving Provider, stores the immutable Activated Snapshot, keeps Takeover inactive, stores the managed path, advances management revision and view sequence, commits the receipt and recovery intent, and projects `restartRequired: true`. It neither allocates nor rotates a Routing Credential, binds a loopback port, nor starts a Model Server.

An existing direct activation can switch to another Direct-compatible Provider. A later Takeover activation can replace a direct configuration after verifying the currently managed direct fields. Direct activation while Takeover is active is rejected before mutation so T04 does not silently bypass the later safe-drain contract.

The Routing Service may exit after the last Control Plane session closes because Direct Activation does not keep it alive.

## Activation Transaction

Activation remains serialized per Target and receipt-first:

1. Replay an existing action receipt before parsing or validation.
2. Under the activation gate, recheck the receipt and expected management revision.
3. Validate Provider existence, completeness, routing requirement, default Configuration Home, compatibility, file safety, and currently owned fields.
4. Build an immutable Activated Snapshot from the saved declaration and Credential Reference.
5. Build the exact direct desired state without starting a listener or creating a Routing Credential.
6. Persist a pending Recovery Intent containing before state, desired state, file identity, action ID, and expected revision.
7. Atomically apply the merged TOML and reread/verify owned values plus unrelated semantic content.
8. In one immediate SQLite transaction, recheck receipt, revision, and Recovery Required; commit snapshot, Current, mode, managed path, capability warning, recovery state, receipt, and complete Target View.
9. Publish exactly one complete Target View after commit.

Any failure after the Recovery Intent uses the existing three-state rollback rule: exact before is confirmed, exact desired is restored to before and verified, and a third state becomes Recovery Required. Database state remains unchanged unless the final transaction commits.

## Control Plane Workflow

The Codex Target context exposes `/direct` through the centralized command catalog. It applies the Current Provider, or the first Provider when Current is absent. The Provider picker exposes a separate selected-row Direct command because overlay command scopes cannot share the background Target handler.

For a complete Direct-compatible Provider, the Control Plane dispatches `activate-provider` with `mode: "direct"`, installs the authoritative returned view, appends one localized success activity, and shows the existing restart-required guidance.

For a Takeover-required Provider, it opens one modal with only:

- Enable Takeover — dispatch the existing Takeover activation for that exact Provider;
- Cancel — close without action.

Incomplete, stale, incompatible, unsupported-home, collision, Recovery Required, and active-Takeover failures use stable codes and authoritative Target Views. Credentials are never retained by this workflow because it operates only on saved Provider identities.

## Verification Strategy

Protocol and migration tests prove schema-v3 routing requirement projection, additive compatibility, upgraded historical receipts, and exact Direct/Takeover wire identities.

Codec tests prove exact direct owned fields, bearer formatting, unrelated TOML and mode preservation, prior-absence restoration, symlink/collision handling, and unchanged `auth.json` fingerprints.

Activation tests use real temporary SQLite and files plus deterministic probes and failpoints to prove:

- Direct success without listener or Routing Credential allocation;
- Current/Snapshot/config atomicity and single publication;
- direct-to-direct and direct-to-Takeover transitions;
- active-Takeover and Takeover-required rejection before recovery intent;
- stale final commit and every post-intent failure restore the prior state;
- rollback failure enters Recovery Required; and
- incompatible CLI, nondefault home, collision, and symlink failures do not write.

OpenTUI tests prove named command identity, selected Provider behavior, Takeover-or-cancel confirmation, localization, restart guidance, overlay priority, authoritative failures, and secret-free frames.

The real-process tracer uses a temporary Muxvia Home and temporary `HOME`, real UDS, SQLite, Routing Service, OpenTUI test renderer, fake Codex probe executable, and real `config.toml`/`auth.json` sentinels. It proves Direct activation, process restart projection, absence of a model listener, Routing Service exit after the TUI disconnects, exact configuration ownership, and end-to-end secret scanning.

## Out of Scope

- Disabling or draining an already-active Target Takeover.
- Direct inference testing or launching an interactive Codex session.
- Configuration Drift Adopt/Reapply/Restore and Shadowing Configuration UI.
- Nondefault Configuration Homes, project/profile writes, or CLI override management.
- Alternative authentication profiles, arbitrary custom headers, or credential helpers.
- Claude Code Direct Activation or Takeover.
- Route Health, Request Records, Native Usage, imports, backups, and release packaging.
