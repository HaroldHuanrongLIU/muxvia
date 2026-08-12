use std::fs;

use muxvia_routing::{
    codex::CodexConfigCodec,
    home::MuxviaHome,
    state::{RecoveryIntent, RecoveryState, StateStore},
};
use tempfile::TempDir;
use uuid::Uuid;

struct Fixture {
    _root: TempDir,
    home: MuxviaHome,
    codec: CodexConfigCodec,
    store: StateStore,
}

impl Fixture {
    async fn new(initial: &str) -> Self {
        let root = TempDir::new().unwrap();
        let codec = CodexConfigCodec::for_user_home(root.path()).unwrap();
        fs::create_dir_all(codec.config_path().parent().unwrap()).unwrap();
        fs::write(codec.config_path(), initial).unwrap();
        let home = MuxviaHome::from_user_home(root.path());
        let store = StateStore::open(&home).await.unwrap();
        Self {
            _root: root,
            home,
            codec,
            store,
        }
    }

    async fn pending(&self) -> RecoveryIntent {
        let before = self.codec.inspect().unwrap();
        let desired = self.codec.desired(
            "gpt-test",
            "http://127.0.0.1:43123/v1",
            "recovery-route-secret",
        );
        let intent = RecoveryIntent::pending(
            Uuid::new_v4(),
            Uuid::new_v4(),
            self.codec.config_path().to_owned(),
            before,
            desired,
            0,
        );
        self.store.insert_recovery_intent(&intent).await.unwrap();
        intent
    }
}

