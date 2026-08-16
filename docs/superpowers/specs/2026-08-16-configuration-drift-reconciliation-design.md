# Configuration Drift, Shadowing, and Compatibility Reconciliation Design

## Context

T07 gives Codex CLI and Claude Code one explicit workflow for detecting external changes to Muxvia-owned configuration, explaining higher-priority configuration sources, classifying Target CLI compatibility, and resolving drift through Adopt, Reapply, or Restore.

The design follows the repository domain language in `CONTEXT.md`, ADR 0026's requirement to reconcile drift before managed writes, and ADR 0045's read-only compatibility probe and restart guarantees. The Routing Service remains the sole authority for SQLite, Managed Configuration, recovery, credentials, and route runtime. The Control Plane never opens product storage or Target configuration directly.

## Scope

T07 delivers:

- target-local Configuration Drift detection and durable projection;
- observable Shadowing Configuration classification without modifying the shadow source;
- read-only Target Compatibility Probe results classified as tested, unknown-compatible, or incompatible;
- a persistent acknowledgement scoped to one Target CLI and one exact unknown-compatible version;
- server-owned, read-only previews for Adopt, Reapply, and Restore;
- race-safe application of an exact preview through an ephemeral observation token;
- a shared Control Plane reconciliation workflow for Codex and Claude;
- restart guidance after any Managed Configuration change;
- target isolation, idempotency, rollback, redaction, and real-process evidence.

T07 does not deliver normal Takeover removal with waiting and stream drain. Drift-time Restore requires zero in-flight model requests and returns `target-busy` without mutation otherwise. T10 remains responsible for ordinary safe Takeover removal, coordinated drain, and broader Routing Service lifecycle and handover behavior.

T07 also does not add Universal Providers, Provider Synchronization itself, Failover Chains, Recovery Backup restore, nondefault Configuration Homes, project configuration management, or detection of unobservable CLI flags and resumed-session state.

## Product Invariants

1. Configuration Drift is target-local. It blocks Provider save, activation, synchronization, and ordinary restore writes only for the affected Target CLI.
2. Existing routing may continue from its already committed Activated Snapshot while the affected Target is drifted. A request never changes snapshot after it begins.
3. Editors continue to render saved Provider state. They never silently import observed Managed Configuration.
4. Adopt, Reapply, and Restore are explicit, previewed, revision-guarded, and verified.
5. Every reconciliation preserves unrelated Target configuration.
6. Observable Shadowing Configuration is identified by source and never modified.
7. Unobservable CLI flags and resumed-session state are disclosed as a support boundary, never guessed.
8. Unknown-compatible acknowledgement is bound to the exact Target and exact observed version. A version change invalidates the acknowledgement.
9. Incompatible Target versions retain read-only inspection but cannot perform Provider saves or any Managed Configuration write.
10. Directory symlinks are canonicalized. Managed configuration file symlinks remain blocked.
11. Secrets never enter Target Views, preview summaries, ordinary diagnostics, logs, receipts, or test failure output.

## Architecture

### Reconciliation Coordinator

A new crate-internal, target-neutral Reconciliation Coordinator owns the workflow. It is separate from the existing activation coordinator so activation, drift reconciliation, and later T10 lifecycle behavior do not accumulate in one module.

The coordinator owns:

- preview construction and ephemeral observation-token storage;
- revision, target, strategy, compatibility, shadow, identity, and semantic-fingerprint binding;
- compatibility acknowledgement policy;
- durable reconciliation intent and receipt ordering;
- transaction boundaries and view publication;
- target-local runtime checks, including zero in-flight Restore;
- exact rollback or transition to Recovery Required.

Target-specific adapters for Codex and Claude own only their native configuration behavior:

- observe approved fields and unrelated semantic state;
- identify observable shadow sources;
- construct a redacted field-change summary;
- derive Adopt, Reapply, and Restore writes;
- apply, verify, and exact-restore a configuration;
- expose the immutable recovery material needed by the coordinator.

The adapters do not open SQLite, publish Target Views, decide acknowledgement policy, or own RPC identity.

### Compatibility Probe

The existing read-only command probes remain the production seam. T07 formalizes their result as:

- `tested`: the exact version is in the release-pinned tested matrix and required capabilities are present;
- `unknown-compatible`: required capabilities are present, but the exact version is outside the tested matrix;
- `incompatible`: the executable, version/help surface, or required configuration capability is absent or contradictory.

The probe uses public, read-only CLI surfaces only. It does not read credentials, send inference, modify configuration, or invoke interactive repair commands.

The Routing Service persists the last exact Target/version classification and the acknowledged exact Target/version pair. A new version clears the effective acknowledgement. An acknowledgement never upgrades a version to `tested`; it only authorizes managed writes for that unknown-compatible version.

## Protocol

### Preview

`preview-reconciliation` is a target-scoped, read-only request with one strategy:

- `adopt`
- `reapply`
- `restore`

It returns a view-free `ReconciliationPreview` containing:

