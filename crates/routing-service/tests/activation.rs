use std::{
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use muxvia_routing::{
    codex::{CodexCapability, CodexProbe, CodexProblem, CommandCodexProbe},
    control::{
        framing::{read_frame, write_frame},
        protocol::{ActionStatus, TakeoverMode},
        server::ControlServer,
    },
    home::MuxviaHome,
    model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
    service::activate::{
        ActivateProviderCommand, ActivationFailpoint, ActivationHooks, ActivationObserver,
        ActivationPause, ActivationService, ActivationStep,
    },
    state::StateStore,
};
use tempfile::TempDir;
use uuid::Uuid;

struct GoodProbe(AtomicUsize);

impl CodexProbe for GoodProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(CodexCapability::Tested {
            version: "test".into(),
        })
    }
}

struct BadProbe;

impl CodexProbe for BadProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        CommandCodexProbe.probe(Path::new("relative-codex"))
    }
}

struct NoopUpstream;

#[async_trait]
impl UpstreamTransport for NoopUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        Err(UpstreamError)
    }
}

#[derive(Default)]
struct Steps(Mutex<Vec<ActivationStep>>);

impl ActivationObserver for Steps {
    fn reached(&self, step: ActivationStep) {
        self.0.lock().unwrap().push(step);
    }
}

struct Fixture {
    _temp: TempDir,
    home: MuxviaHome,
    store: Arc<StateStore>,
    probe: Arc<GoodProbe>,
}

impl Fixture {
    async fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = MuxviaHome::from_user_home(temp.path());
        let store = Arc::new(StateStore::open(&home).await.unwrap());
        Self {
            _temp: temp,
            home,
            store,
            probe: Arc::new(GoodProbe(AtomicUsize::new(0))),
        }
    }

    async fn save(&self, name: &str, model: &str, secret: &str) -> (Uuid, u64) {
        let result = self
            .store
            .apply_save_provider_action(
                Uuid::new_v4(),
                self.store.target_view().await.unwrap().management_revision,
                serde_json::json!({
                    "kind": "save-provider", "name": name,
                    "baseUrl": "https://upstream.example/v1", "model": model,
                    "credential": secret,
                }),
            )
            .await
            .unwrap();
        (
            Uuid::parse_str(&result.view.providers.last().unwrap().id).unwrap(),
            result.view.management_revision,
        )
    }

    fn service(&self, hooks: ActivationHooks) -> ActivationService {
        ActivationService::new(
            Arc::clone(&self.store),
            self.home.clone(),
            self.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        )
        .with_hooks(hooks)
    }

    async fn mutate_provider(&self, provider_id: Uuid, base_url: &str, model: &str) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        let base_url = base_url.to_owned();
        let model = model.to_owned();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE providers SET base_url = ?1, model = ?2 WHERE id = ?3",
                    (base_url, model, provider_id.to_string()),
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn count(&self, table: &'static str) -> u64 {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| {
                connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
            })
            .await
            .unwrap()
    }

    async fn set_route_port(&self, port: u16) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE target_route_state SET route_port = ?1 WHERE target = 'codex'",
                    [port],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn remove_provider_credential(&self, provider_id: Uuid) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "DELETE FROM provider_credentials WHERE provider_id = ?1",
                    [provider_id.to_string()],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
}

fn command(provider_id: Uuid, revision: u64, action_id: Uuid) -> ActivateProviderCommand {
    ActivateProviderCommand {
        action_id,
        expected_revision: revision,
        provider_id,
        mode: TakeoverMode::Takeover,
    }
}

