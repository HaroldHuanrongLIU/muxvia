use serde::{Deserialize, Serialize};
use tokio_rusqlite::rusqlite::{
    Connection, Error, OptionalExtension, Result, Transaction, TransactionBehavior, params,
    types::Type,
};

use crate::control::protocol::{
    ActionOutcome, ActionStatus, ActivatedSnapshotView, ControlProblem, CredentialPresence,
    FailoverView, ManagedConfigurationView, ProviderAuthentication, ProviderCompleteness,
    ProviderFieldOwnershipView, ProviderPresetView, ProviderProtocol, ProviderProvenanceView,
    ProviderReferenceView, ProviderRequirement, ProviderRoutingRequirement, ProviderView,
    RecoveryView, RouteHealthState, RouteHealthView, ServiceView, TakeoverView, Target, TargetView,
};

const SCHEMA: &str = include_str!("schema.sql");

pub const SCHEMA_VERSION: u32 = 17;

pub fn migrate(connection: &mut Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )?;
    let version = connection
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema-version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|value| {
            value
                .parse::<u32>()
                .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)
        })
        .transpose()?;

    match version {
        None | Some(SCHEMA_VERSION) | Some(16) | Some(15) | Some(14) | Some(13) => {}
        Some(1) => {
            migrate_v1(connection)?;
            migrate_v2(connection)?;
            migrate_v3(connection)?;
            migrate_v4(connection)?;
            migrate_v5(connection)?;
            migrate_v6(connection)?;
            migrate_v7(connection)?;
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(2) => {
            migrate_v2(connection)?;
            migrate_v3(connection)?;
            migrate_v4(connection)?;
            migrate_v5(connection)?;
            migrate_v6(connection)?;
            migrate_v7(connection)?;
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(3) => {
            migrate_v3(connection)?;
            migrate_v4(connection)?;
            migrate_v5(connection)?;
            migrate_v6(connection)?;
            migrate_v7(connection)?;
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(4) => {
            migrate_v4(connection)?;
            migrate_v5(connection)?;
            migrate_v6(connection)?;
            migrate_v7(connection)?;
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(5) => {
            migrate_v5(connection)?;
            migrate_v6(connection)?;
            migrate_v7(connection)?;
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(6) => {
            migrate_v6(connection)?;
            migrate_v7(connection)?;
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(7) => {
            migrate_v7(connection)?;
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(8) => {
            migrate_v8(connection)?;
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(9) => {
            migrate_v9(connection)?;
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(10) => {
            migrate_v10(connection)?;
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(11) => {
            migrate_v11(connection)?;
            migrate_v12(connection)?;
        }
        Some(12) => migrate_v12(connection)?,
        Some(_) => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
    }
    if version.is_some_and(|version| version < 14) {
        migrate_v13(connection)?;
    }
    if version.is_some_and(|version| version < 15) {
        migrate_v14(connection)?;
    }
    if version.is_some_and(|version| version < 16) {
        migrate_v15(connection)?;
    }
    if version.is_some_and(|version| version < 17) {
        migrate_v16(connection)?;
    }
    connection.execute_batch(SCHEMA)?;
    Ok(())
}

fn migrate_v16(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE migrated_usage_rollups (
           sequence INTEGER PRIMARY KEY AUTOINCREMENT,
           id TEXT NOT NULL UNIQUE,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           source_product TEXT NOT NULL CHECK (source_product = 'cc-switch'),
           source_export_fingerprint TEXT NOT NULL CHECK (
             length(source_export_fingerprint) = 64
             AND source_export_fingerprint NOT GLOB '*[^0-9a-f]*'
           ),
           local_date TEXT NOT NULL CHECK (
             length(local_date) = 10
             AND local_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
           ),
           source_record_count INTEGER NOT NULL CHECK (source_record_count > 0),
           successful_request_count INTEGER NOT NULL CHECK (successful_request_count >= 0),
           failed_request_count INTEGER NOT NULL CHECK (failed_request_count >= 0),
           input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
           cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0),
           cache_creation_input_tokens INTEGER NOT NULL CHECK (cache_creation_input_tokens >= 0),
           output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
           latency_observation_count INTEGER NOT NULL CHECK (latency_observation_count >= 0),
           total_latency_ms INTEGER NOT NULL CHECK (total_latency_ms >= 0),
           CHECK (successful_request_count + failed_request_count = source_record_count),
           CHECK (latency_observation_count <= source_record_count),
           UNIQUE (target, source_export_fingerprint, local_date)
         );
         CREATE INDEX migrated_usage_rollups_target_sequence
           ON migrated_usage_rollups(target, sequence DESC);
         CREATE TRIGGER migrated_usage_rollups_immutable
         BEFORE UPDATE ON migrated_usage_rollups
         BEGIN
           SELECT RAISE(ABORT, 'immutable-migrated-usage-rollup');
         END;",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '17' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v15(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    for table in ["providers", "universal_providers"] {
        transaction.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN import_source_product TEXT
               CHECK (import_source_product IS NULL OR import_source_product IN ('target-cli', 'cc-switch', 'muxvia'));
             ALTER TABLE {table} ADD COLUMN import_source_target TEXT
               CHECK (
                 (import_source_target IS NULL) = (import_source_product IS NULL)
                 AND (import_source_target IS NULL OR import_source_target IN ('codex', 'claude', 'universal'))
               );
             ALTER TABLE {table} ADD COLUMN import_source_identifier TEXT
               CHECK (
                 (import_source_identifier IS NULL) = (import_source_product IS NULL)
                 AND (import_source_identifier IS NULL OR length(import_source_identifier) BETWEEN 1 AND 256)
               );
             ALTER TABLE {table} ADD COLUMN import_configuration_fingerprint TEXT
               CHECK (
                 (import_configuration_fingerprint IS NULL) = (import_source_product IS NULL)
                 AND (
                   import_configuration_fingerprint IS NULL
                   OR (
                     length(import_configuration_fingerprint) = 64
                     AND import_configuration_fingerprint NOT GLOB '*[^0-9a-f]*'
                   )
                 )
               );"
        ))?;
    }
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '16' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v14(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE native_usage_records (
           sequence INTEGER PRIMARY KEY AUTOINCREMENT,
           id TEXT NOT NULL UNIQUE,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           source_record_fingerprint TEXT NOT NULL CHECK (length(source_record_fingerprint) = 64),
           model TEXT NOT NULL CHECK (length(model) > 0),
           observed_at_unix_ms INTEGER NOT NULL CHECK (observed_at_unix_ms >= 0),
           input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
           cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0),
           cache_creation_input_tokens INTEGER NOT NULL CHECK (cache_creation_input_tokens >= 0),
           output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
           CHECK (
             input_tokens > 0 OR cached_input_tokens > 0
             OR cache_creation_input_tokens > 0 OR output_tokens > 0
           ),
           UNIQUE (target, source_record_fingerprint)
         );
         CREATE INDEX native_usage_records_target_sequence
           ON native_usage_records(target, sequence DESC);
         CREATE TRIGGER native_usage_records_immutable
         BEFORE UPDATE ON native_usage_records
         BEGIN
           SELECT RAISE(ABORT, 'immutable-native-usage-record');
         END;
         CREATE TABLE native_usage_pricing_snapshots (
           native_usage_record_id TEXT PRIMARY KEY
             REFERENCES native_usage_records(id) ON DELETE CASCADE,
           catalog_version TEXT NOT NULL,
           source TEXT NOT NULL,
           source_model TEXT NOT NULL,
           input_nano_usd_per_million INTEGER NOT NULL CHECK (input_nano_usd_per_million >= 0),
           output_nano_usd_per_million INTEGER NOT NULL CHECK (output_nano_usd_per_million >= 0),
           cache_read_multiplier_ppm INTEGER NOT NULL CHECK (cache_read_multiplier_ppm >= 0),
           cache_creation_multiplier_ppm INTEGER NOT NULL CHECK (cache_creation_multiplier_ppm >= 0),
           priced_at_unix_ms INTEGER NOT NULL CHECK (priced_at_unix_ms >= 0),
           estimated_cost_nano_usd INTEGER NOT NULL CHECK (estimated_cost_nano_usd > 0)
         );
         CREATE TRIGGER native_usage_pricing_snapshots_immutable
         BEFORE UPDATE ON native_usage_pricing_snapshots
         BEGIN
           SELECT RAISE(ABORT, 'immutable-native-usage-pricing-snapshot');
         END;
         CREATE TRIGGER native_usage_pricing_snapshots_delete_with_record
         BEFORE DELETE ON native_usage_pricing_snapshots
         WHEN EXISTS (
           SELECT 1 FROM native_usage_records WHERE id = OLD.native_usage_record_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'immutable-native-usage-pricing-snapshot');
         END;
         CREATE TABLE native_usage_import_cursors (
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           source_fingerprint TEXT NOT NULL CHECK (length(source_fingerprint) = 64),
           modified_unix_nanos INTEGER NOT NULL CHECK (modified_unix_nanos >= 0),
           byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
           completed_line_count INTEGER NOT NULL CHECK (completed_line_count >= 0),
           PRIMARY KEY (target, source_fingerprint)
         );
         CREATE TABLE daily_usage_rollups (
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           local_date TEXT NOT NULL CHECK (
             length(local_date) = 10
             AND local_date GLOB '[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'
           ),
           request_record_count INTEGER NOT NULL CHECK (request_record_count >= 0),
           native_usage_record_count INTEGER NOT NULL CHECK (native_usage_record_count >= 0),
           successful_request_count INTEGER NOT NULL CHECK (successful_request_count >= 0),
           failed_request_count INTEGER NOT NULL CHECK (failed_request_count >= 0),
           input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
           cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0),
           cache_creation_input_tokens INTEGER NOT NULL CHECK (cache_creation_input_tokens >= 0),
           output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
           priced_record_count INTEGER NOT NULL CHECK (priced_record_count >= 0),
           unpriced_record_count INTEGER NOT NULL CHECK (unpriced_record_count >= 0),
           estimated_cost_nano_usd INTEGER NOT NULL CHECK (estimated_cost_nano_usd >= 0),
           latency_observation_count INTEGER NOT NULL CHECK (latency_observation_count >= 0),
           total_latency_ms INTEGER NOT NULL CHECK (total_latency_ms >= 0),
           CHECK (successful_request_count + failed_request_count = request_record_count),
           CHECK (
             priced_record_count + unpriced_record_count
               = request_record_count + native_usage_record_count
           ),
           CHECK (latency_observation_count = request_record_count),
           PRIMARY KEY (target, local_date)
         );
         CREATE TABLE usage_settings (
           singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
           detailed_retention_days INTEGER NOT NULL
             CHECK (detailed_retention_days BETWEEN 1 AND 3650)
         );
         INSERT INTO usage_settings (singleton, detailed_retention_days) VALUES (1, 30);
         CREATE TABLE pricing_catalog_state (
           singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
           catalog_version TEXT NOT NULL CHECK (length(catalog_version) > 0),
           source TEXT NOT NULL CHECK (length(source) > 0),
           catalog_json TEXT NOT NULL CHECK (length(catalog_json) > 0),
           updated_at_unix_ms INTEGER NOT NULL CHECK (updated_at_unix_ms >= 0)
         );",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '15' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v13(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS pricing_snapshots_delete_with_request_record
         BEFORE DELETE ON pricing_snapshots
         WHEN EXISTS (
           SELECT 1 FROM request_records WHERE id = OLD.request_record_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'immutable-pricing-snapshot');
         END;",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '14' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v12(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE request_records (
           sequence INTEGER PRIMARY KEY AUTOINCREMENT,
           id TEXT NOT NULL UNIQUE,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           plan_id TEXT NOT NULL,
           plan_epoch TEXT NOT NULL,
           provider_id TEXT,
           provider_name TEXT,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           started_at_unix_ms INTEGER NOT NULL CHECK (started_at_unix_ms >= 0),
           finished_at_unix_ms INTEGER NOT NULL CHECK (finished_at_unix_ms >= started_at_unix_ms),
           latency_ms INTEGER NOT NULL CHECK (
             latency_ms >= 0 AND latency_ms = finished_at_unix_ms - started_at_unix_ms
           ),
           outcome TEXT NOT NULL CHECK (outcome IN (
             'success', 'upstream-error', 'semantic-error', 'transport-error',
             'route-unavailable', 'cancelled', 'stream-error'
           )),
           http_status INTEGER CHECK (http_status IS NULL OR http_status BETWEEN 100 AND 999),
           usage_observed INTEGER NOT NULL CHECK (usage_observed IN (0, 1)),
           input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
           cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0),
           cache_creation_input_tokens INTEGER NOT NULL CHECK (cache_creation_input_tokens >= 0),
           output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
           error_payload BLOB,
           error_payload_truncated INTEGER NOT NULL CHECK (error_payload_truncated IN (0, 1)),
           CHECK ((provider_id IS NULL) = (provider_name IS NULL)),
           CHECK (
             usage_observed = 1
             OR (input_tokens = 0 AND cached_input_tokens = 0
                 AND cache_creation_input_tokens = 0 AND output_tokens = 0)
           ),
           CHECK (error_payload IS NULL OR length(error_payload) <= 65536),
           CHECK (error_payload IS NULL OR outcome = 'upstream-error'),
           CHECK (
             error_payload_truncated = 0
             OR (outcome = 'upstream-error' AND error_payload IS NOT NULL)
           ),
           CHECK (
             outcome != 'success'
             OR (error_payload IS NULL AND error_payload_truncated = 0)
           )
         );
         CREATE INDEX request_records_target_sequence
           ON request_records(target, sequence DESC);
         CREATE TRIGGER request_records_immutable
         BEFORE UPDATE ON request_records
         BEGIN
           SELECT RAISE(ABORT, 'immutable-request-record');
         END;
         CREATE TABLE pricing_snapshots (
           request_record_id TEXT PRIMARY KEY REFERENCES request_records(id) ON DELETE CASCADE,
           catalog_version TEXT NOT NULL,
           source TEXT NOT NULL,
           source_model TEXT NOT NULL,
           input_nano_usd_per_million INTEGER NOT NULL CHECK (input_nano_usd_per_million >= 0),
           output_nano_usd_per_million INTEGER NOT NULL CHECK (output_nano_usd_per_million >= 0),
           cache_read_multiplier_ppm INTEGER NOT NULL CHECK (cache_read_multiplier_ppm >= 0),
           cache_creation_multiplier_ppm INTEGER NOT NULL CHECK (cache_creation_multiplier_ppm >= 0),
           priced_at_unix_ms INTEGER NOT NULL CHECK (priced_at_unix_ms >= 0),
           estimated_cost_nano_usd INTEGER NOT NULL CHECK (estimated_cost_nano_usd > 0)
         );
         CREATE TRIGGER pricing_snapshots_immutable
         BEFORE UPDATE ON pricing_snapshots
         BEGIN
           SELECT RAISE(ABORT, 'immutable-pricing-snapshot');
         END;",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '13' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v11(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE providers_v12 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer', 'codex-subscription')),
           credential_id TEXT,
           provenance_kind TEXT,
           provenance_key TEXT,
           generated_owner_id TEXT,
           routing_requirement TEXT NOT NULL DEFAULT 'direct-compatible'
             CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
           generated_source_revision INTEGER CHECK (generated_source_revision IS NULL OR generated_source_revision >= 1),
           generated_overlay_revision INTEGER CHECK (generated_overlay_revision IS NULL OR generated_overlay_revision >= 1),
           CHECK (
             (target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
             OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer', 'codex-subscription'))
           ),
           CHECK (
             authentication != 'codex-subscription'
             OR (
               base_url = 'https://chatgpt.com/backend-api/codex'
               AND credential_id IS NULL
               AND routing_requirement = 'takeover-required'
             )
           ),
           FOREIGN KEY (target, credential_id) REFERENCES credentials(target, id)
         );
         CREATE TABLE activated_snapshots_v12 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           provider_id TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer', 'codex-subscription')),
           provider_bearer_token TEXT NOT NULL,
           epoch TEXT NOT NULL,
           CHECK (
             (target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
             OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer', 'codex-subscription'))
           ),
           CHECK (
             authentication != 'codex-subscription'
             OR (
               base_url = 'https://chatgpt.com/backend-api/codex'
               AND provider_bearer_token = ''
             )
           )
         );
         CREATE TABLE activated_route_plan_members_v12 (
           plan_id TEXT NOT NULL REFERENCES activated_route_plans(id) ON DELETE CASCADE,
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_id TEXT NOT NULL,
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer', 'codex-subscription')),
           credential_id TEXT REFERENCES credentials(id),
           routing_requirement TEXT NOT NULL CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
           CHECK (
             (
               authentication = 'codex-subscription'
               AND base_url = 'https://chatgpt.com/backend-api/codex'
               AND credential_id IS NULL
               AND routing_requirement = 'takeover-required'
             )
             OR (authentication != 'codex-subscription' AND credential_id IS NOT NULL)
           ),
           PRIMARY KEY (plan_id, position),
           UNIQUE (plan_id, provider_id)
         );
         CREATE TABLE subscription_provider_bindings_v12 (
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           provider_id TEXT NOT NULL REFERENCES providers_v12(id) ON DELETE CASCADE,
           binding_kind TEXT NOT NULL CHECK (binding_kind IN ('fixed', 'follow-default')),
           account_id TEXT,
           CHECK (
             (binding_kind = 'fixed' AND account_id IS NOT NULL AND length(account_id) > 0)
             OR (binding_kind = 'follow-default' AND account_id IS NULL)
           ),
           PRIMARY KEY (target, provider_id)
         );

         INSERT INTO providers_v12 SELECT * FROM providers;
         INSERT INTO activated_snapshots_v12 SELECT * FROM activated_snapshots;
         INSERT INTO activated_route_plan_members_v12 SELECT * FROM activated_route_plan_members;
         INSERT INTO subscription_provider_bindings_v12 SELECT * FROM subscription_provider_bindings;

         DROP TABLE subscription_provider_bindings;
         DROP TABLE activated_route_plan_members;
         DROP TABLE activated_snapshots;
         DROP TABLE providers;
         ALTER TABLE providers_v12 RENAME TO providers;
         ALTER TABLE activated_snapshots_v12 RENAME TO activated_snapshots;
         ALTER TABLE activated_route_plan_members_v12 RENAME TO activated_route_plan_members;
         ALTER TABLE subscription_provider_bindings_v12 RENAME TO subscription_provider_bindings;
         CREATE UNIQUE INDEX providers_generated_owner_target
           ON providers(generated_owner_id, target)
           WHERE generated_owner_id IS NOT NULL;",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '12' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v10(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE subscription_account_catalog_state (
           singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
           revision INTEGER NOT NULL CHECK (revision >= 0),
           view_sequence INTEGER NOT NULL CHECK (view_sequence >= 0),
           recovery_state TEXT NOT NULL CHECK (recovery_state IN ('clean', 'recovery-required'))
         );
         CREATE TABLE subscription_provider_bindings (
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
           binding_kind TEXT NOT NULL CHECK (binding_kind IN ('fixed', 'follow-default')),
           account_id TEXT,
           CHECK (
             (binding_kind = 'fixed' AND account_id IS NOT NULL AND length(account_id) > 0)
             OR (binding_kind = 'follow-default' AND account_id IS NULL)
           ),
           PRIMARY KEY (target, provider_id)
         );
         CREATE TABLE subscription_account_recovery_intents (
           id TEXT PRIMARY KEY,
           action_id TEXT NOT NULL UNIQUE,
           operation TEXT NOT NULL,
           state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
           before_sha256 TEXT NOT NULL,
           desired_sha256 TEXT NOT NULL,
           created_revision INTEGER NOT NULL CHECK (created_revision >= 0)
         );
         CREATE TABLE subscription_account_action_receipts (
           action_id TEXT PRIMARY KEY,
           action_kind TEXT NOT NULL,
           action_json TEXT NOT NULL,
           committed_revision INTEGER NOT NULL CHECK (committed_revision >= 0),
           outcome_json TEXT NOT NULL
         );
         INSERT INTO subscription_account_catalog_state
           (singleton, revision, view_sequence, recovery_state)
         VALUES (1, 0, 0, 'clean');",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '11' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v9(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE failover_drafts (
           target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
           draft_revision INTEGER NOT NULL CHECK (draft_revision >= 1)
         );
         CREATE TABLE failover_draft_members (
           target TEXT NOT NULL REFERENCES failover_drafts(target) ON DELETE CASCADE,
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_id TEXT NOT NULL,
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           PRIMARY KEY (target, position),
           UNIQUE (target, provider_id)
         );
         CREATE TABLE activated_route_plans (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           epoch TEXT NOT NULL,
           created_revision INTEGER NOT NULL CHECK (created_revision >= 0)
         );
         CREATE TABLE activated_route_plan_members (
           plan_id TEXT NOT NULL REFERENCES activated_route_plans(id) ON DELETE CASCADE,
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_id TEXT NOT NULL,
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
           credential_id TEXT NOT NULL REFERENCES credentials(id),
           routing_requirement TEXT NOT NULL CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
           PRIMARY KEY (plan_id, position),
           UNIQUE (plan_id, provider_id)
         );
         CREATE TABLE provider_route_health (
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           provider_id TEXT NOT NULL,
           state TEXT NOT NULL CHECK (state IN ('healthy', 'degraded', 'unavailable')),
           service_epoch TEXT NOT NULL,
           consecutive_successes INTEGER NOT NULL CHECK (consecutive_successes >= 0),
           consecutive_failures INTEGER NOT NULL CHECK (consecutive_failures >= 0),
           total_attempts INTEGER NOT NULL CHECK (total_attempts >= 0),
           failed_attempts INTEGER NOT NULL CHECK (failed_attempts >= 0 AND failed_attempts <= total_attempts),
           observation_sequence INTEGER NOT NULL CHECK (observation_sequence >= 0),
           last_outcome TEXT NOT NULL,
           PRIMARY KEY (target, provider_id)
         );
         ALTER TABLE target_route_state ADD COLUMN active_route_plan_id TEXT
           REFERENCES activated_route_plans(id);
         INSERT INTO failover_drafts (target, draft_revision)
           VALUES ('codex', 1), ('claude', 1);",
    )?;

    let invalid_routes: u64 = transaction.query_row(
        "SELECT COUNT(*)
         FROM target_route_state route
         LEFT JOIN activated_snapshots snapshot
           ON snapshot.id = route.activated_snapshot_id
          AND snapshot.target = route.target
          AND snapshot.provider_id = route.current_provider_id
         LEFT JOIN providers provider
           ON provider.id = route.current_provider_id
          AND provider.target = route.target
         WHERE route.current_provider_id IS NOT NULL
           AND (snapshot.id IS NULL OR provider.id IS NULL)",
        [],
        |row| row.get(0),
    )?;
    if invalid_routes != 0 {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }

    transaction.execute_batch(
        "INSERT INTO failover_draft_members
           (target, position, provider_id, provider_revision)
         SELECT route.target, 0, provider.id, provider.provider_revision
         FROM target_route_state route
         JOIN providers provider
           ON provider.id = route.current_provider_id AND provider.target = route.target;

         INSERT INTO activated_route_plans (id, target, epoch, created_revision)
         SELECT snapshot.id, route.target, snapshot.epoch, route.management_revision
         FROM target_route_state route
         JOIN activated_snapshots snapshot
           ON snapshot.id = route.activated_snapshot_id
          AND snapshot.target = route.target
          AND snapshot.provider_id = route.current_provider_id;

         INSERT INTO credentials (id, target, bearer_token)
         SELECT 'route-plan-' || snapshot.id, route.target, snapshot.provider_bearer_token
         FROM target_route_state route
         JOIN activated_snapshots snapshot
           ON snapshot.id = route.activated_snapshot_id
          AND snapshot.target = route.target
          AND snapshot.provider_id = route.current_provider_id
         JOIN providers provider
           ON provider.id = route.current_provider_id AND provider.target = route.target
         LEFT JOIN credentials credential
           ON credential.id = provider.credential_id AND credential.target = provider.target
         WHERE credential.bearer_token IS NOT snapshot.provider_bearer_token;

         INSERT INTO activated_route_plan_members
           (plan_id, position, provider_id, provider_revision, name, base_url, model,
            protocol, authentication, credential_id, routing_requirement)
         SELECT snapshot.id, 0, provider.id, provider.provider_revision, provider.name,
                snapshot.base_url, snapshot.model, snapshot.protocol, snapshot.authentication,
                CASE WHEN credential.bearer_token = snapshot.provider_bearer_token
                     THEN provider.credential_id ELSE 'route-plan-' || snapshot.id END,
                provider.routing_requirement
         FROM target_route_state route
         JOIN activated_snapshots snapshot
           ON snapshot.id = route.activated_snapshot_id
          AND snapshot.target = route.target
          AND snapshot.provider_id = route.current_provider_id
         JOIN providers provider
           ON provider.id = route.current_provider_id AND provider.target = route.target
         LEFT JOIN credentials credential
           ON credential.id = provider.credential_id AND credential.target = provider.target;

         UPDATE target_route_state
         SET active_route_plan_id = activated_snapshot_id
         WHERE current_provider_id IS NOT NULL;",
    )?;

    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '10' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v8(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE providers ADD COLUMN generated_source_revision INTEGER
           CHECK (generated_source_revision IS NULL OR generated_source_revision >= 1);
         ALTER TABLE providers ADD COLUMN generated_overlay_revision INTEGER
           CHECK (generated_overlay_revision IS NULL OR generated_overlay_revision >= 1);
         CREATE UNIQUE INDEX providers_generated_owner_target
           ON providers(generated_owner_id, target)
           WHERE generated_owner_id IS NOT NULL;
         CREATE TABLE universal_provider_catalog_state (
           singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
           revision INTEGER NOT NULL CHECK (revision >= 0),
           view_sequence INTEGER NOT NULL CHECK (view_sequence >= 0)
         );
         CREATE TABLE universal_credentials (
           id TEXT PRIMARY KEY,
           bearer_token TEXT NOT NULL
         );
         CREATE TABLE universal_providers (
           id TEXT PRIMARY KEY,
           position INTEGER NOT NULL UNIQUE CHECK (position >= 0),
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           credential_id TEXT REFERENCES universal_credentials(id),
           provenance_kind TEXT,
           provenance_key TEXT,
           CHECK (
             (provenance_kind IS NULL AND provenance_key IS NULL)
             OR (provenance_kind IS NOT NULL AND provenance_key IS NOT NULL)
           )
         );
         CREATE TABLE universal_provider_targets (
           universal_provider_id TEXT NOT NULL REFERENCES universal_providers(id) ON DELETE CASCADE,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
           model TEXT NOT NULL,
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
           routing_requirement TEXT NOT NULL CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
           overlay_revision INTEGER NOT NULL CHECK (overlay_revision >= 1),
           synchronized_source_revision INTEGER CHECK (synchronized_source_revision IS NULL OR synchronized_source_revision >= 1),
           synchronized_overlay_revision INTEGER CHECK (synchronized_overlay_revision IS NULL OR synchronized_overlay_revision >= 1),
           CHECK (
             (target = 'codex' AND authentication = 'openai-bearer')
             OR (target = 'claude' AND authentication IN ('anthropic-api-key', 'anthropic-bearer'))
           ),
           CHECK (
             (synchronized_source_revision IS NULL AND synchronized_overlay_revision IS NULL)
             OR (synchronized_source_revision IS NOT NULL AND synchronized_overlay_revision IS NOT NULL)
           ),
           PRIMARY KEY (universal_provider_id, target)
         );
         CREATE TABLE universal_action_receipts (
           action_id TEXT PRIMARY KEY,
           action_kind TEXT NOT NULL,
           committed_revision INTEGER NOT NULL CHECK (committed_revision >= 0),
           outcome_json TEXT NOT NULL
         );
         CREATE TABLE universal_provider_seeds (
           preset_key TEXT PRIMARY KEY,
           seeded_provider_id TEXT
         );
         INSERT INTO universal_provider_catalog_state
           (singleton, revision, view_sequence) VALUES (1, 0, 0);",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '9' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v7(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
         "ALTER TABLE target_problems ADD COLUMN source TEXT
           CHECK (source IS NULL OR source IN (
             'codex-profile', 'claude-managed', 'claude-shared', 'claude-project',
             'claude-local', 'claude-selector', 'claude-host-managed'
           ));
         ALTER TABLE target_problems ADD COLUMN selector TEXT
           CHECK (selector IS NULL OR selector IN (
             'CLAUDE_CODE_USE_BEDROCK', 'CLAUDE_CODE_USE_VERTEX',
             'CLAUDE_CODE_USE_FOUNDRY', 'CLAUDE_CODE_USE_MANTLE',
             'CLAUDE_CODE_USE_ANTHROPIC_AWS',
             'CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST'
           ));
         CREATE TABLE target_compatibility (
           target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
           observed_version TEXT NOT NULL,
           classification TEXT NOT NULL CHECK (classification IN ('tested', 'unknown-compatible', 'incompatible')),
           acknowledged_version TEXT,
           CHECK (acknowledged_version IS NULL OR classification = 'unknown-compatible')
         );
         CREATE TABLE reconciliation_intents (
           action_id TEXT NOT NULL,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           strategy TEXT NOT NULL CHECK (strategy IN ('adopt', 'reapply', 'restore')),
           state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
           created_revision INTEGER NOT NULL CHECK (created_revision >= 0),
           before_json TEXT NOT NULL,
           desired_json TEXT NOT NULL,
           PRIMARY KEY (target, action_id)
         );",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '8' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v6(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction
        .execute_batch("ALTER TABLE target_route_state ADD COLUMN recovery_intent_id TEXT;")?;
    bind_current_recovery_intents(&transaction)?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '7' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn bind_current_recovery_intents(transaction: &Transaction<'_>) -> Result<()> {
    let routes = transaction
        .prepare(
            "SELECT target, activated_snapshot_id
             FROM target_route_state WHERE activated_snapshot_id IS NOT NULL",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>>>()?;

    for (target, snapshot_id) in routes {
        let candidates = transaction
            .prepare(
                "SELECT a.id
                 FROM activation_recovery a
                 JOIN action_receipts receipt
                   ON receipt.target = a.target AND receipt.action_id = a.action_id
                 WHERE a.target = ?1 AND a.state = 'committed'
                   AND receipt.action_kind = 'activate-provider'
                   AND receipt.committed_revision = a.created_revision + 1
                   AND json_valid(receipt.outcome_json)
                   AND json_extract(receipt.outcome_json, '$.status') = 'applied'
                   AND json_extract(receipt.outcome_json, '$.view.target') = ?1
                   AND json_extract(receipt.outcome_json,
                                    '$.view.managementRevision') = receipt.committed_revision
                   AND json_extract(receipt.outcome_json,
                                    '$.view.activatedSnapshot.id') = ?2",
            )?
            .query_map(params![target, snapshot_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>>>()?;
        match candidates.as_slice() {
            [recovery_id] => {
                transaction.execute(
                    "UPDATE target_route_state SET recovery_intent_id = ?1 WHERE target = ?2",
                    params![recovery_id, target],
                )?;
            }
            [] => {
                let committed_count: u64 = transaction.query_row(
                    "SELECT COUNT(*) FROM activation_recovery
                     WHERE target = ?1 AND state = 'committed'",
                    [&target],
                    |row| row.get(0),
                )?;
                if committed_count != 0 {
                    transaction.execute(
                        "UPDATE target_route_state SET recovery_state = 'recovery-required'
                         WHERE target = ?1",
                        [&target],
                    )?;
                }
            }
            _ => {
                transaction.execute(
                    "UPDATE target_route_state SET recovery_state = 'recovery-required'
                     WHERE target = ?1",
                    [&target],
                )?;
            }
        }
    }
    Ok(())
}

fn migrate_v5(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE target_route_state
           ADD COLUMN managed_config_version INTEGER NOT NULL DEFAULT 1
             CHECK (managed_config_version IN (1,2));",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.execute(
        "UPDATE metadata SET value = '6' WHERE key = 'schema-version'",
        [],
    )?;
    transaction.commit()
}

fn migrate_v4(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE credentials_v5 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           bearer_token TEXT NOT NULL,
           UNIQUE (target, id)
         );
         CREATE TABLE providers_v5 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
           credential_id TEXT,
           provenance_kind TEXT,
           provenance_key TEXT,
           generated_owner_id TEXT,
           routing_requirement TEXT NOT NULL DEFAULT 'direct-compatible'
             CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
           CHECK ((target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
             OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer'))),
           FOREIGN KEY (target, credential_id) REFERENCES credentials_v5(target, id)
         );
         INSERT INTO credentials_v5 SELECT id, target, bearer_token FROM credentials;
         INSERT INTO providers_v5 SELECT * FROM providers;
         DROP TABLE providers;
         DROP TABLE credentials;
         ALTER TABLE credentials_v5 RENAME TO credentials;
         ALTER TABLE providers_v5 RENAME TO providers;
         UPDATE metadata SET value = '5' WHERE key = 'schema-version';",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.commit()
}

fn migrate_v3(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE credentials_v4 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           bearer_token TEXT NOT NULL
         );
         CREATE TABLE providers_v4 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
           credential_id TEXT REFERENCES credentials_v4(id) ON DELETE SET NULL,
           provenance_kind TEXT,
           provenance_key TEXT,
           generated_owner_id TEXT,
           routing_requirement TEXT NOT NULL DEFAULT 'direct-compatible'
             CHECK (routing_requirement IN ('direct-compatible', 'takeover-required')),
           CHECK ((target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
             OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer')))
         );
         CREATE TABLE target_route_state_v4 (
           target TEXT PRIMARY KEY CHECK (target IN ('codex', 'claude')),
           management_revision INTEGER NOT NULL, view_sequence INTEGER NOT NULL,
           current_provider_id TEXT, serving_provider_id TEXT, takeover_state TEXT NOT NULL,
           route_port INTEGER, routing_credential TEXT, activated_snapshot_id TEXT,
           managed_config_path TEXT, recovery_state TEXT NOT NULL
         );
         CREATE TABLE target_problems_v4 (
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           code TEXT NOT NULL, message TEXT NOT NULL, PRIMARY KEY (target, code)
         );
         CREATE TABLE activated_snapshots_v4 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           provider_id TEXT NOT NULL, base_url TEXT NOT NULL, model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol IN ('openai-responses', 'anthropic-messages')),
           authentication TEXT NOT NULL CHECK (authentication IN ('openai-bearer', 'anthropic-api-key', 'anthropic-bearer')),
           provider_bearer_token TEXT NOT NULL, epoch TEXT NOT NULL,
           CHECK ((target = 'codex' AND protocol = 'openai-responses' AND authentication = 'openai-bearer')
             OR (target = 'claude' AND protocol = 'anthropic-messages' AND authentication IN ('anthropic-api-key', 'anthropic-bearer')))
         );
         CREATE TABLE action_receipts_v4 (
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           action_id TEXT NOT NULL, action_kind TEXT NOT NULL, committed_revision INTEGER NOT NULL,
           outcome_json TEXT NOT NULL, PRIMARY KEY (target, action_id)
         );
         CREATE TABLE activation_recovery_v4 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target IN ('codex', 'claude')),
           action_id TEXT NOT NULL, config_path TEXT NOT NULL, file_identity_json TEXT NOT NULL,
           payload_json TEXT NOT NULL,
           state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
           created_revision INTEGER NOT NULL, UNIQUE (target, action_id)
         );
         INSERT INTO credentials_v4 SELECT id, target, bearer_token FROM credentials;
         INSERT INTO providers_v4
           (id, target, position, provider_revision, name, base_url, model, protocol, authentication,
            credential_id, provenance_kind, provenance_key, generated_owner_id, routing_requirement)
           SELECT id, target, position, provider_revision, name, base_url, model, protocol, 'openai-bearer',
                  credential_id, provenance_kind, provenance_key, generated_owner_id, routing_requirement
           FROM providers;
         INSERT INTO target_route_state_v4 SELECT * FROM target_route_state;
         INSERT INTO target_problems_v4 SELECT * FROM target_problems;
         INSERT INTO activated_snapshots_v4
           (id, target, provider_id, base_url, model, protocol, authentication, provider_bearer_token, epoch)
           SELECT id, target, provider_id, base_url, model, 'openai-responses', 'openai-bearer', provider_bearer_token, epoch
           FROM activated_snapshots;
         INSERT INTO target_route_state_v4
           (target, management_revision, view_sequence, takeover_state, recovery_state)
           VALUES ('claude', 0, 0, 'inactive', 'clean');",
    )?;

    let receipts = {
        let mut statement = transaction.prepare(
            "SELECT action_id, action_kind, committed_revision, outcome_json FROM action_receipts",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?
    };
    for (action_id, action_kind, committed_revision, outcome_json) in receipts {
        let legacy: LegacyV3ActionOutcome =
            serde_json::from_str(&outcome_json).map_err(json_conversion_error)?;
        let outcome_json =
            serde_json::to_string(&legacy.into_v4()).map_err(json_conversion_error)?;
        transaction.execute(
            "INSERT INTO action_receipts_v4 (target, action_id, action_kind, committed_revision, outcome_json)
             VALUES ('codex', ?1, ?2, ?3, ?4)",
            params![action_id, action_kind, committed_revision, outcome_json],
        )?;
    }
    let recovery = {
        let mut statement = transaction.prepare(
            "SELECT id, action_id, config_path, file_identity_json, before_owned_json, desired_owned_json, state, created_revision
             FROM activation_recovery",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, u64>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?
    };
    for (
        id,
        action_id,
        config_path,
        file_identity_json,
        before,
        desired,
        state,
        created_revision,
    ) in recovery
    {
        let before = serde_json::from_str(&before).map_err(json_conversion_error)?;
        let desired = serde_json::from_str(&desired).map_err(json_conversion_error)?;
        let payload_json = serde_json::to_string(&LegacyRecoveryPayload {
            target: Target::Codex,
            before,
            desired,
        })
        .map_err(json_conversion_error)?;
        transaction.execute(
            "INSERT INTO activation_recovery_v4
             (id, target, action_id, config_path, file_identity_json, payload_json, state, created_revision)
             VALUES (?1, 'codex', ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, action_id, config_path, file_identity_json, payload_json, state, created_revision],
        )?;
    }
    transaction.execute_batch(
        "DROP TABLE activation_recovery;
         DROP TABLE action_receipts;
         DROP TABLE activated_snapshots;
         DROP TABLE target_problems;
         DROP TABLE target_route_state;
         DROP TABLE providers;
         DROP TABLE credentials;
         ALTER TABLE credentials_v4 RENAME TO credentials;
         ALTER TABLE providers_v4 RENAME TO providers;
         ALTER TABLE target_route_state_v4 RENAME TO target_route_state;
         ALTER TABLE target_problems_v4 RENAME TO target_problems;
         ALTER TABLE activated_snapshots_v4 RENAME TO activated_snapshots;
         ALTER TABLE action_receipts_v4 RENAME TO action_receipts;
         ALTER TABLE activation_recovery_v4 RENAME TO activation_recovery;
         UPDATE metadata SET value = '4' WHERE key = 'schema-version';",
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.commit()
}

fn migrate_v1(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE credentials (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target = 'codex'),
           bearer_token TEXT NOT NULL
         );
         CREATE TABLE providers_v2 (
           id TEXT PRIMARY KEY,
           target TEXT NOT NULL CHECK (target = 'codex'),
           position INTEGER NOT NULL CHECK (position >= 0),
           provider_revision INTEGER NOT NULL CHECK (provider_revision >= 1),
           name TEXT NOT NULL,
           base_url TEXT NOT NULL,
           model TEXT NOT NULL,
           protocol TEXT NOT NULL CHECK (protocol = 'openai-responses'),
           credential_id TEXT REFERENCES credentials(id) ON DELETE SET NULL,
           provenance_kind TEXT,
           provenance_key TEXT,
           generated_owner_id TEXT
         );",
    )?;

    let providers = {
        let mut statement = transaction.prepare(
            "SELECT p.id, p.target, p.name, p.base_url, p.model, c.bearer_token
             FROM providers p
             LEFT JOIN provider_credentials c ON c.provider_id = p.id
             ORDER BY p.rowid",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?
    };
    for (position, (id, target, name, base_url, model, credential)) in
        providers.into_iter().enumerate()
    {
        let credential_id = credential.as_ref().map(|_| id.as_str());
        if let Some(credential) = credential {
            transaction.execute(
                "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, ?3)",
                params![id, target, credential],
            )?;
        }
        transaction.execute(
            "INSERT INTO providers_v2
             (id, target, position, provider_revision, name, base_url, model, protocol, credential_id,
              provenance_kind, provenance_key, generated_owner_id)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, 'openai-responses', ?7, NULL, NULL, NULL)",
            params![id, target, position as u32, name, base_url, model, credential_id],
        )?;
    }
    migrate_v1_receipts(&transaction)?;
    transaction.execute_batch(
        "DROP TABLE provider_credentials;
         DROP TABLE providers;
         ALTER TABLE providers_v2 RENAME TO providers;
         UPDATE metadata SET value = '2' WHERE key = 'schema-version';",
    )?;
    transaction.commit()?;
    Ok(())
}

