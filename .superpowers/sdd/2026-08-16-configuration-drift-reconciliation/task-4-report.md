# Task 4 implementation report — apply target reconciliation

## Outcome

Implemented receipt-first Adopt, Reapply, and zero-in-flight Restore through the existing reconciliation coordinator and target-native adapters.

- Adopt creates a new ordinary Provider, creates a new Credential Reference only when the observed secret differs, creates a new immutable Activated Snapshot and recovery binding, and leaves all historical rows unchanged.
- Adopt from active Takeover rejects the recursive Muxvia local route before intent, requires an atomic zero-in-flight runtime reservation, adopts an external upstream as managed Direct, clears the applied route credential, and stops only that Target listener. Direct Adopt remains Direct.
- Reapply preserves Provider/Snapshot/Recovery Intent identity, restores committed owned fields, preserves the latest unrelated configuration, and refreshes the committed recovery expectation so restart does not classify the preserved unrelated state as drift.
- Restore requires zero active requests, exact-restores the versioned before payload, exits managed state, stops only the affected Target listener, clears current/applied projections, and retains Provider/Credential/Snapshot history.
- Exact unknown-compatible acknowledgement is required by reconciliation apply and managed ordinary writes. Incompatible reconciliation never writes; live incompatible compatibility also blocks Provider mutations on unmanaged Targets. Managed ordinary save/activation actions re-observe the live target inside the shared mutation gate and are blocked target-locally by fresh drift or incompatible compatibility, while the peer Target and open, preview, discovery, and reachability remain available.
- Receipt replay precedes revision/token/probe/work, consumes no token, performs no file/runtime work, and emits no duplicate Target View.

## RED evidence

- Real UDS Reapply initially returned `error` where the test required `response`, proving the missing Reconcile dispatch boundary.
- A controlled rollback mutation made `exact_rollback` report success without restoring observed bytes; the fixed diagnostic test failed with `rollback did not restore the exact observed bytes` before the implementation was restored.
- The write-gate real UDS test initially hit the unsupported path; a subsequent aggregate run exposed a regression where probing every ordinary write stalled an unmanaged control path. The gate was narrowed to already-managed Targets, retaining the existing unmanaged save/activation preflight ordering.
- The ListenerStop test initially used a Direct fixture and entered Recovery Required before the listener seam. It was corrected to a real Takeover listener and then proved exact rollback and listener identity preservation.
- The shared-gate concurrency RED showed same-Target activation could enter while reconciliation was paused after its file write. Sharing the exact same gate pair and adding an explicit already-held activation path made same-Target save and activation wait while the peer Target still completed; releasing the pause then proved there was no stale overwrite or partial state.
- The live-observation RED showed an external edit after the last persisted observation could reach an ordinary write. Re-observing under the shared gate made the first ordinary write pin and reject the drift with zero mutation, while peer writes and read-only operations remained available.
- The idle-reservation RED admitted a real request after Restore's active-count check. ModelServer now atomically disables admission before checking active requests and reserving the handle; the race receives 503 and cannot enter the upstream, while a busy reservation restores admission without stopping the service.
- The active-Takeover Adopt RED exposed the previously inconsistent retained Takeover state. The final tests prove external upstream Takeover→Direct, recursive local-route rejection before intent, busy zero mutation, peer isolation, and runtime reservation restoration on transaction abort.
- The active-Takeover preview RED exposed `takeover: unchanged` even though apply exits Takeover. Preview now discloses `takeover: absent`; `restartRequired` remains false by the approved binding because Adopt does not write the file, while the preview retains its unobservable runtime-boundary disclosure.
- The unmanaged incompatible-write RED received an ordinary success with no problem. The control boundary now probes only successfully parsed unmanaged Provider mutations; it rejects incompatible CLI state without disturbing receipt-first, malformed-action, or ActivationService preflight ordering.
- The fresh-drift write-gate RED returned a clean authoritative View. The first live mismatch now durably pins only that Target's `configuration-drift`, returns the updated authoritative View, and publishes it once after the error response.
- A final ordering RED showed a malformed unmanaged Claude action returning `preflight-context-required`. The unmanaged non-Provider path now returns to ActivationService before the managed-only context requirement and produces the legacy parse-first `invalid-provider` result.
- CredentialInsert, ProviderInsert, SnapshotInsert, FinalRevision, and FinalTransaction are transaction-internal failpoints. Each executes its preceding SQL statement and then returns an error before commit, proving real SQLite transaction rollback rather than a coordinator-level simulated failure.

