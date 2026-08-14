use std::{fs, path::PathBuf, sync::Arc};

use muxvia_routing::{
    control::protocol::{
        ActionStatus, CredentialPresence, ProviderReferenceView, ProviderRoutingRequirement,
    },
    home::MuxviaHome,
    state::StateStore,
};
use tokio_rusqlite::rusqlite::{Result, params};
use uuid::Uuid;

struct StoreFixture {
    root: PathBuf,
    home: MuxviaHome,
}

impl StoreFixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("muxvia-provider-duplication-{}", Uuid::new_v4()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        Self {
            root,
            home: MuxviaHome::from_user_home(&user_home),
        }
    }

    async fn open(&self) -> Arc<StateStore> {
        Arc::new(StateStore::open(&self.home).await.unwrap())
    }
}

impl Drop for StoreFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn action_id(last_byte: u8) -> Uuid {
    let mut bytes = [0; 16];
    bytes[15] = last_byte;
    Uuid::from_bytes(bytes)
}

fn create(name: &str, secret: &str, preset_key: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "kind": "create-provider",
        "name": name,
        "baseUrl": if preset_key.is_some() {
            "https://api.openai.com/v1".to_owned()
        } else {
            format!("https://{}.example/v1", name.to_lowercase())
        },
        "model": format!("{}-model", name.to_lowercase()),
        "credential": { "kind": "replace", "value": secret },
        "presetKey": preset_key,
    })
}

fn duplicate(
    source_provider_id: Uuid,
    source_provider_revision: u64,
    name: &str,
    base_url: &str,
    model: &str,
    credential: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "duplicate-provider",
        "sourceProviderId": source_provider_id,
        "sourceProviderRevision": source_provider_revision,
        "name": name,
        "baseUrl": base_url,
        "model": model,
        "credential": credential,
        "provenance": { "kind": "client-must-not-own", "key": "client-must-not-own" },
    })
}

async fn credential_id(home: &MuxviaHome, provider_id: Uuid) -> Option<String> {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT credential_id FROM providers WHERE id = ?1",
                [provider_id.to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap()
}

async fn credential_value(home: &MuxviaHome, credential_id: String) -> String {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT bearer_token FROM credentials WHERE id = ?1",
                [credential_id],
                |row| row.get(0),
            )
        })
        .await
        .unwrap()
}