fn migrate_v2(connection: &mut Connection) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE providers
         ADD COLUMN routing_requirement TEXT NOT NULL DEFAULT 'direct-compatible'
           CHECK (routing_requirement IN ('direct-compatible', 'takeover-required'));",
    )?;
    migrate_v2_receipts(&transaction)?;
    transaction.execute(
        "UPDATE metadata SET value = '3' WHERE key = 'schema-version'",
        [],
    )?;
    let mut foreign_key_check = transaction.prepare("PRAGMA foreign_key_check")?;
    if foreign_key_check.query([])?.next()?.is_some() {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    drop(foreign_key_check);
    transaction.commit()?;
    Ok(())
}

fn migrate_v1_receipts(transaction: &Transaction<'_>) -> Result<()> {
    let receipts = {
        let mut statement =
            transaction.prepare("SELECT action_id, outcome_json FROM action_receipts")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>>>()?
    };

    for (action_id, outcome_json) in receipts {
        let legacy: LegacyActionOutcome =
            serde_json::from_str(&outcome_json).map_err(json_conversion_error)?;
        let outcome = legacy.into_v2()?;
        let outcome_json = serde_json::to_string(&outcome).map_err(json_conversion_error)?;
        transaction.execute(
            "UPDATE action_receipts SET outcome_json = ?1 WHERE action_id = ?2",
            params![outcome_json, action_id],
        )?;
    }
    Ok(())
}