## Ordering and recovery evidence

Apply ordering is receipt → target/key serialization → exact token and full re-observation → revision/compatibility/shadow/busy gates → durable pending intent → adapter atomic apply/verify → one IMMEDIATE state transaction → response → one publication.

The failpoint matrix covers AfterIntent, AtomicWrite, Verify, CredentialInsert, ProviderInsert, SnapshotInsert, FinalRevision, ListenerStop, FinalTransaction, and RollbackVerify:

- normal post-intent failures exact-restore the observed native bytes, mark the intent rolled back, leave no receipt or publication, and consume no token;
- every database boundary leaves Credential/Provider/Snapshot/Recovery/Receipt row counts and the complete authoritative Target View unchanged;
- ListenerStop failure leaves the real affected listener endpoint installed and live;
- RollbackVerify failure marks only the affected Target Recovery Required and writes a terminal replay-consistent receipt;
- committed listener shutdown failure rewrites the committed receipt outcome coherently to Recovery Required.

All fallible Adopt/Reapply commit material is prepared before the intent and native write. Every failure after intent, including reservation/listener/stale-state branches, uses verified exact rollback; rollback failure returns the same persisted Recovery Required outcome that receipt-first replay returns.

The held-request test increments the active counter after authenticated Snapshot acquisition and holds it through the streamed response body. Restore returns `target-busy` with zero mutation, and the held request completes against its originally pinned Snapshot.

## Scope and interface notes

The approved plan-boundary expansions were necessary and remain crate-private:

- `PreparedConfiguration` carries the target-native observed snapshot and owns apply/verify/exact-rollback; it has no wire serialization or secret-bearing Debug surface.
- `ActivationService` exposes only a narrow reconciliation runtime projection sharing the existing `Arc<Mutex<Option<ModelServerHandle>>>` slots. The coordinator uses active-count/reserve/restore/shutdown operations and never calls activation apply logic.
- `ActivationService` and `ReconciliationService` share the same crate-private pair of `Arc<Mutex<()>>` Target mutation gates. Ordinary managed writes and reconciliation therefore hold one Target-local gate across live preflight, file mutation, and state transaction; peer Targets remain independent. The explicit already-held activation path avoids recursive locking.
- `ModelServer` adds a crate-private atomic admission reservation used only for zero-in-flight Restore and active-Takeover Adopt. It does not expose protocol or business orchestration through the runtime seam.
- `control/server.rs` dispatches only `TargetAction::Reconcile` to the coordinator, performs receipt-first replay before Claude session-context requirements, and performs managed ordinary-write live authorization under the shared gate; all other ordinary actions retain the activation/state path.
- `control/server.rs` classifies only already-successfully-parsed Provider mutations for the unmanaged compatibility gate. Activation and malformed actions keep their original ActivationService validation/probe ordering. The affected control-socket fixtures use deterministic tested probes so the new live compatibility boundary is exercised without depending on host CLI installation.
- Codex and Claude codecs expose only the crate-private target-native atomic apply/verify/restore operations required by the adapter.

The repository currently has no synchronization action or ordinary restore action in its closed wire protocol. Task 4 therefore tests every reachable ordinary write command (Provider mutations and activation) and records the absent operations here rather than inventing new APIs.

## Verification

```text
cargo test -p muxvia-routing service::reconcile::tests:: -- --test-threads=1
24 passed; 0 failed

cargo test -p muxvia-routing service::reconciliation_adapter::tests:: -- --test-threads=1
11 passed; 0 failed

cargo test -p muxvia-routing --test reconciliation --test activation \
  --test control_socket --test process_lifecycle --test recovery -- --test-threads=1
156 passed; 0 failed
  activation 65; control_socket 44; process_lifecycle 13;
  reconciliation 9; recovery 25

cargo fmt --all -- --check
passed

cargo clippy --workspace --all-targets -- -D warnings
passed

git diff --check
passed
```

Real UDS/loopback suites used the already-approved local escalation because sandbox socket binding returned `Operation not permitted`.

## Security

