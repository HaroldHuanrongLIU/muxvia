use std::{fs, path::PathBuf};

use muxvia_routing::{
    control::protocol::{ActionStatus, CredentialPresence, Target},
    home::MuxviaHome,
    state::StateStore,
};
use uuid::Uuid;

struct CatalogFixture {
    root: PathBuf,
    home: MuxviaHome,
}

#[tokio::test]
async fn update_guards_catalog_and_source_revisions_and_rejects_noop() {
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x201),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Original",
                "baseUrl": "https://original.example/v1",
                "credential": { "kind": "remove" },
                "presetKey": null,
                "targets": [
                    { "target": "codex", "enabled": true, "model": "old-codex", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "old-claude", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
                ]
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;

    let stale_catalog = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x202),
            0,
            update_action(provider_id, 1, "Updated"),
        )
        .await
        .unwrap_err();
    assert_eq!(
        stale_catalog.problem.code,
        "stale-universal-catalog-revision"
    );

    let stale_source = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x203),
            1,
            update_action(provider_id, 99, "Updated"),
        )
        .await
        .unwrap_err();
    assert_eq!(
        stale_source.problem.code,
        "stale-universal-provider-revision"
    );

    let updated = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x204),
            1,
            update_action(provider_id, 1, "Updated"),
        )
        .await
        .unwrap();
    let provider = &updated.view.providers[0];
    assert_eq!(provider.provider_revision, 2);
    assert_eq!(provider.name, "Updated");
    assert_eq!(provider.base_url, "https://updated.example/v1");
    assert_eq!(provider.targets[0].overlay_revision, 2);
    assert_eq!(provider.targets[0].model, "new-codex");
    assert_eq!(provider.targets[1].overlay_revision, 1);
    assert_eq!(provider.targets[1].model, "old-claude");

    let no_change = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x205),
            2,
            update_action(provider_id, 2, "Updated"),
        )
        .await
        .unwrap_err();
    assert_eq!(no_change.problem.code, "no-universal-provider-change");
    assert_eq!(no_change.authoritative_view, updated.view);
    assert_eq!(
        store.universal_provider_catalog().await.unwrap(),
        updated.view
    );
}

fn update_action(provider_id: Uuid, provider_revision: u64, name: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "update-universal-provider",
        "providerId": provider_id,
        "providerRevision": provider_revision,
        "name": name,
        "baseUrl": "https://updated.example/v1",
        "credential": { "kind": "keep" },
        "targets": [
            { "target": "codex", "enabled": true, "model": "new-codex", "authentication": "openai-bearer", "routingRequirement": "takeover-required" },
            { "target": "claude", "enabled": false, "model": "old-claude", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
        ]
    })
}

#[tokio::test]
async fn duplicate_detaches_identity_and_requires_an_explicit_credential_choice() {
    const SECRET: &str = "UNIVERSAL_DUPLICATE_SECRET_30195";
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x301),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Preset Source",
                "baseUrl": "https://source.example/v1",
                "credential": { "kind": "replace", "value": SECRET },
                "presetKey": "openai-api-responses",
                "targets": universal_targets("source")
            }),
        )
        .await
        .unwrap();
    let source = &created.view.providers[0];
    let source_id = source.id;
    assert!(source.provenance.is_some());

    let reused = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x302),
            1,
            duplicate_action(source_id, "Reuse Copy", "reuse-source"),
        )
        .await
        .unwrap();
    let reused_copy = &reused.view.providers[1];
    assert_ne!(reused_copy.id, source_id);
    assert!(reused_copy.provenance.is_none());
    assert_eq!(reused_copy.provider_revision, 1);
    assert_eq!(reused_copy.credential, CredentialPresence::Present);

    let without = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x303),
            2,
            duplicate_action(source_id, "Credential-free Copy", "without"),
        )
        .await
        .unwrap();
    let without_copy = &without.view.providers[2];
    assert_ne!(without_copy.id, source_id);
    assert_ne!(without_copy.id, reused_copy.id);
    assert!(without_copy.provenance.is_none());
    assert_eq!(without_copy.credential, CredentialPresence::Missing);
    assert_eq!(without.view.providers[0].id, source_id);
    assert!(without.view.providers[0].provenance.is_some());

    let serialized = serde_json::to_string(&without).unwrap();
    let debugged = format!("{without:?}");
    assert!(
        !serialized.contains(SECRET),
        "duplicate outcome serialized a credential"
    );
    assert!(
        !debugged.contains(SECRET),
        "duplicate outcome debugged a credential"
    );
}

