use std::{fs, path::PathBuf, sync::Arc};

use muxvia_routing::{
    claude::CommandClaudeProbe,
    codex::CodexConfigCodec,
    codex::CommandCodexProbe,
    control::protocol::{
        ActionStatus, CredentialPresence, ProviderCompleteness, ProviderReferenceView,
        ProviderRequirement, ProviderRoutingRequirement, Target,
    },
    control::server::ControlServer,
    home::MuxviaHome,
    model::ReqwestUpstream,
    service::activate::ActivationService,
    state::{RecoveryPayload, StateStore},
};
use tokio_rusqlite::rusqlite::{Connection, params};
use uuid::Uuid;

const V1_SCHEMA: &str = r#"
CREATE TABLE metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE providers (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  name TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL
);

CREATE TABLE provider_credentials (
  provider_id TEXT PRIMARY KEY REFERENCES providers(id) ON DELETE CASCADE,
  bearer_token TEXT NOT NULL
);

CREATE TABLE target_route_state (
  target TEXT PRIMARY KEY CHECK (target = 'codex'),
  management_revision INTEGER NOT NULL,
  view_sequence INTEGER NOT NULL,
  current_provider_id TEXT,
  serving_provider_id TEXT,
  takeover_state TEXT NOT NULL,
  route_port INTEGER,
  routing_credential TEXT,
  activated_snapshot_id TEXT,
  managed_config_path TEXT,
  recovery_state TEXT NOT NULL
);

CREATE TABLE target_problems (
  target TEXT NOT NULL CHECK (target = 'codex'),
  code TEXT NOT NULL,
  message TEXT NOT NULL,
  PRIMARY KEY (target, code)
);

CREATE TABLE activated_snapshots (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  provider_id TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL,
  provider_bearer_token TEXT NOT NULL,
  epoch TEXT NOT NULL
);

CREATE TABLE action_receipts (
  action_id TEXT PRIMARY KEY,
  action_kind TEXT NOT NULL,
  committed_revision INTEGER NOT NULL,
  outcome_json TEXT NOT NULL
);

CREATE TABLE activation_recovery (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  action_id TEXT NOT NULL UNIQUE,
  config_path TEXT NOT NULL,
  file_identity_json TEXT NOT NULL,
  before_owned_json TEXT NOT NULL,
  desired_owned_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  created_revision INTEGER NOT NULL
);
"#;

const V2_SCHEMA: &str = r#"
CREATE TABLE metadata (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE credentials (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  bearer_token TEXT NOT NULL
);

CREATE TABLE providers (
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
);

CREATE TABLE target_route_state (
  target TEXT PRIMARY KEY CHECK (target = 'codex'),
  management_revision INTEGER NOT NULL,
  view_sequence INTEGER NOT NULL,
  current_provider_id TEXT,
  serving_provider_id TEXT,
  takeover_state TEXT NOT NULL,
  route_port INTEGER,
  routing_credential TEXT,
  activated_snapshot_id TEXT,
  managed_config_path TEXT,
  recovery_state TEXT NOT NULL
);

CREATE TABLE target_problems (
  target TEXT NOT NULL CHECK (target = 'codex'),
  code TEXT NOT NULL,
  message TEXT NOT NULL,
  PRIMARY KEY (target, code)
);

CREATE TABLE activated_snapshots (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  provider_id TEXT NOT NULL,
  base_url TEXT NOT NULL,
  model TEXT NOT NULL,
  provider_bearer_token TEXT NOT NULL,
  epoch TEXT NOT NULL
);

CREATE TABLE action_receipts (
  action_id TEXT PRIMARY KEY,
  action_kind TEXT NOT NULL,
  committed_revision INTEGER NOT NULL,
  outcome_json TEXT NOT NULL
);

CREATE TABLE activation_recovery (
  id TEXT PRIMARY KEY,
  target TEXT NOT NULL CHECK (target = 'codex'),
  action_id TEXT NOT NULL UNIQUE,
  config_path TEXT NOT NULL,
  file_identity_json TEXT NOT NULL,
  before_owned_json TEXT NOT NULL,
  desired_owned_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'committed', 'rolled-back', 'recovery-required')),
  created_revision INTEGER NOT NULL
);
"#;

const V5_SCHEMA: &str = include_str!("fixtures/state-schema-v5.sql");
const T05_CLAUDE_RECOVERY_PAYLOAD: &str = include_str!("fixtures/claude-recovery-t05.json");

struct StoreFixture {
    root: PathBuf,
    home: MuxviaHome,
}

impl StoreFixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("muxvia-provider-declarations-{}", Uuid::new_v4()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        Self {
            root,
            home: MuxviaHome::from_user_home(&user_home),
        }
    }
}

