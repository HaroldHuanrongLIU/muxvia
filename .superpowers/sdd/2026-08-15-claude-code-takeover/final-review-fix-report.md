# Final review fix report

Base: `719ccdeb33a461bca3895a1d19b266ab6b07a3c7`

This was one surgical Standards/Spec review fix wave. Each finding was checked against the existing contracts before editing and was implemented one RED-to-GREEN slice at a time.

## 1. Target-local startup isolation

- Finding confirmed: reconciliation and committed-route bootstrap errors could abort the whole service even when only one Target was damaged.
- RED: dual-Target startup tests showed that an occupied/reconciliation-failed Claude route prevented the healthy Codex route and shared control socket from starting.
- GREEN: Target-local failures are persisted as stable Target problems, projected as control-only `unavailable` with no advertised endpoint, and skipped during bootstrap. The healthy peer resumes its exact committed route; explicit shutdown drains it. State-store/global errors remain process-fatal.
- Evidence: activation `41/41`, control socket `27/27`, recovery `19/19`, process lifecycle `12/12`.

## 2. Claude preflight context across gap refresh

- Finding confirmed: the renderer session did not retain the normalized Claude preflight context, and a gap-triggered `open-target` could replace the server context with `None`.
- RED: the Target-session gap test observed a refresh without `claudeContext`; the real UDS flow then lost the context needed for Takeover.
- GREEN: `TargetSessionImpl` retains and reattaches the context on every refresh, and the server preserves an already-opened context when a same-Target reopen omits it.
- Evidence: Target-session focused `1/1`; real UDS gap/reopen/Takeover `1/1`; full control socket `27/27`. The complete frame sequence is asserted secret-free.

## 3. Explicit draft discovery authentication

- Finding confirmed: a draft did not carry an authentication profile, so Claude draft discovery defaulted to API-key authentication even when the editor selected Bearer.
- RED: renderer and loopback tests observed the missing profile/default header behavior.
- GREEN: the wire contract requires `authentication` on draft discovery; the editor sends its current selection; the inspector uses that selection for ephemeral and saved credential references. Codex remains explicitly OpenAI Bearer.
- Evidence: provider inspection `14/14`, provider-inspection UI `10/10`, Rust protocol `15/15`, combined TypeScript protocol/session/socket tests GREEN. API-key and Bearer loopback tests assert the exact selected header, absence of the peer header, and the Anthropic version header.

## 4. Actionable, secret-safe known problems

- Finding confirmed: known provider-mode, shadow, and drift failures localized only the code and did not identify the blocking closed source/selector.
- RED: mutation-sensitive renderer/UDS assertions failed to distinguish the blocking source/selector without relying on backend prose.
- GREEN: `ControlProblem` projects only closed, optional `source` and `selector` values. Claude configuration inspection classifies the documented source and selector; the control server preserves those fields. English and zh-CN catalogs render stable known-code guidance and never render raw backend messages or credential-bearing paths.
- Evidence: Claude configuration `17/17`, localization `12/12`, app renderer `32/32`, real UDS structured projection test GREEN, protocol/schema tests GREEN.

## 5. Target-qualified Credential References

- Finding confirmed: the database foreign key referenced credential identity alone and therefore did not enforce same-Target ownership.
- RED: a direct SQL fixture could create a Claude Provider referencing a Codex Credential.
- GREEN: schema version 5 adds the minimal `(target, id)` credential uniqueness and composite Provider foreign key. The v4-to-v5 migration rebuilds only the two affected tables, copies existing rows, runs `foreign_key_check`, and preserves receipt projections.
- Evidence: fresh state `23/23`, provider declarations/migrations `11/11`, v1/v2 sequential migration coverage GREEN, direct cross-Target SQL corruption rejected.

## 6. Authentication/body-order proof and oversized-body reset diagnosis

- Finding confirmed: the old oversized-body test used Reqwest to upload the whole body while the server returned an early response. On macOS, the continuing client upload can race the early close and surface a connection reset instead of the already-produced response. This was a client-write race, not a retryable server failure.
- RED: the full-body Reqwest regression reproduced the platform reset; a raw TCP probe isolated the response timing by declaring a nonzero/oversized `Content-Length` and withholding the body.
- GREEN: the raw TCP regression proves invalid authentication returns the generic `401` before reading a declared body and an authenticated 32 MiB + 1 declaration returns the fixed `413` before reading a body, with zero upstream calls. The 32 MiB policy and streaming implementation are unchanged; no retry was added and no assertion was weakened.
- Evidence: Claude route `18/18`, including raw auth-before-body and declared-limit tests.