fn duplicate_action(source_id: Uuid, name: &str, credential_kind: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "duplicate-universal-provider",
        "sourceProviderId": source_id,
        "sourceProviderRevision": 1,
        "name": name,
        "baseUrl": "https://copy.example/v1",
        "credential": { "kind": credential_kind },
        "targets": universal_targets("copy")
    })
}

fn universal_targets(model_prefix: &str) -> serde_json::Value {
    serde_json::json!([
        {
            "target": "codex",
            "enabled": true,
            "model": format!("{model_prefix}-codex"),
            "authentication": "openai-bearer",
            "routingRequirement": "direct-compatible"
        },
        {
            "target": "claude",
            "enabled": true,
            "model": format!("{model_prefix}-claude"),
            "authentication": "anthropic-api-key",
            "routingRequirement": "takeover-required"
        }
    ])
}

#[tokio::test]
async fn delete_without_generated_records_cascades_targets_and_orphaned_credential() {
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let first = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x401),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Delete Me",
                "baseUrl": "https://delete.example/v1",
                "credential": { "kind": "replace", "value": "DELETE_SECRET_44710" },
                "presetKey": null,
                "targets": universal_targets("delete")
            }),
        )
        .await
        .unwrap();
    let delete_id = first.view.providers[0].id;
    let second = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x402),
            1,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Keep Me",
                "baseUrl": "https://keep.example/v1",
                "credential": { "kind": "remove" },
                "presetKey": null,
                "targets": universal_targets("keep")
            }),
        )
        .await
        .unwrap();
    let keep_id = second.view.providers[1].id;

    let deleted = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x403),
            2,
            serde_json::json!({
                "kind": "delete-universal-provider",
                "providerId": delete_id,
                "providerRevision": 1
            }),
        )
        .await
        .unwrap();
    assert_eq!(deleted.view.providers.len(), 1);
    assert_eq!(deleted.view.providers[0].id, keep_id);
    assert_eq!(deleted.view.providers[0].position, 0);

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let (target_rows, credential_rows): (u64, u64) = database
        .call(move |connection| {
            let target_rows = connection.query_row(
                "SELECT COUNT(*) FROM universal_provider_targets
                 WHERE universal_provider_id = ?1",
                [delete_id.to_string()],
                |row| row.get(0),
            )?;
            let credential_rows =
                connection.query_row("SELECT COUNT(*) FROM universal_credentials", [], |row| {
                    row.get(0)
                })?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>((target_rows, credential_rows))
        })
        .await
        .unwrap();
    assert_eq!(target_rows, 0);
    assert_eq!(credential_rows, 0);
}

#[tokio::test]
async fn one_time_seed_is_keyed_only_by_the_stable_preset_key() {
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let ordinary = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x501),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "OpenAI API",
                "baseUrl": "https://ordinary.example/v1",
                "credential": { "kind": "remove" },
                "presetKey": null,
                "targets": universal_targets("ordinary")
            }),
        )
        .await
        .unwrap();
    assert_eq!(ordinary.view.providers.len(), 1);

    let seeded = store
        .seed_universal_provider_from_preset("openai-api-responses")
        .await
        .unwrap();
    assert_eq!(seeded.revision, 2);
    assert_eq!(seeded.providers.len(), 2);
    let seeded_provider = seeded
        .providers
        .iter()
        .find(|provider| provider.provenance.is_some())
        .unwrap();
    let seeded_id = seeded_provider.id;
    assert_eq!(seeded_provider.name, "OpenAI API");
    assert_eq!(seeded_provider.base_url, "https://api.openai.com/v1");

    let repeated = store
        .seed_universal_provider_from_preset("openai-api-responses")
        .await
        .unwrap();
    assert_eq!(repeated, seeded);

    let deleted = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x502),
            2,
            serde_json::json!({
                "kind": "delete-universal-provider",
                "providerId": seeded_id,
                "providerRevision": 1
            }),
        )
        .await
        .unwrap();
    assert_eq!(deleted.view.providers.len(), 1);
    let after_delete = store
        .seed_universal_provider_from_preset("openai-api-responses")
        .await
        .unwrap();
    assert_eq!(after_delete, deleted.view);

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let marker: String = database
        .call(|connection| {
            connection.query_row(
                "SELECT seeded_provider_id FROM universal_provider_seeds
                 WHERE preset_key = 'openai-api-responses'",
                [],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(marker, seeded_id.to_string());
}

impl CatalogFixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("muxvia-universal-providers-{}", Uuid::new_v4()));
        let user_home = root.join("home");
        fs::create_dir_all(&user_home).unwrap();
        Self {
            root,
            home: MuxviaHome::from_user_home(&user_home),
        }
    }
}