#[tokio::test]
async fn schema_v7_fresh_database_has_ownership_version_and_recovery_binding() {
    let fixture = StoreFixture::new();
    drop(fixture.open().await);

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let (version, not_null, default_value, table_sql, route_versions, recovery_binding_is_nullable) =
        database
            .call(|connection| -> tokio_rusqlite::rusqlite::Result<_> {
                let version = connection.query_row(
                    "SELECT value FROM metadata WHERE key = 'schema-version'",
                    [],
                    |row| row.get::<_, String>(0),
                )?;
                let (not_null, default_value) = connection.query_row(
                    "SELECT \"notnull\", dflt_value
                 FROM pragma_table_info('target_route_state')
                 WHERE name = 'managed_config_version'",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )?;
                let table_sql = connection.query_row(
                    "SELECT sql FROM sqlite_schema
                 WHERE type = 'table' AND name = 'target_route_state'",
                    [],
                    |row| row.get::<_, String>(0),
                )?;
                let route_versions = connection
                    .prepare(
                        "SELECT target, managed_config_version
                     FROM target_route_state ORDER BY target",
                    )?
                    .query_map([], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let recovery_binding_is_nullable: bool = connection.query_row(
                    "SELECT \"notnull\" = 0 FROM pragma_table_info('target_route_state')
                 WHERE name = 'recovery_intent_id'",
                    [],
                    |row| row.get(0),
                )?;
                Ok((
                    version,
                    not_null,
                    default_value,
                    table_sql,
                    route_versions,
                    recovery_binding_is_nullable,
                ))
            })
            .await
            .unwrap();

    assert_eq!(version, "7");
    assert_eq!(not_null, 1);
    assert_eq!(default_value.as_deref(), Some("1"));
    assert!(table_sql.contains("CHECK (managed_config_version IN (1,2))"));
    assert_eq!(route_versions, [("claude".into(), 1), ("codex".into(), 1)]);
    assert!(recovery_binding_is_nullable);
}

#[tokio::test]
async fn schema_v7_migrates_real_v5_claude_states_and_binds_the_unique_committed_intent() {
    for active in [false, true] {
        let fixture = StoreFixture::new();
        fs::create_dir_all(fixture.home.database_path().parent().unwrap()).unwrap();
        let provider_id = "00000000-0000-4000-8000-000000000201";
        let snapshot_id = "00000000-0000-4000-8000-000000000202";
        let action_id = "00000000-0000-4000-8000-000000000203";
        let recovery_id = "00000000-0000-4000-8000-000000000204";
        let payload =
            r#"{"target":"claude","before":{"legacy":"before"},"desired":{"legacy":"desired"}}"#;
        let receipt = migration_receipt("claude", 7, snapshot_id);
        let connection = Connection::open(fixture.home.database_path()).unwrap();
        connection.execute_batch(V5_SCHEMA).unwrap();
        if active {
            connection
                .execute(
                    "INSERT INTO credentials (id, target, bearer_token)
                     VALUES (?1, 'claude', 'provider-secret')",
                    [provider_id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO providers
                     (id, target, position, provider_revision, name, base_url, model, protocol,
                      authentication, credential_id, routing_requirement)
                     VALUES (?1, 'claude', 0, 1, 'Claude', 'https://provider.test', 'claude-test',
                             'anthropic-messages', 'anthropic-bearer', ?1, 'direct-compatible')",
                    [provider_id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO activated_snapshots
                     (id, target, provider_id, base_url, model, protocol, authentication,
                      provider_bearer_token, epoch)
                     VALUES (?1, 'claude', ?2, 'https://provider.test', 'claude-test',
                             'anthropic-messages', 'anthropic-bearer', 'provider-secret',
                             '00000000-0000-4000-8000-000000000205')",
                    params![snapshot_id, provider_id],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE target_route_state SET management_revision = 7, view_sequence = 9,
                       current_provider_id = ?1, takeover_state = 'active', route_port = 43124,
                       routing_credential = 'route-secret', activated_snapshot_id = ?2,
                       managed_config_path = '/tmp/settings.json'
                     WHERE target = 'claude'",
                    params![provider_id, snapshot_id],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO action_receipts
                     (target, action_id, action_kind, committed_revision, outcome_json)
                     VALUES ('claude', ?1, 'activate-provider', 7, ?2)",
                    params![action_id, receipt],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO activation_recovery
                     (id, target, action_id, config_path, file_identity_json, payload_json,
                      state, created_revision)
                     VALUES (?1, 'claude', ?2, '/tmp/settings.json', 'null', ?3,
                             'committed', 6)",
                    params![recovery_id, action_id, payload],
                )
                .unwrap();
        }
        drop(connection);

        drop(fixture.open().await);
        let database = Connection::open(fixture.home.database_path()).unwrap();
        assert_eq!(
            database
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema-version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "7"
        );
        let route: (
            i64,
            String,
            Option<i64>,
            bool,
            Option<String>,
            i64,
            Option<String>,
        ) = database
            .query_row(
                "SELECT management_revision, takeover_state, route_port,
                        routing_credential IS 'route-secret',
                        activated_snapshot_id, managed_config_version, recovery_intent_id
                 FROM target_route_state WHERE target = 'claude'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(route.5, 1);
        if active {
            assert_eq!(
                (route.0, route.1.clone(), route.2, route.3, route.4.clone()),
                (
                    7,
                    "active".to_owned(),
                    Some(43124),
                    true,
                    Some(snapshot_id.to_owned())
                )
            );
            assert_eq!(route.6.as_deref(), Some(recovery_id));
            assert!(
                database
                    .query_row(
                        "SELECT bearer_token = 'provider-secret' FROM credentials WHERE id = ?1",
                        [provider_id],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap(),
                "v5 migration changed the Provider credential"
            );
            assert_eq!(
                database
                    .query_row(
                        "SELECT name || '|' || base_url || '|' || model || '|' || protocol || '|' || authentication
                         FROM providers WHERE id = ?1",
                        [provider_id],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap(),
                "Claude|https://provider.test|claude-test|anthropic-messages|anthropic-bearer"
            );
            assert!(
                database
                    .query_row(
                        "SELECT provider_bearer_token = 'provider-secret'
                         FROM activated_snapshots WHERE id = ?1",
                        [snapshot_id],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap(),
                "v5 migration changed the Activated Snapshot credential"
            );
            assert!(
                database
                    .query_row(
                        "SELECT outcome_json = ?1 FROM action_receipts WHERE action_id = ?2",
                        params![receipt, action_id],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap(),
                "v5 migration rewrote the action receipt"
            );
            assert!(
                database
                    .query_row(
                        "SELECT payload_json = ?1 FROM activation_recovery WHERE id = ?2",
                        params![payload, recovery_id],
                        |row| row.get::<_, bool>(0),
                    )
                    .unwrap(),
                "v5 migration rewrote the Recovery Intent payload"
            );
        } else {
            assert_eq!(
                route,
                (0, "inactive".to_owned(), None, false, None, 1, None)
            );
        }
    }
}

#[tokio::test]
async fn schema_v7_selects_current_bindings_from_v6_receipts_after_retry_history() {
    let fixture = StoreFixture::new();
    fs::create_dir_all(fixture.home.database_path().parent().unwrap()).unwrap();
    let connection = Connection::open(fixture.home.database_path()).unwrap();
    connection.execute_batch(V5_SCHEMA).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE target_route_state
               ADD COLUMN managed_config_version INTEGER NOT NULL DEFAULT 1
                 CHECK (managed_config_version IN (1,2));
             UPDATE metadata SET value = '6' WHERE key = 'schema-version';",
        )
        .unwrap();

    for (target, protocol, authentication, base) in [
        ("codex", "openai-responses", "openai-bearer", 400_u16),
        ("claude", "anthropic-messages", "anthropic-bearer", 500_u16),
    ] {
        let route_port = i64::from(base) + 43100;
        let prior_snapshot = format!("00000000-0000-4000-8000-000000000{base}");
        let current_snapshot = format!("00000000-0000-4000-8000-000000000{}", base + 1);
        let provider_id = format!("00000000-0000-4000-8000-000000000{}", base + 2);
        let prior_action = format!("10000000-0000-4000-8000-000000000{}", base + 3);
        let retry_action = format!("10000000-0000-4000-8000-000000000{}", base + 4);
        let prior_recovery = format!("20000000-0000-4000-8000-000000000{}", base + 5);
        let rolled_back_recovery = format!("20000000-0000-4000-8000-000000000{}", base + 6);
        let retried_recovery = format!("20000000-0000-4000-8000-000000000{}", base + 7);
        for (snapshot_id, model, epoch) in [
            (&prior_snapshot, "prior-model", base + 8),
            (&current_snapshot, "current-model", base + 9),
        ] {
            connection
                .execute(
                    "INSERT INTO activated_snapshots
                     (id, target, provider_id, base_url, model, protocol, authentication,
                      provider_bearer_token, epoch)
                     VALUES (?1, ?2, ?3, 'https://provider.test', ?4, ?5, ?6,
                             'provider-secret', ?7)",
                    params![
                        snapshot_id,
                        target,
                        provider_id,
                        model,
                        protocol,
                        authentication,
                        format!("00000000-0000-4000-8000-000000000{epoch}")
                    ],
                )
                .unwrap();
        }
        connection
            .execute(
                "UPDATE target_route_state SET management_revision = 9, view_sequence = 9,
                   current_provider_id = ?1, takeover_state = 'active', route_port = ?2,
                   routing_credential = 'route-secret', activated_snapshot_id = ?3,
                   managed_config_path = '/tmp/managed-config'
                 WHERE target = ?4",
                params![provider_id, route_port, current_snapshot, target],
            )
            .unwrap();

        let payload_for = |model: &str| {
            if target == "codex" {
                let codec = CodexConfigCodec::for_user_home(fixture.home.user_home()).unwrap();
                serde_json::to_string(&RecoveryPayload::Codex {
                    before: Box::new(codec.inspect().unwrap()),
                    desired: Box::new(codec.desired_takeover(
                        model,
                        &format!("http://127.0.0.1:{route_port}/v1"),
                        "route-secret",
                    )),
                })
                .unwrap()
            } else {
                legacy_claude_migration_payload(model, route_port)
            }
        };

        connection
            .execute(
                "INSERT INTO activation_recovery
                 (id, target, action_id, config_path, file_identity_json, payload_json,
                  state, created_revision)
                 VALUES (?1, ?2, ?3, '/tmp/managed-config', 'null', ?4, 'committed', 4)",
                params![
                    prior_recovery,
                    target,
                    prior_action,
                    payload_for("prior-model")
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO action_receipts
                 (target, action_id, action_kind, committed_revision, outcome_json)
                 VALUES (?1, ?2, 'activate-provider', 5, ?3)",
                params![
                    target,
                    prior_action,
                    migration_receipt(target, 5, &prior_snapshot)
                ],
            )
            .unwrap();

        connection
            .execute(
                "INSERT INTO activation_recovery
                 (id, target, action_id, config_path, file_identity_json, payload_json,
                  state, created_revision)
                 VALUES (?1, ?2, ?3, '/tmp/managed-config', 'null', ?4, 'rolled-back', 2)",
                params![
                    rolled_back_recovery,
                    target,
                    retry_action,
                    payload_for("rolled-back-model")
                ],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE activation_recovery
                 SET id = ?1, payload_json = ?2, state = 'committed', created_revision = 7
                 WHERE target = ?3 AND action_id = ?4 AND state = 'rolled-back'",
                params![
                    retried_recovery,
                    payload_for("current-model"),
                    target,
                    retry_action
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO action_receipts
                 (target, action_id, action_kind, committed_revision, outcome_json)
                 VALUES (?1, ?2, 'activate-provider', 8, ?3)",
                params![
                    target,
                    retry_action,
                    migration_receipt(target, 8, &current_snapshot)
                ],
            )
            .unwrap();
    }
    drop(connection);

    drop(fixture.open().await);
    let database = Connection::open(fixture.home.database_path()).unwrap();
    let bindings = database
        .prepare(
            "SELECT target, recovery_intent_id, recovery_state
             FROM target_route_state ORDER BY target",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        bindings,
        [
            (
                "claude".to_owned(),
                Some("20000000-0000-4000-8000-000000000507".to_owned()),
                "clean".to_owned(),
            ),
            (
                "codex".to_owned(),
                Some("20000000-0000-4000-8000-000000000407".to_owned()),
                "clean".to_owned(),
            ),
        ]
    );
    assert!(
        database
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query([])
            .unwrap()
            .next()
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn schema_v7_does_not_guess_between_multiple_legacy_committed_intents() {
    let fixture = StoreFixture::new();
    fs::create_dir_all(fixture.home.database_path().parent().unwrap()).unwrap();
    let connection = Connection::open(fixture.home.database_path()).unwrap();
    connection.execute_batch(V5_SCHEMA).unwrap();
    connection
        .execute(
            "INSERT INTO activated_snapshots
             (id, target, provider_id, base_url, model, protocol, authentication,
              provider_bearer_token, epoch)
             VALUES ('00000000-0000-4000-8000-000000000211', 'claude',
                     '00000000-0000-4000-8000-000000000212', 'https://provider.test',
                     'claude-test', 'anthropic-messages', 'anthropic-bearer',
                     'provider-secret', '00000000-0000-4000-8000-000000000213')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE target_route_state SET takeover_state = 'active', route_port = 43124,
               routing_credential = 'route-secret',
               activated_snapshot_id = '00000000-0000-4000-8000-000000000211'
             WHERE target = 'claude'",
            [],
        )
        .unwrap();
    for suffix in ["221", "222"] {
        let action_id = format!("10000000-0000-4000-8000-000000000{suffix}");
        connection
            .execute(
                "INSERT INTO activation_recovery
                 (id, target, action_id, config_path, file_identity_json, payload_json,
                  state, created_revision)
                 VALUES (?1, 'claude', ?2, '/tmp/settings.json', 'null', '{}',
                         'committed', 0)",
                params![
                    format!("00000000-0000-4000-8000-000000000{suffix}"),
                    action_id
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO action_receipts
                 (target, action_id, action_kind, committed_revision, outcome_json)
                 VALUES ('claude', ?1, 'activate-provider', 1, ?2)",
                params![
                    action_id,
                    migration_receipt("claude", 1, "00000000-0000-4000-8000-000000000211")
                ],
            )
            .unwrap();
    }
    drop(connection);

    drop(fixture.open().await);
    let database = Connection::open(fixture.home.database_path()).unwrap();
    let migrated: (String, Option<String>, String) = database
        .query_row(
            "SELECT (SELECT value FROM metadata WHERE key = 'schema-version'),
                    recovery_intent_id, recovery_state
             FROM target_route_state WHERE target = 'claude'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        migrated,
        ("7".to_owned(), None, "recovery-required".to_owned())
    );
}

fn migration_receipt(target: &str, revision: u64, snapshot_id: &str) -> String {
    serde_json::json!({
        "status": "applied",
        "view": {
            "target": target,
            "managementRevision": revision,
            "mode": "takeover",
            "activatedSnapshot": {"id": snapshot_id},
        }
    })
    .to_string()
}

fn legacy_claude_migration_payload(model: &str, route_port: i64) -> String {
    let mut payload: serde_json::Value = serde_json::from_str(T05_CLAUDE_RECOVERY_PAYLOAD).unwrap();
    payload["desired"]["owned"]["base_url"] =
        serde_json::json!(format!("http://127.0.0.1:{route_port}"));
    payload["desired"]["owned"]["auth_token"] = serde_json::json!("route-secret");
    payload["desired"]["owned"]["model"] = serde_json::json!(model);
    payload.to_string()
}

#[cfg(unix)]
#[tokio::test]
async fn schema_v7_real_v5_claude_takeover_bootstraps_legacy_and_isolates_invalid_restart() {
    let root = PathBuf::from("/tmp").join(format!("mv-{}", Uuid::new_v4()));
    let user_home = root.join("home");
    fs::create_dir_all(&user_home).unwrap();
    let fixture = StoreFixture {
        root,
        home: MuxviaHome::from_user_home(&user_home),
    };
    fs::create_dir_all(fixture.home.database_path().parent().unwrap()).unwrap();
    let codex_listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let codex_port = codex_listener.local_addr().unwrap().port();
    drop(codex_listener);
    let claude_listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let claude_port = claude_listener.local_addr().unwrap().port();
    drop(claude_listener);
    let codex_route_credential = "c".repeat(64);
    let claude_route_credential = "d".repeat(64);

    let codex_codec = CodexConfigCodec::for_user_home(fixture.home.user_home()).unwrap();
    let codex_before = codex_codec.inspect().unwrap();
    let codex_desired = codex_codec.desired_takeover(
        "gpt-v5",
        &format!("http://127.0.0.1:{codex_port}/v1"),
        &codex_route_credential,
    );
    codex_codec
        .atomic_apply(&codex_before, &codex_desired)
        .unwrap();
    let claude_path = fixture.home.user_home().join(".claude/settings.json");
    fs::create_dir_all(claude_path.parent().unwrap()).unwrap();
    fs::write(
        &claude_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "env": {
                "ANTHROPIC_BASE_URL": format!("http://127.0.0.1:{claude_port}"),
                "ANTHROPIC_AUTH_TOKEN": claude_route_credential,
                "ANTHROPIC_MODEL": "claude-v5",
                "ANTHROPIC_API_KEY": "legacy-bootstrap-api-key-sentinel"
            },
            "operator": true
        }))
        .unwrap(),
    )
    .unwrap();

    let connection = Connection::open(fixture.home.database_path()).unwrap();
    connection.execute_batch(V5_SCHEMA).unwrap();
    for (target, provider_id, snapshot_id, model, protocol, authentication, secret) in [
        (
            "codex",
            "00000000-0000-4000-8000-000000000301",
            "00000000-0000-4000-8000-000000000302",
            "gpt-v5",
            "openai-responses",
            "openai-bearer",
            "codex-provider-secret-sentinel",
        ),
        (
            "claude",
            "00000000-0000-4000-8000-000000000303",
            "00000000-0000-4000-8000-000000000304",
            "claude-v5",
            "anthropic-messages",
            "anthropic-bearer",
            "claude-provider-secret-sentinel",
        ),
    ] {
        connection
            .execute(
                "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, ?3)",
                params![provider_id, target, secret],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO providers
                 (id, target, position, provider_revision, name, base_url, model, protocol,
                  authentication, credential_id, routing_requirement)
                 VALUES (?1, ?2, 0, 1, 'Provider', 'https://provider.test', ?3, ?4,
                         ?5, ?1, 'direct-compatible')",
                params![provider_id, target, model, protocol, authentication],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, protocol, authentication,
                  provider_bearer_token, epoch)
                 VALUES (?1, ?2, ?3, 'https://provider.test', ?4, ?5, ?6, ?7,
                         '00000000-0000-4000-8000-000000000305')",
                params![
                    snapshot_id,
                    target,
                    provider_id,
                    model,
                    protocol,
                    authentication,
                    secret
                ],
            )
            .unwrap();
    }
    connection
        .execute(
            "UPDATE target_route_state SET management_revision = 1, view_sequence = 1,
               current_provider_id = '00000000-0000-4000-8000-000000000301',
               takeover_state = 'active', route_port = ?1, routing_credential = ?2,
               activated_snapshot_id = '00000000-0000-4000-8000-000000000302',
               managed_config_path = ?3 WHERE target = 'codex'",
            params![
                codex_port,
                codex_route_credential,
                codex_codec.config_path().to_string_lossy()
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE target_route_state SET management_revision = 1, view_sequence = 1,
               current_provider_id = '00000000-0000-4000-8000-000000000303',
               takeover_state = 'active', route_port = ?1, routing_credential = ?2,
               activated_snapshot_id = '00000000-0000-4000-8000-000000000304',
               managed_config_path = ?3 WHERE target = 'claude'",
            params![
                claude_port,
                claude_route_credential,
                claude_path.to_string_lossy()
            ],
        )
        .unwrap();
    drop(connection);

    let first_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let first = Arc::new(
        ActivationService::new(
            Arc::clone(&first_store),
            fixture.home.clone(),
            Arc::new(CommandCodexProbe),
            "/not-used/codex".into(),
            Arc::new(ReqwestUpstream::new().unwrap()),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), "/not-used/claude".into()),
    );
    let first_control = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&first_store),
        "test",
        Arc::clone(&first),
    )
    .await
    .unwrap();
    assert_eq!(
        first.model_endpoint().await.unwrap().port(),
        codex_port,
        "migrated v5 Codex Takeover did not resume"
    );
    assert_eq!(
        first
            .model_endpoint_for(Target::Claude)
            .await
            .unwrap()
            .port(),
        claude_port,
        "migrated v5 Claude Takeover did not resume under legacy ownership"
    );
    assert!(
        serde_json::from_slice::<serde_json::Value>(&fs::read(&claude_path).unwrap()).unwrap()
            ["env"]["ANTHROPIC_API_KEY"]
            .as_str()
            == Some("legacy-bootstrap-api-key-sentinel"),
        "legacy bootstrap changed the unrelated Claude API key"
    );
    first_control.shutdown().await.unwrap();
    first.shutdown_models().await.unwrap();
    drop(first);
    drop(first_store);

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(|connection| {
            connection.execute_batch("PRAGMA ignore_check_constraints = ON;")?;
            connection.execute(
                "UPDATE target_route_state SET managed_config_version = -1
                 WHERE target = 'claude'",
                [],
            )?;
            Ok::<(), tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    drop(database);

    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let second = Arc::new(
        ActivationService::new(
            Arc::clone(&second_store),
            fixture.home.clone(),
            Arc::new(CommandCodexProbe),
            "/not-used/codex".into(),
            Arc::new(ReqwestUpstream::new().unwrap()),
        )
        .with_claude_runtime(Arc::new(CommandClaudeProbe), "/not-used/claude".into()),
    );
    let second_control = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&second_store),
        "test-2",
        Arc::clone(&second),
    )
    .await
    .unwrap();
    assert_eq!(
        second.model_endpoint().await.unwrap().port(),
        codex_port,
        "clean Codex peer did not resume beside invalid Claude ownership"
    );
    assert!(second.model_endpoint_for(Target::Claude).await.is_none());
    assert_eq!(
        second_store
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .recovery
            .state,
        "recovery-required"
    );
    let unbound = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, claude_port))
        .await
        .unwrap();
    drop(unbound);
    second_control.shutdown().await.unwrap();
    second.shutdown_models().await.unwrap();
}

impl Drop for StoreFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl StoreFixture {
    async fn open(&self) -> std::sync::Arc<StateStore> {
        std::sync::Arc::new(StateStore::open(&self.home).await.unwrap())
    }
}

fn fixed_uuid(last_byte: u8) -> Uuid {
    let mut bytes = [0; 16];
    bytes[15] = last_byte;
    Uuid::from_bytes(bytes)
}

fn raw_create(
    name: &str,
    base_url: &str,
    model: &str,
    credential: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "create-provider",
        "name": name,
        "baseUrl": base_url,
        "model": model,
        "credential": credential,
        "presetKey": null
    })
}

fn raw_update(
    provider_id: Uuid,
    provider_revision: u64,
    name: &str,
    base_url: &str,
    model: &str,
    credential: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "update-provider",
        "providerId": provider_id,
        "providerRevision": provider_revision,
        "name": name,
        "baseUrl": base_url,
        "model": model,
        "credential": credential
    })
}

async fn credential_ids(home: &MuxviaHome) -> Vec<String> {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(|connection| {
            let mut statement = connection.prepare("SELECT id FROM credentials ORDER BY id")?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<tokio_rusqlite::rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap()
}

async fn credential_id_for_provider(home: &MuxviaHome, provider_id: Uuid) -> String {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT credential_id FROM providers WHERE id = ?1",
                [provider_id.to_string()],
                |row| row.get::<_, String>(0),
            )
        })
        .await
        .unwrap()
}

