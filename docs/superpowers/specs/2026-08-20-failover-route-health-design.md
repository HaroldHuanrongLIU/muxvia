# Failover Chain, Route Health, and Immutable Route Epochs Design

Status: approved for T09 on 2026-08-20

Issue: [#10 — T09 Failover Chain, Route Health, and immutable route epochs](https://github.com/HaroldHuanrongLIU/muxvia/issues/10)

## Context

T09 lets the Operator edit one Target-scoped Failover Chain, apply it as an immutable Activated Route Plan, and observe request-derived Serving Provider and Route Health without changing the Current Target Provider. The design follows `CONTEXT.md`, ADR 0004, ADR 0017, ADR 0028, ADR 0030, ADR 0031, ADR 0032, and ADR 0045.

The Routing Service remains the sole owner of route drafts, immutable plans, runtime eligibility, request routing, passive health, and persistence. The Control Plane receives complete secret-free Target Views and never opens product storage.

## Scope

T09 delivers:

- one independent Failover Chain draft for Codex CLI and one for Claude Code;
- revision-guarded editing that changes no live traffic;
- atomic Apply that validates every member and creates one immutable Activated Route Plan;
- request pinning to the plan epoch observed at request start;
- priority-first, non-sticky failover before response commitment;
- Current-versus-Serving separation;
- passive per-Provider Route Health plus a Target-level plan summary;
- fixed release-owned circuit-breaker behavior reset for every Routing Service epoch;
- target-isolated TUI editing, application, health projection, and real-process evidence.

T09 does not add account-backed Providers, request history, usage/cost persistence, configurable routing policies, weighted routing, load balancing, background health checks, automatic Provider synchronization, service handover, or a routing-policy DSL.

## Binding Decisions

1. Each Target owns one durable Failover Chain draft independently from its Activated Route Plan.
2. A valid nonempty draft contains unique Provider identities and its first member is the Current Target Provider.
3. Applying a draft atomically snapshots every member and creates a new immutable Activated Route Plan epoch. It does not write Managed Configuration or restart a listener.
4. Activating a Provider creates a new one-member draft and one-member Activated Route Plan for that new Current Provider. This prevents a new Current selection from continuing to route through an older plan.
5. Every request captures one complete plan at request start. Later Apply or Activate operations affect only later requests.
6. Every eligible member is attempted at most once in plan order. A later request always begins again at the first eligible member.
7. Failover is allowed only before the first valid downstream output is committed. Once committed, later errors or cancellation never start another Provider attempt.
8. Route Health is derived only from real routed attempts. Model Discovery, Reachability Check, Provider editing, synchronization, activation preflight, and configuration reconciliation never change it.
9. Circuit eligibility is memory-only for one Routing Service epoch. Durable health remains visible after restart as stale but cannot exclude a Provider until the new epoch has fresh request evidence.
10. Circuit parameters are fixed release behavior rather than a new Operator setting: four consecutive failures, or a 60% failure rate after ten attempts, opens the circuit; one probe is admitted after 60 seconds; two consecutive half-open successes close it.

## Product Invariants

1. Codex and Claude drafts, plans, Serving state, health observations, and circuit state are completely target-isolated.
2. Draft edits never change the active plan, Current, Serving, Activated Snapshot, Managed Configuration, takeover, recovery, listener, or in-flight request.
3. Apply succeeds only when every member exists for the Target, is complete, has a valid protocol/authentication shape, has a usable Credential Reference, and is synchronized when generated.
4. Apply rejects duplicate members, an empty chain, a first member different from Current, stale Provider revisions, and any active managed-write blocker before mutation.
5. The immutable plan stores member order and immutable member snapshots; later Provider edits or synchronization do not alter it.
6. Provider deletion and generated lifecycle checks treat membership in the active plan as an authoritative `activated-route-plan` reference.
7. A request uses only the plan epoch captured at its start, including member credentials, endpoints, models, and order.
8. Failover success changes Serving Provider and view sequence only. It never changes Current Provider, management revision, draft, plan, snapshot, or Managed Configuration.
9. A retryable failure changes only request-derived health and view sequence. A nonretryable client/request error does not poison Provider health.
10. Reachability remains an unauthenticated, headers-only observation and never changes health or eligibility.
11. Secrets never enter drafts, plans, Target Views, receipts, health diagnostics, activities, logs, Debug output, renderer frames, or test failure output.

## Domain and Storage Model

### Failover Chain draft

Each Target has one singleton draft with its own draft revision and ordered membership rows. A member stores:

- Provider identity;
- the Provider revision observed when saved into the draft; and
- zero-based position.

The Target View projects the draft revision, ordered Provider identities, whether it differs from the active plan, and validation problems. Secret or credential identities are not projected.

### Activated Route Plan

Every Apply or Activate creates a new immutable plan row containing:

- plan UUID;
- Target;
- plan epoch UUID;
- creation revision; and
- ordered immutable member snapshots.

Each member snapshot contains the Provider identity and revision, display name, normalized endpoint, model, protocol, authentication shape, routing requirement, and private Credential Reference. Target Views project only the secret-free subset. The Target route row points to at most one active plan.

Historical plans and member snapshots remain immutable. T09 retains them for active-request pinning and later Request Record work; it does not add a retention policy.

### Route Health

Durable per-Provider health stores:

- state: `healthy`, `degraded`, or `unavailable`;
- the last observed outcome category;
- consecutive successes and failures;
- total and failed attempts for the current recorded window;
- the Routing Service epoch that produced the observation; and
- monotonically increasing observation sequence.

Projection maps an observation from the current epoch to its current state and an older epoch to `stale`. A Provider with no routed evidence is `unobserved`.

The Target-level Route Health summary is derived from the active plan:

- `unobserved` when no member has current-epoch evidence;
- `healthy` when the first member is eligible and healthy;
- `degraded` when the first member is degraded/open but a later member is eligible;
- `unavailable` when no member is eligible;
- `stale` after restart when only historical observations exist.

The in-memory circuit owns exact eligibility, half-open permits, and timers. Durable state is observation, not startup authority.

## Deep Modules and Interfaces

### Route Plan Coordinator

The external seam remains `TargetSession.act()` with two new closed Target actions:

- `save-failover-draft`, carrying the complete ordered member list and exact Provider revisions;
- `apply-failover-chain`, carrying the expected draft revision.

A crate-internal Route Plan Coordinator is the deep module behind both actions. Its small interface accepts the already-authorized Target/action identity and one typed draft/apply input. The implementation owns:

- the shared Target mutation gate;
- receipt-first replay;
- revision and managed-write eligibility checks;
- complete member validation;
- immutable plan/member snapshot creation;
- Target route-state update;
- draft/plan/reference projection;
- response-before-push publication.

The coordinator does not start upstream work or mutate Managed Configuration.

### Request Router

A crate-internal Request Router is the deep module used by both Codex Responses and Claude Messages model handlers. Its interface accepts the Target and one already-authorized inbound request and returns one downstream response stream plus a pinned-plan guard.

The implementation owns:

- loading and pinning the active plan once;
- consulting per-member circuit eligibility;
- cloning/replaying the bounded inbound request body for another attempt;
- target-native upstream URL and authentication construction;
- retryable/nonretryable outcome classification;
- bounded semantic response priming;
- health/circuit recording;
- Serving publication after commitment;
- dropping all retry state after commitment or downstream cancellation.

Codex and Claude keep their target-native header and path adapters. There is no generic public LLM schema.

## Attempt and Commitment Semantics

Every eligible member is attempted once in plan order. The baseline retry classification is retained for ordinary Provider credentials:

- retryable: transport/connect failure, timeout, stream-idle failure, local Provider configuration/authentication failure, semantic upstream failure, HTTP 401/403/404/408/409/429/451, and HTTP 5xx except 501;
- nonretryable: malformed client input, downstream cancellation, HTTP 400/405/406/413/414/415/422/501, internal state failure, and any error after downstream commitment.

For non-streaming responses, status and bounded semantic validation occur before the response is written downstream. For SSE:

- lifecycle-only events may be buffered;
- an error/failure terminal before productive output is retryable;
- the first productive protocol-valid event or valid nonfailure terminal commits and replays all buffered bytes;
- a bounded byte/time limit commits buffered bytes conservatively instead of risking duplicate upstream work.

The exact Responses and Messages event classifiers remain target-native and are covered by versioned fixtures.

## Transaction and Publication Ordering

Draft save:

1. check receipt;
2. acquire the Target mutation gate;
3. recheck receipt and management revision;
4. validate the complete ordered draft;
5. commit draft rows, Target revision/view sequence, receipt, and authoritative Target View in one immediate transaction;
6. write response, then publish at most one newer authoritative view.

Apply:

1. check receipt and acquire the Target mutation gate;
2. recheck receipt, Target revision, and draft revision;
3. live-check Target managed-write eligibility without writing configuration;
4. validate Current-first membership and every declaration/revision/credential;
5. create the immutable plan and member snapshots;
6. atomically point the Target route row at the new plan, advance Target revision/view sequence, store receipt, and preserve Current/Serving/configuration/recovery state;
7. write response, then publish at most one newer authoritative view.

An apply failure leaves the draft intact and the active plan unchanged.

## Control Plane

The Target-scoped command family is shared by Codex and Claude:

- `route.open`
- `route.move-up`
- `route.move-down`
- `route.add-provider`
- `route.remove-provider`
- `route.apply`

The route editor is an overlay, not permanent navigation. It shows Current as the required first member, draft/active divergence, member completeness and generated synchronization, per-member health, the active plan epoch, and Current-versus-Serving. It never exposes credential identity or upstream error payload.

Pending Apply makes the modal nondismissible and suppresses duplicate dispatch. Async completion remains bound to the originating Target/session/generation. English and Simplified Chinese share one key set and render at every supported terminal size.

## Stable Problems

- `invalid-failover-chain`
- `stale-failover-draft-revision`
- `duplicate-failover-provider`
- `current-provider-must-be-first`
- `incomplete-route-plan-provider`
- `unsynchronized-route-plan-provider`
- `no-activated-route-plan`
- `all-route-providers-unavailable`
- existing stale Target, managed-write, Provider, recovery, state, and connection codes

Messages never contain raw Provider credentials, routing credentials, upstream response bodies, database values, or request content.

## Testing Strategy

The confirmed public seams are:

1. Rust/TypeScript/JSON-schema protocol fixtures and a real UDS TargetSession;
2. the Routing Service state interface with real SQLite migration, Apply, restart, and receipt behavior;
3. authenticated real loopback Codex and Claude routes with deterministic upstreams;
4. TargetSession through the real OpenTUI renderer/keymap; and
5. a real-process walking tracer using real UDS, SQLite, listeners, renderer, and deterministic upstreams.

Tests prove:

- draft-only editing, strict membership validation, immutable Apply, references, replay, and target isolation;
- activation creates a one-member plan and concurrent requests remain pinned to old epochs;
- priority-first non-sticky routing and each retryable/nonretryable classification;
- Responses and Messages semantic commitment, SSE order, cancellation, and no post-commit failover;
- Current remains unchanged while Serving identifies fallback;
- health/circuit thresholds, half-open concurrency, stale restart projection, and epoch-reset eligibility;
- Reachability and declaration operations leave health unchanged;
- response-before-push, no replay push, reconnect, sequence gaps, and multi-session races;
- scan-first diagnostics with controlled credential, configuration, backend, request, and settings sentinels;
- TUI draft/apply/status/focus/localization/responsive behavior; and
- an end-to-end two-Target tracer with Apply, failure, fallback, failback, concurrent plan switch, restart-stale health, and natural service exit.