Raw UDS responses are scanned before JSON parsing for literal and compact numeric-byte sentinel forms. Credential, model, base URL, unrelated configuration, and probe stdout/stderr values are absent from diagnostics. Prepared native snapshots are neither serialized nor printed, and stable failure codes/messages contain no raw target configuration.

## Review adjudication

The final scoped review confirmed all original concurrency, replay, rollback, authoritative-View, and runtime findings closed, then confirmed the final unmanaged malformed-action ordering regression closed. Two suggestions were intentionally not applied:

- active-Takeover Adopt keeps `restartRequired: false` under the parent's explicit binding because Adopt itself does not write the observed file; the preview now accurately discloses Takeover exit and retains `unobservableRuntimeBoundary: true`;
- failed exact rollback returns the persisted Applied outcome whose View is Recovery Required, matching the existing Activation post-commit recovery contract and receipt-first replay. Returning an initial error while replay returns an outcome would violate the approved replay-consistency requirement.

## Commit

`a5dd084` — `feat: reconcile target configuration`

No push performed.

## Concerns

None blocking. Synchronization and ordinary restore are not implemented product operations in this repository; no speculative protocol surface was added.

## Formal review fix round 1 — 2026-08-16

All six formal-review findings were independently reproduced before production edits and closed with RED→GREEN tests.

1. Claude Restore now resets `managed_config_version` to unmanaged v1 in the same IMMEDIATE transaction that clears the managed projection. A real Claude v2 Takeover→Restore→server/store close→reopen/bootstrap test proves clean unmanaged startup and complete Codex peer isolation.
2. Adopt compares the observed secret with the authoritative Target Routing Credential using the same fixed-width constant-time credential discipline before intent. Codex and Claude tests change the observed endpoint while retaining the live route token and prove fixed secret-free rejection with no file, View, history, receipt, or runtime mutation.
3. Model admission is one packed atomic word: bit 0 is the idle reservation and active requests increment by two. Busy reservation is a failed `0→reserved` CAS with no transient admission change. Reservation occurs after every read-only/fallible pre-intent gate and before the intent/file, and remains held until commit or verified rollback/Recovery Required. Event-driven loopback races prove the old window and transient rejection are closed with exact DB/file/runtime fingerprints.
4. Control startup now reads typed pending reconciliation intents before binding UDS. Target-native before/desired material either already matches before or exact-restores desired to before and verifies it before marking rolled back. Corrupt, ambiguous, or failed restoration marks only that Target Recovery Required and creates the same durable outcome returned by receipt-first replay. Codex and Claude, crash-after-intent and crash-after-write, restart and same-action retry are covered without duplicate Provider history. Pending reconciliation also keeps the process lifecycle required until startup resolves it.
5. Reconciliation and live ordinary-drift detection return a crate-private deferred publication; neither publishes from the service layer. The Control Server attaches a writer acknowledgement to the initiating action response/error, then publishes exactly once only after the real frame write succeeds. Replay and already-persisted drift carry no publication. Dual real UDS sessions with writer backpressure prove success ordering, durable error ordering, and writer-timeout suppression while a new OpenTarget still observes the committed authoritative state.
6. Codex Adopt reads the actual top-level `model_provider` and its selected table, never a stale `muxvia_codex` table. It validates trimmed nonempty name/model/base URL/credential, `wire_api = "responses"`, `supports_websockets = false`, and one representable authentication header before intent. A valid custom selected provider creates the matching ordinary Provider/Snapshot; selector/table/wire/websocket/type/URL/auth mutations reject pre-intent. The selected table is only an ephemeral Adopt candidate: the committed desired/recovery payload binds an explicit stable ownership key for the entire managed epoch. Reapply, activation inspection/write, bootstrap, startup recovery, and Restore use that authoritative bound key and never infer ownership from a later selector edit.

The final review found and closed three additional issues before commit:

- Codex managed ownership and current selected-provider extraction are now separate. Selector mutation no longer makes Reapply delete an external table or makes Adopt→Restore lose unrelated stale provider tables. Target-native tests cover selector→external Reapply/verify/exact rollback and Adopt external→Restore/verify/exact rollback.
- `PendingReconciliationIntent` has a manual redacted `Debug`; before/desired native JSON cannot appear as literal, byte-array, numeric, or JSON diagnostics.
- Codex whitespace-only model/name/base/credential values are unrepresentable and reject before intent.

