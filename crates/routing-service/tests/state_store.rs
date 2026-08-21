use std::{fs, path::PathBuf};

use muxvia_routing::{
    claude::ClaudeConfigCodec,
    codex::CodexConfigCodec,
    control::protocol::{
        ActionStatus, ClientFrame, ControlOperation, CredentialEdit, CredentialPresence,
        ProviderAuthentication, ProviderProtocol, Target, TargetAction,
    },
    domain::activation::ActivatedSnapshot,
    domain::provider::normalize_provider_base_url,
    home::MuxviaHome,
    state::{
        ActivationCommit, ActivationRuntime, CompatibilityClassification, RecoveryIntent,
        RecoveryPayload, StateStore,
    },
};
use secrecy::SecretString;
use tokio_rusqlite::rusqlite::{Connection, types::ValueRef};
use uuid::Uuid;

const V12_SCHEMA: &str = include_str!("fixtures/state-schema-v12.sql");

struct StoreFixture {
    root: PathBuf,
    home: MuxviaHome,
    store: std::sync::Arc<StateStore>,
}

impl StoreFixture {
    async fn new() -> Self {
        let root = std::env::temp_dir().join(format!("muxvia-state-test-{}", Uuid::new_v4()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        let home = MuxviaHome::from_user_home(&user_home);
        let store = std::sync::Arc::new(StateStore::open(&home).await.unwrap());
        Self { root, home, store }
    }
}

impl Drop for StoreFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn fixed_uuid(last_byte: u8) -> Uuid {
    let mut bytes = [0; 16];
    bytes[15] = last_byte;
    Uuid::from_bytes(bytes)
}

fn fingerprint_bytes(mut fingerprint: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        fingerprint ^= u64::from(*byte);
        fingerprint = fingerprint.wrapping_mul(0x100_0000_01b3);
    }
    fingerprint
}

fn v12_state_fingerprint(connection: &Connection) -> Vec<(String, u64)> {
    let tables = connection
        .prepare(
            "SELECT name FROM sqlite_schema
             WHERE type = 'table'
               AND name NOT IN ('metadata', 'request_records', 'pricing_snapshots',
                                'sqlite_sequence')
             ORDER BY name",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|table| {
            let escaped = table.replace('"', "\"\"");
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{escaped}\" ORDER BY rowid"))
                .unwrap();
            let columns = statement.column_count();
            let mut rows = statement.query([]).unwrap();
            let mut fingerprint = fingerprint_bytes(0xcbf2_9ce4_8422_2325, table.as_bytes());
            while let Some(row) = rows.next().unwrap() {
                for column in 0..columns {
                    match row.get_ref(column).unwrap() {
                        ValueRef::Null => fingerprint = fingerprint_bytes(fingerprint, b"n"),
                        ValueRef::Integer(value) => {
                            fingerprint = fingerprint_bytes(fingerprint, b"i");
                            fingerprint = fingerprint_bytes(fingerprint, &value.to_le_bytes());
                        }
                        ValueRef::Real(value) => {
                            fingerprint = fingerprint_bytes(fingerprint, b"r");
                            fingerprint = fingerprint_bytes(fingerprint, &value.to_le_bytes());
                        }
                        ValueRef::Text(value) => {
                            fingerprint = fingerprint_bytes(fingerprint, b"t");
                            fingerprint = fingerprint_bytes(fingerprint, value);
                        }
                        ValueRef::Blob(value) => {
                            fingerprint = fingerprint_bytes(fingerprint, b"b");
                            fingerprint = fingerprint_bytes(fingerprint, value);
                        }
                    }
                }
            }
            (table, fingerprint)
        })
        .collect()
}

fn raw_save_provider(
    name: &str,
    base_url: &str,
    model: &str,
    credential: &str,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "save-provider",
        "name": name,
        "baseUrl": base_url,
        "model": model,
        "credential": credential
    })
}

#[tokio::test]
async fn new_database_target_view_matches_the_canonical_protocol_fixture() {
    let fixture = StoreFixture::new().await;
    let view = fixture.store.target_view().await.unwrap();
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/fixtures/initial-target-view.json");
    let mut expected: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture_path).unwrap()).unwrap();
    expected["service"]["epoch"] = serde_json::json!(view.service.epoch);

    assert_eq!(serde_json::to_value(view).unwrap(), expected);
}