async fn set_runtime_state(home: &MuxviaHome, provider_id: Uuid, snapshot_id: Uuid) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| -> Result<()> {
            connection.execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, provider_bearer_token, epoch)
                 VALUES (?1, 'codex', ?2, 'https://snapshot.example/v1', 'snapshot-model',
                         'snapshot-secret', ?3)",
                params![
                    snapshot_id.to_string(),
                    provider_id.to_string(),
                    Uuid::new_v4().to_string(),
                ],
            )?;
            connection.execute(
                "UPDATE target_route_state
                 SET current_provider_id = ?1, serving_provider_id = ?1,
                     activated_snapshot_id = ?2, takeover_state = 'active', route_port = 4312,
                     managed_config_path = '/tmp/muxvia-config.toml'
                 WHERE target = 'codex'",
                params![provider_id.to_string(), snapshot_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn mark_generated(home: &MuxviaHome, provider_id: Uuid, owner_id: Uuid) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| -> Result<()> {
            connection.execute(
                "UPDATE providers
                 SET generated_owner_id = ?1,
                     provenance_kind = 'universal-provider', provenance_key = ?1
                 WHERE id = ?2",
                params![owner_id.to_string(), provider_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn set_generated_owner(home: &MuxviaHome, provider_id: Uuid, owner_id: Uuid) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| -> Result<()> {
            connection.execute(
                "UPDATE providers SET generated_owner_id = ?1 WHERE id = ?2",
                params![owner_id.to_string(), provider_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn generated_metadata(
    home: &MuxviaHome,
    provider_id: Uuid,
) -> (Option<String>, Option<String>, Option<String>) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| {
            connection.query_row(
                "SELECT generated_owner_id, provenance_kind, provenance_key
                 FROM providers WHERE id = ?1",
                [provider_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn duplicate_inserts_after_source_and_copies_only_server_owned_declarations() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let source = store
        .apply_provider_action(
            action_id(1),
            0,
            create("Source", "source-secret", Some("openai-api-responses")),
        )
        .await
        .unwrap()
        .view
        .providers[0]
        .clone();
    let later = store
        .apply_provider_action(action_id(2), 1, create("Later", "later-secret", None))
        .await
        .unwrap()
        .view
        .providers[1]
        .clone();
    let snapshot_id = action_id(3);
    set_runtime_state(&fixture.home, source.id, snapshot_id).await;
    let before = store.target_view().await.unwrap();

    let outcome = store
        .apply_provider_action(
            action_id(4),
            before.management_revision,
            duplicate(
                source.id,
                source.provider_revision,
                "Duplicate",
                "https://duplicate.example/v1",
                "duplicate-model",
                serde_json::json!({ "kind": "reuse-source" }),
            ),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, ActionStatus::Applied);
    assert_eq!(
        outcome
            .view
            .providers
            .iter()
            .map(|provider| (provider.id, provider.position))
            .collect::<Vec<_>>(),
        [
            (source.id, 0),
            (outcome.view.providers[1].id, 1),
            (later.id, 2),
        ],
    );
    let copied = &outcome.view.providers[1];
    assert_ne!(copied.id, source.id);
    assert_eq!(copied.name, "Duplicate");
    assert_eq!(copied.base_url, "https://duplicate.example/v1");
    assert_eq!(copied.model, "duplicate-model");
    assert_eq!(copied.provider_revision, 1);
    assert_eq!(
        copied.routing_requirement,
        ProviderRoutingRequirement::DirectCompatible
    );
    assert_eq!(copied.provenance, source.provenance);
    assert!(!copied.generated);
    assert_eq!(copied.active_references, []);
    assert_eq!(outcome.view.providers[0], before.providers[0]);
    assert_eq!(
        outcome.view.current_provider_id,
        Some(source.id.to_string())
    );
    assert_eq!(
        outcome.view.serving_provider_id,
        Some(source.id.to_string())
    );
    assert_eq!(
        outcome.view.activated_snapshot.as_ref().unwrap().id,
        snapshot_id
    );
    assert_eq!(
        outcome
            .view
            .activated_snapshot
            .as_ref()
            .unwrap()
            .provider_id,
        source.id
    );
    assert_eq!(outcome.view.mode, before.mode);
    assert_eq!(outcome.view.takeover, before.takeover);
    assert_eq!(
        outcome.view.managed_configuration,
        before.managed_configuration
    );
    assert_eq!(
        outcome.view.providers[0].active_references,
        [
            ProviderReferenceView::Current,
            ProviderReferenceView::ActivatedSnapshot
        ],
    );
    assert_eq!(
        credential_id(&fixture.home, copied.id).await,
        credential_id(&fixture.home, source.id).await,
    );
    assert!(
        !serde_json::to_string(&outcome)
            .unwrap()
            .contains("source-secret")
    );
}

#[tokio::test]
async fn duplicate_credential_intent_is_explicit_and_secret_free_in_results() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let source = store
        .apply_provider_action(action_id(10), 0, create("Source", "source-secret", None))
        .await
        .unwrap()
        .view
        .providers[0]
        .clone();

    let without = store
        .apply_provider_action(
            action_id(11),
            1,
            duplicate(
                source.id,
                source.provider_revision,
                "Without",
                "https://without.example/v1",
                "without-model",
                serde_json::json!({ "kind": "without" }),
            ),
        )
        .await
        .unwrap();
    let without = without
        .view
        .providers
        .into_iter()
        .find(|provider| provider.name == "Without")
        .unwrap();
    assert_eq!(without.credential, CredentialPresence::Missing);
    assert_eq!(credential_id(&fixture.home, without.id).await, None);

    let reused = store
        .apply_provider_action(
            action_id(12),
            2,
            duplicate(
                source.id,
                source.provider_revision,
                "Reuse",
                "https://reuse.example/v1",
                "reuse-model",
                serde_json::json!({ "kind": "reuse-source" }),
            ),
        )
        .await
        .unwrap();
    let reused = reused
        .view
        .providers
        .into_iter()
        .find(|provider| provider.name == "Reuse")
        .unwrap();
    assert_eq!(
        credential_id(&fixture.home, reused.id).await,
        credential_id(&fixture.home, source.id).await,
    );

    let replacement_secret = "replacement-secret-must-not-escape";
    let replaced = store
        .apply_provider_action(
            action_id(13),
            3,
            duplicate(
                source.id,
                source.provider_revision,
                "Replace",
                "https://replace.example/v1",
                "replace-model",
                serde_json::json!({ "kind": "replace", "value": replacement_secret }),
            ),
        )
        .await
        .unwrap();
    let replaced = replaced
        .view
        .providers
        .iter()
        .find(|provider| provider.name == "Replace")
        .unwrap();
    let replacement_id = credential_id(&fixture.home, replaced.id).await.unwrap();
    assert_ne!(
        replacement_id,
        credential_id(&fixture.home, source.id).await.unwrap(),
    );
    assert_eq!(
        credential_value(&fixture.home, replacement_id).await,
        replacement_secret
    );
    assert!(
        !serde_json::to_string(&replaced)
            .unwrap()
            .contains(replacement_secret)
    );
}

#[tokio::test]
async fn duplicate_rejects_stale_missing_and_malformed_source_intent_without_receipts() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let source = store
        .apply_provider_action(action_id(20), 0, create("Source", "source-secret", None))
        .await
        .unwrap()
        .view
        .providers[0]
        .clone();
    let before = store.target_view().await.unwrap();

    for (id, source_id, revision, credential, expected_code) in [
        (
            action_id(21),
            source.id,
            source.provider_revision + 1,
            serde_json::json!({ "kind": "without" }),
            "stale-provider-revision",
        ),
        (
            action_id(22),
            action_id(23),
            1,
            serde_json::json!({ "kind": "without" }),
            "invalid-provider",
        ),
        (
            action_id(24),
            source.id,
            source.provider_revision,
            serde_json::json!({ "kind": "keep" }),
            "invalid-provider",
        ),
    ] {
        let failure = store
            .apply_provider_action(
                id,
                before.management_revision,
                duplicate(
                    source_id,
                    revision,
                    "Rejected",
                    "https://rejected.example/v1",
                    "rejected-model",
                    credential,
                ),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.problem.code, expected_code);
        assert_eq!(failure.authoritative_view, before);
        assert!(store.receipt(id).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn generated_source_duplicates_as_an_ordinary_detached_provider() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let source = store
        .apply_provider_action(
            action_id(30),
            0,
            create("Generated", "generated-secret", None),
        )
        .await
        .unwrap()
        .view
        .providers[0]
        .clone();
    mark_generated(&fixture.home, source.id, action_id(31)).await;
    let generated = store.target_view().await.unwrap();
    assert!(generated.providers[0].generated);
    assert_eq!(
        generated.providers[0].provenance.as_ref().unwrap().kind,
        "universal-provider"
    );

    let outcome = store
        .apply_provider_action(
            action_id(32),
            generated.management_revision,
            duplicate(
                source.id,
                source.provider_revision,
                "Detached",
                "https://detached.example/v1",
                "detached-model",
                serde_json::json!({ "kind": "without" }),
            ),
        )
        .await
        .unwrap();
    let detached = outcome
        .view
        .providers
        .iter()
        .find(|provider| provider.name == "Detached")
        .unwrap();
    assert!(!detached.generated);
    assert_eq!(detached.provenance, None);
    assert_eq!(
        generated_metadata(&fixture.home, detached.id).await,
        (None, None, None)
    );
}

#[tokio::test]
async fn generated_duplicate_retains_nonowning_preset_provenance() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let source = store
        .apply_provider_action(
            action_id(35),
            0,
            create(
                "Preset Generated",
                "preset-generated-secret",
                Some("openai-api-responses"),
            ),
        )
        .await
        .unwrap()
        .view
        .providers[0]
        .clone();
    set_generated_owner(&fixture.home, source.id, action_id(36)).await;
    let generated = store.target_view().await.unwrap();
    assert!(generated.providers[0].generated);
    assert_eq!(
        generated.providers[0].provenance.as_ref().unwrap().kind,
        "preset"
    );

    let outcome = store
        .apply_provider_action(
            action_id(37),
            generated.management_revision,
            duplicate(
                source.id,
                source.provider_revision,
                "Preset Detached",
                "https://preset-detached.example/v1",
                "preset-detached-model",
                serde_json::json!({ "kind": "without" }),
            ),
        )
        .await
        .unwrap();
    let detached = outcome
        .view
        .providers
        .iter()
        .find(|provider| provider.name == "Preset Detached")
        .unwrap();
    assert!(!detached.generated);
    assert_eq!(detached.provenance, generated.providers[0].provenance);
}

#[tokio::test]
async fn create_from_preset_copies_the_draft_endpoint_and_persists_stable_provenance() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let created = store
        .apply_provider_action(
            action_id(40),
            0,
            create("From Preset", "preset-secret", Some("openai-api-responses")),
        )
        .await
        .unwrap();
    let provider = &created.view.providers[0];

    assert_eq!(provider.base_url, "https://api.openai.com/v1");
    assert_eq!(provider.provenance.as_ref().unwrap().kind, "preset");
    assert_eq!(
        provider.provenance.as_ref().unwrap().key,
        "openai-api-responses"
    );
    assert_eq!(
        store.target_view().await.unwrap().providers[0].base_url,
        "https://api.openai.com/v1",
    );
}