The stable-key change required the approved minimal ActivationService preparation expansion. Bound Codex recovery payloads now supply the committed ownership key for ordinary activation inspection, candidate writes, and takeover bootstrap; legacy unbound v1 Codex rows retain the historical `muxvia_codex` fallback. A real UDS sequence covers Adopt external → Control Server restart/OpenTarget → ordinary Takeover activation using the external key → StateStore/ActivationService restart → exact listener/bootstrap and file preservation. A pending cross-key activation crash-before-write test proves startup recognizes the versioned before payload and marks the intent rolled back rather than Recovery Required.

### Round 1 scope notes

- `model/server.rs` and `model/messages.rs` share the single atomic admission word for both Codex and Claude request paths.
- `control/server.rs` owns deferred publication at its existing UDS writer boundary; `tests/control_socket.rs` supplies the required two-session real writer-ordering evidence.
- `state/reconciliation.rs` owns typed pending reconciliation rows and terminal recovery receipts; `state/store.rs` includes pending reconciliation in lifecycle demand.
- Codex selected-provider parsing remains target-native in `codex/config.rs`; the coordinator does not duplicate TOML semantics.
- All additions are crate-private or tests. No control wire shape, public business API, or speculative sync/ordinary-restore operation was introduced.

### Round 1 verification

```text
cargo test -p muxvia-routing -- --test-threads=1
passed: all unit, integration, process, UDS, loopback, and doc tests
  lib 72 passed / 1 helper ignored
  activation 65/65; claude_config 21/21; claude_model_route 18/18
  codex_config 33/33 (+1 helper ignored); control_socket 47/47; model_route 14/14
  process_lifecycle 13/13; reconciliation 11/11; recovery 25/25
  state_store 32/32; remaining suites all passed

cargo fmt --all -- --check
passed

cargo clippy --workspace --all-targets -- -D warnings
passed

git diff --check
passed

rg -n "publish_target_view" crates/routing-service/src/service/reconcile.rs
no matches
```

The full real UDS/loopback gate used the already-approved escalation because sandbox binding returns `Operation not permitted`.

### Round 1 commit

Pending separate local commit. No push will be performed.

### Round 1 independent review

Final scoped review: **0 blocking findings**. The reviewer independently reran reconciliation 11/11, activation 65/65, recovery 25/25, formatting, clippy with warnings denied, and diff checks. One nonblocking follow-up remains: the three routing-credential comparison entry points repeat the same fixed-width normalize/shape/constant-time equality sequence; a later refactor could centralize that byte comparator to reduce security-rule drift. It was not changed here because it is not required for Task 4 correctness.

## Formal review fix round 2 — 2026-08-17

All three findings were independently verified against `bf6c25c57b06fe864fb28fe4ce038f7a623a6fdd` before production edits and closed with strict RED→GREEN evidence.

1. Target View publication is monotonic at the shared `StateStore` publisher. A synchronous per-Target sequence guard makes compare, sequence advance, and broadcast one atomic critical section and suppresses duplicate or older candidates. The Target mutation gate is released before response writer acknowledgement. Two real UDS sessions prove that a delayed old Reapply acknowledgement cannot publish after a newer same-Target action and a delayed live-drift error cannot publish after a newer Reapply; the old initiating response remains valid, subscribers never regress, and receipt replay publishes nothing.
2. Codex Adopt recovery now stores a versioned, bounded `CodexProviderRestoreState` for the later-selected provider key. It contains only that key and the five approved provider-owned fields or explicit absence; it has a manual redacted `Debug`, uses `serde(default)` through the containing snapshot, and never stores a raw TOML document. Restore overwrites the historical managed partition plus the adopted provider partition while preserving other current unrelated semantics. Legacy payloads deterministically derive the bounded state from their semantic unrelated projection and reject unrepresentable shapes before intent. The pending reconciliation journal uses a tagged typed union for crash recovery.
3. `X-Muxvia-Routing-Credential` is no longer representable as an ordinary Codex Provider credential, regardless of its value. Selected-provider extraction accepts only one nonempty `Authorization = "Bearer ..."` field; routing-header-shaped candidates return the fixed `invalid-provider-credential` code before intent. The existing constant-time comparison remains defense in depth for a live Routing Credential placed inside Authorization. Real UDS tests cover both the current route token and a different stale routing token with zero View/file/history/runtime mutation and literal/numeric raw-frame scans.

