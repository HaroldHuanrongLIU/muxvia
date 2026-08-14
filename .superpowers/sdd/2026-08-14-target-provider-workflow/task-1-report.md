# Task 1 implementation report: incomplete provider declarations and schema-v2 migration

Status: complete

## Files and behavior changed

- Added schema-v2 migration support and the final declaration schema. Providers now have stable position and provider revision fields, nullable Credential References, fixed OpenAI Responses protocol, provenance, and generated-owner fields. Credentials are stored separately; opening a database migrates v1 atomically, enables foreign keys, and rejects foreign-key violations.
- Added Provider declaration mutation logic for incomplete creates and revision-guarded edits. A name is required; endpoint and model are persisted as strings and may be empty. The explicit Credential Edit boundary implements Keep, Remove, and nonblank Replace, including orphan-only credential collection and no-provider-change for an identical Keep update.
- Enriched the Target View with Provider completeness/missing requirements, provenance, active references, fixed protocol, and the release-owned `openai-api-responses` Preset catalog. Credential identities and values remain out of Target Views and receipts.
- Replaced the typed wire actions with `create-provider` and `update-provider`; added redacted `CredentialEdit` Debug output; updated JSON schema, fixtures, TypeScript validation/types, and all affected Target View literals.
- Updated activation and existing test fixtures for schema-v2 storage. Activation now rejects incomplete Providers before recovery intent or listener binding and snapshots remain independent of later declaration edits.
- Added real v1 migration and declaration-boundary coverage in `provider_declarations.rs`.

## RED evidence

1. `cargo test -p muxvia-routing --test protocol_contract` initially failed to compile: unresolved imports for `CredentialEdit`, `ProviderCompleteness`, `ProviderProtocol`, and `ProviderRequirement`, plus a missing `TargetAction::CreateProvider`.
2. `bun test packages/control-plane/test/protocol.test.ts` initially had 10 passing and 4 failing assertions: it stripped the Preset/enriched Provider fields and rejected the `create-provider` discriminator.
3. `cargo test -p muxvia-routing --test provider_declarations v1_database_migrates_provider_identity_order_credential_and_active_state -- --exact` executed one test and failed because the schema-version was `"1"`, expected `"2"`.
4. `cargo test -p muxvia-routing --test provider_declarations` then failed to compile with 19 `no method named apply_provider_action` errors.

## GREEN verification

Focused Task 1 checks (all exit 0):

- `cargo test -p muxvia-routing --test provider_declarations` — 10 passed, 0 failed.
- `cargo test -p muxvia-routing --test activation` — 16 passed, 0 failed.
- `cargo test -p muxvia-routing --test protocol_contract` — 9 passed, 0 failed.
- `bun test packages/control-plane/test/protocol.test.ts` — 14 passed, 0 failed, 22 expectations.
- `cargo fmt --all -- --check` — passed.
- `cargo clippy --workspace --all-targets -- -D warnings` — passed.
- `bun run typecheck` — passed.
- `git diff --check` — passed.

Final repository verification used elevated local socket/PTY permissions required by the integration suite:

- `bun run verify` — all stages passed: Rust test suite 111 passed with 1 intentional ignored test; Bun suite 85 passed, 0 failed, 550 expectations; formatting, Clippy, and TypeScript typecheck passed.

## Commit

Implementation commit: `f80236f2fd3ff92ed46cf95609739d78345c4db1` (`feat: persist incomplete provider declarations`).

## Self-review findings

- Confirmed migration copies v1 rows in `rowid` order, preserves Provider UUIDs, creates migrated Credential IDs from the prior Provider IDs, replaces only the two declaration tables, and performs the v2 update inside an IMMEDIATE transaction.
- Confirmed receipt lookup occurs before raw action parsing, including malformed replay behavior; completed receipts and projections remain secret-free.
- Confirmed an empty endpoint/model remains a string on the wire and only a nonempty endpoint is normalized/validated.
- Confirmed the accidental untracked `crates/routing-service/src/state/schema 2.sql` duplicate was removed before staging.

## Concerns

None. The ordinary sandbox blocks loopback/Unix-socket and PTY setup with `EPERM`; the required complete verification was rerun with normal local permissions and passed.