async fn routing_requirement_schema(home: &MuxviaHome) -> (i64, i64, Option<String>, String) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(|connection| -> tokio_rusqlite::rusqlite::Result<_> {
            let column = connection.query_row(
                "SELECT cid, \"notnull\", dflt_value
                 FROM pragma_table_info('providers') WHERE name = 'routing_requirement'",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )?;
            let table_sql = connection.query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'providers'",
                [],
                |row| row.get::<_, String>(0),
            )?;
            Ok((column.0, column.1, column.2, table_sql))
        })
        .await
        .unwrap()
}

async fn assert_schema_v4_routing_requirement(home: &MuxviaHome) {
    let (column_id, not_null, default_value, table_sql) = routing_requirement_schema(home).await;
    assert_eq!(column_id, 13);
    assert_eq!(not_null, 1);
    assert_eq!(default_value.as_deref(), Some("'direct-compatible'"));
    assert!(table_sql.contains("'direct-compatible', 'takeover-required'"));
}

#[tokio::test]
async fn v1_database_migrates_provider_identity_order_credential_and_active_state() {
    let fixture = StoreFixture::new();
    fs::create_dir_all(fixture.home.database_path().parent().unwrap()).unwrap();
    let existing_provider_id = "00000000-0000-4000-8000-000000000101";
    let second_provider_id = "00000000-0000-4000-8000-000000000102";
    let existing_snapshot_id = Uuid::parse_str("00000000-0000-4000-8000-000000000103").unwrap();
    let epoch = "00000000-0000-4000-8000-000000000104";
    let receipt_id = Uuid::parse_str("00000000-0000-4000-8000-000000000105").unwrap();
    let credential = "v1-provider-secret-must-not-escape";
    let malformed_secret = "malformed-replay-secret-must-not-escape";

    let connection = Connection::open(fixture.home.database_path()).unwrap();
    connection.execute_batch(V1_SCHEMA).unwrap();
    connection
        .execute(
            "INSERT INTO metadata (key, value) VALUES ('schema-version', '1')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO providers (id, target, name, base_url, model)
             VALUES (?1, 'codex', 'One', 'https://one.example/v1', 'one')",
            [existing_provider_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO providers (id, target, name, base_url, model)
             VALUES (?1, 'codex', 'Two', 'https://two.example/v1', 'two')",
            [second_provider_id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO provider_credentials (provider_id, bearer_token) VALUES (?1, ?2)",
            params![existing_provider_id, credential],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO activated_snapshots
             (id, target, provider_id, base_url, model, provider_bearer_token, epoch)
             VALUES (?1, 'codex', ?2, 'https://one.example/v1', 'one', ?3, ?4)",
            params![
                existing_snapshot_id.to_string(),
                existing_provider_id,
                credential,
                epoch
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO target_route_state
             (target, management_revision, view_sequence, current_provider_id, serving_provider_id,
              takeover_state, route_port, routing_credential, activated_snapshot_id,
              managed_config_path, recovery_state)
             VALUES ('codex', 7, 9, ?1, NULL, 'active', 1234, 'routing-secret', ?2,
                     '/tmp/config.toml', 'clean')",
            params![existing_provider_id, existing_snapshot_id.to_string()],
        )
        .unwrap();
    let legacy_outcome = serde_json::json!({
        "status": "applied",
        "view": {
            "target": "codex",
            "managementRevision": 7,
            "viewSequence": 9,
            "service": { "epoch": epoch, "state": "running" },
            "mode": "takeover",
            "takeover": { "state": "active", "endpoint": "http://127.0.0.1:1234" },
            "providers": [
                {
                    "id": existing_provider_id,
                    "name": "One",
                    "baseUrl": "https://one.example/v1",
                    "model": "one",
                    "credential": "present"
                },
                {
                    "id": second_provider_id,
                    "name": "Two at receipt time",
                    "baseUrl": "",
                    "model": "",
                    "credential": "missing"
                }
            ],
            "currentProviderId": existing_provider_id,
            "servingProviderId": null,
            "managedConfiguration": {
                "state": "applied",
                "path": "/tmp/config.toml",
                "restartRequired": true
            },
            "recovery": { "intentId": null, "state": "clean" },
            "activatedSnapshot": {
                "id": existing_snapshot_id,
                "providerId": existing_provider_id,
                "model": "one",
                "epoch": epoch
            },
            "problems": []
        }
    });
    connection
        .execute(
            "INSERT INTO action_receipts
             (action_id, action_kind, committed_revision, outcome_json)
             VALUES (?1, 'activate-provider', 7, ?2)",
            params![receipt_id.to_string(), legacy_outcome.to_string()],
        )
        .unwrap();
    drop(connection);

    let store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    assert_schema_v4_routing_requirement(&fixture.home).await;
    let view = store.target_view().await.unwrap();

    assert_eq!(
        view.providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect::<Vec<_>>(),
        ["One", "Two"]
    );
    assert_eq!(view.providers[0].credential, CredentialPresence::Present);
    assert_eq!(
        view.current_provider_id.as_deref(),
        Some(existing_provider_id)
    );
    assert_eq!(
        view.activated_snapshot.as_ref().map(|snapshot| snapshot.id),
        Some(existing_snapshot_id)
    );

    let schema_version = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap()
        .call(|connection| {
            connection.query_row(
                "SELECT value FROM metadata WHERE key = 'schema-version'",
                [],
                |row| row.get::<_, String>(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(schema_version, "7");
    assert_eq!(
        view.providers[0].id,
        Uuid::parse_str(existing_provider_id).unwrap()
    );
    assert_eq!(
        view.providers[1].id,
        Uuid::parse_str(second_provider_id).unwrap()
    );
    assert!(view.providers.iter().all(|provider| {
        provider.routing_requirement == ProviderRoutingRequirement::DirectCompatible
    }));

    let preparation = store
        .prepare_activation(Uuid::parse_str(existing_provider_id).unwrap(), 7)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(&preparation.provider_credential),
        credential
    );

    let service = ActivationService::new(
        Arc::clone(&store),
        fixture.home.clone(),
        Arc::new(CommandCodexProbe),
        "/must/not/probe/codex".into(),
        Arc::new(ReqwestUpstream::new().unwrap()),
    );
    let replay = service
        .apply_raw(
            receipt_id,
            u64::MAX,
            serde_json::json!({ "malformed": malformed_secret }),
        )
        .await
        .unwrap();

    assert_eq!(replay.status, ActionStatus::Replayed);
    assert_eq!(replay.view.management_revision, 7);
    assert_eq!(replay.view.view_sequence, 9);
    assert_eq!(replay.view.service.epoch, epoch);
    assert_eq!(replay.view.providers[0].position, 0);
    assert_eq!(replay.view.providers[0].provider_revision, 1);
    assert_eq!(
        replay.view.providers[0].active_references,
        [
            ProviderReferenceView::Current,
            ProviderReferenceView::ActivatedSnapshot,
        ]
    );
    assert_eq!(replay.view.providers[1].position, 1);
    assert_eq!(
        replay.view.providers[1].missing_fields,
        [
            ProviderRequirement::BaseUrl,
            ProviderRequirement::Model,
            ProviderRequirement::Credential,
        ]
    );
    assert_eq!(replay.view.provider_presets.len(), 1);
    assert_eq!(replay.view.provider_presets[0].key, "openai-api-responses");
    let replay_json = serde_json::to_string(&replay).unwrap();
    assert!(!replay_json.contains(credential));
    assert!(!replay_json.contains(malformed_secret));

    let receipt_metadata = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT action_kind, committed_revision, outcome_json
                 FROM action_receipts WHERE action_id = ?1",
                [receipt_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
        })
        .await
        .unwrap();
    assert_eq!(receipt_metadata.0, "activate-provider");
    assert_eq!(receipt_metadata.1, 7);
    assert!(!receipt_metadata.2.contains(credential));
    let receipt_json: serde_json::Value = serde_json::from_str(&receipt_metadata.2).unwrap();
    assert!(
        receipt_json["view"]["providers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|provider| provider["routingRequirement"] == "direct-compatible")
    );
}

#[tokio::test]
async fn schema_v4_migrates_v2_routing_requirement_and_historical_receipts() {
    let fixture = StoreFixture::new();
    fs::create_dir_all(fixture.home.database_path().parent().unwrap()).unwrap();
    let provider_id = Uuid::parse_str("00000000-0000-4000-8000-000000000101").unwrap();
    let snapshot_id = Uuid::parse_str("00000000-0000-4000-8000-000000000102").unwrap();
    let epoch = Uuid::parse_str("00000000-0000-4000-8000-000000000103").unwrap();
    let receipt_id = Uuid::parse_str("00000000-0000-4000-8000-000000000104").unwrap();
    let credential_id = Uuid::parse_str("00000000-0000-4000-8000-000000000105").unwrap();
    let malformed_secret = "malformed-v2-replay-secret-must-not-escape";

    let connection = Connection::open(fixture.home.database_path()).unwrap();
    connection.execute_batch(V2_SCHEMA).unwrap();
    connection
        .execute(
            "INSERT INTO metadata (key, value) VALUES ('schema-version', '2')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO credentials (id, target, bearer_token)
             VALUES (?1, 'codex', 'v2-provider-secret-must-not-escape')",
            [credential_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO providers
             (id, target, position, provider_revision, name, base_url, model, protocol,
              credential_id, provenance_kind, provenance_key, generated_owner_id)
             VALUES (?1, 'codex', 0, 1, 'Direct Provider', 'https://provider.example/v1',
                     'model-a', 'openai-responses', ?2, NULL, NULL, NULL)",
            params![provider_id.to_string(), credential_id.to_string()],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO activated_snapshots
             (id, target, provider_id, base_url, model, provider_bearer_token, epoch)
             VALUES (?1, 'codex', ?2, 'https://provider.example/v1', 'model-a',
                     'v2-provider-secret-must-not-escape', ?3)",
            params![
                snapshot_id.to_string(),
                provider_id.to_string(),
                epoch.to_string()
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO target_route_state
             (target, management_revision, view_sequence, current_provider_id, serving_provider_id,
              takeover_state, route_port, routing_credential, activated_snapshot_id,
              managed_config_path, recovery_state)
             VALUES ('codex', 4, 5, ?1, NULL, 'active', 1234, 'routing-secret', ?2,
                     '/tmp/config.toml', 'clean')",
            params![provider_id.to_string(), snapshot_id.to_string()],
        )
        .unwrap();
    let historical_outcome = serde_json::json!({
        "status": "applied",
        "view": {
            "target": "codex",
            "managementRevision": 4,
            "viewSequence": 5,
            "service": { "epoch": epoch, "state": "running" },
            "mode": "takeover",
            "takeover": { "state": "active", "endpoint": "http://127.0.0.1:1234" },
            "providers": [{
                "id": provider_id,
                "position": 0,
                "providerRevision": 1,
                "name": "Direct Provider",
                "baseUrl": "https://provider.example/v1",
                "model": "model-a",
                "protocol": "openai-responses",
                "credential": "present",
                "completeness": "complete",
                "missingFields": [],
                "provenance": null,
                "generated": false,
                "activeReferences": ["current", "activated-snapshot"]
            }],
            "providerPresets": [{
                "key": "openai-api-responses",
                "baseUrl": "https://api.openai.com/v1",
                "model": "",
                "protocol": "openai-responses"
            }],
            "currentProviderId": provider_id,
            "servingProviderId": null,
            "managedConfiguration": {
                "state": "applied",
                "path": "/tmp/config.toml",
                "restartRequired": true
            },
            "recovery": { "intentId": null, "state": "clean" },
            "activatedSnapshot": {
                "id": snapshot_id,
                "providerId": provider_id,
                "model": "model-a",
                "epoch": epoch
            },
            "problems": []
        }
    });
    connection
        .execute(
            "INSERT INTO action_receipts
             (action_id, action_kind, committed_revision, outcome_json)
             VALUES (?1, 'activate-provider', 4, ?2)",
            params![receipt_id.to_string(), historical_outcome.to_string()],
        )
        .unwrap();
    drop(connection);

    let store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    assert_schema_v4_routing_requirement(&fixture.home).await;
    let schema_version = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap()
        .call(|connection| {
            connection.query_row(
                "SELECT value FROM metadata WHERE key = 'schema-version'",
                [],
                |row| row.get::<_, String>(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(schema_version, "7");
    assert_eq!(
        store.target_view().await.unwrap().providers[0].routing_requirement,
        ProviderRoutingRequirement::DirectCompatible
    );
    assert_eq!(
        store
            .receipt(receipt_id)
            .await
            .unwrap()
            .unwrap()
            .view
            .providers[0]
            .routing_requirement,
        ProviderRoutingRequirement::DirectCompatible
    );

    let stored_outcome = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT outcome_json FROM action_receipts WHERE action_id = ?1",
                [receipt_id.to_string()],
                |row| row.get::<_, String>(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored_outcome).unwrap()["view"]["providers"][0]
            ["routingRequirement"],
        "direct-compatible"
    );

    let service = ActivationService::new(
        Arc::clone(&store),
        fixture.home.clone(),
        Arc::new(CommandCodexProbe),
        "/must/not/probe/codex".into(),
        Arc::new(ReqwestUpstream::new().unwrap()),
    );
    let replay = service
        .apply_raw(
            receipt_id,
            u64::MAX,
            serde_json::json!({ "malformed": malformed_secret }),
        )
        .await
        .unwrap();
    assert_eq!(replay.status, ActionStatus::Replayed);
    assert!(
        !serde_json::to_string(&replay)
            .unwrap()
            .contains(malformed_secret)
    );
}

#[tokio::test]
async fn create_name_only_persists_an_incomplete_provider_with_all_missing_requirements() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    assert_schema_v4_routing_requirement(&fixture.home).await;

    let outcome = store
        .apply_provider_action(
            fixed_uuid(10),
            0,
            raw_create(
                "Incomplete",
                "",
                "",
                serde_json::json!({ "kind": "remove" }),
            ),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, ActionStatus::Applied);
    assert_eq!(outcome.view.management_revision, 1);
    let provider = &outcome.view.providers[0];
    assert_eq!(provider.provider_revision, 1);
    assert_eq!(
        provider.routing_requirement,
        ProviderRoutingRequirement::DirectCompatible
    );
    assert_eq!(provider.completeness, ProviderCompleteness::Incomplete);
    assert_eq!(
        provider.missing_fields,
        vec![
            ProviderRequirement::BaseUrl,
            ProviderRequirement::Model,
            ProviderRequirement::Credential,
        ]
    );
}

#[tokio::test]
async fn invalid_create_name_or_nonempty_unsafe_url_has_no_receipt_or_revision_change() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let invalid_name = fixed_uuid(11);
    let invalid_url = fixed_uuid(12);

    let name_failure = store
        .apply_provider_action(
            invalid_name,
            0,
            raw_create("   ", "", "", serde_json::json!({ "kind": "remove" })),
        )
        .await
        .unwrap_err();
    let url_failure = store
        .apply_provider_action(
            invalid_url,
            0,
            raw_create(
                "Unsafe",
                "http://unsafe.example/v1",
                "",
                serde_json::json!({ "kind": "remove" }),
            ),
        )
        .await
        .unwrap_err();

    assert_eq!(name_failure.problem.code, "invalid-provider");
    assert_eq!(url_failure.problem.code, "invalid-provider");
    assert_eq!(store.target_view().await.unwrap().management_revision, 0);
    assert!(store.receipt(invalid_name).await.unwrap().is_none());
    assert!(store.receipt(invalid_url).await.unwrap().is_none());
}

#[tokio::test]
async fn known_preset_key_records_provenance_without_overriding_submitted_draft_values() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;

    let outcome = store
        .apply_provider_action(
            fixed_uuid(13),
            0,
            serde_json::json!({
                "kind": "create-provider",
                "name": "Preset draft",
                "baseUrl": "https://draft.example/v1",
                "model": "draft-model",
                "credential": { "kind": "remove" },
                "presetKey": "openai-api-responses"
            }),
        )
        .await
        .unwrap();

    let provider = &outcome.view.providers[0];
    assert_eq!(provider.base_url, "https://draft.example/v1");
    assert_eq!(provider.model, "draft-model");
    let provenance = provider.provenance.as_ref().unwrap();
    assert_eq!(provenance.kind, "preset");
    assert_eq!(provenance.key, "openai-api-responses");
}

#[tokio::test]
async fn update_preserves_provider_identity_and_advances_only_declaration_revisions() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let created = store
        .apply_provider_action(
            fixed_uuid(20),
            0,
            raw_create(
                "One",
                "https://one.example/v1/",
                "one",
                serde_json::json!({ "kind": "replace", "value": "one-secret" }),
            ),
        )
        .await
        .unwrap();
    let before = &created.view.providers[0];

    let updated = store
        .apply_provider_action(
            fixed_uuid(21),
            1,
            raw_update(
                before.id,
                before.provider_revision,
                "One renamed",
                "https://one.example/v1/",
                "one",
                serde_json::json!({ "kind": "keep" }),
            ),
        )
        .await
        .unwrap();
    let after = &updated.view.providers[0];

    assert_eq!(after.id, before.id);
    assert_eq!(after.provider_revision, 2);
    assert_eq!(
        after.routing_requirement,
        ProviderRoutingRequirement::DirectCompatible
    );
    assert_eq!(updated.view.management_revision, 2);
    assert_eq!(after.base_url, "https://one.example/v1");
}

#[tokio::test]
async fn credential_edit_intents_preserve_replace_and_collect_references_atomically() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let created = store
        .apply_provider_action(
            fixed_uuid(30),
            0,
            raw_create(
                "One",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "replace", "value": "first-secret" }),
            ),
        )
        .await
        .unwrap();
    let provider = created.view.providers[0].clone();
    let original_credential_ids = credential_ids(&fixture.home).await;
    assert_eq!(original_credential_ids.len(), 1);

    let _replaced = store
        .apply_provider_action(
            fixed_uuid(31),
            1,
            raw_update(
                provider.id,
                1,
                "One",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "replace", "value": "second-secret" }),
            ),
        )
        .await
        .unwrap();
    let replacement_credential_ids = credential_ids(&fixture.home).await;
    assert_eq!(replacement_credential_ids.len(), 1);
    assert_ne!(replacement_credential_ids, original_credential_ids);

    let kept = store
        .apply_provider_action(
            fixed_uuid(32),
            2,
            raw_update(
                provider.id,
                2,
                "One kept",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "keep" }),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        credential_ids(&fixture.home).await,
        replacement_credential_ids
    );

    let removed = store
        .apply_provider_action(
            fixed_uuid(33),
            3,
            raw_update(
                provider.id,
                3,
                "One removed",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "remove" }),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        kept.view.providers[0].credential,
        CredentialPresence::Present
    );
    assert_eq!(
        removed.view.providers[0].credential,
        CredentialPresence::Missing
    );
    assert!(credential_ids(&fixture.home).await.is_empty());
}

