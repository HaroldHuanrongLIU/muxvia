use std::{fs, path::PathBuf};

use muxvia_routing::{
    control::protocol::{ActionStatus, CredentialPresence, ProviderReferenceView, Target},
    home::MuxviaHome,
    state::{StateStore, UniversalSynchronizationFailpoint},
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
async fn delete_cascades_unreferenced_generated_targets_atomically() {
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x411),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Generated Delete",
                "baseUrl": "https://generated-delete.example/v1",
                "credential": { "kind": "replace", "value": "GENERATED_DELETE_SECRET_99104" },
                "presetKey": null,
                "targets": universal_targets("generated-delete")
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    store
        .synchronize_universal_provider_action(Uuid::from_u128(0x412), 1, provider_id, 1)
        .await
        .unwrap();
    let catalog_before = store.universal_provider_catalog().await.unwrap();
    let codex_before = store.target_view_for(Target::Codex).await.unwrap();
    let claude_before = store.target_view_for(Target::Claude).await.unwrap();
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let owner = provider_id.to_string();
    database
        .call(move |connection| {
            connection.execute_batch(&format!(
                "CREATE TRIGGER fail_generated_delete
                 BEFORE DELETE ON providers
                 WHEN OLD.generated_owner_id = '{owner}' AND OLD.target = 'claude'
                 BEGIN SELECT RAISE(ABORT, 'controlled generated delete failure'); END;"
            ))
        })
        .await
        .unwrap();

    let failed = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x413),
            2,
            serde_json::json!({
                "kind": "delete-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(failed.problem.code, "state-store-error");
    assert_eq!(
        store.universal_provider_catalog().await.unwrap(),
        catalog_before
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        codex_before
    );
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        claude_before
    );
    database
        .call(|connection| connection.execute_batch("DROP TRIGGER fail_generated_delete"))
        .await
        .unwrap();

    let deleted = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x414),
            2,
            serde_json::json!({
                "kind": "delete-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1
            }),
        )
        .await
        .unwrap();
    assert!(deleted.view.providers.is_empty());
    assert_eq!(deleted.view.revision, 3);
    let codex_after = store.target_view_for(Target::Codex).await.unwrap();
    let claude_after = store.target_view_for(Target::Claude).await.unwrap();
    assert!(codex_after.providers.is_empty());
    assert!(claude_after.providers.is_empty());
    assert_eq!(
        codex_after.management_revision,
        codex_before.management_revision + 1
    );
    assert_eq!(
        claude_after.management_revision,
        claude_before.management_revision + 1
    );
    assert_target_runtime_unchanged(&codex_before, &codex_after);
    assert_target_runtime_unchanged(&claude_before, &claude_after);

    let counts: (u64, u64, u64, u64) = database
        .call(move |connection| {
            Ok::<_, tokio_rusqlite::rusqlite::Error>((
                connection.query_row("SELECT COUNT(*) FROM providers", [], |row| row.get(0))?,
                connection.query_row("SELECT COUNT(*) FROM credentials", [], |row| row.get(0))?,
                connection.query_row("SELECT COUNT(*) FROM universal_credentials", [], |row| row.get(0))?,
                connection.query_row(
                    "SELECT COUNT(*) FROM universal_provider_targets WHERE universal_provider_id = ?1",
                    [provider_id.to_string()],
                    |row| row.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(counts, (0, 0, 0, 0));
}

#[tokio::test]
async fn delete_reports_every_generated_reference_before_any_write() {
    const SNAPSHOT_SECRET: &str = "GENERATED_REFERENCE_SECRET_24611";
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x421),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Referenced Delete",
                "baseUrl": "https://referenced-delete.example/v1",
                "credential": { "kind": "replace", "value": "GENERATED_SOURCE_SECRET_24610" },
                "presetKey": null,
                "targets": universal_targets("referenced-delete")
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    let synchronized = store
        .synchronize_universal_provider_action(Uuid::from_u128(0x422), 1, provider_id, 1)
        .await
        .unwrap();
    let codex_id = synchronized.outcome.view.providers[0].targets[0]
        .generated_provider_id
        .unwrap();
    let claude_id = synchronized.outcome.view.providers[0].targets[1]
        .generated_provider_id
        .unwrap();
    let snapshot_id = Uuid::from_u128(0x423);
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(move |connection| {
            connection.execute(
                "UPDATE target_route_state SET current_provider_id = ?1 WHERE target = 'codex'",
                [codex_id.to_string()],
            )?;
            connection.execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, protocol, authentication,
                  provider_bearer_token, epoch)
                 SELECT ?1, target, id, base_url, model, protocol, authentication, ?2, ?3
                 FROM providers WHERE id = ?4 AND target = 'claude'",
                tokio_rusqlite::rusqlite::params![
                    snapshot_id.to_string(),
                    SNAPSHOT_SECRET,
                    Uuid::from_u128(0x424).to_string(),
                    claude_id.to_string(),
                ],
            )?;
            connection.execute(
                "UPDATE target_route_state SET activated_snapshot_id = ?1 WHERE target = 'claude'",
                [snapshot_id.to_string()],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    let codex_before = store.target_view_for(Target::Codex).await.unwrap();
    let claude_before = store.target_view_for(Target::Claude).await.unwrap();

    let failure = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x425),
            2,
            serde_json::json!({
                "kind": "delete-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.problem.code, "generated-provider-referenced");
    assert_eq!(
        failure.authoritative_view.providers[0].targets[0].active_references,
        [ProviderReferenceView::Current]
    );
    assert_eq!(
        failure.authoritative_view.providers[0].targets[1].active_references,
        [ProviderReferenceView::ActivatedSnapshot]
    );
    assert!(!format!("{failure:?}").contains(SNAPSHOT_SECRET));
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        codex_before
    );
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        claude_before
    );
    let receipt_count: u64 = database
        .call(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM universal_action_receipts WHERE action_id = ?1",
                [Uuid::from_u128(0x425).to_string()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(receipt_count, 0);
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

#[tokio::test]
async fn codex_only_synchronization_materializes_one_generated_provider_transactionally() {
    const SECRET: &str = "UNIVERSAL_SYNCHRONIZE_SECRET_92014";
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let codex_before = store.target_view_for(Target::Codex).await.unwrap();
    let claude_before = store.target_view_for(Target::Claude).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x601),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Codex Shared",
                "baseUrl": "https://sync.example/v1",
                "credential": { "kind": "replace", "value": SECRET },
                "presetKey": null,
                "targets": [
                    { "target": "codex", "enabled": true, "model": "sync-codex", "authentication": "openai-bearer", "routingRequirement": "takeover-required" },
                    { "target": "claude", "enabled": false, "model": "sync-claude", "authentication": "anthropic-api-key", "routingRequirement": "direct-compatible" }
                ]
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    let synchronization_id = Uuid::from_u128(0x602);

    let synchronized = store
        .synchronize_universal_provider_action(synchronization_id, 1, provider_id, 1)
        .await
        .unwrap();
    assert_eq!(synchronized.outcome.status, ActionStatus::Applied);
    assert_eq!(synchronized.outcome.view.revision, 2);
    assert_eq!(synchronized.target_views.len(), 1);
    let catalog_provider = &synchronized.outcome.view.providers[0];
    assert_eq!(
        catalog_provider.targets[0].synchronization,
        muxvia_routing::control::protocol::UniversalSynchronizationState::Current
    );
    assert!(catalog_provider.targets[0].generated_provider_id.is_some());
    assert_eq!(
        catalog_provider.targets[1].synchronization,
        muxvia_routing::control::protocol::UniversalSynchronizationState::Current
    );
    assert!(catalog_provider.targets[1].generated_provider_id.is_none());

    let codex_after = store.target_view_for(Target::Codex).await.unwrap();
    assert_eq!(codex_after.management_revision, 1);
    assert_eq!(codex_after.view_sequence, 1);
    assert_eq!(codex_after.providers.len(), 1);
    let generated = &codex_after.providers[0];
    assert!(generated.generated);
    assert_eq!(generated.name, "Codex Shared");
    assert_eq!(generated.base_url, "https://sync.example/v1");
    assert_eq!(generated.model, "sync-codex");
    assert_eq!(generated.credential, CredentialPresence::Present);
    assert_eq!(
        generated.routing_requirement.to_string(),
        "takeover-required"
    );
    assert_eq!(
        generated.provenance.as_ref().map(|value| value.key.clone()),
        Some(provider_id.to_string())
    );
    assert_target_runtime_unchanged(&codex_before, &codex_after);
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        claude_before
    );

    let replayed = store
        .synchronize_universal_provider_action(synchronization_id, 0, Uuid::nil(), 99)
        .await
        .unwrap();
    assert_eq!(replayed.outcome.status, ActionStatus::Replayed);
    assert!(replayed.target_views.is_empty());
    assert_eq!(replayed.outcome.view, synchronized.outcome.view);
    let no_change = store
        .synchronize_universal_provider_action(Uuid::from_u128(0x603), 2, provider_id, 1)
        .await
        .unwrap_err();
    assert_eq!(no_change.problem.code, "no-universal-provider-change");
    assert_eq!(no_change.authoritative_view, synchronized.outcome.view);
    let serialized = serde_json::to_string(&synchronized.outcome).unwrap();
    let debugged = format!("{synchronized:?}");
    assert!(
        !serialized.contains(SECRET),
        "synchronization outcome serialized a credential"
    );
    assert!(
        !debugged.contains(SECRET),
        "synchronization commit debugged a credential"
    );
}

fn assert_target_runtime_unchanged(
    before: &muxvia_routing::control::protocol::TargetView,
    after: &muxvia_routing::control::protocol::TargetView,
) {
    assert_eq!(after.service, before.service);
    assert_eq!(after.mode, before.mode);
    assert_eq!(after.takeover, before.takeover);
    assert_eq!(after.route_health, before.route_health);
    assert_eq!(after.current_provider_id, before.current_provider_id);
    assert_eq!(after.serving_provider_id, before.serving_provider_id);
    assert_eq!(after.managed_configuration, before.managed_configuration);
    assert_eq!(after.recovery, before.recovery);
    assert_eq!(after.activated_snapshot, before.activated_snapshot);
    assert_eq!(after.problems, before.problems);
}

#[tokio::test]
async fn universal_edit_stays_pending_until_both_generated_targets_resynchronize() {
    const FIRST_SECRET: &str = "UNIVERSAL_FIRST_SYNC_SECRET_11704";
    const SECOND_SECRET: &str = "UNIVERSAL_SECOND_SYNC_SECRET_11705";
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x701),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Both Targets",
                "baseUrl": "https://both.example/v1",
                "credential": { "kind": "replace", "value": FIRST_SECRET },
                "presetKey": null,
                "targets": universal_targets("first")
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    let first_sync = store
        .synchronize_universal_provider_action(Uuid::from_u128(0x702), 1, provider_id, 1)
        .await
        .unwrap();
    assert_eq!(first_sync.target_views.len(), 2);
    let first_ids: Vec<Uuid> = first_sync.outcome.view.providers[0]
        .targets
        .iter()
        .map(|target| target.generated_provider_id.unwrap())
        .collect();
    let first_credentials = generated_credential_ids(&fixture, provider_id).await;
    assert_eq!(first_credentials.len(), 2);
    assert_ne!(first_credentials[0], first_credentials[1]);

    let codex_generated_before = store
        .target_view_for(Target::Codex)
        .await
        .unwrap()
        .providers[0]
        .clone();
    let claude_generated_before = store
        .target_view_for(Target::Claude)
        .await
        .unwrap()
        .providers[0]
        .clone();
    let edited = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x703),
            2,
            serde_json::json!({
                "kind": "update-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1,
                "name": "Both Targets Updated",
                "baseUrl": "https://both-updated.example/v1",
                "credential": { "kind": "replace", "value": SECOND_SECRET },
                "targets": [
                    { "target": "codex", "enabled": true, "model": "second-codex", "authentication": "openai-bearer", "routingRequirement": "takeover-required" },
                    { "target": "claude", "enabled": true, "model": "second-claude", "authentication": "anthropic-bearer", "routingRequirement": "direct-compatible" }
                ]
            }),
        )
        .await
        .unwrap();
    assert!(
        edited.view.providers[0]
            .targets
            .iter()
            .all(|target| target.synchronization
                == muxvia_routing::control::protocol::UniversalSynchronizationState::Pending)
    );
    let codex_pending = store.target_view_for(Target::Codex).await.unwrap();
    let claude_pending = store.target_view_for(Target::Claude).await.unwrap();
    let mut expected_codex = codex_generated_before.clone();
    expected_codex.synchronization =
        Some(muxvia_routing::control::protocol::UniversalSynchronizationState::Pending);
    let mut expected_claude = claude_generated_before.clone();
    expected_claude.synchronization =
        Some(muxvia_routing::control::protocol::UniversalSynchronizationState::Pending);
    assert_eq!(codex_pending.providers[0], expected_codex);
    assert_eq!(claude_pending.providers[0], expected_claude);
    assert_eq!(codex_pending.management_revision, 1);
    assert_eq!(claude_pending.management_revision, 1);
    assert_eq!(codex_pending.view_sequence, 2);
    assert_eq!(claude_pending.view_sequence, 2);

    let update_failure = store
        .synchronize_universal_provider_action_with_failpoint(
            Uuid::from_u128(0x705),
            3,
            provider_id,
            2,
            UniversalSynchronizationFailpoint::TargetProviderWrite(Target::Claude),
        )
        .await
        .unwrap_err();
    assert_eq!(update_failure.problem.code, "state-store-error");
    assert_eq!(
        store.universal_provider_catalog().await.unwrap(),
        edited.view
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        codex_pending
    );
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        claude_pending
    );
    assert_eq!(
        generated_credential_ids(&fixture, provider_id).await,
        first_credentials
    );

    let synchronized = store
        .synchronize_universal_provider_action(Uuid::from_u128(0x704), 3, provider_id, 2)
        .await
        .unwrap();
    assert_eq!(synchronized.target_views.len(), 2);
    let provider = &synchronized.outcome.view.providers[0];
    assert!(provider.targets.iter().all(|target| target.synchronization
        == muxvia_routing::control::protocol::UniversalSynchronizationState::Current));
    let second_ids: Vec<Uuid> = provider
        .targets
        .iter()
        .map(|target| target.generated_provider_id.unwrap())
        .collect();
    assert_eq!(second_ids, first_ids);
    let second_credentials = generated_credential_ids(&fixture, provider_id).await;
    assert_eq!(second_credentials.len(), 2);
    assert_ne!(second_credentials[0], second_credentials[1]);
    assert_ne!(second_credentials, first_credentials);
    let codex = store.target_view_for(Target::Codex).await.unwrap();
    let claude = store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(codex.providers[0].provider_revision, 2);
    assert_eq!(codex.providers[0].name, "Both Targets Updated");
    assert_eq!(codex.providers[0].model, "second-codex");
    assert_eq!(claude.providers[0].provider_revision, 2);
    assert_eq!(claude.providers[0].name, "Both Targets Updated");
    assert_eq!(claude.providers[0].model, "second-claude");
    for value in [
        serde_json::to_string(&synchronized.outcome).unwrap(),
        format!("{synchronized:?}"),
    ] {
        assert!(
            !value.contains(FIRST_SECRET),
            "synchronization exposed the first credential"
        );
        assert!(
            !value.contains(SECOND_SECRET),
            "synchronization exposed the replacement credential"
        );
    }
}

