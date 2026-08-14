use std::{fs, path::PathBuf, sync::Arc};

use muxvia_routing::{
    control::protocol::{ActionStatus, CredentialPresence, ProviderReferenceView},
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
            std::env::temp_dir().join(format!("muxvia-provider-lifecycle-{}", Uuid::new_v4()));
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

fn create(name: &str, secret: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "create-provider",
        "name": name,
        "baseUrl": format!("https://{}.example/v1", name.to_lowercase()),
        "model": format!("{}-model", name.to_lowercase()),
        "credential": { "kind": "replace", "value": secret },
        "presetKey": null,
    })
}

fn reorder(provider_ids: &[Uuid]) -> serde_json::Value {
    serde_json::json!({
        "kind": "reorder-providers",
        "providerIds": provider_ids,
    })
}

fn delete(provider_id: Uuid, provider_revision: u64) -> serde_json::Value {
    serde_json::json!({
        "kind": "delete-provider",
        "providerId": provider_id,
        "providerRevision": provider_revision,
    })
}

async fn create_three(
    store: &Arc<StateStore>,
) -> Vec<muxvia_routing::control::protocol::ProviderView> {
    let mut revision = 0;
    let mut providers = Vec::new();
    for (id, name) in [(1, "A"), (2, "B"), (3, "C")] {
        let outcome = store
            .apply_provider_action(
                action_id(id),
                revision,
                create(name, &format!("{name}-secret")),
            )
            .await
            .unwrap();
        revision = outcome.view.management_revision;
        providers.push(outcome.view.providers.last().unwrap().clone());
    }
    providers
}

async fn credential_count(home: &MuxviaHome) -> u64 {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(|connection| {
            connection.query_row("SELECT COUNT(*) FROM credentials", [], |row| row.get(0))
        })
        .await
        .unwrap()
}