fn migrate_v2_receipts(transaction: &Transaction<'_>) -> Result<()> {
    let receipts = {
        let mut statement =
            transaction.prepare("SELECT action_id, outcome_json FROM action_receipts")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>>>()?
    };

    for (action_id, outcome_json) in receipts {
        let legacy: LegacyV2ActionOutcome =
            serde_json::from_str(&outcome_json).map_err(json_conversion_error)?;
        let outcome = legacy.into_v3();
        let outcome_json = serde_json::to_string(&outcome).map_err(json_conversion_error)?;
        transaction.execute(
            "UPDATE action_receipts SET outcome_json = ?1 WHERE action_id = ?2",
            params![outcome_json, action_id],
        )?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct LegacyActionOutcome {
    status: ActionStatus,
    view: LegacyTargetView,
}

impl LegacyActionOutcome {
    fn into_v2(self) -> Result<LegacyV2ActionOutcome> {
        let current_provider_id = self.view.current_provider_id.clone();
        let activated_provider_id = self
            .view
            .activated_snapshot
            .as_ref()
            .map(|snapshot| snapshot.provider_id);
        let providers = self
            .view
            .providers
            .into_iter()
            .enumerate()
            .map(|(position, provider)| {
                provider.into_v2(
                    position,
                    current_provider_id.as_deref(),
                    activated_provider_id,
                )
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(LegacyV2ActionOutcome {
            status: self.status,
            view: LegacyV2TargetView {
                target: self.view.target,
                management_revision: self.view.management_revision,
                view_sequence: self.view.view_sequence,
                service: self.view.service,
                mode: self.view.mode,
                takeover: self.view.takeover,
                providers,
                provider_presets: super::providers::provider_presets(Target::Codex)
                    .into_iter()
                    .map(|preset| LegacyV3ProviderPresetView {
                        key: preset.key,
                        base_url: preset.base_url,
                        model: preset.model,
                        protocol: preset.protocol,
                    })
                    .collect(),
                current_provider_id,
                serving_provider_id: self.view.serving_provider_id,
                managed_configuration: self.view.managed_configuration,
                recovery: self.view.recovery,
                activated_snapshot: self.view.activated_snapshot,
                problems: self.view.problems,
            },
        })
    }
}

#[derive(Deserialize, Serialize)]
struct LegacyV2ActionOutcome {
    status: ActionStatus,
    view: LegacyV2TargetView,
}

impl LegacyV2ActionOutcome {
    fn into_v3(self) -> ActionOutcome {
        ActionOutcome {
            status: self.status,
            view: TargetView {
                target: self.view.target,
                management_revision: self.view.management_revision,
                view_sequence: self.view.view_sequence,
                service: self.view.service,
                mode: self.view.mode,
                takeover: self.view.takeover,
                route_health: RouteHealthView {
                    state: RouteHealthState::Unobserved,
                },
                providers: self
                    .view
                    .providers
                    .into_iter()
                    .map(LegacyV2ProviderView::into_v3)
                    .collect(),
                provider_presets: self
                    .view
                    .provider_presets
                    .into_iter()
                    .map(|preset| ProviderPresetView {
                        key: preset.key,
                        base_url: preset.base_url,
                        model: preset.model,
                        protocol: preset.protocol,
                        authentication: ProviderAuthentication::OpenaiBearer,
                    })
                    .collect(),
                current_provider_id: self.view.current_provider_id,
                serving_provider_id: self.view.serving_provider_id,
                managed_configuration: self.view.managed_configuration,
                recovery: self.view.recovery,
                activated_snapshot: self
                    .view
                    .activated_snapshot
                    .map(LegacySnapshotView::into_v4),
                failover: FailoverView::default(),
                problems: self.view.problems,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyTargetView {
    target: Target,
    management_revision: u64,
    view_sequence: u64,
    service: ServiceView,
    mode: String,
    takeover: TakeoverView,
    providers: Vec<LegacyProviderView>,
    current_provider_id: Option<String>,
    serving_provider_id: Option<String>,
    managed_configuration: ManagedConfigurationView,
    recovery: RecoveryView,
    activated_snapshot: Option<LegacySnapshotView>,
    problems: Vec<ControlProblem>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyV2TargetView {
    target: Target,
    management_revision: u64,
    view_sequence: u64,
    service: ServiceView,
    mode: String,
    takeover: TakeoverView,
    providers: Vec<LegacyV2ProviderView>,
    provider_presets: Vec<LegacyV3ProviderPresetView>,
    current_provider_id: Option<String>,
    serving_provider_id: Option<String>,
    managed_configuration: ManagedConfigurationView,
    recovery: RecoveryView,
    activated_snapshot: Option<LegacySnapshotView>,
    problems: Vec<ControlProblem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyProviderView {
    id: String,
    name: String,
    base_url: String,
    model: String,
    credential: CredentialPresence,
}

impl LegacyProviderView {
    fn into_v2(
        self,
        position: usize,
        current_provider_id: Option<&str>,
        activated_provider_id: Option<uuid::Uuid>,
    ) -> Result<LegacyV2ProviderView> {
        let id = uuid::Uuid::parse_str(&self.id).map_err(json_conversion_error)?;
        let mut missing_fields = Vec::new();
        if self.base_url.is_empty() {
            missing_fields.push(ProviderRequirement::BaseUrl);
        }
        if self.model.is_empty() {
            missing_fields.push(ProviderRequirement::Model);
        }
        if self.credential == CredentialPresence::Missing {
            missing_fields.push(ProviderRequirement::Credential);
        }
        let mut active_references = Vec::new();
        if current_provider_id == Some(self.id.as_str()) {
            active_references.push(ProviderReferenceView::Current);
        }
        if activated_provider_id == Some(id) {
            active_references.push(ProviderReferenceView::ActivatedSnapshot);
        }

        Ok(LegacyV2ProviderView {
            id,
            position: u32::try_from(position).map_err(json_conversion_error)?,
            provider_revision: 1,
            name: self.name,
            base_url: self.base_url,
            model: self.model,
            protocol: ProviderProtocol::OpenaiResponses,
            credential: self.credential,
            completeness: if missing_fields.is_empty() {
                ProviderCompleteness::Complete
            } else {
                ProviderCompleteness::Incomplete
            },
            missing_fields,
            provenance: None,
            generated: false,
            active_references,
        })
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyV2ProviderView {
    id: uuid::Uuid,
    position: u32,
    provider_revision: u64,
    name: String,
    base_url: String,
    model: String,
    protocol: ProviderProtocol,
    credential: CredentialPresence,
    completeness: ProviderCompleteness,
    missing_fields: Vec<ProviderRequirement>,
    provenance: Option<ProviderProvenanceView>,
    generated: bool,
    active_references: Vec<ProviderReferenceView>,
}

impl LegacyV2ProviderView {
    fn into_v3(self) -> ProviderView {
        ProviderView {
            id: self.id,
            position: self.position,
            provider_revision: self.provider_revision,
            name: self.name,
            base_url: self.base_url,
            model: self.model,
            protocol: self.protocol,
            authentication: ProviderAuthentication::OpenaiBearer,
            routing_requirement: ProviderRoutingRequirement::DirectCompatible,
            credential: self.credential,
            completeness: self.completeness,
            missing_fields: self.missing_fields,
            provenance: self.provenance,
            import_provenance: None,
            imported_current: false,
            generated: self.generated,
            universal_provider_id: None,
            synchronization: None,
            ownership: ProviderFieldOwnershipView::target_provider(),
            route_health: RouteHealthView::default(),
            active_references: self.active_references,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacySnapshotView {
    id: uuid::Uuid,
    provider_id: uuid::Uuid,
    model: String,
    epoch: uuid::Uuid,
}

impl LegacySnapshotView {
    fn into_v4(self) -> ActivatedSnapshotView {
        ActivatedSnapshotView {
            id: self.id,
            provider_id: self.provider_id,
            model: self.model,
            protocol: ProviderProtocol::OpenaiResponses,
            authentication: ProviderAuthentication::OpenaiBearer,
            epoch: self.epoch,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyV3ActionOutcome {
    status: ActionStatus,
    view: LegacyV3TargetView,
}

impl LegacyV3ActionOutcome {
    fn into_v4(self) -> ActionOutcome {
        ActionOutcome {
            status: self.status,
            view: TargetView {
                target: self.view.target,
                management_revision: self.view.management_revision,
                view_sequence: self.view.view_sequence,
                service: self.view.service,
                mode: self.view.mode,
                takeover: self.view.takeover,
                route_health: RouteHealthView {
                    state: RouteHealthState::Unobserved,
                },
                providers: self
                    .view
                    .providers
                    .into_iter()
                    .map(LegacyV3ProviderView::into_v4)
                    .collect(),
                provider_presets: self
                    .view
                    .provider_presets
                    .into_iter()
                    .map(|preset| ProviderPresetView {
                        key: preset.key,
                        base_url: preset.base_url,
                        model: preset.model,
                        protocol: preset.protocol,
                        authentication: ProviderAuthentication::OpenaiBearer,
                    })
                    .collect(),
                current_provider_id: self.view.current_provider_id,
                serving_provider_id: self.view.serving_provider_id,
                managed_configuration: self.view.managed_configuration,
                recovery: self.view.recovery,
                activated_snapshot: self
                    .view
                    .activated_snapshot
                    .map(LegacySnapshotView::into_v4),
                failover: FailoverView::default(),
                problems: self.view.problems,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyV3TargetView {
    target: Target,
    management_revision: u64,
    view_sequence: u64,
    service: ServiceView,
    mode: String,
    takeover: TakeoverView,
    providers: Vec<LegacyV3ProviderView>,
    provider_presets: Vec<LegacyV3ProviderPresetView>,
    current_provider_id: Option<String>,
    serving_provider_id: Option<String>,
    managed_configuration: ManagedConfigurationView,
    recovery: RecoveryView,
    activated_snapshot: Option<LegacySnapshotView>,
    problems: Vec<ControlProblem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyV3ProviderView {
    id: uuid::Uuid,
    position: u32,
    provider_revision: u64,
    name: String,
    base_url: String,
    model: String,
    protocol: ProviderProtocol,
    routing_requirement: ProviderRoutingRequirement,
    credential: CredentialPresence,
    completeness: ProviderCompleteness,
    missing_fields: Vec<ProviderRequirement>,
    provenance: Option<ProviderProvenanceView>,
    generated: bool,
    active_references: Vec<ProviderReferenceView>,
}

impl LegacyV3ProviderView {
    fn into_v4(self) -> ProviderView {
        ProviderView {
            id: self.id,
            position: self.position,
            provider_revision: self.provider_revision,
            name: self.name,
            base_url: self.base_url,
            model: self.model,
            protocol: self.protocol,
            authentication: ProviderAuthentication::OpenaiBearer,
            routing_requirement: self.routing_requirement,
            credential: self.credential,
            completeness: self.completeness,
            missing_fields: self.missing_fields,
            provenance: self.provenance,
            import_provenance: None,
            imported_current: false,
            generated: self.generated,
            universal_provider_id: None,
            synchronization: None,
            ownership: ProviderFieldOwnershipView::target_provider(),
            route_health: RouteHealthView::default(),
            active_references: self.active_references,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyV3ProviderPresetView {
    key: String,
    base_url: String,
    model: String,
    protocol: ProviderProtocol,
}

#[derive(Serialize)]
struct LegacyRecoveryPayload {
    target: Target,
    before: serde_json::Value,
    desired: serde_json::Value,
}

fn json_conversion_error(error: impl std::error::Error + Send + Sync + 'static) -> Error {
    Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
}
