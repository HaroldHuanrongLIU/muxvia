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