async fn share_credential(home: &MuxviaHome, source: Uuid, destination: Uuid) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| -> Result<()> {
            let source_credential: String = connection.query_row(
                "SELECT credential_id FROM providers WHERE id = ?1",
                [source.to_string()],
                |row| row.get(0),
            )?;
            let previous_credential: String = connection.query_row(
                "SELECT credential_id FROM providers WHERE id = ?1",
                [destination.to_string()],
                |row| row.get(0),
            )?;
            connection.execute(
                "UPDATE providers SET credential_id = ?1 WHERE id = ?2",
                params![source_credential, destination.to_string()],
            )?;
            connection.execute(
                "DELETE FROM credentials WHERE id = ?1",
                [previous_credential],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn set_current_and_snapshot(home: &MuxviaHome, current: Uuid, snapshot: Uuid) {
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
                    snapshot.to_string(),
                    current.to_string(),
                    Uuid::new_v4().to_string()
                ],
            )?;
            connection.execute(
                "UPDATE target_route_state
                 SET current_provider_id = ?1, activated_snapshot_id = ?2,
                     managed_config_path = '/tmp/muxvia-config.toml' WHERE target = 'codex'",
                params![current.to_string(), snapshot.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn set_current_only(home: &MuxviaHome, provider_id: Uuid) {
    tokio_rusqlite::Connection::open(home.database_path())
        .await
        .unwrap()
        .call(move |connection| -> Result<()> {
            connection.execute(
                "UPDATE target_route_state
                 SET current_provider_id = ?1, activated_snapshot_id = NULL WHERE target = 'codex'",
                [provider_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn set_snapshot_only(home: &MuxviaHome, provider_id: Uuid, snapshot: Uuid) {
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
                    snapshot.to_string(),
                    provider_id.to_string(),
                    Uuid::new_v4().to_string()
                ],
            )?;
            connection.execute(
                "UPDATE target_route_state
                 SET current_provider_id = NULL, activated_snapshot_id = ?1 WHERE target = 'codex'",
                [snapshot.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn reorder_projects_the_requested_full_order_without_changing_provider_or_runtime_state() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let providers = create_three(&store).await;
    let snapshot = action_id(40);
    set_current_and_snapshot(&fixture.home, providers[0].id, snapshot).await;
    let before = store.target_view().await.unwrap();

    let outcome = store
        .apply_provider_action(
            action_id(4),
            before.management_revision,
            reorder(&[providers[2].id, providers[0].id, providers[1].id]),
        )
        .await
        .unwrap();

    assert_eq!(outcome.status, ActionStatus::Applied);
    assert_eq!(
        outcome.view.management_revision,
        before.management_revision + 1
    );
    assert_eq!(outcome.view.view_sequence, before.view_sequence + 1);
    assert_eq!(
        outcome
            .view
            .providers
            .iter()
            .map(|provider| provider.id)
            .collect::<Vec<_>>(),
        [providers[2].id, providers[0].id, providers[1].id],
    );
    assert_eq!(
        outcome
            .view
            .providers
            .iter()
            .map(|provider| provider.provider_revision)
            .collect::<Vec<_>>(),
        [
            providers[2].provider_revision,
            providers[0].provider_revision,
            providers[1].provider_revision
        ],
    );
    assert_eq!(outcome.view.current_provider_id, before.current_provider_id);
    assert_eq!(outcome.view.activated_snapshot, before.activated_snapshot);
    assert_eq!(
        outcome.view.managed_configuration,
        before.managed_configuration
    );
}

#[tokio::test]
async fn invalid_or_unchanged_provider_orders_are_atomic_and_do_not_write_receipts() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let providers = create_three(&store).await;
    let before = store.target_view().await.unwrap();

    for (id, order) in [
        (
            action_id(10),
            vec![providers[0].id, providers[0].id, providers[1].id],
        ),
        (action_id(11), vec![providers[0].id, providers[1].id]),
        (
            action_id(12),
            vec![providers[0].id, providers[1].id, action_id(13)],
        ),
    ] {
        let failure = store
            .apply_provider_action(id, before.management_revision, reorder(&order))
            .await
            .unwrap_err();
        assert_eq!(failure.problem.code, "invalid-provider-order");
        assert_eq!(failure.authoritative_view, before);
        assert!(store.receipt(id).await.unwrap().is_none());
    }

    let unchanged_id = action_id(14);
    let unchanged = store
        .apply_provider_action(
            unchanged_id,
            before.management_revision,
            reorder(
                &providers
                    .iter()
                    .map(|provider| provider.id)
                    .collect::<Vec<_>>(),
            ),
        )
        .await
        .unwrap_err();
    assert_eq!(unchanged.problem.code, "no-provider-change");
    assert_eq!(unchanged.authoritative_view, before);
    assert!(store.receipt(unchanged_id).await.unwrap().is_none());
}

#[tokio::test]
async fn delete_compacts_positions_and_collects_only_orphaned_credentials() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let providers = create_three(&store).await;
    let before_count = credential_count(&fixture.home).await;
    assert_eq!(before_count, 3);

    let deleted = store
        .apply_provider_action(
            action_id(20),
            3,
            delete(providers[1].id, providers[1].provider_revision),
        )
        .await
        .unwrap();
    assert_eq!(credential_count(&fixture.home).await, 2);
    assert_eq!(
        deleted
            .view
            .providers
            .iter()
            .map(|provider| (provider.id, provider.position))
            .collect::<Vec<_>>(),
        [(providers[0].id, 0), (providers[2].id, 1)],
    );

    share_credential(&fixture.home, providers[0].id, providers[2].id).await;
    let shared = store.target_view().await.unwrap();
    assert_eq!(credential_count(&fixture.home).await, 1);
    let remaining = shared
        .providers
        .iter()
        .find(|provider| provider.id == providers[2].id)
        .unwrap();
    let after_shared_delete = store
        .apply_provider_action(
            action_id(21),
            shared.management_revision,
            delete(providers[0].id, providers[0].provider_revision),
        )
        .await
        .unwrap();
    assert_eq!(credential_count(&fixture.home).await, 1);
    assert_eq!(after_shared_delete.view.providers[0].id, remaining.id);
    assert_eq!(after_shared_delete.view.providers[0].position, 0);
    assert_eq!(
        after_shared_delete.view.providers[0].credential,
        CredentialPresence::Present
    );
}

#[tokio::test]
async fn current_or_activated_snapshot_references_protect_delete_with_the_authoritative_view() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let providers = create_three(&store).await;
    set_current_only(&fixture.home, providers[0].id).await;
    let current_view = store.target_view().await.unwrap();
    let current_failure = store
        .apply_provider_action(
            action_id(31),
            current_view.management_revision,
            delete(providers[0].id, providers[0].provider_revision),
        )
        .await
        .unwrap_err();

    assert_eq!(current_failure.problem.code, "provider-referenced");
    assert_eq!(current_failure.authoritative_view, current_view);
    assert_eq!(
        current_failure.authoritative_view.providers[0].active_references,
        [ProviderReferenceView::Current],
    );
    assert!(store.receipt(action_id(31)).await.unwrap().is_none());

    set_snapshot_only(&fixture.home, providers[1].id, action_id(32)).await;
    let snapshot_view = store.target_view().await.unwrap();
    let snapshot_failure = store
        .apply_provider_action(
            action_id(33),
            snapshot_view.management_revision,
            delete(providers[1].id, providers[1].provider_revision),
        )
        .await
        .unwrap_err();
    assert_eq!(snapshot_failure.problem.code, "provider-referenced");
    assert_eq!(snapshot_failure.authoritative_view, snapshot_view);
    assert_eq!(
        snapshot_failure.authoritative_view.providers[1].active_references,
        [ProviderReferenceView::ActivatedSnapshot],
    );
    assert!(store.receipt(action_id(33)).await.unwrap().is_none());
}

#[tokio::test]
async fn stale_delete_requests_and_duplicate_action_ids_do_not_apply_more_than_once() {
    let fixture = StoreFixture::new();
    let store = fixture.open().await;
    let provider = create_three(&store).await.remove(0);
    let before = store.target_view().await.unwrap();

    let stale_provider = store
        .apply_provider_action(
            action_id(50),
            before.management_revision,
            delete(provider.id, provider.provider_revision + 1),
        )
        .await
        .unwrap_err();
    assert_eq!(stale_provider.problem.code, "stale-provider-revision");
    assert_eq!(stale_provider.authoritative_view, before);

    let stale_target = store
        .apply_provider_action(
            action_id(51),
            before.management_revision - 1,
            delete(provider.id, provider.provider_revision),
        )
        .await
        .unwrap_err();
    assert_eq!(stale_target.problem.code, "stale-revision");
    assert_eq!(stale_target.authoritative_view, before);

    let duplicate_id = action_id(52);
    let first = store.apply_provider_action(
        duplicate_id,
        before.management_revision,
        delete(provider.id, provider.provider_revision),
    );
    let second = store.apply_provider_action(
        duplicate_id,
        before.management_revision,
        delete(provider.id, provider.provider_revision),
    );
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    let statuses = [first.status, second.status];
    assert!(statuses.contains(&ActionStatus::Applied));
    assert!(statuses.contains(&ActionStatus::Replayed));
    assert_eq!(first.view, second.view);
    assert_eq!(
        store.target_view().await.unwrap().management_revision,
        before.management_revision + 1
    );
    assert_eq!(store.target_view().await.unwrap().providers.len(), 2);
}