#[tokio::test]
async fn fresh_schema_reopens_with_codex_and_claude_route_rows() {
    let root = std::env::temp_dir().join(format!("muxvia-schema-reopen-{}", Uuid::new_v4()));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    let first = StateStore::open(&home).await.unwrap();
    drop(first);
    let second = StateStore::open(&home).await.unwrap();
    drop(second);

    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    let (version, targets): (String, Vec<String>) = database
        .call(|connection| -> tokio_rusqlite::rusqlite::Result<_> {
            let version = connection.query_row(
                "SELECT value FROM metadata WHERE key = 'schema-version'",
                [],
                |row| row.get(0),
            )?;
            let targets = connection
                .prepare("SELECT target FROM target_route_state ORDER BY target")?
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()?;
            Ok((version, targets))
        })
        .await
        .unwrap();
    assert_eq!(version, "13");
    assert_eq!(targets, ["claude", "codex"]);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn request_record_schema_bounds_failed_payload_and_freezes_pricing_snapshot() {
    let fixture = StoreFixture::new().await;
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let oversized = database
        .call(|connection| {
            connection.execute(
                "INSERT INTO request_records
                   (id, target, plan_id, plan_epoch, provider_id, provider_name, model, protocol,
                    started_at_unix_ms, finished_at_unix_ms, latency_ms, outcome, http_status,
                    usage_observed, input_tokens, cached_input_tokens,
                    cache_creation_input_tokens, output_tokens, error_payload,
                    error_payload_truncated)
                 VALUES (?1, 'codex', ?2, ?3, NULL, NULL, 'gpt-5.6', 'openai-responses',
                         1, 2, 1, 'upstream-error', 503, 0, 0, 0, 0, 0, ?4, 1)",
                tokio_rusqlite::rusqlite::params![
                    fixed_uuid(131).to_string(),
                    fixed_uuid(132).to_string(),
                    fixed_uuid(133).to_string(),
                    vec![b'x'; 65_537],
                ],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await;
    assert!(
        oversized.is_err(),
        "accepted an error payload larger than 64 KiB"
    );

    let wrong_outcome = database
        .call(|connection| {
            connection.execute(
                "INSERT INTO request_records
                   (id, target, plan_id, plan_epoch, provider_id, provider_name, model, protocol,
                    started_at_unix_ms, finished_at_unix_ms, latency_ms, outcome, http_status,
                    usage_observed, input_tokens, cached_input_tokens,
                    cache_creation_input_tokens, output_tokens, error_payload,
                    error_payload_truncated)
                 VALUES (?1, 'codex', ?2, ?3, NULL, NULL, 'gpt-5.6', 'openai-responses',
                         1, 2, 1, 'transport-error', NULL, 0, 0, 0, 0, 0, ?4, 0)",
                tokio_rusqlite::rusqlite::params![
                    fixed_uuid(137).to_string(),
                    fixed_uuid(138).to_string(),
                    fixed_uuid(139).to_string(),
                    b"failed transport detail".to_vec(),
                ],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await;
    assert!(
        wrong_outcome.is_err(),
        "accepted a failed upstream payload for a non-upstream outcome"
    );

    database
        .call(|connection| {
            connection.execute(
                "INSERT INTO request_records
                   (id, target, plan_id, plan_epoch, provider_id, provider_name, model, protocol,
                    started_at_unix_ms, finished_at_unix_ms, latency_ms, outcome, http_status,
                    usage_observed, input_tokens, cached_input_tokens,
                    cache_creation_input_tokens, output_tokens, error_payload,
                    error_payload_truncated)
                 VALUES (?1, 'codex', ?2, ?3, NULL, NULL, 'gpt-5.6', 'openai-responses',
                         1, 2, 1, 'success', 200, 1, 12, 2, 0, 4, NULL, 0)",
                tokio_rusqlite::rusqlite::params![
                    fixed_uuid(134).to_string(),
                    fixed_uuid(135).to_string(),
                    fixed_uuid(136).to_string(),
                ],
            )?;
            connection.execute(
                "INSERT INTO pricing_snapshots
                   (request_record_id, catalog_version, source, source_model,
                    input_nano_usd_per_million, output_nano_usd_per_million,
                    cache_read_multiplier_ppm, cache_creation_multiplier_ppm,
                    priced_at_unix_ms, estimated_cost_nano_usd)
                 VALUES (?1, 'muxvia-0.1.0', 'models.dev', 'gpt-5.6',
                         2000000000, 10000000000, 100000, 1250000, 2, 42000)",
                [fixed_uuid(134).to_string()],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    let update = database
        .call(|connection| {
            connection.execute(
                "UPDATE pricing_snapshots SET estimated_cost_nano_usd = 1",
                [],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await;
    assert!(update.is_err(), "mutated an immutable Pricing Snapshot");
    let record_update = database
        .call(|connection| {
            connection.execute("UPDATE request_records SET model = 'rewritten'", [])?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await;
    assert!(
        record_update.is_err(),
        "mutated an immutable Request Record"
    );
}

#[tokio::test]
async fn schema_v13_migration_preserves_v12_target_state() {
    let root = std::env::temp_dir().join(format!("muxvia-schema-v12-{}", Uuid::new_v4()));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    fs::create_dir_all(home.database_path().parent().unwrap()).unwrap();
    let database = Connection::open(home.database_path()).unwrap();
    database.execute_batch(V12_SCHEMA).unwrap();
    database
        .execute_batch(
            "INSERT INTO credentials (id, target, bearer_token)
               VALUES ('00000000-0000-4000-8000-000000000131', 'codex',
                       'REQUEST_HISTORY_MIGRATION_SECRET_13103');
             INSERT INTO providers
               (id, target, position, provider_revision, name, base_url, model, protocol,
                authentication, credential_id, routing_requirement)
               VALUES ('00000000-0000-4000-8000-000000000132', 'codex', 0, 3,
                       'Historical', 'https://history.example/v1', 'history-model',
                       'openai-responses', 'openai-bearer',
                       '00000000-0000-4000-8000-000000000131', 'direct-compatible');
             INSERT INTO universal_credentials (id, bearer_token)
               VALUES ('00000000-0000-4000-8000-000000000133',
                       'REQUEST_HISTORY_UNIVERSAL_SECRET_13104');
             INSERT INTO universal_providers
               (id, position, provider_revision, name, base_url, credential_id)
               VALUES ('00000000-0000-4000-8000-000000000134', 0, 2, 'Universal',
                       'https://universal.example/v1',
                       '00000000-0000-4000-8000-000000000133');
             INSERT INTO activation_recovery
               (id, target, action_id, config_path, file_identity_json, payload_json,
                state, created_revision)
               VALUES ('00000000-0000-4000-8000-000000000135', 'codex',
                       '00000000-0000-4000-8000-000000000136', '/tmp/history.json', '{}',
                       '{\"credential\":\"REQUEST_HISTORY_RECOVERY_SECRET_13105\"}',
                       'rolled-back', 0);
             INSERT INTO action_receipts
               (target, action_id, action_kind, committed_revision, outcome_json)
               VALUES ('codex', '00000000-0000-4000-8000-000000000137',
                       'save-provider', 0, '{\"status\":\"replayed\"}');
             INSERT INTO subscription_account_recovery_intents
               (id, action_id, operation, state, before_sha256, desired_sha256,
                created_revision)
               VALUES ('00000000-0000-4000-8000-000000000138',
                       '00000000-0000-4000-8000-000000000139', 'set-default',
                       'committed', 'before', 'desired', 0);
             INSERT INTO subscription_account_action_receipts
               (action_id, action_kind, action_json, committed_revision, outcome_json)
               VALUES ('00000000-0000-4000-8000-000000000139', 'set-default', '{}', 0,
                       '{\"status\":\"replayed\"}');",
        )
        .unwrap();
    let before = v12_state_fingerprint(&database);
    drop(database);

    drop(StateStore::open(&home).await.unwrap());
    let database = Connection::open(home.database_path()).unwrap();
    let version: String = database
        .query_row(
            "SELECT value FROM metadata WHERE key = 'schema-version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let after = v12_state_fingerprint(&database);
    let tables: u64 = database
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name IN ('request_records', 'pricing_snapshots')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "13");
    assert_eq!(
        after, before,
        "schema-v13 migration changed existing v12 state"
    );
    assert_eq!(tables, 2);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn schema_v13_failed_migration_rolls_back_then_reruns() {
    let root = std::env::temp_dir().join(format!("muxvia-schema-v13-failure-{}", Uuid::new_v4()));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    fs::create_dir_all(home.database_path().parent().unwrap()).unwrap();
    let database = Connection::open(home.database_path()).unwrap();
    database.execute_batch(V12_SCHEMA).unwrap();
    database
        .execute_batch("CREATE TABLE request_records (collision TEXT NOT NULL);")
        .unwrap();
    drop(database);

    assert!(StateStore::open(&home).await.is_err());
    let database = Connection::open(home.database_path()).unwrap();
    let failed: (String, u64, u64) = database
        .query_row(
            "SELECT
               (SELECT value FROM metadata WHERE key = 'schema-version'),
               (SELECT COUNT(*) FROM sqlite_schema
                  WHERE type = 'table' AND name = 'pricing_snapshots'),
               (SELECT COUNT(*) FROM sqlite_schema
                  WHERE type = 'trigger' AND name = 'pricing_snapshots_immutable')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(failed, ("12".into(), 0, 0));
    database.execute("DROP TABLE request_records", []).unwrap();
    drop(database);

    drop(StateStore::open(&home).await.unwrap());
    let database = Connection::open(home.database_path()).unwrap();
    let rerun: (String, u64, u64) = database
        .query_row(
            "SELECT
               (SELECT value FROM metadata WHERE key = 'schema-version'),
               (SELECT COUNT(*) FROM sqlite_schema
                  WHERE type = 'table' AND name = 'pricing_snapshots'),
               (SELECT COUNT(*) FROM sqlite_schema
                  WHERE type = 'trigger' AND name = 'pricing_snapshots_immutable')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(rerun, ("13".into(), 1, 1));
    let _ = fs::remove_dir_all(root);
}

// Catches compatibility persistence that either shares acknowledgement between
// Targets, retains it after a version change, or treats an impossible row as safe.
#[tokio::test]
async fn reconciliation_compatibility_acknowledgement_is_exact_and_fails_closed() {
    let fixture = StoreFixture::new().await;
    let unknown = fixture
        .store
        .record_compatibility(
            Target::Codex,
            "0.42.0".into(),
            CompatibilityClassification::UnknownCompatible,
        )
        .await
        .unwrap();
    assert!(unknown.acknowledgement_required);

    let acknowledged = fixture
        .store
        .acknowledge_compatibility(Target::Codex, "0.42.0")
        .await
        .unwrap();
    assert!(!acknowledged.acknowledgement_required);

    let claude = fixture
        .store
        .record_compatibility(
            Target::Claude,
            "0.42.0".into(),
            CompatibilityClassification::UnknownCompatible,
        )
        .await
        .unwrap();
    assert!(claude.acknowledgement_required);

    let changed_version = fixture
        .store
        .record_compatibility(
            Target::Codex,
            "0.43.0".into(),
            CompatibilityClassification::UnknownCompatible,
        )
        .await
        .unwrap();
    assert!(changed_version.acknowledgement_required);

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(|connection| {
            connection.execute_batch("PRAGMA ignore_check_constraints = ON;")?;
            connection.execute(
                "UPDATE target_compatibility
                 SET classification = 'tested', acknowledged_version = '0.43.0'
                 WHERE target = 'codex'",
                [],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    let error = fixture
        .store
        .compatibility_for(Target::Codex)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "state store contains invalid compatibility state"
    );
}

#[tokio::test]
async fn managed_config_version_prepare_exposes_valid_target_ownership_version() {
    let fixture = StoreFixture::new().await;
    for target in [Target::Codex, Target::Claude] {
        let result = fixture
            .store
            .prepare_activation_for(target, Uuid::new_v4(), 0)
            .await
            .unwrap();
        let failure = match result {
            Ok(_) => panic!("missing Provider unexpectedly prepared"),
            Err(failure) => failure,
        };
        assert_eq!(failure.problem.code, "incomplete-provider");

        let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
            .await
            .unwrap();
        let version = database
            .call(move |connection| {
                connection.query_row(
                    "SELECT managed_config_version FROM target_route_state WHERE target = ?1",
                    [target.as_str()],
                    |row| row.get::<_, u32>(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(version, 1);
    }

    let created = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            0,
            serde_json::json!({
                "kind": "create-provider",
                "name": "Claude",
                "baseUrl": "https://api.anthropic.com/v1",
                "model": "claude-test",
                "credential": { "kind": "replace", "value": "secret" },
                "authentication": "anthropic-bearer",
                "presetKey": "anthropic-api-messages"
            }),
        )
        .await
        .unwrap();
    let preparation = fixture
        .store
        .prepare_activation_for(Target::Claude, created.view.providers[0].id, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(preparation.managed_config_version, 1);
}

#[tokio::test]
async fn managed_config_version_invalid_pairs_persist_only_target_recovery_required() {
    for (target, invalid_version) in [
        (Target::Claude, 99_i64),
        (Target::Codex, 2_i64),
        (Target::Claude, -1_i64),
        (Target::Claude, i64::from(u32::MAX) + 1),
    ] {
        let fixture = StoreFixture::new().await;
        let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| {
                connection.execute_batch("PRAGMA ignore_check_constraints = ON;")?;
                connection.execute(
                    "UPDATE target_route_state SET managed_config_version = ?1 WHERE target = ?2",
                    tokio_rusqlite::rusqlite::params![invalid_version, target.as_str()],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        drop(database);

        let prepared = fixture
            .store
            .prepare_activation_for(target, Uuid::new_v4(), 0)
            .await;
        let result = match prepared {
            Ok(result) => result,
            Err(_) => panic!("invalid ownership version escaped as a generic State failure"),
        };
        let failure = match result {
            Ok(_) => panic!("invalid ownership version unexpectedly prepared"),
            Err(failure) => failure,
        };
        assert_eq!(failure.problem.code, "recovery-required");
        assert_eq!(failure.authoritative_view.target, target);
        assert_eq!(
            failure.authoritative_view.recovery.state,
            "recovery-required"
        );
        assert_eq!(
            fixture
                .store
                .target_view_for(target)
                .await
                .unwrap()
                .recovery
                .state,
            "recovery-required"
        );
        let peer = match target {
            Target::Codex => Target::Claude,
            Target::Claude => Target::Codex,
        };
        assert_eq!(
            fixture
                .store
                .target_view_for(peer)
                .await
                .unwrap()
                .recovery
                .state,
            "clean"
        );
    }
}

#[tokio::test]
async fn managed_config_version_startup_audit_marks_only_an_invalid_target() {
    let fixture = StoreFixture::new().await;
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(|connection| {
            connection.execute_batch("PRAGMA ignore_check_constraints = ON;")?;
            connection.execute(
                "UPDATE target_route_state SET managed_config_version = 99 WHERE target = 'claude'",
                [],
            )?;
            Ok::<(), tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    drop(database);

    let reopened = StateStore::open(&fixture.home).await.unwrap();
    assert_eq!(
        reopened
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .recovery
            .state,
        "recovery-required"
    );
    assert_eq!(
        reopened.target_view().await.unwrap().recovery.state,
        "clean"
    );
}

#[tokio::test]
async fn managed_config_state_validation_is_identical_at_preparation_and_startup() {
    for malformed_state in [
        "legacy-takeover-without-snapshot",
        "direct-with-route-runtime",
        "takeover-without-routing-credential",
        "takeover-with-invalid-route-port",
    ] {
        for boundary in ["preparation", "startup"] {
            let fixture = StoreFixture::new().await;
            seed_malformed_claude_committed_state(&fixture.home, malformed_state).await;

            let store = if boundary == "startup" {
                std::sync::Arc::new(StateStore::open(&fixture.home).await.unwrap())
            } else {
                std::sync::Arc::clone(&fixture.store)
            };
            if boundary == "preparation" {
                let prepared = store
                    .prepare_activation_for(Target::Claude, Uuid::new_v4(), 999)
                    .await
                    .unwrap();
                let failure = match prepared {
                    Ok(_) => panic!("malformed committed state unexpectedly prepared"),
                    Err(failure) => failure,
                };
                assert_eq!(failure.problem.code, "recovery-required");
            }

            assert_eq!(
                store
                    .target_view_for(Target::Claude)
                    .await
                    .unwrap()
                    .recovery
                    .state,
                "recovery-required",
                "malformed committed state did not persist target recovery at {boundary}"
            );
            let blocked = store
                .apply_provider_action_for(Target::Claude, Uuid::new_v4(), 0, serde_json::json!({}))
                .await
                .unwrap_err();
            assert_eq!(blocked.problem.code, "recovery-required");
            assert_eq!(
                store.target_view().await.unwrap().recovery.state,
                "clean",
                "malformed Claude state contaminated the Codex peer at {boundary}"
            );
        }
    }
}

async fn seed_malformed_claude_committed_state(home: &MuxviaHome, malformed_state: &'static str) {
    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    database
        .call(move |connection| {
            if malformed_state != "legacy-takeover-without-snapshot" {
                connection.execute(
                    "INSERT INTO activated_snapshots
                     (id, target, provider_id, base_url, model, protocol, authentication,
                      provider_bearer_token, epoch)
                     VALUES ('00000000-0000-4000-8000-000000000401', 'claude',
                             '00000000-0000-4000-8000-000000000402',
                             'https://api.anthropic.test', 'claude-test',
                             'anthropic-messages', 'anthropic-bearer', 'snapshot-secret',
                             '00000000-0000-4000-8000-000000000403')",
                    [],
                )?;
            }
            match malformed_state {
                "legacy-takeover-without-snapshot" => connection.execute(
                    "UPDATE target_route_state SET managed_config_version = 1,
                       takeover_state = 'active', route_port = 43124,
                       routing_credential = 'routing-secret', activated_snapshot_id = NULL
                     WHERE target = 'claude'",
                    [],
                )?,
                "direct-with-route-runtime" => connection.execute(
                    "UPDATE target_route_state SET managed_config_version = 2,
                       takeover_state = 'inactive', route_port = 43124,
                       routing_credential = NULL,
                       activated_snapshot_id = '00000000-0000-4000-8000-000000000401'
                     WHERE target = 'claude'",
                    [],
                )?,
                "takeover-without-routing-credential" => connection.execute(
                    "UPDATE target_route_state SET managed_config_version = 2,
                       takeover_state = 'active', route_port = 43124,
                       routing_credential = NULL,
                       activated_snapshot_id = '00000000-0000-4000-8000-000000000401'
                     WHERE target = 'claude'",
                    [],
                )?,
                "takeover-with-invalid-route-port" => connection.execute(
                    "UPDATE target_route_state SET managed_config_version = 2,
                       takeover_state = 'active', route_port = -1,
                       routing_credential = 'routing-secret',
                       activated_snapshot_id = '00000000-0000-4000-8000-000000000401'
                     WHERE target = 'claude'",
                    [],
                )?,
                _ => unreachable!("test declares every malformed committed state"),
            };
            Ok::<(), tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn managed_config_version_two_without_a_committed_claude_snapshot_requires_recovery() {
    let fixture = StoreFixture::new().await;
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(|connection| {
            connection.execute(
                "UPDATE target_route_state SET managed_config_version = 2
                 WHERE target = 'claude'",
                [],
            )?;
            Ok::<(), tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    drop(database);

    let result = fixture
        .store
        .prepare_activation_for(Target::Claude, Uuid::new_v4(), 0)
        .await
        .unwrap();
    let failure = match result {
        Ok(_) => panic!("inconsistent ownership/runtime unexpectedly prepared"),
        Err(failure) => failure,
    };
    assert_eq!(failure.problem.code, "recovery-required");
    assert_eq!(
        failure.authoritative_view.recovery.state,
        "recovery-required"
    );
    assert_eq!(
        fixture.store.target_view().await.unwrap().recovery.state,
        "clean"
    );
}

#[tokio::test]
async fn managed_config_version_commit_writes_requested_version_atomically_without_wire_projection()
{
    let fixture = StoreFixture::new().await;
    let created = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            0,
            serde_json::json!({
                "kind": "create-provider", "name": "Claude",
                "baseUrl": "https://api.anthropic.com/v1", "model": "claude-test",
                "credential": { "kind": "replace", "value": "provider-secret" },
                "authentication": "anthropic-bearer", "presetKey": "anthropic-api-messages"
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    let codec = ClaudeConfigCodec::for_user_home(fixture.home.user_home()).unwrap();
    let action_id = Uuid::new_v4();
    let recovery_id = Uuid::new_v4();
    let intent = RecoveryIntent::pending_claude(
        recovery_id,
        action_id,
        codec.settings_path().to_owned(),
        codec.inspect().unwrap(),
        codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "route-secret"),
        1,
    );
    fixture.store.insert_recovery_intent(&intent).await.unwrap();
    let snapshot_id = Uuid::new_v4();
    let commit = fixture
        .store
        .commit_activation_for(
            Target::Claude,
            action_id,
            1,
            ActivatedSnapshot {
                id: snapshot_id,
                target: Target::Claude,
                provider_id,
                base_url: "https://api.anthropic.com/v1".into(),
                model: "claude-test".into(),
                protocol: ProviderProtocol::AnthropicMessages,
                authentication: ProviderAuthentication::AnthropicBearer,
                provider_credential: SecretString::from("provider-secret"),
                epoch: fixture.store.service_epoch(),
            },
            ActivationRuntime::Takeover {
                route_port: 43124,
                routing_credential: SecretString::from("route-secret"),
            },
            2,
            recovery_id,
            codec.settings_path().to_string_lossy().into_owned(),
            None,
        )
        .await
        .unwrap();
    let outcome = match commit {
        ActivationCommit::Applied(outcome) => outcome,
        _ => panic!("valid Claude version-two commit did not apply"),
    };
    assert!(
        !serde_json::to_string(&outcome)
            .unwrap()
            .contains("managedConfigVersion")
    );

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let persisted = database
        .call(move |connection| {
            connection.query_row(
                "SELECT r.managed_config_version, a.state, r.activated_snapshot_id,
                        r.recovery_intent_id = ?1
                 FROM target_route_state r JOIN activation_recovery a
                   ON a.target = r.target AND a.id = ?1
                 WHERE r.target = 'claude'",
                [recovery_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, bool>(3)?,
                    ))
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(
        persisted,
        (2, "committed".into(), snapshot_id.to_string(), true)
    );
}

#[tokio::test]
async fn managed_config_version_commit_rejects_payload_ownership_mismatch_before_db_mutation() {
    for (managed_config_version, legacy_payload) in [(1, false), (2, true)] {
        let fixture = StoreFixture::new().await;
        let codec = ClaudeConfigCodec::for_user_home(fixture.home.user_home()).unwrap();
        let action_id = Uuid::new_v4();
        let recovery_id = Uuid::new_v4();
        fixture
            .store
            .insert_recovery_intent(&RecoveryIntent::pending_claude(
                recovery_id,
                action_id,
                codec.settings_path().to_owned(),
                codec.inspect().unwrap(),
                codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "route-secret"),
                0,
            ))
            .await
            .unwrap();
        if legacy_payload {
            let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
                .await
                .unwrap();
            database
                .call(move |connection| {
                    connection.execute(
                        "UPDATE activation_recovery SET payload_json = ?1 WHERE id = ?2",
                        tokio_rusqlite::rusqlite::params![
                            include_str!("fixtures/claude-recovery-t05.json"),
                            recovery_id.to_string()
                        ],
                    )?;
                    Ok::<(), tokio_rusqlite::rusqlite::Error>(())
                })
                .await
                .unwrap();
        }
        let mut updates = fixture.store.subscribe_target_views();

        let result = fixture
            .store
            .commit_activation_for(
                Target::Claude,
                action_id,
                0,
                ActivatedSnapshot {
                    id: Uuid::new_v4(),
                    target: Target::Claude,
                    provider_id: Uuid::new_v4(),
                    base_url: "https://api.anthropic.com/v1".into(),
                    model: "claude-test".into(),
                    protocol: ProviderProtocol::AnthropicMessages,
                    authentication: ProviderAuthentication::AnthropicBearer,
                    provider_credential: SecretString::from("provider-secret"),
                    epoch: fixture.store.service_epoch(),
                },
                ActivationRuntime::Takeover {
                    route_port: 43124,
                    routing_credential: SecretString::from("route-secret"),
                },
                managed_config_version,
                recovery_id,
                codec.settings_path().to_string_lossy().into_owned(),
                None,
            )
            .await;
        assert!(
            matches!(
                result,
                Err(muxvia_routing::state::StateError::InvalidRecoveryPayload)
            ),
            "ownership mismatch did not return the fixed recovery payload error"
        );

        let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
            .await
            .unwrap();
        let unchanged = database
            .call(move |connection| {
                connection.query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM activated_snapshots WHERE target = 'claude') = 0,
                       (SELECT COUNT(*) FROM action_receipts
                        WHERE target = 'claude' AND action_id = ?1) = 0,
                       (SELECT state = 'pending' FROM activation_recovery WHERE id = ?2),
                       management_revision = 0 AND view_sequence = 0
                         AND activated_snapshot_id IS NULL AND recovery_state = 'clean'
                     FROM target_route_state WHERE target = 'claude'",
                    tokio_rusqlite::rusqlite::params![
                        action_id.to_string(),
                        recovery_id.to_string()
                    ],
                    |row| {
                        Ok((
                            row.get::<_, bool>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, bool>(3)?,
                        ))
                    },
                )
            })
            .await
            .unwrap();
        assert_eq!(unchanged, (true, true, true, true));
        assert!(updates.try_recv().is_err());
    }
}

#[tokio::test]
async fn managed_config_version_invalid_commit_marks_only_its_target_before_snapshot_mutation() {
    let fixture = StoreFixture::new().await;
    let codec = ClaudeConfigCodec::for_user_home(fixture.home.user_home()).unwrap();
    let action_id = Uuid::new_v4();
    let recovery_id = Uuid::new_v4();
    fixture
        .store
        .insert_recovery_intent(&RecoveryIntent::pending_claude(
            recovery_id,
            action_id,
            codec.settings_path().to_owned(),
            codec.inspect().unwrap(),
            codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "route-secret"),
            0,
        ))
        .await
        .unwrap();
    let commit = fixture
        .store
        .commit_activation_for(
            Target::Claude,
            action_id,
            0,
            ActivatedSnapshot {
                id: Uuid::new_v4(),
                target: Target::Claude,
                provider_id: Uuid::new_v4(),
                base_url: "https://api.anthropic.com/v1".into(),
                model: "claude-test".into(),
                protocol: ProviderProtocol::AnthropicMessages,
                authentication: ProviderAuthentication::AnthropicBearer,
                provider_credential: SecretString::from("provider-secret"),
                epoch: fixture.store.service_epoch(),
            },
            ActivationRuntime::Takeover {
                route_port: 43124,
                routing_credential: SecretString::from("route-secret"),
            },
            99,
            recovery_id,
            codec.settings_path().to_string_lossy().into_owned(),
            None,
        )
        .await
        .unwrap();
    let view = match commit {
        ActivationCommit::RecoveryRequired(view) => view,
        _ => panic!("invalid Claude ownership version did not require recovery"),
    };
    assert_eq!(view.target, Target::Claude);
    assert_eq!(view.recovery.state, "recovery-required");
    assert_eq!(
        fixture.store.target_view().await.unwrap().recovery.state,
        "clean"
    );

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let persisted = database
        .call(move |connection| {
            let snapshot_count = connection.query_row(
                "SELECT COUNT(*) FROM activated_snapshots WHERE target = 'claude'",
                [],
                |row| row.get::<_, i64>(0),
            )?;
            let recovery_state = connection.query_row(
                "SELECT state FROM activation_recovery WHERE id = ?1",
                [recovery_id.to_string()],
                |row| row.get::<_, String>(0),
            )?;
            let version = connection.query_row(
                "SELECT managed_config_version FROM target_route_state WHERE target = 'claude'",
                [],
                |row| row.get::<_, u32>(0),
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>((snapshot_count, recovery_state, version))
        })
        .await
        .unwrap();
    assert_eq!(persisted, (0, "recovery-required".into(), 1));
}

#[tokio::test]
async fn credential_reference_foreign_key_rejects_cross_target_corruption() {
    let root = std::env::temp_dir().join(format!("muxvia-v5-target-fk-{}", Uuid::new_v4()));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let home = MuxviaHome::from_user_home(&user_home);
    drop(StateStore::open(&home).await.unwrap());

    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    let rejected = database
        .call(|connection| -> tokio_rusqlite::rusqlite::Result<bool> {
            connection.execute_batch("PRAGMA foreign_keys = ON;")?;
            connection.execute(
                "INSERT INTO credentials (id, target, bearer_token)
                 VALUES ('cross-target-credential', 'codex', 'secret')",
                [],
            )?;
            let result = connection.execute(
                "INSERT INTO providers
                 (id, target, position, provider_revision, name, base_url, model, protocol,
                  authentication, credential_id, routing_requirement)
                 VALUES ('cross-target-provider', 'claude', 0, 1, 'Claude',
                         'https://api.anthropic.com/v1', 'claude-test', 'anthropic-messages',
                         'anthropic-api-key', 'cross-target-credential', 'direct-compatible')",
                [],
            );
            Ok(result.is_err())
        })
        .await
        .unwrap();
    assert!(rejected);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn claude_target_view_projects_only_its_two_target_presets() {
    let fixture = StoreFixture::new().await;
    let view = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(view.target, Target::Claude);
    assert_eq!(
        view.route_health.state,
        muxvia_routing::control::protocol::RouteHealthState::Unobserved
    );
    assert!(
        view.provider_presets.len() == 2
            && view.provider_presets[0].key == "anthropic-api-messages"
            && view.provider_presets[0].protocol.to_string() == "anthropic-messages"
            && view.provider_presets[0].authentication.to_string() == "anthropic-api-key"
            && view.provider_presets[1].key == "codex-subscription-bridge"
            && view.provider_presets[1].base_url == "https://chatgpt.com/backend-api/codex"
            && view.provider_presets[1].protocol.to_string() == "anthropic-messages"
            && view.provider_presets[1].authentication.to_string() == "codex-subscription",
        "Claude Target preset catalog changed"
    );
}

#[tokio::test]
async fn claude_provider_create_update_and_duplicate_preserve_its_declaration() {
    let fixture = StoreFixture::new().await;
    let created = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(91), 0, serde_json::json!({
        "kind": "create-provider", "name": "Claude", "baseUrl": "https://api.anthropic.com/v1",
        "model": "claude-test", "credential": { "kind": "replace", "value": "secret" },
        "authentication": "anthropic-bearer", "presetKey": "anthropic-api-messages"
    })).await.unwrap();
    let provider = &created.view.providers[0];
    assert_eq!(provider.protocol.to_string(), "anthropic-messages");
    assert_eq!(provider.authentication.to_string(), "anthropic-bearer");
    let duplicate = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(92), 1, serde_json::json!({
        "kind": "duplicate-provider", "sourceProviderId": provider.id, "sourceProviderRevision": 1,
        "name": "Claude copy", "baseUrl": "https://api.anthropic.com/v1", "model": "claude-test",
        "credential": { "kind": "reuse-source" }
    })).await.unwrap();
    assert_eq!(
        duplicate.view.providers[1].authentication.to_string(),
        "anthropic-bearer"
    );
    let updated = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(93),
            2,
            serde_json::json!({
                "kind": "update-provider", "providerId": provider.id, "providerRevision": 1,
                "name": "Claude", "baseUrl": "https://api.anthropic.com/v1", "model": "claude-test",
                "credential": { "kind": "keep" }, "authentication": "anthropic-api-key"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        updated.view.providers[0].authentication.to_string(),
        "anthropic-api-key"
    );
    let api_key = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(94), 3, serde_json::json!({
        "kind": "create-provider", "name": "Claude key", "baseUrl": "", "model": "", "credential": { "kind": "remove" },
        "authentication": "anthropic-api-key"
    })).await.unwrap();
    assert_eq!(
        api_key.view.providers[2].authentication.to_string(),
        "anthropic-api-key"
    );
    let rejected = fixture.store.apply_provider_action_for(Target::Claude, fixed_uuid(95), 4, serde_json::json!({
        "kind": "create-provider", "name": "Bad", "baseUrl": "", "model": "", "credential": { "kind": "remove" },
        "authentication": "openai-bearer"
    })).await.unwrap_err();
    assert_eq!(rejected.problem.code, "invalid-provider");
}

#[tokio::test]
async fn claude_preset_create_defaults_to_its_api_key_authentication() {
    let fixture = StoreFixture::new().await;
    let outcome = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(96),
            0,
            serde_json::json!({
                "kind": "create-provider", "name": "Claude preset",
                "baseUrl": "https://api.anthropic.com/v1", "model": "claude-test",
                "credential": { "kind": "remove" }, "presetKey": "anthropic-api-messages"
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.view.providers[0].authentication.to_string(),
        "anthropic-api-key"
    );
    let manual = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(98),
            1,
            serde_json::json!({
                "kind": "create-provider", "name": "Claude manual",
                "baseUrl": "https://api.example.test/v1", "model": "claude-test",
                "credential": { "kind": "remove" }
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(manual.problem.code, "invalid-provider");
}

#[tokio::test]
async fn malformed_claude_action_returns_claude_view_without_mutating_codex() {
    let fixture = StoreFixture::new().await;
    let codex_before = fixture.store.target_view().await.unwrap();
    let failure = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            fixed_uuid(97),
            0,
            serde_json::json!({ "kind": "bad" }),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.authoritative_view.target, Target::Claude);
    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .management_revision,
        codex_before.management_revision
    );
}

#[tokio::test]
async fn reopen_decodes_legacy_v4_claude_recovery_payload() {
    let root = std::env::temp_dir().join(format!("muxvia-legacy-v4-{}", Uuid::new_v4()));
    let home = MuxviaHome::from_user_home(&root);
    drop(StateStore::open(&home).await.unwrap());
    let id = Uuid::new_v4();
    let action_id = Uuid::new_v4();
    let database = tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap();
    database.call(move |connection| {
        connection.execute(
            "INSERT INTO activation_recovery (id, target, action_id, config_path, file_identity_json, payload_json, state, created_revision) VALUES (?1, 'claude', ?2, '/tmp/claude', 'null', ?3, 'pending', 0)",
            tokio_rusqlite::rusqlite::params![id.to_string(), action_id.to_string(), r#"{"target":"claude","before":{"legacy":true},"desired":{"legacy":true}}"#],
        )?;
        Ok::<(), tokio_rusqlite::rusqlite::Error>(())
    }).await.unwrap();
    drop(database);
    let store = StateStore::open(&home).await.unwrap();
    assert_eq!(
        store
            .recovery_intent_for(Target::Claude, action_id)
            .await
            .unwrap()
            .unwrap()
            .target(),
        Target::Claude
    );
    drop(store);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn recovery_constructor_rejects_a_target_payload_mismatch() {
    let result = RecoveryIntent::pending_for_target(
        Target::Codex,
        Uuid::new_v4(),
        Uuid::new_v4(),
        PathBuf::from("/tmp/claude"),
        RecoveryPayload::ClaudeLegacy {
            payload: serde_json::json!({ "opaque": true }),
        },
        0,
    );
    assert!(result.is_err());
}

#[tokio::test]
async fn save_provider_persists_secret_separately_and_projects_no_secret() {
    let fixture = StoreFixture::new().await;
    let result = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(10),
            0,
            raw_save_provider(
                "Local test",
                "http://127.0.0.1:4567/v1/",
                "gpt-test",
                "provider-secret-must-not-escape",
            ),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ActionStatus::Applied);
    assert_eq!(result.view.management_revision, 1);
    assert_eq!(
        result.view.providers[0].base_url,
        "http://127.0.0.1:4567/v1"
    );
    assert_eq!(
        result.view.providers[0].credential,
        CredentialPresence::Present
    );
    assert!(
        !serde_json::to_string(&result)
            .unwrap()
            .contains("provider-secret-must-not-escape")
    );

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let (credential, provider_secret_columns) = database
        .call(
            |connection| -> tokio_rusqlite::rusqlite::Result<(String, i64)> {
                let credential =
                    connection
                        .query_row("SELECT bearer_token FROM credentials", [], |row| row.get(0))?;
                let provider_secret_columns = connection.query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('providers')
                 WHERE name IN ('bearer_token', 'credential')",
                    [],
                    |row| row.get(0),
                )?;
                Ok((credential, provider_secret_columns))
            },
        )
        .await
        .unwrap();
    assert_eq!(credential, "provider-secret-must-not-escape");
    assert_eq!(provider_secret_columns, 0);
}

#[tokio::test]
async fn receipt_lookup_replays_before_a_malformed_second_payload_is_examined() {
    let fixture = StoreFixture::new().await;
    let action_id = fixed_uuid(10);
    fixture
        .store
        .apply_save_provider_action(
            action_id,
            0,
            raw_save_provider(
                "Local test",
                "https://api.example.com/v1",
                "gpt-test",
                "first-secret",
            ),
        )
        .await
        .unwrap();
    let malformed_second_payload = serde_json::json!({
        "kind": "save-provider",
        "name": 42,
        "expectedRevision": 999
    });
    let replayed = fixture
        .store
        .apply_save_provider_action(action_id, 999, malformed_second_payload)
        .await
        .unwrap();

    assert_eq!(replayed.status, ActionStatus::Replayed);
    assert_eq!(replayed.view.management_revision, 1);
    assert_eq!(replayed.view.providers.len(), 1);
    assert_eq!(
        fixture.store.target_view().await.unwrap().providers.len(),
        1
    );
}

#[tokio::test]
async fn stale_revision_returns_authoritative_view_without_mutating_or_receipting() {
    let fixture = StoreFixture::new().await;
    fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(20),
            0,
            raw_save_provider(
                "First",
                "https://first.example/v1",
                "gpt-first",
                "first-secret",
            ),
        )
        .await
        .unwrap();
    let stale_action_id = fixed_uuid(21);

    let failure = fixture
        .store
        .apply_save_provider_action(
            stale_action_id,
            0,
            raw_save_provider(
                "Stale",
                "https://stale.example/v1",
                "gpt-stale",
                "stale-secret",
            ),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "stale-revision");
    assert_eq!(failure.authoritative_view.management_revision, 1);
    assert_eq!(failure.authoritative_view.providers.len(), 1);
    assert_eq!(
        fixture.store.target_view().await.unwrap().providers.len(),
        1
    );
    assert!(
        fixture
            .store
            .receipt(stale_action_id)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn new_malformed_raw_action_rejects_without_receipt_or_secret_echo() {
    let fixture = StoreFixture::new().await;
    let action_id = fixed_uuid(22);
    let secret = "malformed-secret-must-not-escape";
    let failure = fixture
        .store
        .apply_save_provider_action(
            action_id,
            0,
            serde_json::json!({
                "kind": "save-provider",
                "name": 42,
                "credential": secret
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "invalid-provider");
    assert_eq!(failure.authoritative_view.management_revision, 0);
    assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    assert!(!format!("{failure:?}\n{failure}").contains(secret));
}

#[tokio::test]
async fn non_save_raw_actions_reject_without_consuming_action_ids() {
    let fixture = StoreFixture::new().await;
    let activate_id = fixed_uuid(23);
    let activate = fixture
        .store
        .apply_save_provider_action(
            activate_id,
            0,
            serde_json::json!({
                "kind": "activate-provider",
                "providerId": "00000000-0000-4000-8000-000000000001",
                "mode": "takeover"
            }),
        )
        .await
        .unwrap_err();
    let unknown_id = fixed_uuid(24);
    let unknown = fixture
        .store
        .apply_save_provider_action(
            unknown_id,
            0,
            serde_json::json!({ "kind": "delete-provider" }),
        )
        .await
        .unwrap_err();

    assert_eq!(activate.problem.code, "unsupported-operation");
    assert_eq!(unknown.problem.code, "invalid-provider");
    assert!(fixture.store.receipt(activate_id).await.unwrap().is_none());
    assert!(fixture.store.receipt(unknown_id).await.unwrap().is_none());
}

#[test]
fn provider_urls_normalize_https_and_all_loopback_http_hosts() {
    let cases = [
        (
            "https://api.example.com/v1///",
            "https://api.example.com/v1",
        ),
        ("http://127.42.5.9:4567/v1/", "http://127.42.5.9:4567/v1"),
        ("http://localhost:4567/v1/", "http://localhost:4567/v1"),
        ("http://[::1]:4567/v1/", "http://[::1]:4567/v1"),
    ];

    for (input, expected) in cases {
        assert_eq!(normalize_provider_base_url(input).unwrap(), expected);
    }
}

#[tokio::test]
async fn unsafe_provider_urls_reject_without_consuming_action_ids() {
    let fixture = StoreFixture::new().await;
    let invalid_urls = [
        "http://api.example.com/v1",
        "https://operator@api.example.com/v1",
        "https://api.example.com/v1?model=gpt-test",
        "https://api.example.com/v1#responses",
    ];

    for (index, base_url) in invalid_urls.into_iter().enumerate() {
        let action_id = fixed_uuid(30 + index as u8);
        let failure = fixture
            .store
            .apply_save_provider_action(
                action_id,
                0,
                raw_save_provider("Unsafe", base_url, "gpt-test", "unsafe-secret"),
            )
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, "invalid-provider", "{base_url}");
        assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    }
    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .management_revision,
        0
    );
}

#[tokio::test]
async fn only_invalid_provider_fields_reject_while_missing_model_persists() {
    let fixture = StoreFixture::new().await;
    let cases = [
        ("", "gpt-test", "provider-secret"),
        ("Local", "gpt-test", ""),
    ];

    for (index, (name, model, credential)) in cases.into_iter().enumerate() {
        let action_id = fixed_uuid(40 + index as u8);
        let failure = fixture
            .store
            .apply_save_provider_action(
                action_id,
                0,
                raw_save_provider(name, "https://api.example.com/v1", model, credential),
            )
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, "invalid-provider");
        assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    }
    let incomplete = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(42),
            0,
            raw_save_provider("Local", "https://api.example.com/v1", "", "provider-secret"),
        )
        .await
        .unwrap();
    assert_eq!(
        incomplete.view.providers[0].credential,
        CredentialPresence::Present
    );
    assert_eq!(incomplete.view.providers[0].model, "");
}

#[tokio::test]
async fn opening_is_read_only_and_saving_increments_revision_and_sequence_together() {
    let fixture = StoreFixture::new().await;
    let opened = fixture.store.target_view().await.unwrap();
    assert_eq!(opened.management_revision, 0);
    assert_eq!(opened.view_sequence, 0);
    assert!(opened.providers.is_empty());

    let saved = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(50),
            0,
            raw_save_provider(
                "Local",
                "https://api.example.com/v1",
                "gpt-test",
                "counter-secret",
            ),
        )
        .await
        .unwrap();

    assert_eq!(saved.view.management_revision, 1);
    assert_eq!(saved.view.view_sequence, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn muxvia_home_directories_and_credential_database_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = StoreFixture::new().await;
    let mode = |path: &std::path::Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;

    assert_eq!(mode(fixture.home.root()), 0o700);
    assert_eq!(mode(fixture.home.state_dir()), 0o700);
    assert_eq!(mode(fixture.home.database_path()), 0o600);
}

#[tokio::test]
async fn target_views_receipts_and_failures_never_render_persisted_secrets() {
    let fixture = StoreFixture::new().await;
    for (index, secret) in ["persisted-secret-one", "persisted-secret-two"]
        .into_iter()
        .enumerate()
    {
        fixture
            .store
            .apply_save_provider_action(
                fixed_uuid(60 + index as u8),
                index as u64,
                raw_save_provider(
                    &format!("Provider {index}"),
                    &format!("https://api{index}.example.com/v1"),
                    &format!("gpt-{index}"),
                    secret,
                ),
            )
            .await
            .unwrap();
    }
    let failure = fixture
        .store
        .apply_save_provider_action(
            fixed_uuid(62),
            0,
            raw_save_provider(
                "Stale",
                "https://stale.example.com/v1",
                "gpt-stale",
                "not-persisted-secret",
            ),
        )
        .await
        .unwrap_err();
    let view = fixture.store.target_view().await.unwrap();
    let receipt = fixture
        .store
        .receipt(fixed_uuid(60))
        .await
        .unwrap()
        .unwrap();
    let rendered = format!(
        "{}\n{:?}\n{}\n{:?}\n{:?}",
        serde_json::to_string(&view).unwrap(),
        view,
        failure,
        failure,
        receipt
    );

    assert!(!rendered.contains("persisted-secret-one"));
    assert!(!rendered.contains("persisted-secret-two"));
    assert!(!rendered.contains("not-persisted-secret"));
}

#[tokio::test]
async fn concurrent_duplicate_action_ids_apply_once_and_replay_once() {
    let fixture = StoreFixture::new().await;
    let store = fixture.store.clone();
    let action_id = fixed_uuid(70);
    let first = store.apply_save_provider_action(
        action_id,
        0,
        raw_save_provider(
            "First arrival",
            "https://first.example.com/v1",
            "gpt-first",
            "concurrent-secret-one",
        ),
    );
    let second = store.apply_save_provider_action(
        action_id,
        0,
        raw_save_provider(
            "Second arrival",
            "https://second.example.com/v1",
            "gpt-second",
            "concurrent-secret-two",
        ),
    );

    let (first, second) = tokio::join!(first, second);
    let statuses = [first.unwrap().status, second.unwrap().status];

    assert!(statuses.contains(&ActionStatus::Applied));
    assert!(statuses.contains(&ActionStatus::Replayed));
    assert_eq!(store.target_view().await.unwrap().providers.len(), 1);
}

#[tokio::test]
async fn target_scoped_receipts_and_recovery_action_ids_do_not_cross_targets() {
    let fixture = StoreFixture::new().await;
    let action_id = fixed_uuid(71);
    let codex = fixture.store.target_view().await.unwrap();
    let mut claude = codex.clone();
    claude.target = Target::Claude;
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
            for (target, outcome) in [("codex", codex), ("claude", claude)] {
                connection.execute(
                    "INSERT INTO action_receipts
                     (target, action_id, action_kind, committed_revision, outcome_json)
                     VALUES (?1, ?2, 'create-provider', 0, ?3)",
                    tokio_rusqlite::rusqlite::params![
                        target,
                        action_id.to_string(),
                        serde_json::to_string(&muxvia_routing::control::protocol::ActionOutcome {
                            status: ActionStatus::Applied,
                            view: outcome,
                        })
                        .unwrap(),
                    ],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();

    let codec = CodexConfigCodec::for_user_home(fixture.home.user_home()).unwrap();
    fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
    fs::write(codec.config_path(), "approval_policy = \"never\"\n").unwrap();
    let codex_intent = RecoveryIntent::pending(
        Uuid::new_v4(),
        action_id,
        codec.config_path().to_owned(),
        codec.inspect().unwrap(),
        codec.desired_direct("gpt-test", "https://api.openai.com/v1", "openai"),
        0,
    );
    let claude_intent = RecoveryIntent::pending_for_target(
        Target::Claude,
        Uuid::new_v4(),
        action_id,
        PathBuf::from("/tmp/claude-settings"),
        RecoveryPayload::ClaudeLegacy {
            payload: serde_json::json!({ "owned": "claude" }),
        },
        0,
    )
    .unwrap();
    fixture
        .store
        .insert_recovery_intent(&codex_intent)
        .await
        .unwrap();
    fixture
        .store
        .insert_recovery_intent(&claude_intent)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .store
            .receipt_for(Target::Codex, action_id)
            .await
            .unwrap()
            .unwrap()
            .view
            .target,
        Target::Codex
    );
    assert_eq!(
        fixture
            .store
            .receipt_for(Target::Claude, action_id)
            .await
            .unwrap()
            .unwrap()
            .view
            .target,
        Target::Claude
    );
    assert_eq!(
        fixture
            .store
            .recovery_intent_for(Target::Codex, action_id)
            .await
            .unwrap()
            .unwrap()
            .id(),
        codex_intent.id()
    );
    assert_eq!(
        fixture
            .store
            .recovery_intent_for(Target::Claude, action_id)
            .await
            .unwrap()
            .unwrap()
            .id(),
        claude_intent.id()
    );
    let targets = fixture
        .store
        .pending_recovery_intents()
        .await
        .unwrap()
        .into_iter()
        .map(|intent| intent.target())
        .collect::<Vec<_>>();
    assert_eq!(targets, vec![Target::Codex, Target::Claude]);
}

#[test]
fn typed_and_raw_action_debug_output_redacts_credentials() {
    let secret = "debug-secret-must-not-escape";
    let action = TargetAction::CreateProvider {
        name: "Local".into(),
        base_url: "https://api.example.com/v1".into(),
        model: "gpt-test".into(),
        credential: CredentialEdit::Replace {
            value: secret.into(),
        },
        authentication: None,
        preset_key: None,
    };
    let operation = ControlOperation::Act {
        target: Target::Codex,
        action_id: fixed_uuid(80),
        expected_revision: 0,
        action: serde_json::to_value(&action).unwrap(),
    };
    let rendered_action = format!("{action:?}");
    let rendered_operation = format!("{operation:?}");
    let rendered_frame = format!(
        "{:?}",
        ClientFrame::Request {
            request_id: "request-80".into(),
            operation,
        }
    );

    for rendered in [rendered_action, rendered_operation, rendered_frame] {
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("<redacted>"));
    }
}