### Recovery partition and exact rollback

The typed provider-before tests exposed two additional correctness requirements on the same approved seam:

- ordinary Codex activation must keep its current before/desired pair as the pending crash journal while the same commit atomically binds the managed epoch's historical recovery-before plus the new desired payload. This preserves Adopt's pre-Muxvia Restore boundary across restart and later activation without weakening pending-write rollback;
- typed `Absent` means all five provider-owned fields are absent, even when the provider table remains for unrelated fields or decorations.

Same-process post-intent rollback uses a `CodexObservedDocument` held only by `PreparedConfiguration`. It is non-serializable and has a fixed redacted `Debug`; after verifying the applied union, `ManagedFile` CAS-restores its exact bytes and original mode. Durable startup recovery continues to use only the bounded typed semantic payload. A real sequence covers pre-Muxvia same-key fixture → Takeover → external Adopt → Control Server restart/OpenTarget → ordinary Takeover activation → StateStore/ActivationService restart → drift Restore, with clean unmanaged state, no Recovery Required, historical owned semantics and decorations, current unrelated values, and mode `0640`. Focused tests also cover later-selected provider originally absent, legacy untyped payload recovery, pending-after-write recovery, and listener-stop exact byte/mode rollback.

The bounded provider-before allowlist is also the reversibility and secret-location boundary, not an incomplete target-native parser. A historical selected-provider partition using `wire_api = "chat"`, websocket support, or custom headers cannot be encoded in the approved ordinary Provider recovery shape, so Adopt rejects it pre-intent with fixed `invalid-configuration`. A matrix including unrelated secret sentinels proves exact file bytes/mode preservation and literal/numeric diagnostic redaction. The implementation deliberately does not persist arbitrary TOML values, comments, or custom-header secrets in recovery JSON.

Historical and pending file modes are versioned separately. The normal Restore union uses the historical recovery-before mode (`0640` in the roundtrip fixture), while startup rollback of a pending Restore uses that intent's immediate before mode (`0600`). The focused sequence proves `0640 → externally changed 0600 → Restore 0640 → pending recovery 0600` without using raw durable material.

Restore also retains historical file absence. After the provider union is applied, an absent historical file is removed only when the resulting semantic TOML table is empty; a sibling fixture proves that current unrelated configuration instead keeps the file with only that unrelated content. Same-process exact rollback still reinstates the observed external bytes.

### Round 2 secret/storage audit

- `CodexProviderRestoreState`, `CodexObservedDocument`, `PreparedConfiguration`, and `RecoveryPayload` diagnostics expose no values; literal and numeric-byte sentinels are scanned.
- The raw observed document exists only in memory during one prepared action and is not `Serialize`, wire-visible, or stored in SQLite.
- The serialized and committed `activation_recovery.payload_json` allow only the historical bounded provider-owned state and the adopted desired state. A current-only unrelated secret sentinel is proven absent from both the serialized payload and the committed SQLite payload JSON.
- A differently valued `X-Muxvia-Routing-Credential` is rejected before any Provider, Credential Reference, Activated Snapshot, receipt, or runtime can retain or send it.

### Round 2 scope notes

- `state/store.rs` owns both the shared monotonic publisher guard and the atomic pending-to-committed recovery payload replacement.
- `service/activate.rs` carries separate pending rollback and committed historical recovery material without exposing reconciliation through the ActivationService business API.
- `config/managed_file.rs` adds a crate-private mode-source replacement seam used only for ephemeral exact rollback.
- `codex/config.rs` remains the sole owner of selected-provider parsing, typed provider partitions, union apply/verify/recovery, and exact target-native rollback.
- `tests/control_socket.rs` and `tests/reconciliation.rs` provide the real two-session and full restart roundtrip evidence. No wire shape or public API changed.

### Round 2 verification

```text
cargo test -p muxvia-routing -- --test-threads=1
passed: complete package, including all unit/integration/process/UDS/loopback/doc tests
  lib 75 passed / 1 helper ignored
  activation 65/65; claude_config 21/21; claude_model_route 18/18
  codex_config 33/33 (+1 helper ignored); control_socket 47/47; model_route 14/14
  process_lifecycle 13/13; reconciliation 11/11; recovery 25/25
  state_store 32/32; remaining suites all passed

cargo test -p muxvia-routing --lib service::reconcile::tests -- --test-threads=1
26/26 passed

cargo test -p muxvia-routing --lib service::reconciliation_adapter::tests -- --test-threads=1
19/19 passed

cargo fmt --all -- --check
passed

cargo clippy -p muxvia-routing --all-targets -- -D warnings
passed

git diff --check
passed
```