#[tokio::test]
async fn replacing_a_shared_credential_keeps_the_other_provider_secret_available() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let first = store
        .apply_provider_action(
            fixed_uuid(40),
            0,
            raw_create(
                "One",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "replace", "value": "shared-secret" }),
            ),
        )
        .await
        .unwrap();
    let second = store
        .apply_provider_action(
            fixed_uuid(41),
            1,
            raw_create(
                "Two",
                "https://two.example/v1",
                "two",
                serde_json::json!({ "kind": "replace", "value": "two-secret" }),
            ),
        )
        .await
        .unwrap();
    let first_id = first.view.providers[0].id;
    let second_id = second.view.providers[1].id;
    let shared_id = credential_id_for_provider(&fixture.home, first_id).await;
    tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap()
        .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
            connection.execute(
                "UPDATE providers SET credential_id = ?1 WHERE id = ?2",
                params![shared_id, second_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    store
        .apply_provider_action(
            fixed_uuid(42),
            2,
            raw_update(
                first_id,
                1,
                "One",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "replace", "value": "replacement-secret" }),
            ),
        )
        .await
        .unwrap();

    let second_preparation = store
        .prepare_activation(second_id, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(&second_preparation.provider_credential),
        "shared-secret"
    );
}

#[tokio::test]
async fn identical_keep_update_is_no_provider_change_without_receipt_or_revision_change() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let created = store
        .apply_provider_action(
            fixed_uuid(50),
            0,
            raw_create(
                "One",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "replace", "value": "one-secret" }),
            ),
        )
        .await
        .unwrap();
    let provider = &created.view.providers[0];
    let action_id = fixed_uuid(51);

    let failure = store
        .apply_provider_action(
            action_id,
            1,
            raw_update(
                provider.id,
                provider.provider_revision,
                &provider.name,
                &provider.base_url,
                &provider.model,
                serde_json::json!({ "kind": "keep" }),
            ),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "no-provider-change");
    assert_eq!(store.target_view().await.unwrap().management_revision, 1);
    assert!(store.receipt(action_id).await.unwrap().is_none());
}

