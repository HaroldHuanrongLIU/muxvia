# Provider Import and Redacted Export Design

Status: implementation design for T15 on 2026-08-21

Issue: [#16 — T15 Provider import and redacted export](https://github.com/HaroldHuanrongLIU/muxvia/issues/16)

## Context

T15 lets the Operator preview and import Provider declarations from live Target CLI configuration, a pasted CC-Switch provider deep link, or a Muxvia Provider Configuration Export. It also creates one shareable, always-redacted export format. The design follows `CONTEXT.md`, ADR 0002, ADR 0003, ADR 0007, ADR 0010, ADR 0015, ADR 0022, ADR 0024, ADR 0025, ADR 0028, ADR 0029, ADR 0032, ADR 0033, ADR 0035, ADR 0039, ADR 0040, and ADR 0044.

Import is migration, not activation or reconciliation. It never changes the Current Target Provider, an Activated Snapshot or Activated Route Plan, Managed Configuration, Subscription Accounts, or Recovery state. A distinct live Target configuration is represented as an unselected Imported Current so its observed status is retained without claiming it is the Current Target Provider.

## Binding Decisions

1. Import has two phases. Preview performs all parsing, bounds checking, normalization, duplicate detection, provenance construction, and exact-match discovery without database mutation. Confirm consumes one opaque preview token and applies the reviewed choices in one SQLite transaction.
2. Secret-bearing source material remains behind the Routing Service seam. The preview projects only Credential Presence and an opaque token. The token indexes a bounded, expiring in-memory plan and is consumed once on confirmation.
3. Every created record receives a fresh Muxvia UUID. A source identity, display name, or prior Muxvia UUID is never reused as the destination identity and never selects a row to overwrite.
4. An exact normalized configuration match may be selected as `use-existing`. That choice does not create an imported record and does not modify the existing record. Equal names and nonmatching configurations always remain valid `create` choices.
5. The accepted pasted CC-Switch shape is its pinned `ccswitch://v1/import?resource=provider` payload for Codex CLI or Claude Code. Muxvia accepts it only as pasted text and registers no operating-system URL handler.
6. The Muxvia export is a versioned closed JSON document. Its only creation operation has no redaction or secret-inclusion option.
7. Provider ordering is source-relative. Imports append each source-ordered block after existing records. Generated Target Provider ownership and Universal Provider Target Overlays are reconstructed with new identities. Failover drafts are remapped only to providers present in the same export; Current selection and applied runtime plans are not exported.

## Deep Module and Interface

The Routing Service owns one Provider Transfer module:

```text
preview(source) -> ProviderImportPreview
confirm(previewToken, choices) -> ProviderImportOutcome
export() -> ProviderConfigurationExport
```

The interface hides the three source adapters, secret retention, canonicalization, configuration fingerprints, exact-match comparison, source-ID remapping, transaction ordering, and redaction scan. Callers cannot submit already-parsed candidates or request a partially redacted export.

The live Target adapters read only the supported default Configuration Homes through the existing Codex and Claude configuration codecs. The CC-Switch adapter accepts only the pinned v1 provider URL grammar. The Muxvia adapter accepts only the closed export document. All adapters produce the same private normalized import plan.

## Import Provenance and Normalization

Created Target and Universal Providers retain complete Import Provenance:

- source product: `target-cli`, `cc-switch`, or `muxvia`;
- source target: `codex`, `claude`, or `universal`;
- source identifier: the live provider key/settings source, CC-Switch payload identity, or exported source UUID; and
- a lowercase SHA-256 fingerprint of the normalized secret-free declaration.

The fingerprint is not a destination identity and is not unique. It excludes secret bytes while retaining Credential Presence. Exact match comparison uses the full normalized declaration and constant-time secret equality when both sides contain credentials; the fingerprint alone never authorizes a match.

Target normalization includes target, base URL, model, protocol, authentication, routing requirement, Credential Presence and secret equality, and generated Universal ownership where applicable. Universal normalization includes base URL, Credential Presence and secret equality, plus the ordered closed Target Overlay declarations. Display name, source identity, Muxvia identity, positions outside the source block, and runtime state do not affect equality.

## Imported Current

A live Target preview describes exactly one Target Provider candidate. When created, its Import Provenance source product is `target-cli`, which projects it as Imported Current. Confirmation appends the record but leaves `target_route_state.current_provider_id`, serving state, takeover state, Managed Configuration, activation records, and route plans byte-for-byte unchanged.

A live configuration carrying a Muxvia Routing Credential is not importable as an upstream Provider credential. Exact direct configuration may be offered as an existing match. Malformed or unrepresentable live configuration fails with a fixed secret-free problem.

## Provider Configuration Export

The export contains:

- ordered Universal Provider declarations and Target Overlays;
- ordered Target Provider declarations, including Generated ownership links;
- Provider models, protocols, authentication modes, routing requirements, and Credential Presence fixed to missing in the exported declaration; and
- per-Target Failover Draft ordering remapped through exported source identities.

It never contains Provider credential bytes, Subscription Account identities or tokens, Routing Credentials, Managed Configuration, Recovery state or payloads, Activated Snapshots, Activated Route Plans, Request Records, Native Usage Records, or database/action receipts. Importing the export into an empty store recreates the declarations, ownership, ordering, models, and failover draft structure with fresh identities and incomplete credentials.

Before return, serialization is scanned for every credential and routing secret known to the database transaction. Any match fails closed rather than returning a partial artifact. There is no `includeSecrets`, recovery, private, or compatibility mode.

## Bounds, Hostile Input, and Atomicity

The control frame remains capped at 1 MiB. A source payload is capped below that limit, a document has a bounded candidate count and bounded string fields, duplicate source identities and duplicate normalized declarations are rejected, and unknown export fields fail closed. CC-Switch query keys may appear at most once. URLs use the existing Provider URL safety rules: HTTPS, or loopback HTTP only, with no user information, query, or fragment.

Preview errors use fixed categories and never echo input. Debug implementations redact pasted source text, secret-bearing plans, credentials, and exports under construction. Confirmation requires exactly one valid choice per candidate. Invalid, stale, missing, duplicated, or replayed choices fail before commit. All Provider rows, credentials, Import Provenance, catalog revisions, Target view revisions, and failover members commit together or roll back together.

## Control Plane

TargetSession exposes preview, confirm, and export methods through the existing control socket. Import starts from the active Target context. The preview overlay identifies source, candidates, Credential Presence, Imported Current status, and exact existing matches without showing secrets. Confirmation defaults to creating fresh records; the Operator may explicitly choose the offered exact existing match. Pasted text is cleared when the workflow closes or settles.

Export opens a read-only overlay containing the shareable JSON payload for copying. The Control Plane does not read SQLite, inspect Target configuration files, register URL schemes, or offer an export-secret toggle.

## Stable Problems

- `invalid-provider-import`
- `provider-import-too-large`
- `duplicate-provider-import`
- `hostile-provider-import`
- `stale-provider-import-preview`
- `invalid-provider-import-choice`
- `provider-import-secret-rejected`
- `provider-export-redaction-failed`
- existing `invalid-configuration`, `state-store-error`, `invalid-response`, and `connection-closed`

## Confirmed Testing Seams

1. Rust/TypeScript/JSON-schema protocol fixtures and a real Unix-domain Target session.
2. Provider Transfer through real Codex/Claude files and real SQLite, using only preview, confirm, Target/Universal views, and export.
3. TargetSession through the real OpenTUI renderer for paste, reviewed confirmation, exact-match choice, Imported Current presentation, export, and sensitive-field clearing.
4. A real-process tracer proving new identities, complete provenance, no current/config rewrite, redacted round trip, and atomic rejection of corrupt, oversized, duplicate, hostile, and secret-bearing inputs.

Tests use known literal normalized declarations and serialized artifacts. They do not query private helpers or assert internal call ordering.