async fn generated_credential_ids(fixture: &CatalogFixture, provider_id: Uuid) -> Vec<String> {
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(move |connection| {
            connection
                .prepare(
                    "SELECT credential_id FROM providers
                     WHERE generated_owner_id = ?1 ORDER BY target",
                )?
                .query_map([provider_id.to_string()], |row| row.get(0))?
                .collect::<Result<Vec<String>, _>>()
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn disabling_one_target_stays_pending_until_sync_removes_only_that_generated_record() {
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x801),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Disable Target",
                "baseUrl": "https://disable.example/v1",
                "credential": { "kind": "replace", "value": "DISABLE_SECRET_80311" },
                "presetKey": null,
                "targets": universal_targets("disable")
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    store
        .synchronize_universal_provider_action(Uuid::from_u128(0x802), 1, provider_id, 1)
        .await
        .unwrap();
    let codex_before = store.target_view_for(Target::Codex).await.unwrap();
    let claude_before = store.target_view_for(Target::Claude).await.unwrap();

    let disabled = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x803),
            2,
            serde_json::json!({
                "kind": "update-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1,
                "name": "Disable Target",
                "baseUrl": "https://disable.example/v1",
                "credential": { "kind": "keep" },
                "targets": [
                    { "target": "codex", "enabled": true, "model": "disable-codex", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "disable-claude", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                ]
            }),
        )
        .await
        .unwrap();
    assert_eq!(disabled.view.providers[0].provider_revision, 1);
    assert_eq!(
        disabled.view.providers[0].targets[0].synchronization,
        muxvia_routing::control::protocol::UniversalSynchronizationState::Current
    );
    assert_eq!(
        disabled.view.providers[0].targets[1].synchronization,
        muxvia_routing::control::protocol::UniversalSynchronizationState::Pending
    );
    let codex_after_edit = store.target_view_for(Target::Codex).await.unwrap();
    let claude_after_edit = store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(codex_after_edit, codex_before);
    assert_eq!(
        claude_after_edit.management_revision,
        claude_before.management_revision
    );
    assert_eq!(
        claude_after_edit.view_sequence,
        claude_before.view_sequence + 1
    );
    assert_eq!(
        claude_after_edit.providers[0].synchronization,
        Some(muxvia_routing::control::protocol::UniversalSynchronizationState::Pending)
    );

    let removal_failure = store
        .synchronize_universal_provider_action_with_failpoint(
            Uuid::from_u128(0x805),
            3,
            provider_id,
            1,
            UniversalSynchronizationFailpoint::TargetRemoval(Target::Claude),
        )
        .await
        .unwrap_err();
    assert_eq!(removal_failure.problem.code, "state-store-error");
    assert_eq!(
        store.universal_provider_catalog().await.unwrap(),
        disabled.view
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        codex_after_edit
    );
    assert_eq!(
        store.target_view_for(Target::Claude).await.unwrap(),
        claude_after_edit
    );
    assert_eq!(
        generated_credential_ids(&fixture, provider_id).await.len(),
        2
    );

    let synchronized = store
        .synchronize_universal_provider_action(Uuid::from_u128(0x804), 3, provider_id, 1)
        .await
        .unwrap();
    assert_eq!(synchronized.target_views.len(), 1);
    assert_eq!(synchronized.target_views[0].target, Target::Claude);
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        codex_before
    );
    let claude_after = store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(
        claude_after.management_revision,
        claude_before.management_revision + 1
    );
    assert!(claude_after.providers.is_empty());
    let catalog_provider = &synchronized.outcome.view.providers[0];
    assert_eq!(
        catalog_provider.targets[1].synchronization,
        muxvia_routing::control::protocol::UniversalSynchronizationState::Current
    );
    assert!(catalog_provider.targets[1].generated_provider_id.is_none());
    let credentials = generated_credential_ids(&fixture, provider_id).await;
    assert_eq!(credentials.len(), 1);
}

