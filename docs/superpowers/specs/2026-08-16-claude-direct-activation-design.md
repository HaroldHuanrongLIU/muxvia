# Claude Code Direct Activation Design

**Issue:** [#7 — T06 Claude Code Direct Activation](https://github.com/HaroldHuanrongLIU/muxvia/issues/7)

**Status:** Approved design. The Operator selected support for both explicit Claude authentication profiles.

## Goal

Let the Operator explicitly apply one complete, Direct-compatible Claude Target Provider to the default Claude Configuration Home. The operation writes only approved Claude settings, commits the Current Target Provider and immutable Activated Snapshot only after verified configuration success, restores exact prior values or absence on failure, starts no model route, and tells the Operator to restart existing Claude Code processes.

## Scope

T06 adds Direct Activation to the real Claude Target context delivered by T05. It includes:

- Claude Direct desired-state construction for `anthropic-api-key` and `anthropic-bearer` Providers;
- typed recognition of committed Claude `Unmanaged`, `Direct`, and `Takeover` states;
- the existing receipt-first, recovery-intent, atomic-write, reread-verification, final-revision, immutable-snapshot, rollback, and publication contract;
- exact replacement and restoration of the approved Claude settings while preserving unrelated JSON and file mode;
- named Direct commands and Provider-picker behavior in the Claude Control Plane context; and
- real-process proof that Direct remains control-only and secret-safe.

T06 does not remove an active Target Takeover, add Configuration Drift repair actions, change Route Health, launch an interactive Claude inference session, add Universal Providers or Failover Chains, or broadly refactor the activation coordinator or Control Plane shell.

## Chosen approach

Extend the existing Claude configuration adapter and shared `ActivationService` transaction pipeline. The target-specific adapter owns Claude JSON semantics; the shared activation module retains ordering, receipts, immutable snapshots, recovery, final commits, and view publication.

Two alternatives are rejected:

1. A Claude Direct-specific coordinator would duplicate the security-sensitive receipt, recovery, rollback, revision, and publication behavior already shared by Codex Direct and both Takeover paths.
2. A new generic target-configuration trait would add a third interface over only two existing adapters, expand the regression surface, and provide no new caller leverage for T06.

The chosen design deepens the existing Target Configuration seam: callers continue to request `activate-provider` with an explicit target and mode, while JSON ownership and state recognition remain behind the Claude adapter.

## Managed Claude configuration

Claude Direct manages only these `~/.claude/settings.json` paths:

- `env.ANTHROPIC_BASE_URL`;
- `env.ANTHROPIC_MODEL`;
- `env.ANTHROPIC_AUTH_TOKEN`; and
- `env.ANTHROPIC_API_KEY`.

The desired authentication state follows the Provider's explicit stored profile:

| Provider authentication | Desired credential state |
| --- | --- |
| `anthropic-bearer` | Set `ANTHROPIC_AUTH_TOKEN` to the Provider credential and remove `ANTHROPIC_API_KEY` |
| `anthropic-api-key` | Set `ANTHROPIC_API_KEY` to the Provider credential and remove `ANTHROPIC_AUTH_TOKEN` |

Muxvia deliberately manages both credential members during Direct Activation. This avoids depending on undocumented precedence when both variables are present and makes a profile switch deterministic. Each prior value or prior absence is captured and restored independently.

Takeover continues to set `ANTHROPIC_AUTH_TOKEN` to the Routing Credential and now explicitly removes `ANTHROPIC_API_KEY`. This is required for an exact Direct-to-Takeover transition from an API-key Direct state. It does not make `ANTHROPIC_API_KEY` a general Muxvia-owned setting outside an authorized activation transaction.

Muxvia never owns the complete `env` object, top-level `model`, Provider selectors, unrelated settings, `~/.claude.json`, Claude application authorization state, or OS credential stores. JSON formatting is not preserved, but unrelated JSON semantics, current file mode, and the exact prior approved-field values or absence are preserved.

The existing Managed File seam continues to canonicalize a Configuration Home directory symlink and reject a symlinked `settings.json`, unsafe file-identity changes, non-regular targets, and non-durable replacement or rollback states.

## Typed managed state and ownership

The Claude adapter exposes an internal typed observation:

- `Unmanaged` — no caller-authorized committed state matches;
- `Direct` — the file exactly matches a caller-supplied Direct desired state; or
- `Takeover` — the file exactly matches a caller-supplied Takeover desired state.

The caller must construct the expected state from persisted Target Route State and the immutable Activated Snapshot. The adapter never infers Muxvia ownership merely because a user-authored file happens to contain matching values.

Persisted state is interpreted as follows:

- no Activated Snapshot and no route runtime: unmanaged;
- Activated Snapshot and no route runtime: Direct;
- Activated Snapshot and route runtime: Takeover;
- route runtime without a Snapshot, or another inconsistent combination: Recovery Required.

When a prior Direct or Takeover exists, activation verifies the current approved fields against the desired state reconstructed from that committed Snapshot. A mismatch is Configuration Drift and returns `recovery-required` before a new Recovery Intent or write. A first activation may capture pre-existing approved values as the before state but cannot adopt them as an already-managed state.

## Validation and fail-closed behavior

Claude Direct uses the same target-scoped preparation as Takeover and adds no listener work. Before a Recovery Intent or filesystem mutation, it requires:

- a receipt miss followed by a valid positive expected management revision;
- an existing complete Claude Provider with `anthropic-messages` protocol;
- explicit `anthropic-api-key` or `anthropic-bearer` authentication and a credential;
- `routingRequirement: direct-compatible`;
- no active Target Takeover;
- the default canonical Claude Configuration Home;
- a complete, internally consistent Claude preflight context;
- all five cloud Provider selectors disabled or unset and host-managed mode inactive;
- no observable managed, shared-project, or local-project shadow of an approved field;
- a tested or unknown-compatible Target Compatibility Probe result; and
- a safe, valid, non-drifted managed file.

Routing-required Providers return `takeover-required`. An active Takeover returns `takeover-active`. Nondefault home, managed-file symlink, shadowing, provider mode, invalid JSON shape, Configuration Drift, and incompatible capability failures retain stable actionable codes and perform no intent, file, snapshot, credential, listener, database, receipt, or publication work.

The six currently documented Claude blockers remain authoritative: Bedrock, Vertex, Foundry, Mantle, Anthropic AWS, and host-managed provider mode. T06 does not clear or take ownership of any blocker.

## Activation transaction

Claude Direct remains serialized by the Claude activation gate and follows the existing transaction:

1. Look up and replay an existing target-scoped receipt before parsing or validation.
2. Acquire the Claude activation gate and recheck the receipt.
3. Validate revision, Provider, routing requirement, no active Takeover, context, capability, shadows, file safety, and committed managed state.
4. Construct the Direct desired state and immutable Activated Snapshot from the saved Provider and Credential Reference.
5. Persist a pending Claude Recovery Intent containing the exact before snapshot, desired approved fields, file identity, action ID, and expected revision.
6. Atomically merge the approved fields into `settings.json` and reread the file to verify desired fields plus unchanged unrelated semantics.
7. In one immediate SQLite transaction, recheck the receipt, management revision, and managed-write status; commit Current, immutable Snapshot, Direct runtime, managed path, capability warning, committed recovery state, receipt, and complete Target View.
8. Publish the committed complete Target View exactly once.

The Direct branch never binds a listener, generates or reads a Routing Credential, starts a Model Server, or performs runtime handoff. A committed Direct state is control-only, so the Routing Service may exit after its final control session and pending action drain.

## Failure and recovery semantics

Any failure after the pending Recovery Intent uses the existing three-state rule:

- exact before: verify it and mark the intent rolled back;
- exact desired with unchanged unrelated semantics: restore the exact before values or absence and verify, then mark rolled back;
- any third or unverifiable state: retain the intent and mark only Claude Recovery Required.

A final revision race or SQLite failure restores the prior Direct or unmanaged state before returning. Direct-to-Direct and Direct-to-Takeover failures restore the previously committed Direct state, including the previous authentication profile and the exact absence of the other credential variable.

Startup reconciliation uses the same typed Claude Recovery Intent payload. A pending before or desired state is safely rolled back; a third state keeps Claude control-only. A clean committed Direct state starts no listener and does not keep the Routing Service alive by itself.

No credential, raw settings snapshot, recovery payload, or backend message may appear in Target Views, receipts, activities, logs, Debug output, renderer frames, or test diagnostics.

## Control Plane behavior

Claude gains the existing named Direct commands:

- `/direct` and `<leader>d` apply the Current Provider, or the first Provider if Current is absent;
- the Claude Provider picker exposes its selected-row Direct command separately from Takeover.

All dispatch goes through the centralized keymap. No global keyboard listener or target-specific parallel command path is added.

Incomplete Providers fail locally with stable guidance. A projected Routing-required Provider opens the existing Takeover-or-cancel confirmation. The Control Plane also handles an authoritative server-side `takeover-required` response through the same exact Provider confirmation path.

Pending state, outcome installation, activities, restart guidance, notices, selection, overlay tokens, and focus restoration remain target-keyed. Switching to Codex while Claude activation is pending cannot dispatch against Codex or install a Claude result into the Codex view. Every visible string uses the English and Simplified-Chinese catalogs, and backend message text is never rendered.

Successful Direct projects:

- `managedConfiguration.state: applied`;
- `managedConfiguration.mode: direct`;
- the canonical `settings.json` path;
- `restartRequired: true`;
- the committed Current Provider and Activated Snapshot; and
- no model endpoint, Routing Credential, or Serving Provider.

## Verification strategy

### Claude codec tests

- exact Bearer and API-key desired JSON, including removal of the inactive credential key;
- Direct-to-Direct and Direct-to-Takeover authentication-profile transitions;
- unrelated JSON semantics and existing mode preservation;
- restoration of every prior value and prior absence, including later unrelated edits;
- typed committed-state recognition and forged matching-file rejection;
- invalid JSON/root/env shapes, shadow sources, provider modes, directory/file symlinks, identity races, restrictive umask, and durable rollback; and
- fixed secret-free diagnostics under controlled mutation.

### Activation and recovery tests

- both authentication profiles commit Direct without a listener or Routing Credential;
- Current, Snapshot, receipt, recovery state, managed path, revision, and publication are atomic and target-scoped;
- receipt-first replay remains side-effect free;
- Direct-to-Direct and Direct-to-Takeover use the prior immutable Snapshot rather than the editable Provider declaration;
- active Takeover and Routing-required rejection happen before probe, intent, file, snapshot, or runtime work;
- nondefault home, all provider modes, shadowing, symlink, drift, incomplete Provider, incompatible capability, and stale revision do not write;
- every post-intent failpoint and final revision race restores exact before state; and
- restore failure enters Claude-only Recovery Required while Codex remains unchanged.

### OpenTUI tests

- Claude slash, leader, palette, and picker paths dispatch one exact Direct command identity;
- Current/first default and exact selected Provider behavior;
- pending gating, success, restart guidance, stable errors, Takeover confirmation, overlay identity, target switching, and focus restoration;
- English and Simplified-Chinese copy at extreme terminal sizes; and
- scan-first fixed diagnostics for every post-credential frame and action surface.

### Real-process tracer

A temporary HOME and Muxvia Home, real Routing Service process, real UDS/SQLite, OpenTUI test renderer, fake Claude probe, and real `settings.json` prove:

- Bearer Direct followed by API-key Direct replacement;
- exact approved-field semantics, unrelated settings and file-mode preservation, and immutable Snapshot behavior;
- no TCP model listener or Routing Credential;
- restart-required projection and idle service exit after Control Plane disconnect;
- failed activation restoration and Recovery Required behavior; and
- scan-first secret location checks across configuration, SQLite, recovery payloads, RPC frames, Target Views, receipts, renderer frames, process output, and test diagnostics.

## Out of scope

- Safe disabling or draining of an existing Target Takeover.
- Configuration Drift Adopt, Reapply, or Restore actions.
- Interactive Claude inference or hot-reload guarantees for an existing process.
- Universal Providers, Provider Synchronization, Failover Chains, Route Health transitions, Request Records, usage, import/export, backup, or distribution.
- Alternative credential helpers, custom headers, arbitrary Configuration Homes, or project/local settings writes.
- Broad extraction of the activation coordinator or target-keyed UI state shell.