#[tokio::test]
async fn activation_commits_one_complete_view_in_exact_observer_order_and_replays_without_effects()
{
    let fixture = Fixture::new().await;
    fs::create_dir_all(fixture.home.user_home().join(".codex")).unwrap();
    fs::write(
        fixture.home.user_home().join(".codex/config.toml"),
        "[features]\nfoo = true\n",
    )
    .unwrap();
    let (provider_id, revision) = fixture.save("First", "gpt-first", "provider-secret").await;
    let observer = Arc::new(Steps::default());
    let service = fixture.service(ActivationHooks::observed(observer.clone()));
    let action_id = Uuid::new_v4();

    let outcome = service
        .activate(command(provider_id, revision, action_id))
        .await
        .unwrap();

    assert_eq!(outcome.status, ActionStatus::Applied);
    assert_eq!(outcome.view.management_revision, revision + 1);
    assert_eq!(outcome.view.view_sequence, revision + 1);
    assert_eq!(
        outcome.view.current_provider_id.as_deref(),
        Some(provider_id.to_string().as_str())
    );
    assert!(outcome.view.serving_provider_id.is_none());
    assert_eq!(outcome.view.mode, "takeover");
    assert_eq!(outcome.view.takeover.state, "active");
    assert_eq!(outcome.view.managed_configuration.state, "applied");
    assert!(outcome.view.managed_configuration.restart_required);
    assert_eq!(
        observer.0.lock().unwrap().as_slice(),
        &[
            ActivationStep::Validate,
            ActivationStep::BindListener,
            ActivationStep::PersistRoutingCredential,
            ActivationStep::Snapshot,
            ActivationStep::RecoveryIntent,
            ActivationStep::AtomicConfigWrite,
            ActivationStep::ConfigVerify,
            ActivationStep::StateAndReceiptCommit,
            ActivationStep::PublishView,
        ]
    );
    let config = fs::read_to_string(fixture.home.user_home().join(".codex/config.toml")).unwrap();
    assert!(config.contains("http://127.0.0.1:"));
    assert!(config.contains("X-Muxvia-Routing-Credential"));
    assert!(config.contains("foo = true"));
    let credential = fixture.store.routing_credential().await.unwrap().unwrap();
    assert_eq!(secrecy::ExposeSecret::expose_secret(&credential).len(), 64);
    assert!(
        secrecy::ExposeSecret::expose_secret(&credential)
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );

    let steps_before = observer.0.lock().unwrap().len();
    let probe_before = fixture.probe.0.load(Ordering::SeqCst);
    let replay = service
        .activate(command(Uuid::new_v4(), 999, action_id))
        .await
        .unwrap();
    assert_eq!(replay.status, ActionStatus::Replayed);
    assert_eq!(observer.0.lock().unwrap().len(), steps_before);
    assert_eq!(fixture.probe.0.load(Ordering::SeqCst), probe_before);
}

#[tokio::test]
async fn snapshot_is_immutable_when_provider_declaration_changes_later() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt-first", "provider-secret").await;
    let service = fixture.service(ActivationHooks::default());
    service
        .activate(command(provider_id, revision, Uuid::new_v4()))
        .await
        .unwrap();
    fixture
        .mutate_provider(provider_id, "https://changed.invalid/v1", "changed")
        .await;

    let snapshot = fixture.store.activated_snapshot().await.unwrap().unwrap();
    assert_eq!(snapshot.base_url(), "https://upstream.example/v1");
    assert_eq!(snapshot.model(), "gpt-first");
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(snapshot.provider_credential()),
        "provider-secret"
    );
}