impl Drop for CatalogFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn empty_catalog_reopens_with_the_stable_preset_projection() {
    let fixture = CatalogFixture::new();
    let first = StateStore::open(&fixture.home).await.unwrap();
    let first_view = first.universal_provider_catalog().await.unwrap();
    drop(first);

    let reopened = StateStore::open(&fixture.home).await.unwrap();
    let reopened_view = reopened.universal_provider_catalog().await.unwrap();

    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/fixtures/universal-provider-catalog.json");
    let canonical: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture_path).unwrap()).unwrap();
    let expected = canonical["result"]["view"].clone();

    assert_eq!(
        serde_json::to_value(&first_view).unwrap(),
        expected,
        "fresh Universal Provider catalog did not match the stable projection"
    );
    assert_eq!(
        serde_json::to_value(reopened_view).unwrap(),
        expected,
        "reopened Universal Provider catalog did not match the stable projection"
    );
}

#[tokio::test]
async fn create_blank_and_preset_sources_uses_submitted_values_and_stable_provenance() {
    const SECRET: &str = "UNIVERSAL_CREATE_SECRET_74021";
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let codex_before = store.target_view_for(Target::Codex).await.unwrap();
    let claude_before = store.target_view_for(Target::Claude).await.unwrap();

    let blank = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x101),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Blank Source",
                "baseUrl": "",
                "credential": { "kind": "remove" },
                "presetKey": null,
                "targets": [
                    {
                        "target": "codex",
                        "enabled": false,
                        "model": "",
                        "authentication": "openai-bearer",
                        "routingRequirement": "direct-compatible"
                    },
                    {
                        "target": "claude",
                        "enabled": false,
                        "model": "",
                        "authentication": "anthropic-api-key",
                        "routingRequirement": "direct-compatible"
                    }
                ]
            }),
        )
        .await
        .unwrap();
    assert_eq!(blank.status, ActionStatus::Applied);

    let preset_action_id = Uuid::from_u128(0x102);
    let preset = store
        .apply_universal_provider_action(
            preset_action_id,
            1,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Submitted Name",
                "baseUrl": "https://submitted.example/v1/",
                "credential": { "kind": "replace", "value": SECRET },
                "presetKey": "openai-api-responses",
                "targets": [
                    {
                        "target": "codex",
                        "enabled": true,
                        "model": "submitted-codex-model",
                        "authentication": "openai-bearer",
                        "routingRequirement": "takeover-required"
                    },
                    {
                        "target": "claude",
                        "enabled": true,
                        "model": "submitted-claude-model",
                        "authentication": "anthropic-bearer",
                        "routingRequirement": "direct-compatible"
                    }
                ]
            }),
        )
        .await
        .unwrap();

    assert_eq!(preset.status, ActionStatus::Applied);
    assert_eq!(preset.view.revision, 2);
    assert_eq!(preset.view.view_sequence, 2);
    assert_eq!(preset.view.providers.len(), 2);
    let blank_view = &preset.view.providers[0];
    assert_eq!(blank_view.name, "Blank Source");
    assert_eq!(blank_view.base_url, "");
    assert!(blank_view.provenance.is_none());
    let preset_view = &preset.view.providers[1];
    assert_eq!(preset_view.name, "Submitted Name");
    assert_eq!(preset_view.base_url, "https://submitted.example/v1");
    assert_eq!(
        preset_view
            .provenance
            .as_ref()
            .map(|value| value.key.as_str()),
        Some("openai-api-responses")
    );
    assert!(
        preset_view
            .targets
            .iter()
            .all(|target| target.synchronization
                == muxvia_routing::control::protocol::UniversalSynchronizationState::Pending)
    );

    let replayed = store
        .apply_universal_provider_action(
            preset_action_id,
            0,
            serde_json::json!({ "kind": "definitely-malformed", "secret": SECRET }),
        )
        .await
        .unwrap();
    assert_eq!(replayed.status, ActionStatus::Replayed);
    assert_eq!(replayed.view, preset.view);

    let serialized = serde_json::to_string(&preset).unwrap();
    let debugged = format!("{preset:?}");
    assert!(
        !serialized.contains(SECRET),
        "catalog view serialized a credential"
    );
    assert!(
        !debugged.contains(SECRET),
        "catalog outcome debugged a credential"
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        codex_before
    );
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        claude_before
    );

    drop(store);
    let reopened = StateStore::open(&fixture.home).await.unwrap();
    assert_eq!(
        reopened.universal_provider_catalog().await.unwrap(),
        preset.view
    );
}