The real UDS/loopback gates used the already-approved escalation because sandbox binding returns `Operation not permitted`.

### Round 2 commit

Pending separate local commit. No push will be performed.

## Formal review fix round 3 — 2026-08-17

All three Important findings were verified against `ab029f0da4d2f05a9ce0bce5db3054b180e3e953` and closed one vertical RED→GREEN slice at a time.

1. Target View publication now serializes asynchronously per Target and compares each deferred candidate with the latest durable `TargetView` before broadcasting. A candidate is eligible only when its `view_sequence` exactly equals the authoritative durable sequence and is newer than the last successful broadcast. The SQLite projection read completes before the synchronous broadcast; no database connection guard or mutation gate is held across response acknowledgement. A two-session real UDS race proves that A=N with a blocked response, followed by B=N+1 whose writer disconnects without publication, cannot let A's later acknowledgement publish stale N; a new OpenTarget reads N+1. The existing successful N+1 and live-drift→Reapply races remain green, and replay still publishes nothing.
2. `OwnedCodexState` uses its explicit serialized/captured `owned_provider_key` independently of the observed top-level `model_provider` value. The selector remains an owned field whose absence or non-string value is drift, but it cannot erase the committed provider-table partition. Tests delete the selector and mutate it to an integer while the bound table remains, then cover Reapply/verify, exact rollback, serialized pending startup recovery, and legacy payload defaulting. The ownership key and credentials remain redacted from diagnostics.
3. Pending Codex Restore classification now includes file existence and permission mode, but deliberately excludes inode identity. The durable tagged Restore union carries a bounded, serde-default installed file state (`exists` plus `mode`) computed from the already observed document and historical mode before intent. Startup accepts exact pending-before, or exact applied desired semantics/existence/mode before restoring pending-before. Equal semantics with `0600` pending-before versus `0640` applied desired is rolled back to `0600`; an unexpected `0644` is a third state and remains untouched with Recovery Required. Crash-after-write with an absent immediate before removes the reconstructed file instead of retaining an empty shell, while crash-before-write recognizes absence directly. Unrelated sibling semantics remain preserved. Legacy round-2 pending unions without the new optional field still deserialize through the compatibility path.

### Round 3 scope notes

- `state/store.rs` owns the authoritative per-Target publication serialization; callers only await the same shared publisher.
- `codex/config.rs` remains the sole owner of bound target-native partitions and installed file-state planning/verification.
- `service/reconciliation_adapter.rs` carries the bounded installed state only in the existing typed durable Restore union; it adds no wire field or raw document persistence.
- `tests/control_socket.rs` supplies the real writer-failure race. No public API or normal write behavior changed.

### Round 3 verification

```text
cargo test -p muxvia-routing -- --test-threads=1
passed: complete package, including all unit/integration/process/UDS/loopback/doc tests
  lib 79 passed / 1 helper ignored
  activation 65/65; claude_config 21/21; claude_model_route 18/18
  codex_config 33/33 (+1 helper ignored); control_socket 48/48; model_route 14/14
  process_lifecycle 13/13; reconciliation 11/11; recovery 25/25
  state_store 32/32; remaining suites all passed

cargo test -p muxvia-routing --lib service::reconciliation_adapter::tests -- --test-threads=1
22/22 passed

cargo test -p muxvia-routing --test control_socket -- --test-threads=1
48/48 passed

cargo test -p muxvia-routing --test reconciliation -- --test-threads=1
11/11 passed

cargo test -p muxvia-routing --test recovery -- --test-threads=1
25/25 passed

cargo test -p muxvia-routing --test codex_config -- --test-threads=1
33 passed / 1 helper ignored

cargo fmt --all -- --check
passed

cargo clippy -p muxvia-routing --all-targets -- -D warnings
passed

git diff --check
passed
```

The real UDS/loopback gates used the already-approved escalation because sandbox binding returns `Operation not permitted`.

### Round 3 commit

Pending separate local commit. No push will be performed.

## Formal review fix round 4 — 2026-08-17