#[tokio::test]
async fn post_intent_failures_restore_exact_before_state_without_success_view() {
    for failpoint in [
        ActivationFailpoint::AtomicConfigWrite,
        ActivationFailpoint::ConfigVerify,
        ActivationFailpoint::FinalCommit,
    ] {
        let fixture = Fixture::new().await;
        fs::create_dir_all(fixture.home.user_home().join(".codex")).unwrap();
        let before = "# keep\nmodel = \"operator\"\n[features]\nfoo = true\n";
        fs::write(fixture.home.user_home().join(".codex/config.toml"), before).unwrap();
        let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
        let mut updates = fixture.store.subscribe_target_views();
        let service = fixture.service(ActivationHooks::failing(failpoint));

        let failure = service
            .activate(command(provider_id, revision, Uuid::new_v4()))
            .await
            .unwrap_err();

        assert!(matches!(
            failure.problem.code.as_str(),
            "configuration-write-failed" | "internal-failure" | "stale-revision"
        ));
        assert_eq!(
            fs::read_to_string(fixture.home.user_home().join(".codex/config.toml")).unwrap(),
            before
        );
        let view = fixture.store.target_view().await.unwrap();
        assert_eq!(view.management_revision, revision);
        assert!(view.current_provider_id.is_none());
        assert!(view.activated_snapshot.is_none());
        assert!(updates.try_recv().is_err());
    }
}

#[tokio::test]
async fn restore_verification_failure_marks_recovery_required_and_blocks_writes() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
    let service = fixture.service(ActivationHooks::failing(ActivationFailpoint::RestoreVerify));
    let failure = service
        .activate(command(provider_id, revision, Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(failure.problem.code, "recovery-required");
    assert_eq!(
        fixture.store.target_view().await.unwrap().recovery.state,
        "recovery-required"
    );

    let save = fixture.store.apply_save_provider_action(
        Uuid::new_v4(), revision, serde_json::json!({"kind":"save-provider","name":"x","baseUrl":"https://x.test/v1","model":"m","credential":"s"})
    ).await.unwrap_err();
    assert_eq!(save.problem.code, "recovery-required");
}

#[tokio::test]
async fn stale_revision_and_unbindable_persisted_port_fail_before_configuration_write() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
    let service = fixture.service(ActivationHooks::default());
    let stale = service
        .activate(command(provider_id, revision - 1, Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(stale.problem.code, "stale-revision");
    assert!(!fixture.home.user_home().join(".codex/config.toml").exists());
    assert_eq!(fixture.count("activation_recovery").await, 0);

    let held = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    fixture
        .set_route_port(held.local_addr().unwrap().port())
        .await;
    let failed = service
        .activate(command(provider_id, revision, Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(failed.problem.code, "configuration-write-failed");
    assert!(!fixture.home.user_home().join(".codex/config.toml").exists());
}

#[tokio::test]
async fn save_committing_during_config_io_wins_and_final_revision_check_rolls_activation_back() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
    let pause = Arc::new(ActivationPause::default());
    let service = Arc::new(fixture.service(ActivationHooks::pausing_final_commit(pause.clone())));
    let activation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .activate(command(provider_id, revision, Uuid::new_v4()))
                .await
        })
    };
    pause.wait_until_reached().await;
    let (_, saved_revision) = fixture.save("Concurrent", "gpt-2", "other-secret").await;
    pause.release();

    let failure = activation.await.unwrap().unwrap_err();
    assert_eq!(failure.problem.code, "stale-revision");
    let view = fixture.store.target_view().await.unwrap();
    assert_eq!(view.management_revision, saved_revision);
    assert!(view.current_provider_id.is_none());
    assert!(view.activated_snapshot.is_none());
    assert!(!fixture.home.user_home().join(".codex/config.toml").exists());
}

#[tokio::test]
async fn concurrent_same_action_serializes_and_publishes_once_then_malformed_replays_receipt_first()
{
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
    let pause = Arc::new(ActivationPause::default());
    let service = Arc::new(fixture.service(ActivationHooks::pausing_final_commit(pause.clone())));
    let action_id = Uuid::new_v4();
    let mut updates = fixture.store.subscribe_target_views();
    let first = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .activate(command(provider_id, revision, action_id))
                .await
        })
    };
    pause.wait_until_reached().await;
    let second = {
        let service = Arc::clone(&service);
        tokio::spawn(async move { service.activate(command(provider_id, 999, action_id)).await })
    };
    pause.release();
    assert_eq!(first.await.unwrap().unwrap().status, ActionStatus::Applied);
    assert_eq!(
        second.await.unwrap().unwrap().status,
        ActionStatus::Replayed
    );
    updates.recv().await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), updates.recv())
            .await
            .is_err()
    );

    let replay = service
        .apply_raw(action_id, 999, serde_json::json!({"not":"an action"}))
        .await
        .unwrap();
    assert_eq!(replay.status, ActionStatus::Replayed);
}