#[tokio::test]
async fn declaration_edits_do_not_mutate_active_snapshot_bytes() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let created = store
        .apply_provider_action(
            fixed_uuid(60),
            0,
            raw_create(
                "One",
                "https://one.example/v1",
                "one",
                serde_json::json!({ "kind": "replace", "value": "snapshot-secret" }),
            ),
        )
        .await
        .unwrap();
    let provider = created.view.providers[0].clone();
    let snapshot_id = fixed_uuid(61);
    tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap()
        .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
            connection.execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, protocol, authentication, provider_bearer_token, epoch)
                 VALUES (?1, 'codex', ?2, 'https://one.example/v1', 'one', 'openai-responses', 'openai-bearer', 'snapshot-secret', ?3)",
                params![
                    snapshot_id.to_string(),
                    provider.id.to_string(),
                    fixed_uuid(62).to_string()
                ],
            )?;
            connection.execute(
                "UPDATE target_route_state
                 SET current_provider_id = ?1, activated_snapshot_id = ?2,
                     managed_config_path = '/tmp/config.toml'
                 WHERE target = 'codex'",
                params![provider.id.to_string(), snapshot_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    store
        .apply_provider_action(
            fixed_uuid(63),
            1,
            raw_update(
                provider.id,
                1,
                "One edited",
                "https://edited.example/v1",
                "edited",
                serde_json::json!({ "kind": "replace", "value": "edited-secret" }),
            ),
        )
        .await
        .unwrap();
    let snapshot = store.activated_snapshot().await.unwrap().unwrap();
    let view = store.target_view().await.unwrap();

    assert_eq!(snapshot.id(), snapshot_id);
    assert_eq!(snapshot.base_url(), "https://one.example/v1");
    assert_eq!(snapshot.model(), "one");
    assert_eq!(view.mode, "direct");
    assert_eq!(view.managed_configuration.state, "applied");
    assert_eq!(
        view.managed_configuration.path.as_deref(),
        Some("/tmp/config.toml")
    );
    assert!(view.managed_configuration.restart_required);
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(snapshot.provider_credential()),
        "snapshot-secret"
    );
    assert_eq!(
        view.providers[0].active_references,
        vec![
            ProviderReferenceView::Current,
            ProviderReferenceView::ActivatedSnapshot,
        ]
    );
}

#[tokio::test]
async fn concurrent_same_action_id_and_malformed_replay_return_the_recorded_outcome_first() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let action_id = fixed_uuid(70);
    let action = raw_create(
        "One",
        "https://one.example/v1",
        "one",
        serde_json::json!({ "kind": "replace", "value": "one-secret" }),
    );
    let first = store.apply_provider_action(action_id, 0, action.clone());
    let second = store.apply_provider_action(action_id, 0, action);
    let (first, second) = tokio::join!(first, second);
    let statuses = [first.unwrap().status, second.unwrap().status];
    assert!(statuses.contains(&ActionStatus::Applied));
    assert!(statuses.contains(&ActionStatus::Replayed));

    let replay = store
        .apply_provider_action(action_id, 999, serde_json::json!({ "kind": "malformed" }))
        .await
        .unwrap();
    assert_eq!(replay.status, ActionStatus::Replayed);
    assert_eq!(replay.view.management_revision, 1);
}
