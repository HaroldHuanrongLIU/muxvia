# CC-Switch SQL and Historical Usage Migration Design

Status: binding for T16 by issue #17, ADRs 0005, 0009, 0010, 0015, 0033, 0037, and 0038

Issue: [#17 — T16 CC-Switch SQL and historical usage migration](https://github.com/HaroldHuanrongLIU/muxvia/issues/17)

## Context and scope

T16 adds one explicit migration source to the existing Provider import workflow: an Operator-selected SQL export produced by CC-Switch v3.19.2 at commit `43eaf07355af145aebfee301801779e824d4c221`. The Routing Service remains the only process that reads the selected export and mutates Muxvia state. It never searches for, opens, locks, copies, or writes `cc-switch.db`.

Migration is target-scoped. A preview opened from the Codex or Claude Target View reads one export but projects only rows for that Target CLI. The Operator can repeat the workflow for the other target. This preserves the target-scoped Control Session and gives Import Provenance an exact source target.

T16 imports Provider declarations and optional historical usage. It does not import CC-Switch current, takeover, failover, health, live configuration, MCP, prompt, skill, account, or application settings state.

## Selected export boundary

1. The request carries one absolute file path explicitly supplied by the Operator. No directory enumeration or default CC-Switch path lookup occurs.
2. The selected file must be a bounded regular UTF-8 SQL file whose first non-BOM line is the fixed CC-Switch export header.
3. The SQL runs only in a new in-memory SQLite database. An SQLite authorizer rejects attach/detach, virtual tables, unknown actions, and every PRAGMA except `foreign_keys` and `user_version`.
4. Preview requires `PRAGMA user_version = 16`, a successful integrity check, and the exact required v3.19.2 column contracts for `providers`, `proxy_request_logs`, and `usage_daily_rollups`. Missing, renamed, or incompatible required columns reject the whole source.
5. The parser reads only the validated in-memory copy. It never retains or projects the selected path or SQL text.

## Provider normalization

1. Only `app_type = codex` or `app_type = claude` matching the current Target CLI is read. Other application rows are ignored.
2. Every selected Provider receives a fresh Muxvia UUID when created. The CC-Switch `(app_type, id)` becomes the bounded source identifier in Import Provenance; the normalized Muxvia declaration supplies the configuration fingerprint.
3. Provider order follows `sort_index`, then source identity. Same-name records coexist. Exact normalized matches are offered but never selected automatically.
4. Codex rows read the selected `model_provider`, `model`, provider `base_url`, and `OPENAI_API_KEY` from the closed `{ auth, config }` shape. Claude rows read the closed `env` keys for base URL, model, and exactly one supported credential form.
5. Missing endpoint, model, or credential values produce an Incomplete Provider. Malformed JSON/TOML, duplicate source identities, unsupported Target CLI values, ambiguous credentials, hostile URLs, or unbounded fields reject the entire preview.
6. Import never changes Current Target Provider, Activated Snapshots, Activated Route Plans, Failover drafts, Managed Configuration, or Target Takeover.

## Historical usage

CC-Switch request logs are not represented as Muxvia Request Records or Native Usage Records. They become distinct Migrated Usage Records so the UI never claims that Muxvia routed them or that a Target CLI native log produced them.

1. Preview always computes the Target CLI's historical source record count, inclusive local-date range, and estimated Muxvia storage size. Selection defaults to false.
2. Detailed `proxy_request_logs` and retained `usage_daily_rollups` normalize into immutable daily Migrated Usage rollups. Error text, request/session identities, bodies, headers, source paths, and Provider credentials are not imported.
3. Imported fields are counts, success/failure counts, token dimensions, and latency totals. CC-Switch cost values are not imported because the export does not contain the unit-price evidence required for a Muxvia Pricing Snapshot; the migrated usage remains explicitly unpriced.
4. An export content fingerprint prevents the same Target CLI's usage from being imported twice. The fingerprint is source provenance, not a path or database identity.
5. Migrated Usage Records appear in Target activity with an explicit source label, survive ordinary detail retention because they are already daily aggregates, and are removed by the existing atomic clear-all-usage operation.

## Transaction and failure semantics

Confirmation may select any subset of Provider candidates and independently select historical usage. At least one Provider or historical usage must be selected. Provider rows, credentials, Import Provenance, catalog revisions, Target view revisions, and all Migrated Usage rollups commit in one immediate SQLite transaction.

Every invalid or stale choice, duplicate export fingerprint, arithmetic overflow, constraint failure, or injected write failure rolls back the complete transaction. A failed preview never opens a Muxvia write transaction. Replayed or expired preview tokens fail closed.

## Protocol and UI

The existing Provider import protocol adds a `cc-switch-sql` source with an absolute `path`, an optional historical-usage choice on confirmation, a secret-free historical-usage preview, and the imported usage count in the outcome. Existing source variants remain additive-compatible and continue to require a nonempty Provider choice.

The Provider import wizard adds the SQL export source, shows the bounded path field, Provider candidates, exact matches, and the usage summary. The `u` key toggles the default-off historical usage choice only when that summary is present. English and Simplified Chinese catalogs name the source and the Migrated Usage Record explicitly.

## Verification seams

1. A source-derived SQL fixture records repository, immutable commit, original schema/export paths, behavior, and hash.
2. Parser tests use a real in-memory SQLite import and cover valid Codex/Claude providers, malformed JSON/TOML, wrong schema version, missing columns, corrupt SQL, duplicate identities, attach/vtable attempts, size bounds, and proof that no live database path is touched.
3. State tests prove fresh schema v17, v16-to-v17 migration, transactional Provider-plus-usage commit, duplicate-export rejection, injected rollback, activity projection, retention stability, and atomic clear.
4. Rust, TypeScript, and JSON-schema fixtures cover the additive protocol.
5. Real UDS and Control Session/UI tests prove default-off selection, preview, confirmation, source labels, fixed errors, and absence of SQL/path/credential/error/session markers from every projected or Debug surface.