#[tokio::test]
async fn rolled_back_action_id_can_retry_and_commit() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
    let action_id = Uuid::new_v4();
    let failing = fixture.service(ActivationHooks::failing(ActivationFailpoint::FinalCommit));
    assert!(
        failing
            .activate(command(provider_id, revision, action_id))
            .await
            .is_err()
    );
    assert!(failing.model_endpoint().await.is_none());

    let retry = fixture.service(ActivationHooks::default());
    let outcome = retry
        .activate(command(provider_id, revision, action_id))
        .await
        .unwrap();
    assert_eq!(outcome.status, ActionStatus::Applied);
    assert_eq!(fixture.count("activation_recovery").await, 1);
    assert_eq!(fixture.count("activated_snapshots").await, 1);
}

#[tokio::test]
async fn second_provider_reuses_port_and_routing_credential_but_creates_a_new_snapshot() {
    let fixture = Fixture::new().await;
    let (first_id, revision) = fixture.save("First", "gpt-1", "secret-1").await;
    let service = fixture.service(ActivationHooks::default());
    let first = service
        .activate(command(first_id, revision, Uuid::new_v4()))
        .await
        .unwrap();
    let endpoint = first.view.takeover.endpoint.clone();
    let credential = fixture.store.routing_credential().await.unwrap().unwrap();

    let (second_id, second_revision) = fixture.save("Second", "gpt-2", "secret-2").await;
    let second = service
        .activate(command(second_id, second_revision, Uuid::new_v4()))
        .await
        .unwrap();
    assert_eq!(second.view.takeover.endpoint, endpoint);
    assert_eq!(
        secrecy::ExposeSecret::expose_secret(
            &fixture.store.routing_credential().await.unwrap().unwrap()
        ),
        secrecy::ExposeSecret::expose_secret(&credential),
    );
    assert_eq!(fixture.count("activated_snapshots").await, 2);
    assert_eq!(
        second.view.activated_snapshot.unwrap().provider_id,
        second_id
    );
}

#[tokio::test]
async fn unsupported_home_bad_probe_collision_symlink_and_missing_provider_are_pre_intent() {
    let cases = [
        "unsupported",
        "probe",
        "collision",
        "symlink",
        "missing",
        "incomplete",
    ];
    for case in cases {
        let fixture = Fixture::new().await;
        let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
        let service = match case {
            "unsupported" => fixture
                .service(ActivationHooks::default())
                .with_configuration_home_override(Some(fixture.home.user_home().join("elsewhere"))),
            "probe" => ActivationService::new(
                Arc::clone(&fixture.store),
                fixture.home.clone(),
                Arc::new(BadProbe),
                "/usr/bin/codex".into(),
                Arc::new(NoopUpstream),
            ),
            "collision" => {
                fs::create_dir_all(fixture.home.user_home().join(".codex")).unwrap();
                fs::write(
                    fixture.home.user_home().join(".codex/config.toml"),
                    "[model_providers.muxvia_codex]\nname = \"forged\"\n",
                )
                .unwrap();
                fixture.service(ActivationHooks::default())
            }
            "symlink" => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink;
                    fs::create_dir_all(fixture.home.user_home().join(".codex")).unwrap();
                    let outside = fixture.home.user_home().join("outside.toml");
                    fs::write(&outside, "model = \"outside\"\n").unwrap();
                    symlink(outside, fixture.home.user_home().join(".codex/config.toml")).unwrap();
                }
                fixture.service(ActivationHooks::default())
            }
            _ => fixture.service(ActivationHooks::default()),
        };
        if case == "incomplete" {
            fixture.remove_provider_credential(provider_id).await;
        }
        let selected = if case == "missing" {
            Uuid::new_v4()
        } else {
            provider_id
        };
        let failure = service
            .activate(command(selected, revision, Uuid::new_v4()))
            .await
            .unwrap_err();
        let expected = match case {
            "unsupported" => "unsupported-configuration-home",
            "probe" => "incompatible-target-cli",
            "collision" => "configuration-collision",
            "symlink" => "configuration-write-failed",
            "missing" | "incomplete" => "incomplete-provider",
            _ => unreachable!(),
        };
        assert_eq!(failure.problem.code, expected, "case {case}");
        assert_eq!(fixture.count("activation_recovery").await, 0, "case {case}");
        assert_eq!(fixture.count("activated_snapshots").await, 0, "case {case}");
        assert!(service.model_endpoint().await.is_none(), "case {case}");
    }
}

