use std::{fs, path::Path};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use muxvia_routing::{
    claude::ClaudeConfigCodec,
    codex::CodexConfigCodec,
    control::protocol::Target,
    home::MuxviaHome,
    state::{RecoveryIntent, RecoveryPayload, RecoveryState, StateStore},
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
        let desired = self.codec.desired_takeover(
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

    async fn pending_direct(&self) -> RecoveryIntent {
        let before = self.codec.inspect().unwrap();
        let desired = self.codec.desired_direct(
            "model-a",
            "https://provider.example/api/v1",
            "provider-secret",
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
#[derive(PartialEq, Eq)]
struct FileFingerprint {
    bytes: Vec<u8>,
    mode: u32,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[cfg(unix)]
impl std::fmt::Debug for FileFingerprint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileFingerprint")
            .field("bytes", &"<redacted>")
            .field("mode", &self.mode)
            .field("size", &self.size)
            .field("modified_seconds", &self.modified_seconds)
            .field("modified_nanoseconds", &self.modified_nanoseconds)
            .finish()
    }
}

#[cfg(unix)]
fn fingerprint(path: &Path) -> FileFingerprint {
    let metadata = fs::metadata(path).unwrap();
    FileFingerprint {
        bytes: fs::read(path).unwrap(),
        mode: metadata.permissions().mode() & 0o777,
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn direct_pending_reconciliation_restores_before_without_touching_auth_json() {
    let fixture = Fixture::new("approval_policy = \"never\"\n").await;
    let auth_path = fixture.codec.config_path().with_file_name("auth.json");
    fs::write(&auth_path, b"operator-auth-sentinel\n").unwrap();
    fs::set_permissions(&auth_path, fs::Permissions::from_mode(0o640)).unwrap();
    let auth_before = fingerprint(&auth_path);
    let intent = fixture.pending_direct().await;
    fixture
        .codec
        .atomic_apply(intent.before(), intent.desired())
        .unwrap();

    fixture
        .codec
        .reconcile_pending(&fixture.store)
        .await
        .unwrap();

    assert!(
        fs::read_to_string(fixture.codec.config_path()).unwrap() == "approval_policy = \"never\"\n",
        "Direct recovery did not restore the expected prior configuration"
    );
    let auth_debug = format!("{:?}", fingerprint(&auth_path));
    assert!(
        !auth_debug.contains("operator-auth-sentinel"),
        "recovery fingerprint diagnostics exposed auth text"
    );
    assert!(
        !auth_debug.contains(&format!("{:?}", b"operator-auth-sentinel\n")),
        "recovery fingerprint diagnostics exposed auth bytes"
    );
    assert_eq!(fingerprint(&auth_path), auth_before);
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
}

#[tokio::test]
async fn direct_pending_owned_or_unrelated_third_state_requires_recovery() {
    for changed in [
        "model = \"operator-owned-third-state\"\n",
        "approval_policy = \"never\"\noperator_changed = true\n",
    ] {
        let fixture = Fixture::new("approval_policy = \"never\"\n").await;
        let intent = fixture.pending_direct().await;
        fs::write(fixture.codec.config_path(), changed).unwrap();

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
        assert!(!format!("{error:?}\n{error}").contains("provider-secret"));
    }
}

#[test]
fn direct_and_legacy_takeover_desired_payloads_remain_mode_free_and_round_trip() {
    let root = TempDir::new().unwrap();
    let codec = CodexConfigCodec::for_user_home(root.path()).unwrap();
    for desired in [
        codec.desired_takeover(
            "gpt-test",
            "http://127.0.0.1:43123/v1",
            "recovery-route-secret",
        ),
        codec.desired_direct(
            "model-a",
            "https://provider.example/api/v1",
            "provider-secret",
        ),
    ] {
        let encoded = serde_json::to_value(&desired).unwrap();
        assert!(encoded.get("mode").is_none());
        let decoded = serde_json::from_value(encoded).unwrap();
        assert_eq!(desired, decoded);
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
    let active = fixture.codec.desired_takeover(
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
                "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, 'codex', 'secret')",
                [provider_id.to_string()],
            )?;
            transaction.execute(
                "INSERT INTO providers
                 (id, target, position, provider_revision, name, base_url, model, protocol, authentication,
                  routing_requirement, credential_id, provenance_kind, provenance_key,
                  generated_owner_id)
                 VALUES (?1, 'codex', 0, 1, 'Active', 'https://upstream.example/v1', 'gpt-active',
                         'openai-responses', 'openai-bearer', 'direct-compatible', ?1, NULL, NULL, NULL)",
                [provider_id.to_string()],
            )?;
            transaction.execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, protocol, authentication,
                  provider_bearer_token, epoch)
                 VALUES (?1, 'codex', ?2, 'https://upstream.example/v1', 'gpt-active',
                         'openai-responses', 'openai-bearer', 'secret', ?3)",
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
        fixture.codec.desired_takeover(
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
    write_frame(
        &mut stream,
        &serde_json::json!({
            "type":"request", "requestId":"open",
            "operation":{"kind":"open-target","target":"codex"}
        }),
    )
    .await
    .unwrap();
    read_frame(&mut stream).await.unwrap();
    for (request_id, action) in [
        (
            "save",
            serde_json::json!({
                "kind":"create-provider", "name":"blocked", "baseUrl":"https://blocked.test/v1",
                "model":"gpt", "credential":{"kind":"replace","value":"blocked-secret"}, "presetKey":null
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
    write_frame(
        &mut second_stream,
        &serde_json::json!({
            "type":"request", "requestId":"open-2",
            "operation":{"kind":"open-target","target":"codex"}
        }),
    )
    .await
    .unwrap();
    read_frame(&mut second_stream).await.unwrap();
    for (request_id, action) in [
        (
            "save-2",
            serde_json::json!({
                "kind":"create-provider", "name":"blocked", "baseUrl":"https://blocked.test/v1",
                "model":"gpt", "credential":{"kind":"replace","value":"blocked-secret"}, "presetKey":null
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

#[tokio::test]
async fn claude_pending_desired_state_is_restored_and_marked_rolled_back() {
    let root = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(root.path()).unwrap();
    fs::create_dir_all(codec.settings_path().parent().unwrap()).unwrap();
    fs::write(
        codec.settings_path(),
        r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"operator-prior"},"permissions":{"allow":["Read"]}}"#,
    )
    .unwrap();
    let store = StateStore::open(&MuxviaHome::from_user_home(root.path()))
        .await
        .unwrap();
    let before = codec.inspect().unwrap();
    let desired = codec.desired_takeover(
        "claude-test",
        "http://127.0.0.1:43124",
        "claude-routing-secret",
    );
    let intent = RecoveryIntent::pending_claude(
        Uuid::new_v4(),
        Uuid::new_v4(),
        codec.settings_path().to_owned(),
        before,
        desired,
        0,
    );
    store.insert_recovery_intent(&intent).await.unwrap();
    codec
        .atomic_apply(
            intent.claude_before().unwrap(),
            intent.claude_desired().unwrap(),
        )
        .unwrap();

    codec.reconcile_pending(&store).await.unwrap();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &fs::read_to_string(codec.settings_path()).unwrap()
        )
        .unwrap(),
        serde_json::json!({
            "env": {"ANTHROPIC_AUTH_TOKEN": "operator-prior"},
            "permissions": {"allow": ["Read"]}
        })
    );
    assert_eq!(
        store
            .recovery_intent(intent.id())
            .await
            .unwrap()
            .unwrap()
            .state(),
        RecoveryState::RolledBack
    );
}

#[cfg(unix)]
#[tokio::test]
async fn control_server_reconciles_claude_pending_before_accepting_sessions() {
    use muxvia_routing::control::server::ControlServer;
    use std::sync::Arc;

    let root = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(root.path()).unwrap();
    fs::create_dir_all(codec.settings_path().parent().unwrap()).unwrap();
    fs::write(
        codec.settings_path(),
        r#"{"permissions":{"allow":["Read"]}}"#,
    )
    .unwrap();
    let home = MuxviaHome::from_user_home(root.path());
    let store = Arc::new(StateStore::open(&home).await.unwrap());
    let intent = RecoveryIntent::pending_claude(
        Uuid::new_v4(),
        Uuid::new_v4(),
        codec.settings_path().to_owned(),
        codec.inspect().unwrap(),
        codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "routing-secret"),
        0,
    );
    store.insert_recovery_intent(&intent).await.unwrap();
    codec
        .atomic_apply(
            intent.claude_before().unwrap(),
            intent.claude_desired().unwrap(),
        )
        .unwrap();

    let handle = ControlServer::bind(&home, Arc::clone(&store), "test")
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
        serde_json::from_slice::<serde_json::Value>(&fs::read(codec.settings_path()).unwrap())
            .unwrap(),
        serde_json::json!({"permissions":{"allow":["Read"]}})
    );
    assert_eq!(store.target_view().await.unwrap().recovery.state, "clean");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn claude_pending_third_state_marks_only_claude_recovery_required() {
    let root = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(root.path()).unwrap();
    fs::create_dir_all(codec.settings_path().parent().unwrap()).unwrap();
    fs::write(codec.settings_path(), "{}").unwrap();
    let store = StateStore::open(&MuxviaHome::from_user_home(root.path()))
        .await
        .unwrap();
    let intent = RecoveryIntent::pending_claude(
        Uuid::new_v4(),
        Uuid::new_v4(),
        codec.settings_path().to_owned(),
        codec.inspect().unwrap(),
        codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "routing-secret"),
        0,
    );
    store.insert_recovery_intent(&intent).await.unwrap();
    fs::write(
        codec.settings_path(),
        r#"{"env":{"ANTHROPIC_MODEL":"operator-third-state"}}"#,
    )
    .unwrap();

    let error = codec.reconcile_pending(&store).await.unwrap_err();

    assert_eq!(error.code(), "recovery-required");
    assert_eq!(
        store
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .recovery
            .state,
        "recovery-required"
    );
    assert_eq!(store.target_view().await.unwrap().recovery.state, "clean");
    let blocked = store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            0,
            serde_json::json!({"credential": "must-not-store"}),
        )
        .await
        .unwrap_err();
    assert_eq!(blocked.problem.code, "recovery-required");
    assert_eq!(blocked.authoritative_view.target, Target::Claude);
    assert!(!format!("{intent:?}\n{error:?}\n{error}").contains("routing-secret"));
    assert!(!format!("{blocked:?}\n{blocked}").contains("must-not-store"));
}

#[test]
fn claude_recovery_payload_is_typed_tagged_and_accepts_legacy_schema_v4_shape() {
    let root = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(root.path()).unwrap();
    let before = codec.inspect().unwrap();
    let desired = codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "routing-secret");
    let payload = RecoveryPayload::Claude {
        before: Box::new(before.clone()),
        desired: Box::new(desired.clone()),
    };
    let encoded = serde_json::to_value(&payload).unwrap();
    assert_eq!(encoded["target"], "claude");
    assert!(encoded.get("before").is_some());
    assert!(encoded.get("desired").is_some());
    assert!(!format!("{payload:?}").contains("routing-secret"));

    let mut legacy_before = encoded["before"].clone();
    legacy_before
        .as_object_mut()
        .unwrap()
        .remove("unrelated_fingerprint");
    legacy_before
        .as_object_mut()
        .unwrap()
        .insert("unrelated".to_owned(), serde_json::json!({}));
    let legacy = serde_json::json!({
        "target": "claude",
        "before": legacy_before,
        "desired": serde_json::to_value(desired).unwrap()
    });
    let decoded: RecoveryPayload = serde_json::from_value(legacy).unwrap();
    match decoded {
        RecoveryPayload::Claude { before, desired } => {
            assert_eq!(*before, codec.inspect().unwrap());
            assert_eq!(
                *desired,
                codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "routing-secret")
            );
        }
        RecoveryPayload::ClaudeLegacy { .. } => panic!("typed Claude payload decoded as legacy"),
        RecoveryPayload::Codex { .. } => panic!("legacy Claude payload decoded as Codex"),
    }
}

#[tokio::test]
async fn claude_recovery_payload_persists_only_owned_values_and_unrelated_fingerprint() {
    let root = TempDir::new().unwrap();
    let codec = ClaudeConfigCodec::for_user_home(root.path()).unwrap();
    fs::create_dir_all(codec.settings_path().parent().unwrap()).unwrap();
    fs::write(
        codec.settings_path(),
        r#"{"env":{"ANTHROPIC_AUTH_TOKEN":"operator-prior-token","ANTHROPIC_API_KEY":"unrelated-api-key-sentinel","CUSTOM_SECRET":"custom-secret-sentinel"},"custom":{"secret":"custom-secret-sentinel"}}"#,
    )
    .unwrap();
    let store = StateStore::open(&MuxviaHome::from_user_home(root.path()))
        .await
        .unwrap();
    let intent = RecoveryIntent::pending_claude(
        Uuid::new_v4(),
        Uuid::new_v4(),
        codec.settings_path().to_owned(),
        codec.inspect().unwrap(),
        codec.desired_takeover("claude-test", "http://127.0.0.1:43124", "routing-token"),
        0,
    );
    store.insert_recovery_intent(&intent).await.unwrap();
    let database =
        tokio_rusqlite::Connection::open(MuxviaHome::from_user_home(root.path()).database_path())
            .await
            .unwrap();
    let payload_json = database
        .call(move |connection| {
            connection.query_row(
                "SELECT payload_json FROM activation_recovery WHERE id = ?1",
                [intent.id().to_string()],
                |row| row.get::<_, String>(0),
            )
        })
        .await
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();

    assert!(
        !payload_json.contains("unrelated-api-key-sentinel"),
        "Claude recovery payload retained an unrelated API key"
    );
    assert!(
        !payload_json.contains("custom-secret-sentinel"),
        "Claude recovery payload retained an unrelated custom secret"
    );
    assert!(
        payload["before"]["owned"]["auth_token"] == "operator-prior-token",
        "Claude recovery payload omitted the approved prior owned token field"
    );
    assert!(
        payload["desired"]["owned"]["auth_token"] == "routing-token",
        "Claude recovery payload omitted the approved desired owned token field"
    );
    assert!(
        payload["before"].get("unrelated_fingerprint").is_some(),
        "Claude recovery payload omitted the unrelated semantic fingerprint"
    );
    assert!(
        count_json_string(&payload, "operator-prior-token") == 1,
        "Claude recovery payload duplicated the prior owned token"
    );
    assert!(
        count_json_string(&payload, "routing-token") == 1,
        "Claude recovery payload duplicated the desired owned token"
    );
}

fn count_json_string(value: &serde_json::Value, needle: &str) -> usize {
    match value {
        serde_json::Value::String(value) => usize::from(value == needle),
        serde_json::Value::Array(values) => values
            .iter()
            .map(|value| count_json_string(value, needle))
            .sum(),
        serde_json::Value::Object(values) => values
            .values()
            .map(|value| count_json_string(value, needle))
            .sum(),
        _ => 0,
    }
}