- an opaque `observationToken`;
- the Target and strategy;
- the authoritative Target revision;
- exact nonsecret Target CLI version and compatibility classification;
- whether an unknown-compatible acknowledgement is required;
- known Shadowing Configuration source identities;
- a field-level redacted change summary using only `present`, `absent`, `unchanged`, and `changed` states;
- nonsecret Provider, Current, Activated Snapshot, and Takeover effects;
- whether a newly started Target CLI is required;
- the unobservable CLI flag and resumed-session boundary disclosure.

The response contains no credential bytes, Routing Credential, recovery payload, raw native configuration, backend path, raw probe output, or raw error message.

Preview performs no SQLite, file, credential, runtime, receipt, or publication mutation.

### Observation Token

The opaque token is stored only in Routing Service memory. Its server-side record binds:

- Target and strategy;
- Target revision;
- compatibility version and classification;
- acknowledgement requirement;
- known shadow result;
- canonical Configuration Home;
- managed-file identity;
- owned semantic fingerprint;
- unrelated semantic fingerprint;
- committed Snapshot and Recovery Intent identities relevant to the strategy.

Tokens are single-use after a successful apply and invalid after service restart. Apply always re-observes the environment. Any mismatch returns `stale-reconciliation-preview` and performs no mutation. The client cannot turn a preview into a different strategy or Target action.

### Apply

`apply-reconciliation` is an idempotent action carrying:

- action UUID;
- expected Target revision;
- strategy;
- observation token;
- explicit compatibility acknowledgement when required.

The normal receipt-first rule applies. Reusing an action UUID replays its committed outcome without re-probing, rewriting, publishing, or consuming a newer preview.

The protocol evolves additively under the existing RPC compatibility rules. Unknown fields remain ignorable; strategy and result discriminators remain closed.

## Reconciliation Strategies

### Adopt

Adopt explicitly converts the currently observed Managed Configuration into new Muxvia state.

- It creates a new ordinary Target Provider rather than modifying the existing Provider.
- It creates a new Credential Reference when the observed credential differs. It never overwrites or aliases the old secret implicitly.
- It creates a new immutable Activated Snapshot and sets the new Provider as Current.
- It records the observed managed state and unrelated fingerprint as the new committed expectation.
- The prior Provider, Credential Reference, and Snapshot remain immutable history.

The preview explains the new identities and whether a credential reference will be created, but never displays the credential.

### Reapply

Reapply reconstructs desired Managed Configuration from the authoritative committed Snapshot and bound Recovery Intent. It rewrites only Muxvia-owned fields and preserves the newly observed unrelated configuration. It then verifies the exact owned result and unchanged unrelated fingerprint before clearing Configuration Drift.

Reapply never reads an editor draft, silently updates a Provider, or substitutes a newer credential reference.

### Restore

Restore reconstructs the pre-Muxvia state from the bound recovery payload and restores only the fields that payload version originally owned. This preserves the legacy Claude three-field versus current four-field ownership contract.

For an active Takeover, Restore requires zero in-flight model requests. If any request is active, it returns `target-busy` with no file, database, runtime, credential, receipt, or publication mutation. With zero in-flight requests, Restore:

- restores and verifies the pre-Muxvia configuration;
- exits the Target's managed state;
- stops that Target's listener;
- clears the active managed route/current snapshot projection needed to claim applied configuration;
- retains Provider and credential declarations as reusable history.

Normal Takeover removal that waits for and drains requests remains T10 scope.

## Transaction and Recovery Ordering

Apply follows this order:

1. Check for an existing receipt.
2. Validate expected revision and observation token.
3. Re-run compatibility, shadow, canonical-home, symlink, file-identity, semantic, and strategy preflight.
4. Reject `target-busy` before a Restore mutation.
5. Insert a durable reconciliation intent containing versioned before and desired recovery material.
6. Apply the target-specific atomic write.
7. Verify owned fields, unrelated fields, file identity, permissions, and absence/presence semantics.
8. Commit Provider/Credential/Snapshot/route/problem/acknowledgement/receipt state in one immediate SQLite transaction.
9. Publish exactly one Target View after the response boundary.

If a failure occurs after the intent but before commit, the coordinator exact-restores and verifies the pre-action file state. Successful rollback marks the intent rolled back and returns a stable failure. Failed rollback marks only the affected Target Recovery Required, blocks further managed writes, publishes the authoritative recovery view once, and makes same-action replay consistent with that view.

Adopt secret capture is copied directly into the credential write boundary and is never retained in a preview, diagnostic, or public action object.

## Drift and Shadow Policy

Drift is the difference between currently observed owned Managed Configuration and the last committed or explicitly adopted expectation. Unrelated changes are preserved and do not become drift, but are part of the race fingerprint so a preview never overwrites a changed unrelated document.

Known higher-priority sources are reported with closed source identities. These include the observable Codex and Claude sources already recognized by the target codecs. T07 never edits, clears, renames, or follows these sources. A known shadow blocks managed writes even when the underlying global file matches.

Command-line flags, environment supplied directly to a separately started process, and resumed-session in-memory state may be unobservable. The UI states this limitation and does not claim that a clean file observation proves effective runtime configuration.

