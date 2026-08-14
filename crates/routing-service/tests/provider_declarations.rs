use std::{fs, path::PathBuf, sync::Arc};

use muxvia_routing::{
    codex::CommandCodexProbe,
    control::protocol::{
        ActionStatus, CredentialPresence, ProviderCompleteness, ProviderReferenceView,
        ProviderRequirement,
    },
    home::MuxviaHome,
    model::ReqwestUpstream,
    service::activate::ActivationService,
    state::StateStore,
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
    assert_eq!(schema_version, "2");
    assert_eq!(
        view.providers[0].id,
        Uuid::parse_str(existing_provider_id).unwrap()
    );
    assert_eq!(
        view.providers[1].id,
        Uuid::parse_str(second_provider_id).unwrap()
    );

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
}

#[tokio::test]
async fn create_name_only_persists_an_incomplete_provider_with_all_missing_requirements() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;

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
                 (id, target, provider_id, base_url, model, provider_bearer_token, epoch)
                 VALUES (?1, 'codex', ?2, 'https://one.example/v1', 'one', 'snapshot-secret', ?3)",
                params![
                    snapshot_id.to_string(),
                    provider.id.to_string(),
                    fixed_uuid(62).to_string()
                ],
            )?;
            connection.execute(
                "UPDATE target_route_state
                 SET current_provider_id = ?1, activated_snapshot_id = ?2
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