#[cfg(unix)]
#[tokio::test]
async fn control_server_reconciles_pending_desired_before_accepting_sessions() {
    use muxvia_routing::control::server::ControlServer;
    use std::sync::Arc;

    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let intent = fixture.pending().await;
    fixture
        .codec
        .atomic_apply(intent.before(), intent.desired())
        .unwrap();
    let store = Arc::new(fixture.store);

    let handle = ControlServer::bind(&fixture.home, Arc::clone(&store), "test")
        .await
        .unwrap();

    assert_eq!(
        store
            .recovery_intent(intent.id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        RecoveryState::RolledBack
    );
    assert_eq!(
        fs::read_to_string(fixture.codec.config_path()).unwrap(),
        "approval_policy = \"never\"\n"
    );
    handle.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn control_server_persists_third_state_before_allowing_read_only_control() {
    use muxvia_routing::control::server::ControlServer;
    use std::sync::Arc;

    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    fixture.pending().await;
    fs::write(fixture.codec.config_path(), "operator_changed = true\n").unwrap();
    let store = Arc::new(fixture.store);

    let handle = ControlServer::bind(&fixture.home, Arc::clone(&store), "test")
        .await
        .unwrap();

    let view = store.target_view().await.unwrap();
    assert_eq!(view.recovery.state, "recovery-required");
    let blocked = store
        .apply_save_provider_action(
            Uuid::new_v4(),
            0,
            serde_json::json!({
                "kind": "save-provider", "name": "blocked",
                "baseUrl": "https://api.example/v1", "model": "gpt",
                "credential": "must-not-store"
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(blocked.problem.code, "recovery-required");
    handle.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_required_startup_opens_read_only_control_without_resuming_committed_model_route()
{
    use muxvia_routing::control::{
        framing::{read_frame, write_frame},
        server::ControlServer,
    };
    use std::{net::Ipv4Addr, sync::Arc};

    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let routing_credential = "a".repeat(64);
    let before_takeover = fixture.codec.inspect().unwrap();
    let active = fixture.codec.desired(
        "gpt-active",
        &format!("http://127.0.0.1:{port}/v1"),
        &routing_credential,
    );
    fixture
        .codec
        .atomic_apply(&before_takeover, &active)
        .unwrap();
    let active_snapshot = fixture.codec.inspect_managed(&active).unwrap();
    let provider_id = Uuid::new_v4();
    let snapshot_id = Uuid::new_v4();
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    let routing_credential_for_db = routing_credential.clone();
    let config_path_for_db = fixture.codec.config_path().to_string_lossy().into_owned();
    database
        .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO providers (id, target, name, base_url, model)
                 VALUES (?1, 'codex', 'Active', 'https://upstream.example/v1', 'gpt-active')",
                [provider_id.to_string()],
            )?;
            transaction.execute(
                "INSERT INTO provider_credentials (provider_id, bearer_token) VALUES (?1, 'secret')",
                [provider_id.to_string()],
            )?;
            transaction.execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, provider_bearer_token, epoch)
                 VALUES (?1, 'codex', ?2, 'https://upstream.example/v1', 'gpt-active', 'secret', ?3)",
                (snapshot_id.to_string(), provider_id.to_string(), Uuid::new_v4().to_string()),
            )?;
            transaction.execute(
                "UPDATE target_route_state SET management_revision = 1, view_sequence = 1,
                 current_provider_id = ?1, takeover_state = 'active', route_port = ?2,
                 routing_credential = ?3, activated_snapshot_id = ?4,
                 managed_config_path = ?5 WHERE target = 'codex'",
                (
                    provider_id.to_string(),
                    port,
                    routing_credential_for_db,
                    snapshot_id.to_string(),
                    config_path_for_db,
                ),
            )?;
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    let pending = RecoveryIntent::pending(
        Uuid::new_v4(),
        Uuid::new_v4(),
        fixture.codec.config_path().to_owned(),
        active_snapshot,
        fixture.codec.desired(
            "gpt-next",
            &format!("http://127.0.0.1:{port}/v1"),
            &routing_credential,
        ),
        1,
    );
    fixture
        .store
        .insert_recovery_intent(&pending)
        .await
        .unwrap();
    fs::write(fixture.codec.config_path(), "operator_changed = true\n").unwrap();
    let store = Arc::new(fixture.store);

    let handle = ControlServer::bind(&fixture.home, Arc::clone(&store), "test")
        .await
        .unwrap();

    assert_eq!(
        store.target_view().await.unwrap().recovery.state,
        "recovery-required"
    );
    let unbound = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("recovery-required startup must not resume the committed model route");
    let mut stream = tokio::net::UnixStream::connect(handle.socket_path())
        .await
        .unwrap();
    write_frame(
        &mut stream,
        &serde_json::json!({
            "type":"hello", "rpc":{"major":1,"minor":0}, "release":"test"
        }),
    )
    .await
    .unwrap();
    read_frame(&mut stream).await.unwrap();
    for (request_id, action) in [
        (
            "save",
            serde_json::json!({
                "kind":"save-provider", "name":"blocked", "baseUrl":"https://blocked.test/v1",
                "model":"gpt", "credential":"blocked-secret"
            }),
        ),
        (
            "activate",
            serde_json::json!({
                "kind":"activate-provider", "providerId":provider_id, "mode":"takeover"
            }),
        ),
    ] {
        write_frame(
            &mut stream,
            &serde_json::json!({
                "type":"request", "requestId":request_id,
                "operation":{"kind":"act","target":"codex","actionId":Uuid::new_v4(),
                  "expectedRevision":1,"action":action}
            }),
        )
        .await
        .unwrap();
        let response = read_frame(&mut stream).await.unwrap();
        assert_eq!(response["problem"]["code"], "recovery-required");
    }
    drop(unbound);
    handle.shutdown().await.unwrap();

    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let second_handle = ControlServer::bind(&fixture.home, Arc::clone(&second_store), "test-2")
        .await
        .unwrap();

    assert_eq!(
        second_store.target_view().await.unwrap().recovery.state,
        "recovery-required"
    );
    let second_epoch_unbound = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .expect("persisted recovery-required must remain control-only in a new service epoch");
    let mut second_stream = tokio::net::UnixStream::connect(second_handle.socket_path())
        .await
        .unwrap();
    write_frame(
        &mut second_stream,
        &serde_json::json!({
            "type":"hello", "rpc":{"major":1,"minor":0}, "release":"test-2"
        }),
    )
    .await
    .unwrap();
    read_frame(&mut second_stream).await.unwrap();
    for (request_id, action) in [
        (
            "save-2",
            serde_json::json!({
                "kind":"save-provider", "name":"blocked", "baseUrl":"https://blocked.test/v1",
                "model":"gpt", "credential":"blocked-secret"
            }),
        ),
        (
            "activate-2",
            serde_json::json!({
                "kind":"activate-provider", "providerId":provider_id, "mode":"takeover"
            }),
        ),
    ] {
        write_frame(
            &mut second_stream,
            &serde_json::json!({
                "type":"request", "requestId":request_id,
                "operation":{"kind":"act","target":"codex","actionId":Uuid::new_v4(),
                  "expectedRevision":1,"action":action}
            }),
        )
        .await
        .unwrap();
        let response = read_frame(&mut second_stream).await.unwrap();
        assert_eq!(response["problem"]["code"], "recovery-required");
    }
    drop(second_epoch_unbound);
    second_handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn pending_file_matching_before_is_marked_rolled_back() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let intent = fixture.pending().await;

    fixture
        .codec
        .reconcile_pending(&fixture.store)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .store
            .recovery_intent(intent.id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        RecoveryState::RolledBack
    );
    assert_eq!(
        fs::read_to_string(fixture.codec.config_path()).unwrap(),
        "approval_policy = \"never\"\n"
    );
}

#[tokio::test]
async fn pending_file_matching_desired_is_restored_and_marked_rolled_back() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let intent = fixture.pending().await;
    fixture
        .codec
        .atomic_apply(intent.before(), intent.desired())
        .unwrap();

    fixture
        .codec
        .reconcile_pending(&fixture.store)
        .await
        .unwrap();

    assert_eq!(
        fixture
            .store
            .recovery_intent(intent.id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        RecoveryState::RolledBack
    );
    assert_eq!(
        fs::read_to_string(fixture.codec.config_path()).unwrap(),
        "approval_policy = \"never\"\n"
    );
}

#[tokio::test]
async fn pending_file_in_third_state_requires_recovery_and_blocks_writes() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let intent = fixture.pending().await;
    fs::write(fixture.codec.config_path(), "operator_changed = true\n").unwrap();

    let error = fixture
        .codec
        .reconcile_pending(&fixture.store)
        .await
        .unwrap_err();

    assert_eq!(error.code(), "recovery-required");
    assert_eq!(
        fixture
            .store
            .recovery_intent(intent.id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        RecoveryState::RecoveryRequired
    );
    let view = fixture.store.target_view().await.unwrap();
    assert_eq!(view.managed_configuration.state, "recovery-required");
    assert_eq!(view.recovery.state, "recovery-required");
    assert_eq!(view.recovery.intent_id, Some(intent.id().to_string()));
    assert_eq!(view.management_revision, 0);
    assert_eq!(view.view_sequence, 1);
    assert_eq!(
        fixture
            .store
            .ensure_managed_writes_allowed()
            .await
            .unwrap_err()
            .code(),
        "recovery-required"
    );
}

#[tokio::test]
async fn pending_unrelated_semantic_drift_is_a_third_state() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let intent = fixture.pending().await;
    let mut changed = fs::read_to_string(fixture.codec.config_path()).unwrap();
    changed.push_str("operator_changed = true\n");
    fs::write(fixture.codec.config_path(), changed).unwrap();

    assert_eq!(
        fixture
            .codec
            .reconcile_pending(&fixture.store)
            .await
            .unwrap_err()
            .code(),
        "recovery-required"
    );
    assert_eq!(
        fixture
            .store
            .recovery_intent(intent.id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        RecoveryState::RecoveryRequired
    );
}

#[tokio::test]
async fn recovery_required_blocks_new_save_at_raw_boundary_but_receipt_replays_first() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let action_id = Uuid::new_v4();
    let first = serde_json::json!({
        "kind": "save-provider",
        "name": "First",
        "baseUrl": "https://api.example.test/v1",
        "model": "gpt-test",
        "credential": "provider-secret"
    });
    fixture
        .store
        .apply_save_provider_action(action_id, 0, first)
        .await
        .unwrap();
    let intent = fixture.pending().await;
    fixture
        .store
        .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
        .await
        .unwrap();

    let replay = fixture
        .store
        .apply_save_provider_action(action_id, 999, serde_json::json!({ "malformed": true }))
        .await
        .unwrap();
    assert_eq!(
        replay.status,
        muxvia_routing::control::protocol::ActionStatus::Replayed
    );
    let blocked = fixture
        .store
        .apply_save_provider_action(
            Uuid::new_v4(),
            1,
            serde_json::json!({ "kind": "save-provider", "credential": "blocked-secret" }),
        )
        .await
        .unwrap_err();
    assert_eq!(blocked.problem.code, "recovery-required");
    assert!(!format!("{blocked:?}\n{blocked}").contains("blocked-secret"));
}

#[tokio::test]
async fn completed_rows_do_not_mutate_the_file_at_startup() {
    for completed in [RecoveryState::Committed, RecoveryState::RolledBack] {
        let fixture = Fixture::new("approval_policy = \"never\"\n").await;
        let intent = fixture.pending().await;
        fixture
            .store
            .set_recovery_state(intent.id(), completed)
            .await
            .unwrap();
        fs::write(fixture.codec.config_path(), "operator_changed = true\n").unwrap();

        fixture
            .codec
            .reconcile_pending(&fixture.store)
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(fixture.codec.config_path()).unwrap(),
            "operator_changed = true\n"
        );
    }
}

#[tokio::test]
async fn restore_failure_keeps_row_and_target_recovery_required_without_secret_leak() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let intent = fixture.pending().await;
    fixture
        .codec
        .atomic_apply(intent.before(), intent.desired())
        .unwrap();
    let failing = CodexConfigCodec::for_user_home_with_pre_rename_hook(
        fixture._root.path(),
        std::sync::Arc::new(|_| Err(std::io::Error::other("injected restore failure"))),
    )
    .unwrap();

    let error = failing.reconcile_pending(&fixture.store).await.unwrap_err();
    let displayed = format!("{error:?}\n{error}");

    assert_eq!(error.code(), "recovery-required");
    assert_eq!(
        fixture
            .store
            .recovery_intent(intent.id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        RecoveryState::RecoveryRequired
    );
    assert_eq!(
        fixture
            .store
            .target_view()
            .await
            .unwrap()
            .managed_configuration
            .state,
        "recovery-required"
    );
    assert!(!displayed.contains("recovery-route-secret"));
    assert!(!displayed.contains("provider-secret"));
}

#[tokio::test]
async fn recovery_intent_debug_projects_only_public_identity_and_state() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let intent = fixture.pending().await;
    let debug = format!("{intent:?}");

    assert!(debug.contains(&intent.id().to_string()));
    assert!(debug.contains("Pending"));
    assert!(!debug.contains("recovery-route-secret"));
    assert!(!debug.contains("approval_policy"));
}