#[tokio::test]
async fn referenced_generated_provider_blocks_disable_before_any_synchronization_write() {
    let fixture = CatalogFixture::new();
    let store = StateStore::open(&fixture.home).await.unwrap();
    let created = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x811),
            0,
            serde_json::json!({
                "kind": "create-universal-provider",
                "name": "Referenced Source",
                "baseUrl": "https://referenced.example/v1",
                "credential": { "kind": "replace", "value": "REFERENCE_SECRET_90317" },
                "presetKey": null,
                "targets": [
                    { "target": "codex", "enabled": true, "model": "reference-codex", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "reference-claude", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                ]
            }),
        )
        .await
        .unwrap();
    let provider_id = created.view.providers[0].id;
    let synchronized = store
        .synchronize_universal_provider_action(Uuid::from_u128(0x812), 1, provider_id, 1)
        .await
        .unwrap();
    let generated_id = synchronized.outcome.view.providers[0].targets[0]
        .generated_provider_id
        .unwrap();
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(move |connection| {
            connection.execute(
                "UPDATE target_route_state SET current_provider_id = ?1 WHERE target = 'codex'",
                [generated_id.to_string()],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    let catalog_before = store.universal_provider_catalog().await.unwrap();
    let target_before = store.target_view_for(Target::Codex).await.unwrap();
    let failure = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x813),
            2,
            serde_json::json!({
                "kind": "update-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1,
                "name": "Referenced Source",
                "baseUrl": "https://referenced.example/v1",
                "credential": { "kind": "keep" },
                "targets": [
                    { "target": "codex", "enabled": false, "model": "reference-codex", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "reference-claude", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                ]
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "generated-provider-referenced");
    assert_eq!(failure.authoritative_view, catalog_before);
    assert!(failure.authoritative_view.providers[0].targets[0].enabled);
    assert_eq!(
        failure.authoritative_view.providers[0].targets[0].active_references,
        vec![muxvia_routing::control::protocol::ProviderReferenceView::Current]
    );
    assert_eq!(
        store.target_view_for(Target::Codex).await.unwrap(),
        target_before
    );
    let receipt_count = database
        .call(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM universal_action_receipts WHERE action_id = ?1",
                [Uuid::from_u128(0x813).to_string()],
                |row| row.get::<_, u64>(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(receipt_count, 0);

    database
        .call(|connection| {
            connection.execute(
                "UPDATE target_route_state SET current_provider_id = NULL WHERE target = 'codex'",
                [],
            )?;
            Ok::<_, tokio_rusqlite::rusqlite::Error>(())
        })
        .await
        .unwrap();
    let disabled = store
        .apply_universal_provider_action(
            Uuid::from_u128(0x815),
            2,
            serde_json::json!({
                "kind": "update-universal-provider",
                "providerId": provider_id,
                "providerRevision": 1,
                "name": "Referenced Source",
                "baseUrl": "https://referenced.example/v1",
                "credential": { "kind": "keep" },
                "targets": [
                    { "target": "codex", "enabled": false, "model": "reference-codex", "authentication": "openai-bearer", "routingRequirement": "direct-compatible" },
                    { "target": "claude", "enabled": false, "model": "reference-claude", "authentication": "anthropic-api-key", "routingRequirement": "takeover-required" }
                ]
            }),
        )
        .await
        .unwrap();
    assert!(!disabled.view.providers[0].targets[0].enabled);
}

#[tokio::test]
async fn synchronization_failpoints_roll_back_every_target_and_catalog_boundary() {
    for (index, failpoint) in [
        UniversalSynchronizationFailpoint::TargetCredential(Target::Codex),
        UniversalSynchronizationFailpoint::TargetProviderWrite(Target::Codex),
        UniversalSynchronizationFailpoint::TargetRevision(Target::Codex),
        UniversalSynchronizationFailpoint::TargetCredential(Target::Claude),
        UniversalSynchronizationFailpoint::TargetProviderWrite(Target::Claude),
        UniversalSynchronizationFailpoint::TargetRevision(Target::Claude),
        UniversalSynchronizationFailpoint::CatalogRevision,
        UniversalSynchronizationFailpoint::Receipt,
    ]
    .into_iter()
    .enumerate()
    {
        let fixture = CatalogFixture::new();
        let store = StateStore::open(&fixture.home).await.unwrap();
        let created = store
            .apply_universal_provider_action(
                Uuid::from_u128(0xA00 + index as u128),
                0,
                serde_json::json!({
                    "kind": "create-universal-provider",
                    "name": "Failpoint Source",
                    "baseUrl": "https://failpoint.example/v1",
                    "credential": { "kind": "replace", "value": "FAILPOINT_SECRET_22401" },
                    "presetKey": null,
                    "targets": universal_targets("failpoint")
                }),
            )
            .await
            .unwrap();
        let provider_id = created.view.providers[0].id;
        let codex_before = store.target_view_for(Target::Codex).await.unwrap();
        let claude_before = store.target_view_for(Target::Claude).await.unwrap();

        let failure = store
            .synchronize_universal_provider_action_with_failpoint(
                Uuid::from_u128(0xB00 + index as u128),
                1,
                provider_id,
                1,
                failpoint,
            )
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, "state-store-error");
        assert_eq!(
            store.universal_provider_catalog().await.unwrap(),
            created.view
        );
        assert_eq!(
            store.target_view_for(Target::Codex).await.unwrap(),
            codex_before
        );
        assert_eq!(
            store.target_view_for(Target::Claude).await.unwrap(),
            claude_before
        );
        assert!(
            generated_credential_ids(&fixture, provider_id)
                .await
                .is_empty()
        );
    }
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