All three Important findings and the typed-state Minor were verified against `019cd930610a31577399a27729a789a666782440` before production edits and closed with strict RED→GREEN evidence.

1. Pending Codex Restore startup recovery now validates and mutates one captured parsed document, `FileIdentity`, and installed mode. Classification, union patching, and `ManagedFile` CAS all use that exact capture; no second read can silently adopt a semantic or chmod race as the new expected state. An event hook after classification proves that both an external semantic edit and a mode-only edit make the CAS fail closed while preserving the edit. Temporarily restoring the old validation-then-second-read sequence made the semantic branch RED by overwriting the external edit; the single-capture implementation is GREEN.
2. Deferred Target View publication now performs the authoritative sequence read, last-successful-broadcast comparison, and synchronous nonblocking broadcast in one `tokio_rusqlite::Connection::call` worker closure. This is the shared durable mutation serialization boundary for control commits and model-path `record_serving(_for)` commits, so no view-sequence mutation can interleave between authoritative validation and send. No target mutation gate is held across response acknowledgement or asynchronous subscriber delivery. A deterministic hook after the authoritative read proves that a queued model-path durable mutation cannot commit before the old candidate's synchronous send; temporarily moving the read outside the SQLite worker makes the test RED.
3. A round-2 Restore union payload that omits `installed_file_state` remains serde-readable but is never treated as a wildcard. Because no authoritative bounded legacy field can recover exact existence and mode, startup fails closed with the fixed `recovery-required` result and leaves the file untouched. The controlled legacy mutation previously rolled an equal-semantics `0640` file back to `0600`; it now preserves `0640` with no write.
4. Restore preparation now uses one `Option<CodexRestorePreparation>` containing both the bounded provider state and installed file state. Its manual `Debug` is redacted, and the impossible half-populated state and associated `expect` are gone. Durable JSON retains the round-2 `provider_restore` plus optional `installed_file_state` shape for deserialization compatibility; missing installed state follows the fail-closed rule above.

### Round 4 scope and lock audit

- `codex/config.rs` owns the single captured target-native Restore classification/patch/CAS seam and a test-only post-validation event hook.
- `state/store.rs` owns publication eligibility inside the same single SQLite worker used by every durable Target mutation, including model serving-state updates. Its only additional lock is the per-Target synchronous last-broadcast sequence guard, always acquired inside the worker and never acquired before a database call elsewhere, so there is no reverse lock ordering.
- `service/reconciliation_adapter.rs` owns the typed all-or-none Restore preparation and fail-closed legacy durable-material classification.
- `service/reconcile.rs` contains the deterministic model-path publication race regression. No wire field, public API, target mutation-gate lifetime, or raw durable configuration payload was added.

### Round 4 verification

```text
cargo test -p muxvia-routing -- --test-threads=1
passed: complete package, including all unit/integration/process/UDS/loopback/doc tests
  lib 81 passed / 1 helper ignored
  activation 65/65; claude_config 21 passed / 1 helper ignored
  claude_model_route 18/18; codex_config 33 passed / 1 helper ignored
  control_socket 48/48; model_route 14/14; process_lifecycle 13/13
  reconciliation 11/11; recovery 25/25; state_store 32/32
  remaining suites and doc tests all passed

cargo test -p muxvia-routing --lib service::reconcile::tests::authoritative_publication_serializes_model_path_mutation_until_sync_send -- --exact --nocapture
1/1 passed

cargo test -p muxvia-routing --lib service::reconciliation_adapter::tests::pending_codex_restore_cas_preserves_semantic_and_mode_races -- --exact --nocapture
1/1 passed

cargo fmt --all -- --check
passed

cargo clippy -p muxvia-routing --all-targets -- -D warnings
passed

git diff --check
passed
```

The full real UDS/loopback gate used the already-approved escalation because sandbox binding returns `Operation not permitted`.

### Round 4 independent review

Final incremental verdict: **0 Critical / 0 Important / 0 Minor; Spec passed and Quality approved**. The reviewer independently checked the one-capture Restore classification/patch/CAS seam, fail-closed legacy payload path, typed redacted preparation, SQLite-worker publication linearization including `record_serving_for`, lock ordering, and mutation-sensitive race tests. Both focused round-4 tests and `git diff --check` passed in the read-only review.

### Round 4 commit

Pending separate local commit. No push will be performed.