#[tokio::test]
async fn second_service_epoch_cannot_select_a_new_port_while_persisted_port_is_held() {
    let fixture = Fixture::new().await;
    let (first_id, revision) = fixture.save("First", "gpt-1", "secret-1").await;
    let first_service = fixture.service(ActivationHooks::default());
    first_service
        .activate(command(first_id, revision, Uuid::new_v4()))
        .await
        .unwrap();
    let before = fs::read_to_string(fixture.home.user_home().join(".codex/config.toml")).unwrap();
    let (second_id, second_revision) = fixture.save("Second", "gpt-2", "secret-2").await;
    let second_epoch = fixture.service(ActivationHooks::default());

    let failure = second_epoch
        .activate(command(second_id, second_revision, Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(failure.problem.code, "configuration-write-failed");
    assert_eq!(
        fs::read_to_string(fixture.home.user_home().join(".codex/config.toml")).unwrap(),
        before
    );
    assert_eq!(fixture.count("activated_snapshots").await, 1);
    assert_eq!(fixture.count("activation_recovery").await, 1);
}

#[tokio::test]
async fn uds_activate_returns_then_pushes_one_complete_secret_free_view() {
    let fixture = Fixture::new().await;
    let activation = Arc::new(fixture.service(ActivationHooks::default()));
    let handle = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&fixture.store),
        "test",
        activation,
    )
    .await
    .unwrap();
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
    let save_id = Uuid::new_v4();
    write_frame(
        &mut stream,
        &serde_json::json!({
            "type":"request", "requestId":"save",
            "operation":{"kind":"act","target":"codex","actionId":save_id,
              "expectedRevision":0,"action":{"kind":"save-provider","name":"First",
              "baseUrl":"https://upstream.example/v1","model":"gpt","credential":"provider-secret"}}
        }),
    )
    .await
    .unwrap();
    let saved = read_frame(&mut stream).await.unwrap();
    let save_push = read_frame(&mut stream).await.unwrap();
    assert_eq!(save_push["type"], "target-view");
    let provider_id = saved["result"]["outcome"]["view"]["providers"][0]["id"]
        .as_str()
        .unwrap();
    write_frame(&mut stream, &serde_json::json!({
        "type":"request", "requestId":"activate",
        "operation":{"kind":"act","target":"codex","actionId":Uuid::new_v4(),
          "expectedRevision":1,"action":{"kind":"activate-provider","providerId":provider_id,"mode":"takeover"}}
    })).await.unwrap();
    let response = read_frame(&mut stream).await.unwrap();
    let push = read_frame(&mut stream).await.unwrap();
    assert_eq!(response["type"], "response");
    assert_eq!(response["result"]["outcome"]["view"], push["view"]);
    assert_eq!(push["view"]["managedConfiguration"]["state"], "applied");
    assert!(!format!("{response}{push}").contains("provider-secret"));
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            read_frame(&mut stream)
        )
        .await
        .is_err()
    );
    handle.shutdown().await.unwrap();
}
