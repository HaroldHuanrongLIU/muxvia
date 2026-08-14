# Target Provider Workflow Design

Status: approved for T03 on 2026-08-14

Issue: [#4 — T03 Target Provider workflow](https://github.com/HaroldHuanrongLIU/muxvia/issues/4)

## Goal

Let an Operator manage ordinary Codex Target Providers, their models, and their Credential References from the Codex Target context without changing current traffic or Managed Configuration implicitly.

T03 completes declaration management only. It does not add Claude Code as a live Target context; that belongs to #6. It also does not implement Direct Activation, Universal Providers, Failover Chains, Route Health, import/export, accounts, or usage.

## Binding decisions

- Provider create, edit, reorder, duplicate, delete, Preset selection, Model Discovery, and Reachability Check never change Current Target Provider, Serving Provider, Activated Snapshot, Target Takeover, Managed Configuration, or routed traffic.
- A Provider name is required. A nonempty endpoint must pass the existing URL safety policy. Missing endpoint, model, or Credential Reference produces a structurally valid Incomplete Provider.
- Codex T03 has one fixed protocol/auth profile: OpenAI-compatible Responses with bearer authentication. Anthropic auth profiles and pagination belong to the Claude Target ticket.
- Incomplete Providers can be saved, inspected, edited, reordered, duplicated, and deleted when unreferenced. Activation continues to reject them before any recovery intent, configuration write, or snapshot creation.
- Credentials have identities independent of Providers. Create or replace makes a new Credential Reference; keep retains the current reference; remove detaches it. Replacing a shared credential rebinds only the edited Provider and never mutates the secret used by another Provider.
- Duplicate opens an editor prefilled from the source, with the localized default name `<source> Copy`, and inserts the saved duplicate immediately after the source. It creates a new Provider identity and copies declaration state only. Reusing the source Credential Reference requires a separate explicit confirmation; declining leaves the duplicate credential missing unless the Operator enters a replacement.
- T03 ships Blank creation plus one release-owned Preset with stable key `openai-api-responses`. The Preset copies `https://api.openai.com/v1` and the fixed Responses protocol into a new editable draft; it contains no credential, model, affiliate content, or `auth.json` behavior. Saving may retain the non-owning Preset provenance key for display, but later catalog changes never update the Provider.
- Automatic Model Discovery runs once only when an existing Provider editor opens. It uses the last-saved endpoint and Credential Reference. A create or Preset draft has no automatic network request.
- Editing endpoint or credential fields never starts a request. Explicit refresh uses the current draft endpoint and either the newly typed ephemeral credential or, when unchanged, the saved Credential Reference.
- Reachability follows the pinned CC-Switch algorithm selected in Q102: unauthenticated `GET` of the normalized endpoint, 8 seconds per attempt, one retry only for a timeout-like failure, and any received HTTP status means reachable. It reads response headers only and never changes Route Health.
- English and Simplified Chinese remain the only v0.1 locales. Operator data such as names, endpoints, and model IDs is never translated.

## Domain and storage model

The Routing Service remains the sole storage owner. Schema version 2 introduces explicit declaration order and independent credential identities while migrating existing T01 state without changing Provider UUIDs, current state, snapshots, or credential bytes.

Each Provider stores:

- UUID, target, and per-target zero-based position;
- required display name;
- optional normalized base URL and optional model;
- fixed protocol `openai-responses`;
- optional Credential Reference;
- configuration revision, incremented only when that Provider declaration changes;
- ordinary or Preset provenance; and
- optional generated-owner metadata reserved for later Universal Provider work.

Credential records store a UUID, target, and secret bearer token. Multiple same-target Providers may reference one credential identity. Deleting or rebinding a Provider removes an orphaned credential in the same transaction only after no Provider references it.

Completeness is derived rather than persisted. The required activation fields for T03 are endpoint, model, Credential Reference, and the fixed Responses protocol. Target View projection exposes `complete` or `incomplete` plus the exact missing field names.

Active references are also derived. T03 projects Current and Activated Snapshot references. The wire shape admits an Activated Route Plan reference later, but T03 does not create route-plan state. An ordinary Provider referenced by Current or an Activated Snapshot cannot be deleted. Generated Provider deletion remains reserved and blocked when that state appears in later tickets.

## Mutation boundary

All persistent Provider mutations remain Target actions guarded by a new action UUID and the expected target management revision:

- create Provider;
- update Provider with explicit credential `keep`, `remove`, or `replace` intent;
- reorder the complete Provider identity permutation;
- duplicate Provider with a source identity, edited declaration, and explicit credential intent; and
- delete Provider.

The Routing Service checks a recorded receipt before parsing the raw action. A successful mutation, including a successful no-op only when explicitly defined, commits its declaration changes, management revision, view sequence, secret-free receipt, and complete Target View in one immediate transaction. Stale, invalid, incomplete-order, referenced-delete, and recovery-required failures do not partially mutate state. All Provider mutations remain blocked while the Codex Target is Recovery Required.

Reorder changes display order only. It is not a Failover Chain and never changes routing attempt order. The service accepts only an exact permutation of all current ordinary Provider IDs for the target.

## Target View and RPC

Provider projection adds:

- stable position and per-Provider configuration revision;
- nullable endpoint and model;
- fixed protocol;
- Credential Reference presence, never its identity or bytes;
- completeness and missing fields;
- provenance and generated status; and
- secret-free active references.

The Preset catalog is a secret-free Target View field owned by the Routing Service so both executables agree on stable keys and copied values.

Persistent mutations continue through `TargetSession.act()` and return `ActionOutcome` with a complete authoritative Target View. Model Discovery and Reachability use separate read-only RPC results with no Target View and no action receipt. A read-only result therefore cannot roll the session back after a newer pushed view.

Probe requests are redacted in Rust and TypeScript diagnostics. Automatic discovery sends only Provider identity plus configuration revision; the Routing Service resolves the saved Credential Reference. Explicit refresh may carry one ephemeral draft credential, which is never persisted, placed in a receipt, echoed in a result, or included in ordinary errors. Editor generation suppresses late or superseded results.

## Provider inspection adapter

A focused internal `ProviderInspector` owns Model Discovery and Reachability. It uses a redirect-free and proxy-free HTTP client independent from routed model transport.

For T03 Model Discovery it:

1. validates and normalizes the endpoint;
2. constructs the pinned ordered OpenAI-compatible models candidates described in [the research note](../../research/model-discovery-and-reachability-contracts.md);
3. sends bearer-authenticated `GET` requests with a 15-second per-candidate timeout;
4. falls through only after 404 or 405;
5. accepts a bounded JSON `data` array, preserves nonblank IDs, de-duplicates exact IDs, and sorts deterministically; and
6. returns stable secret-free categories without bodies, headers, credential values, or library error text.

Discovery has explicit response-byte and model-count bounds below the 1 MiB RPC frame limit. A valid empty list is a successful result. Manual model entry never depends on discovery success.

For Reachability it sends `Accept: */*` and `Accept-Encoding: identity`, sends no credential, stops after response headers, reports status and time-to-first-byte, and labels responses over 6 seconds as slow without calling that state Route Health or Provider Health. Timeout-like failures retry once; DNS, connect, TLS, cancellation, and HTTP responses do not.

Neither operation writes SQLite, Request Records, usage, Current/Serving state, snapshots, Managed Configuration, health, or circuit state.

## Control Plane experience

The Codex context keeps the accepted OpenCode shell. `/providers` opens an OpenCode-style selection dialog rather than adding permanent navigation. The dialog shows ordered Provider rows, completeness, provenance/generated state, and active-reference labels while secrets remain presence-only.

From the selected row, named commands open inspect/edit, move the Provider, duplicate, run Reachability against the saved endpoint, or request deletion. Delete and Credential Reference reuse use focused confirmation overlays. A referenced delete explains every blocking reference and performs no mutation.

Create offers Blank and the single safe Preset, then opens the same editor used by edit and duplicate. The editor owns its normal draft fields, ephemeral credential, discovery suggestions, discovery status, and dirty state. It never reconstructs a saved secret. Model suggestions are selectable but do not replace manual input automatically.

Editor open starts at most one saved-state discovery. Typing never causes network traffic. `/refresh-models` is an explicit editor command; `/check-reachability` is an explicit saved-Provider command in the selection/detail flow. Closing, cancelling, confirmed dirty exit, successful save, and unmount clear transient credentials and ignore late discovery results.

## Error and security boundaries

- Empty names and nonempty unsafe endpoints are invalid; missing activation values are incomplete, not invalid.
- Every ordinary result, Target View, receipt, activity item, error, debug representation, captured renderer frame, and test diagnostic is secret-free.
- Probe errors expose stable categories and safe status/timing metadata only. Raw upstream bodies and headers are neither returned nor retained.
- Redirects, environment proxies, cross-origin fallback, userinfo, fragments, and plaintext non-loopback endpoints are rejected.
- Provider declaration edits never rewrite an Activated Snapshot. Editing the Current Provider changes its saved declaration while existing traffic remains pinned to the prior immutable snapshot.
- Delete cleans an orphaned Credential Reference only after all references are gone and never breaks a shared credential silently.

## Verification strategy

Pure and state tests prove schema migration, completeness derivation, credential reference sharing and garbage collection, exact ordering, declaration-only duplication, active-reference deletion guards, receipt-first replay, stale revisions, and activation rejection of incomplete records.

Protocol and UDS tests prove Rust/TypeScript agreement, bounded read-only results, out-of-order probe safety, Target View projection, subscription sequencing, and secret-free framing.

Deterministic loopback tests prove candidate order, 404/405-only discovery fallback, bearer auth isolation, parsing bounds, cancellation/stale suppression, Reachability retry/status behavior, and no Route Health or persistent-state mutation.

OpenTUI renderer tests exercise create, inspect, edit, reorder, duplicate, Credential Reference confirmation, delete confirmation/blockers, Preset copy, discovery suggestions, manual fallback, explicit refresh, Reachability, localization, small terminals, overlay priority, dirty exit, focus restoration, and frame-level secret scans.

A real-process tracer uses a temporary Muxvia Home, temporary Codex home, real UDS, real SQLite, OpenTUI test renderer, and deterministic HTTP upstream to demonstrate create-incomplete, complete edit, discovery, duplicate, reorder, active-delete rejection, inactive delete, and unchanged takeover traffic/configuration.

## Out of scope

- Claude Target Provider state or Anthropic discovery/authentication.
- Direct Activation or removal of Target Takeover.
- Universal Provider synchronization and real Generated Target Providers.
- Failover Chain editing, Activated Route Plans, Route Health, or circuit breaking.
- Imported provenance sources beyond the wire shape reserved for later tickets.
- Managed Codex Subscription discovery, account identity headers, or Subscription Bridge behavior.
- Background discovery while typing, persisted discovery results, reachability history, provider custom headers, or arbitrary auth profiles.
- Any Provider CRUD surface in the noninteractive CLI.