## Final verification

- `bun run verify`: GREEN; Rust workspace GREEN and TypeScript/E2E `187 passed, 0 failed`.
- `cargo clippy --workspace --all-targets -- -D warnings`: GREEN.
- `cargo fmt --all -- --check`: GREEN.
- `bun run typecheck`: GREEN.
- `git diff --check`: GREEN.
- Focused combined UI review run: `137 passed, 0 failed` before the final whole-repository gate.

The initial non-escalated final run failed broadly because the sandbox denied every loopback bind (`EPERM`), including a minimal standalone TCP bind. Re-running the identical repository gate with localhost permission passed; this environmental diagnosis required no source change.

## Deferred maintainability judgments

The 1,225-line activation coordinator and 886-line UI shell remain follow-up refactoring candidates. They were deliberately not reorganized here: extracting broad strategy/state modules is outside Issue #6's final-fix scope and would make this security/correctness wave less surgical.

No unresolved correctness or security concern remains from the reviewed findings.

## Re-review extension

### Startup failure precedence and classification

The re-review identified a conflict between the general target-isolation reading and the explicitly approved Task 5 startup contract. Task 5 says that an occupied persisted port fails closed before the shared UDS. That explicit exception controls: listener `Io`/`AddrInUse`, runtime task failure, non-loopback binding, and global StateStore/DB failures now drain any peer already resumed in the same bootstrap attempt and abort startup before UDS.

The actual isolation gaps remain target-local. Missing committed snapshots, missing Routing Credentials, and an unconstructible target Configuration Home/codec persist `model-route-unavailable`, project no endpoint, and leave that Target control-only while the healthy peer and UDS start. REDs reproduced the former global `State` abort for both committed-state corruptions and the malformed `.claude` home; the occupied-port RED reproduced the unintended UDS success. The full activation suite is GREEN at `44/44`.

### Closed actionable selector contract

Rust, TypeScript/Zod, and JSON Schema now share the same six-value closed selector set: the five supported Claude environment selectors plus the host-managed selector. `ControlProblem.selector` and `ClaudePreflightContext.blockingSelector` use that closed type rather than unrestricted strings. Conditional validation requires an exact environment selector for `enabled`/`unknown-nonempty`, the host selector for a managed/unknown host with no active environment selector, and no selector for an inactive context.

REDs showed Rust, Zod, direct preflight/activation, and real UDS accepting, panicking on, or projecting provider mode without a missing conditional selector; the TypeScript problem decoder accepted an arbitrary selector, and JSON Schema exposed an unrestricted string. GREEN evidence includes Rust protocol `15/15`, control socket `29/29`, Claude configuration `18 passed, 1 helper ignored`, activation `44/44`, the exact real-UDS provider-mode projection, TypeScript protocol/schema and mutation-sensitive renderer coverage. Invalid direct preflight/activation context now returns a fixed `preflight-context-required` before configuration, probe, listener, or mutation. The renderer shows the fixed localized selector/source without `undefined` or backend prose.

### Target-valid draft authentication

Draft Discovery now validates authentication against the canonical target/protocol/authentication declaration rule at the server boundary. Claude rejects OpenAI Bearer; Codex rejects both Anthropic profiles. A real UDS plus loopback RED previously reached upstream and returned a response; GREEN rejects all three with fixed `invalid-provider-authentication`, zero upstream calls, and no credential echo. Existing valid Claude API-key/Bearer tests continue to assert the exact selected header, absent peer header, and `anthropic-version`.

### Extension verification

- Affected Rust suites: activation `44/44`, Claude configuration `18 passed, 1 helper ignored`, control socket `29/29`, protocol `15/15`, provider inspection `14/14`.
- Focused TypeScript protocol and renderer: `61 passed, 0 failed`.
- Final `bun run verify`: GREEN; TypeScript/E2E `190 passed, 0 failed`, full Rust workspace GREEN.
- Final strict Clippy, rustfmt, TypeScript typecheck, JSON parse, and diff checks: GREEN.

No broad coordinator or UI-shell refactor was included.