## Stable Problems

The workflow uses stable, localizable codes:

- `configuration-drift`
- `shadowing-configuration`
- `incompatible-target-cli`
- `untested-target-cli`
- `compatibility-acknowledgement-required`
- `stale-reconciliation-preview`
- `target-busy`
- `configuration-write-failed`
- `recovery-required`

Raw paths, native configuration, probe output, backend messages, and secrets do not cross the control protocol. Where a source identity is useful, it uses a closed enum already safe for localization.

## Control Plane

The Control Plane registers one target-scoped command family:

- `target.reconciliation.open`
- `target.reconciliation.preview.adopt`
- `target.reconciliation.preview.reapply`
- `target.reconciliation.preview.restore`
- `target.reconciliation.apply`
- `target.reconciliation.cancel`

The same command identities and overlay component serve Codex and Claude. No target-specific global handler or second keyboard listener is added.

When the Target View reports drift, shadowing, unknown-compatible, or incompatible state, the Target page shows localized guidance and a Reconcile entry point. Provider editors retain saved values. Drifted or incompatible Targets expose inspection but disable save and managed-write commands.

The Reconciliation overlay shows:

- compatibility class and exact version;
- observable shadow source;
- unobservable-boundary disclosure;
- redacted field and state effects for the selected strategy;
- restart guidance;
- an explicit acknowledgement control for unknown-compatible versions.

Apply pending makes the overlay nondismissible and disables background command layers. A stale preview remains open and asks the Operator to preview again; it never retries automatically. Success closes the exact overlay token, installs the authoritative Target View once, appends one localized activity, and restores focus to the originating Target shell.

English and Simplified Chinese catalogs contain the same keys and preserve the established minimal target-context layout at all supported terminal sizes.

## Testing Strategy

### Pure and codec tests

- Codex TOML and Claude JSON Adopt, Reapply, and Restore.
- Owned versus unrelated field mutations.
- Claude legacy three-field and current four-field recovery ownership.
- Directory symlink canonicalization and managed-file symlink rejection.
- Every known observable shadow source.
- Tested, unknown-compatible, incompatible, version-change, and acknowledgement cases.
- Fixed, secret-free diagnostics with mutation-sensitive opposite-branch tests.

### State and coordinator tests

- Target/version-scoped acknowledgement persistence and invalidation.
- Preview performs zero writes.
- Token binds Target, strategy, revision, compatibility, shadows, file identity, semantics, Snapshot, and Recovery Intent.
- Receipt-first replay and response-before-one-push ordering.
- Adopt creates new identities without mutating prior Provider or credential records.
- Reapply and Restore preserve unrelated configuration.
- Every pre-intent and post-intent failure boundary.
- Exact rollback, rollback verification failure, and target-local Recovery Required.
- Drift blocks only the affected Target's ordinary mutations.

### Real UDS and loopback tests

- Full preview/apply protocol, malformed frames, reconnect, replay, and target isolation.
- No secret in any request, response, push, receipt, problem, or diagnostic.
- Existing Takeover requests stay pinned to their starting Snapshot.
- Restore succeeds with zero in-flight requests and stops only the affected Target listener.
- Restore with an in-flight request returns `target-busy` and has a complete no-mutation fingerprint.

### OpenTUI tests

- Shared Codex/Claude commands and overlay priority.
- English and Simplified Chinese copy.
- Tested, unknown-compatible acknowledgement, incompatible read-only, shadow, stale preview, pending, success, and failure states.
- Exact overlay identity, duplicate dispatch suppression, focus restoration, target switching, and target-local activities.
- `1x1`, `2x2`, `20x5`, `40x10`, `80x24`, and `121x30` renderer coverage.
- Scan-first auditing of frames, actions, activities, views, errors, and timeout diagnostics.

### Real-process tracer

The tracer creates real Codex and Claude configuration drift and exercises Adopt, Reapply, and Restore through the real Control Plane, UDS, Routing Service, SQLite, target codecs, and deterministic loopback servers. It verifies restart behavior, persisted acknowledgement, immutable historical Providers/Snapshots, listener state, zero-in-flight Restore, natural service exit, UDS removal, and secret scans across raw frames, native frames, process output, SQLite text, configuration files, receipts, and captured upstream requests.

The final gate remains `bun run verify` plus workspace Rust tests, formatting, Clippy with warnings denied, TypeScript type checking, and `git diff --check`. Real UDS and loopback tests run with the established approved local permissions.

## Explicit Decisions

- Drift-time Restore is implemented in T07 only for zero in-flight requests; T10 owns normal drain-and-remove behavior.
- Unknown-compatible acknowledgement persists per Target and exact version.
- Adopt creates a new ordinary Target Provider, Credential Reference when needed, and Activated Snapshot; it does not edit the old Provider.
- Preview tokens are ephemeral and must be regenerated after service restart.
- The Routing Service, not the Control Plane, owns all observation, preview, compatibility, write, verification, and recovery authority.
- A shared coordinator and shared UI workflow are preferred over expanding ActivationService or duplicating target-specific flows.
