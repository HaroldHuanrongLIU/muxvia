# Universal Providers and Transactional Synchronization Design

Status: approved for T08 on 2026-08-20

Issue: [#9 — T08 Universal Providers and transactional synchronization](https://github.com/HaroldHuanrongLIU/muxvia/issues/9)

## Context

T08 lets the Operator maintain a shared upstream definition once and explicitly materialize it into protected Generated Target Providers for Codex CLI, Claude Code, or both. The design follows `CONTEXT.md`, ADR 0003, ADR 0024, ADR 0028, ADR 0029, ADR 0032, and ADR 0033.

The Routing Service remains the sole storage owner. Provider Synchronization changes declarations only. It never changes Current Target Providers, Serving Providers, Activated Snapshots, Activated Route Plans, Target Takeover, Managed Configuration, recovery material, or routed traffic.

## Scope

T08 delivers:

- a cross-target Universal Provider catalog with create, inspect, edit, duplicate, delete, and copy-on-create Presets;
- explicit per-Universal-Provider synchronization across every enabled Target in one SQLite transaction;
- protected Generated Target Providers with Universal-owned fields and editable Target Overlay fields;
- target enablement for Codex, Claude, or both;
- safe generated duplication into ordinary detached Target Providers;
- reference-aware target disablement and Universal Provider deletion;
- stable Preset and one-time seed matching by Preset key;
- one shared Control Plane workflow, English and Simplified Chinese copy, and real-process evidence.

T08 does not add Failover Chain editing, Activated Route Plan application, routing failover, account-backed providers, import/export, backup/restore, or automatic background synchronization. T08 reserves and projects the closed `activated-route-plan` reference kind so the later route-plan ticket can populate it without changing generated lifecycle semantics.

## Binding Decisions

1. Universal Providers live behind an independent `UniversalProviderSession`, not a TargetSession. The catalog has its own revision, view sequence, receipts, and push stream.
2. Universal-owned fields are display name, normalized Base URL, Credential Reference, Preset provenance, and target enablement.
3. Target Overlay fields are model, target-native authentication profile, and routing requirement. Target protocol is fixed by Target CLI.
4. A Universal edit does not synchronize implicitly. It advances the catalog revision and leaves generated records visibly pending until an explicit synchronization.
5. Provider Synchronization is scoped to one Universal Provider and covers all its enabled, disabled, created, updated, and removed Target materializations in one immediate transaction.
6. Disabling a Target is rejected immediately while its Generated Target Provider is Current, referenced by an Activated Snapshot, or referenced by an Activated Route Plan. The failure lists every reference.
7. Deleting a Universal Provider atomically deletes the source, its unreferenced Generated Target Providers, and orphaned generated Credential References. Any blocking reference rejects the whole deletion.
8. Copy-on-create Presets and optional one-time initialization use stable Preset keys. Provider identity and display name never participate in Preset matching.

## Product Invariants

1. The Routing Service consumes only Target Providers. Universal Providers never serve traffic directly.
2. Synchronization is all-or-nothing across Codex and Claude for one Universal Provider.
3. Synchronization never changes Current, Serving, Activated Snapshot, Activated Route Plan, Managed Configuration, takeover, recovery, or runtime listener state.
4. Universal-owned fields in a Generated Target Provider are read-only at every wire and state mutation seam.
5. Target Overlay fields cannot replace Universal-owned fields and remain editable from the generated record.
6. Generated records cannot be independently deleted or detached in place.
7. Duplicating a Generated Target Provider creates a new ordinary identity, clears generated ownership, and copies declaration state only.
8. Replacing a Universal credential creates new target-scoped Credential References during synchronization. It never mutates a Credential Reference shared with a detached Provider.
9. A failed materialization, validation, credential operation, reference check, or target publication preparation leaves every generated declaration unchanged.
10. Configuration Drift, Shadowing Configuration, incompatible Target compatibility, Recovery Required, or missing exact compatibility acknowledgement blocks synchronization for the affected Target and therefore rolls back the entire synchronization.
11. Secrets never enter catalog views, Target Views, receipts, activities, diagnostics, logs, Debug output, renderer frames, or test failure output.

## Domain and Storage Model

### Catalog state

The schema adds one singleton catalog-state row with `revision` and `view_sequence`, plus secret-free action receipts scoped to the Universal Provider catalog.

Each Universal Provider stores:

- UUID and provider revision;
- required display name;
- normalized Base URL, with empty meaning incomplete;
- optional Universal Credential Reference;
- ordinary or Preset provenance with a stable Preset key; and
- no Target CLI runtime state.

Universal credentials have identities independent from existing target-scoped credentials. Their secret bytes are private Routing Service state.

Each Universal Provider has exactly one target-setting row for Codex and one for Claude. A row stores:

- enabled state;
- model;
- target-native authentication profile;
- routing requirement;
- overlay revision; and
- the last source and overlay revisions successfully synchronized.

The generated Target Provider identity is derived by authoritative lookup through `(universal_provider_id, target)` rather than trusted from a client. A uniqueness constraint permits at most one generated record per Universal Provider and Target.

### Ownership projection

Generated Target Providers retain the existing Target Provider fields. Their provenance and generated-owner metadata identify the source. Target View projection adds:

- the Universal Provider identity;
- synchronization state `current` or `pending`;
- closed field ownership for name, Base URL, Credential, model, authentication, routing requirement, and protocol; and
- the existing complete list of active references, extended with `activated-route-plan`.

The Universal catalog view exposes credential presence only. It shows enabled targets, Target Overlay values, generated Provider identities, synchronization state, and blocking references without target credential identities or secret bytes.

Completeness remains derived at the Target Provider level. A Universal Provider may be incomplete and may synchronize an Incomplete Provider; activation and future route-plan application keep their existing completeness checks.

## Deep Modules and Interfaces

### Universal Provider Catalog

The external seam is one independent session:

```text
MuxviaControl.openUniversalProviders() -> UniversalProviderSession

UniversalProviderSession.get() -> UniversalProviderCatalogView
UniversalProviderSession.act(UniversalProviderAction) -> UniversalProviderOutcome
UniversalProviderSession.subscribe(listener)
UniversalProviderSession.close()
```

The session owns action serialization, call-time deep capture, revision binding, authoritative replacement, and catalog push de-duplication. Callers do not coordinate Target revisions or inspect SQLite.

Closed catalog actions are:

- create Universal Provider;
- update Universal Provider;
- duplicate Universal Provider;
- delete Universal Provider; and
- synchronize Universal Provider.

Each action carries a client action UUID and the expected catalog revision. Source-specific actions also carry the expected Universal Provider revision. Receipt-first replay returns the original catalog outcome without repeating target materialization or publication.

### Provider Synchronization Coordinator

A crate-internal Provider Synchronization Coordinator is the deep module behind synchronization. Its interface accepts one source identity, expected revisions, and the already-authorized action identity. Its implementation owns:

- acquiring both Target mutation gates in the fixed Codex-then-Claude order;
- live managed-write eligibility for every affected Target;
- reference discovery and blocker projection;
- generated identity lookup;
- target-native declaration validation;
- fresh target credential insertion and safe orphan cleanup;
- one immediate SQLite transaction for all generated create/update/remove work;
- catalog receipt and authoritative catalog/Target views; and
- response-before-push publication after commit.

The coordinator never calls configuration codecs, writes Target configuration, changes runtime listeners, or creates Activated Snapshots.

### Generated Provider mutation

The existing Target Provider mutation module remains the seam for generated record inspection, duplication, and Target Overlay edit. For a Generated Target Provider:

- update requires all Universal-owned submitted values to equal the authoritative source;
- credential edit must be `keep`;
- only model, authentication, and routing requirement may change;
- the corresponding target-setting overlay is updated in the same transaction;
- both the Target and catalog revisions advance; and
- the generated record remains synchronized with the unchanged Universal source revision.

Independent delete remains rejected. Duplicate clears generated ownership and copies the current declaration into a new ordinary Provider. Credential reuse remains an explicit Operator choice.

## Presets and One-time Initialization

The catalog contains two release-owned copy-on-create Presets using the existing stable keys:

- `openai-api-responses`: initializes the OpenAI Base URL, enables Codex, leaves the model and credential missing, and uses Codex's fixed authentication profile;
- `anthropic-api-messages`: initializes the Anthropic Base URL, enables Claude, leaves the model and credential missing, and uses Claude API-key authentication.

Preset values are copied into an editable draft. Saving records non-owning Preset provenance; later Preset changes never update the source or generated records.

Any one-time initialization marker is keyed only by the Preset key. Renaming a Universal Provider or changing its UUID never makes a seed eligible again, and a same-name ordinary Universal Provider never suppresses a keyed seed.

## Transaction and Publication Ordering

Every catalog mutation follows:

1. Check an existing catalog receipt before parsing the raw action.
2. Validate catalog and Universal Provider revisions.
3. Parse and structurally validate the source and both target settings.
4. For disable/delete/synchronize, compute every authoritative generated reference.
5. For synchronization, acquire affected Target mutation gates and re-check target managed-write eligibility.
6. Open one SQLite `IMMEDIATE` transaction.
7. Create, update, or remove every affected generated declaration and target credential.
8. Re-read and assert that Current, Serving, snapshots, route state, recovery, compatibility, problems, and reconciliation state are unchanged except for target declaration revisions and view sequences.
9. Commit the catalog receipt, catalog revision/view sequence, affected target management/view revisions, and complete authoritative views in that transaction.
10. Write the response, then publish at most one catalog view and one view for each changed Target.

Any failure before commit rolls back every source, target setting, generated declaration, and credential change. A publication failure never rolls back committed declarations; a reconnect reads the durable authoritative state.

## Reference Policy

Reference discovery is authoritative and closed:

- `current` from Target Route State;
- `activated-snapshot` from the currently applied immutable snapshot; and
- `activated-route-plan` from the route-plan membership seam when that table exists.

Failures return every `(target, generatedProviderId, referenceKind)` in deterministic Target/reference order. The client never supplies or filters blockers.

Disabling an unreferenced Target is a catalog edit and becomes a pending generated removal. The later explicit synchronization removes the generated record. Deleting a Universal Provider performs its unreferenced generated removals in the delete transaction because no source remains to synchronize later.

## Control Plane

The current Target shell registers one global command family that opens the same Universal Provider catalog from Codex or Claude:

- `universal-provider.list`
- `universal-provider.create`
- `universal-provider.edit`
- `universal-provider.duplicate`
- `universal-provider.delete`
- `universal-provider.synchronize`

The catalog is an overlay, not permanent navigation. It shows source completeness, enabled targets, generated identities, pending/current synchronization state, Preset provenance, credential presence, and all blockers.

The Universal editor owns source fields plus both Target settings. The generated Target Provider editor renders name, Base URL, Credential, and protocol as read-only and exposes only Target Overlay fields. Delete and target disable blockers remain in their originating overlay and list every reference. Synchronization uses explicit confirmation when it will remove a generated record.

Pending actions make the exact overlay nondismissible, suppress duplicate dispatch, capture the origin session before awaiting, and restore focus after completion. English and Simplified Chinese use one shared key set at all supported terminal sizes.

## Stable Problems

- `invalid-universal-provider`
- `stale-universal-provider-revision`
- `stale-universal-catalog-revision`
- `no-universal-provider-change`
- `generated-provider-read-only`
- `generated-provider-delete-forbidden`
- `generated-provider-referenced`
- `provider-synchronization-blocked`
- existing target-local drift, shadow, compatibility, and recovery codes
- existing `state-store-error` and `connection-closed`

Messages contain no raw provider secret, target credential, backend error, SQLite value, or configuration content.

## Testing Strategy

The confirmed public seams are:

1. Rust/TypeScript/JSON-schema protocol fixtures and a real UDS Universal Provider session;
2. the Routing Service catalog/state interface with real SQLite transactions;
3. TargetSession plus UniversalProviderSession through the real OpenTUI renderer; and
4. a real-process walking tracer with real UDS, SQLite, renderer, and deterministic loopback upstreams.

Tests prove:

- schema migration and secret-safe catalog projection;
- CRUD, stable Preset keys, one-time seed keys, revisions, receipts, replay, and stale actions;
- one- and two-Target synchronization, every create/update/remove combination, and failpoint rollback;
- zero changes to Current, Serving, snapshots, route plans, Managed Configuration, recovery, compatibility, runtime listeners, and held requests;
- target eligibility gates and fixed lock order;
- Universal-owned read-only enforcement, Target Overlay persistence, detached duplication, and forbidden generated delete;
- complete deterministic reference blockers;
- response-before-push, no replay push, catalog/Target publication ordering, reconnect, and multi-session races;
- scan-first diagnostics with controlled credential, configuration, backend, and settings sentinels; and
- end-to-end create, edit, sync-both, overlay edit, duplicate-detach, reference-blocked disable/delete, release, resynchronize, delete, natural exit, and UDS removal.

