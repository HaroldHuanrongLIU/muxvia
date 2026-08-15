use std::{
    fmt, fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
};
use futures_util::stream;
use muxvia_routing::{
    claude::{ClaudeCapability, ClaudeProbe, ClaudeProblem},
    codex::{CodexCapability, CodexProbe, CodexProblem, CommandCodexProbe},
    control::{
        framing::{read_frame, write_frame},
        protocol::{
            ActionOutcome, ActionStatus, ActivationMode, ClaudeHostManagedState,
            ClaudePreflightContext, ClaudeSelectorState, ControlProblem, Target, TargetView,
        },
        server::{ControlServer, ControlServerError},
    },
    home::MuxviaHome,
    model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
    service::activate::{
        ActivateProviderCommand, ActivationFailpoint, ActivationHooks, ActivationObserver,
        ActivationPause, ActivationService, ActivationStep,
    },
    state::{ActionFailure, StateStore},
};
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast};
use tokio_rusqlite::rusqlite::{Connection as SqliteConnection, OptionalExtension};
use toml_edit::DocumentMut;
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

struct UnknownCompatibleProbe;

impl CodexProbe for UnknownCompatibleProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        Ok(CodexCapability::UnknownCompatible {
            version: "99.0.0".into(),
            warning: "Codex CLI 99.0.0 is untested; required capabilities were detected".into(),
        })
    }
}

struct BadProbe;

impl CodexProbe for BadProbe {
    fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
        CommandCodexProbe.probe(Path::new("relative-codex"))
    }
}

struct GoodClaudeProbe(AtomicUsize);

impl ClaudeProbe for GoodClaudeProbe {
    fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ClaudeCapability::Tested {
            version: "test".into(),
        })
    }
}

struct NoopUpstream;

#[async_trait]
impl UpstreamTransport for NoopUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        Err(UpstreamError)
    }
}

struct SuccessfulUpstream;

#[async_trait]
impl UpstreamTransport for SuccessfulUpstream {
    async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Box::pin(stream::once(async { Ok(Bytes::from_static(b"ok")) })),
        })
    }
}

#[derive(Default)]
struct PinningUpstream {
    calls: AtomicUsize,
    models: Mutex<Vec<String>>,
    first_started: Notify,
    release_first: Notify,
}

#[async_trait]
impl UpstreamTransport for PinningUpstream {
    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let body: serde_json::Value = serde_json::from_slice(
            request
                .body
                .as_bytes()
                .expect("Messages activation route rebuilds a buffered identity body"),
        )
        .unwrap();
        self.models
            .lock()
            .unwrap()
            .push(body["model"].as_str().unwrap().to_owned());
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            self.release_first.notified().await;
        }
        Ok(UpstreamResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Box::pin(stream::once(async { Ok(Bytes::from_static(b"ok")) })),
        })
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

struct ActivationCommitAbort {
    connection: SqliteConnection,
}

impl ActivationCommitAbort {
    fn install(home: &MuxviaHome) -> Self {
        let connection = SqliteConnection::open(home.database_path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER abort_claude_activation_commit
                 BEFORE UPDATE OF current_provider_id ON target_route_state
                 WHEN NEW.target = 'claude'
                 BEGIN SELECT RAISE(ABORT, 'test-claude-activation-commit-abort'); END;",
            )
            .unwrap();
        Self { connection }
    }
}

impl Drop for ActivationCommitAbort {
    fn drop(&mut self) {
        let _ = self
            .connection
            .execute_batch("DROP TRIGGER IF EXISTS abort_claude_activation_commit;");
    }
}

#[derive(PartialEq, Eq)]
struct MutationFingerprint {
    sqlite: Vec<u8>,
    config: Option<Vec<u8>>,
    auth: Option<Vec<u8>>,
}

impl fmt::Debug for MutationFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MutationFingerprint(<redacted>)")
    }
}

fn redacted_match<T: PartialEq>(actual: &T, expected: &T) -> Result<(), &'static str> {
    if actual == expected {
        Ok(())
    } else {
        Err("redacted values differed")
    }
}

const ACTIVATION_SECRET_SENTINELS: &[&str] = &[
    "secret",
    "api-provider-secret",
    "bearer-provider-secret",
    "claude-provider-secret",
    "claude-secret",
    "codex-provider-secret",
    "codex-secret",
    "controlled-secret-sentinel",
    "direct-api-secret",
    "direct-secret",
    "edited-first-secret",
    "edited-provider-secret",
    "edited-second-secret",
    "first-provider-secret",
    "first-secret",
    "legacy-upstream-secret",
    "other-secret",
    "prior-token",
    "operator-token",
    "prior-key",
    "operator-key",
    "legacy-api-key",
    "provider-secret",
    "provider-secret-must-not-appear-in-warning",
    "provider-secret-must-not-escape",
    "routing-secret",
    "second-provider-secret",
    "second-secret",
    "secret-1",
    "secret-2",
    "secret-a",
    "secret-b",
    "secret-c",
    "takeover-secret",
    "upgraded-upstream-secret",
    "upstream-secret",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
];

fn encoded_surface_is_secret_free(encoded: &str, additional: &[&str]) -> Result<(), &'static str> {
    for sentinel in ACTIVATION_SECRET_SENTINELS.iter().chain(additional) {
        let debug_bytes = format!("{:?}", sentinel.as_bytes());
        let numeric_bytes = serde_json::to_string(sentinel.as_bytes())
            .map_err(|_| "secret signature did not encode")?;
        if encoded.contains(sentinel)
            || encoded.contains(&debug_bytes)
            || encoded.contains(&numeric_bytes)
        {
            return Err("surface contained a credential or settings value");
        }
    }
    Ok(())
}

fn public_surface_is_secret_free<T: fmt::Debug>(
    surface: &T,
    forbidden: &[&str],
) -> Result<(), &'static str> {
    let encoded = format!("{surface:?}");
    encoded_surface_is_secret_free(&encoded, forbidden)
}

fn action_outcome_is_secret_free(outcome: &ActionOutcome) -> Result<(), &'static str> {
    public_surface_is_secret_free(outcome, &[])?;
    let encoded = serde_json::to_string(outcome).map_err(|_| "action outcome did not encode")?;
    encoded_surface_is_secret_free(&encoded, &[])
}

fn action_failure_is_secret_free(failure: &ActionFailure) -> Result<(), &'static str> {
    public_surface_is_secret_free(failure, &[])?;
    let encoded = serde_json::to_string(&serde_json::json!({
        "problem": &failure.problem,
        "authoritativeView": &failure.authoritative_view,
    }))
    .map_err(|_| "action failure did not encode")?;
    encoded_surface_is_secret_free(&encoded, &[])
}

fn take_applied_redacted(
    result: Result<ActionOutcome, ActionFailure>,
) -> Result<ActionOutcome, &'static str> {
    match result {
        Ok(outcome) => {
            action_outcome_is_secret_free(&outcome)?;
            Ok(outcome)
        }
        Err(failure) => {
            action_failure_is_secret_free(&failure)?;
            Err("expected an applied activation outcome")
        }
    }
}

fn take_failure_redacted(
    result: Result<ActionOutcome, ActionFailure>,
) -> Result<ActionFailure, &'static str> {
    match result {
        Ok(outcome) => {
            action_outcome_is_secret_free(&outcome)?;
            Err("expected an activation failure")
        }
        Err(failure) => {
            action_failure_is_secret_free(&failure)?;
            Ok(failure)
        }
    }
}

trait RedactedActivationResult {
    fn expect_applied_redacted(self) -> ActionOutcome;
    fn expect_failure_redacted(self) -> ActionFailure;
}

impl RedactedActivationResult for Result<ActionOutcome, ActionFailure> {
    fn expect_applied_redacted(self) -> ActionOutcome {
        match take_applied_redacted(self) {
            Ok(outcome) => outcome,
            Err(diagnostic) => panic!("{diagnostic}"),
        }
    }

    fn expect_failure_redacted(self) -> ActionFailure {
        match take_failure_redacted(self) {
            Ok(failure) => failure,
            Err(diagnostic) => panic!("{diagnostic}"),
        }
    }
}

fn validate_push_redacted(
    actual: TargetView,
    expected: &TargetView,
) -> Result<TargetView, &'static str> {
    public_surface_is_secret_free(&actual, &[])?;
    public_surface_is_secret_free(expected, &[])?;
    let actual_json = serde_json::to_string(&actual).map_err(|_| "push did not encode")?;
    let expected_json = serde_json::to_string(expected).map_err(|_| "push did not encode")?;
    encoded_surface_is_secret_free(&actual_json, &[])?;
    encoded_surface_is_secret_free(&expected_json, &[])?;
    if actual != *expected {
        return Err("activation push did not match its response view");
    }
    Ok(actual)
}

async fn expect_push_redacted(
    updates: &mut broadcast::Receiver<TargetView>,
    expected: &TargetView,
) -> TargetView {
    let actual = match updates.recv().await {
        Ok(view) => view,
        Err(_) => panic!("activation push was unavailable"),
    };
    match validate_push_redacted(actual, expected) {
        Ok(view) => view,
        Err(diagnostic) => panic!("{diagnostic}"),
    }
}

#[tokio::test]
async fn redacted_test_diagnostics_do_not_echo_secret_values_on_mismatch() {
    let secret = "controlled-secret-sentinel";
    let actual = serde_json::json!({"credential": secret});
    let expected = serde_json::json!({"credential": "mutated"});
    assert!(format!("{actual:?}").contains(secret));
    let diagnostic = redacted_match(&actual, &expected).unwrap_err();
    assert_eq!(diagnostic, "redacted values differed");
    assert!(!diagnostic.contains(secret));

    let fixture = Fixture::new().await;
    let view = fixture.store.target_view().await.unwrap();
    let unexpected_failure = ActionFailure {
        problem: ControlProblem {
            code: "controlled-opposite-branch".to_owned(),
            message: secret.to_owned(),
            source: None,
            selector: None,
        },
        authoritative_view: view.clone(),
    };
    assert!(format!("{unexpected_failure:?}").contains(secret));
    let diagnostic = match take_applied_redacted(Err(unexpected_failure)) {
        Ok(_) => panic!("redacted opposite-branch helper accepted a failure"),
        Err(diagnostic) => diagnostic,
    };
    assert_eq!(
        diagnostic,
        "surface contained a credential or settings value"
    );
    assert!(!diagnostic.contains(secret));

    let mut unexpected_outcome_view = view.clone();
    unexpected_outcome_view.problems.push(ControlProblem {
        code: "controlled-opposite-success-branch".to_owned(),
        message: secret.to_owned(),
        source: None,
        selector: None,
    });
    let unexpected_outcome = ActionOutcome {
        status: ActionStatus::Applied,
        view: unexpected_outcome_view,
    };
    assert!(format!("{unexpected_outcome:?}").contains(secret));
    let diagnostic = match take_failure_redacted(Ok(unexpected_outcome)) {
        Ok(_) => panic!("redacted opposite-branch helper accepted an outcome"),
        Err(diagnostic) => diagnostic,
    };
    assert_eq!(
        diagnostic,
        "surface contained a credential or settings value"
    );
    assert!(!diagnostic.contains(secret));

    for encoded_secret in [
        format!("{:?}", secret.as_bytes()),
        serde_json::to_string(secret.as_bytes()).unwrap(),
    ] {
        let numeric_failure = ActionFailure {
            problem: ControlProblem {
                code: "controlled-numeric-opposite-branch".to_owned(),
                message: encoded_secret,
                source: None,
                selector: None,
            },
            authoritative_view: view.clone(),
        };
        let diagnostic = match take_applied_redacted(Err(numeric_failure)) {
            Ok(_) => panic!("redacted opposite-branch helper accepted encoded secret bytes"),
            Err(diagnostic) => diagnostic,
        };
        assert_eq!(
            diagnostic,
            "surface contained a credential or settings value"
        );
        assert!(!diagnostic.contains(secret));
    }

    let mut secret_bearing_push = view.clone();
    secret_bearing_push.problems.push(ControlProblem {
        code: "controlled-push-mismatch".to_owned(),
        message: secret.to_owned(),
        source: None,
        selector: None,
    });
    assert!(format!("{secret_bearing_push:?}").contains(secret));
    let diagnostic = match validate_push_redacted(secret_bearing_push, &view) {
        Ok(_) => panic!("redacted push helper accepted a secret-bearing mismatch"),
        Err(diagnostic) => diagnostic,
    };
    assert_eq!(
        diagnostic,
        "surface contained a credential or settings value"
    );
    assert!(!diagnostic.contains(secret));
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
            .apply_provider_action(
                Uuid::new_v4(),
                self.store.target_view().await.unwrap().management_revision,
                serde_json::json!({
                    "kind": "create-provider", "name": name,
                    "baseUrl": "https://upstream.example/v1", "model": model,
                    "credential": { "kind": "replace", "value": secret },
                    "presetKey": null,
                }),
            )
            .await
            .expect_applied_redacted();
        assert!(
            public_surface_is_secret_free(&result, &[secret]).is_ok(),
            "provider save outcome exposed a credential"
        );
        (
            result.view.providers.last().unwrap().id,
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

    fn dual_service(
        &self,
        hooks: ActivationHooks,
        claude_probe: Arc<GoodClaudeProbe>,
    ) -> ActivationService {
        self.service(hooks)
            .with_claude_runtime(claude_probe, "/usr/bin/claude".into())
    }

    async fn save_claude(&self, name: &str, model: &str, secret: &str) -> (Uuid, u64) {
        self.save_claude_with_auth(name, model, secret, "anthropic-api-key")
            .await
    }

    async fn save_claude_with_auth(
        &self,
        name: &str,
        model: &str,
        secret: &str,
        authentication: &str,
    ) -> (Uuid, u64) {
        let result = self
            .store
            .apply_provider_action_for(
                Target::Claude,
                Uuid::new_v4(),
                self.store
                    .target_view_for(Target::Claude)
                    .await
                    .unwrap()
                    .management_revision,
                serde_json::json!({
                    "kind": "create-provider", "name": name,
                    "baseUrl": "https://api.anthropic.test", "model": model,
                    "credential": { "kind": "replace", "value": secret },
                    "authentication": authentication,
                    "presetKey": null,
                }),
            )
            .await
            .expect_applied_redacted();
        assert!(
            public_surface_is_secret_free(&result, &[secret]).is_ok(),
            "Claude provider save outcome exposed a credential"
        );
        (
            result.view.providers.last().unwrap().id,
            result.view.management_revision,
        )
    }

    async fn managed_config_version_for(&self, target: Target) -> u32 {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| {
                connection.query_row(
                    "SELECT managed_config_version FROM target_route_state WHERE target = ?1",
                    [target.as_str()],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap()
    }

    async fn downgrade_latest_claude_takeover_to_v1(&self, legacy_api_key: &str) {
        let settings_path = self.home.user_home().join(".claude/settings.json");
        let mut settings: serde_json::Value =
            serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
        settings["env"]["ANTHROPIC_API_KEY"] = serde_json::json!(legacy_api_key);
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).unwrap(),
        )
        .unwrap();

        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        let legacy_api_key = legacy_api_key.to_owned();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                let (id, payload_json): (String, String) = connection.query_row(
                    "SELECT a.id, a.payload_json FROM target_route_state r
                     JOIN activation_recovery a
                       ON a.id = r.recovery_intent_id AND a.target = r.target
                     WHERE r.target = 'claude' AND a.state = 'committed'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                let mut payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
                let object = payload.as_object_mut().unwrap();
                object.remove("ownership_version");
                let before = object["before"].as_object_mut().unwrap();
                before.remove("ownership_version");
                before.remove("unrelated_fingerprint");
                before.insert(
                    "unrelated".to_owned(),
                    serde_json::json!({
                        "env": {"ANTHROPIC_API_KEY": legacy_api_key},
                        "permissions": {"allow": ["Read"]}
                    }),
                );
                before["owned"].as_object_mut().unwrap().remove("api_key");
                let desired = object["desired"].as_object_mut().unwrap();
                desired.remove("ownership_version");
                desired.remove("mode");
                desired["owned"].as_object_mut().unwrap().remove("api_key");
                connection.execute(
                    "UPDATE activation_recovery SET payload_json = ?1 WHERE id = ?2",
                    (serde_json::to_string(&payload).unwrap(), id),
                )?;
                connection.execute(
                    "UPDATE target_route_state SET managed_config_version = 1
                     WHERE target = 'claude'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
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

    async fn mutate_provider_credential(&self, provider_id: Uuid, credential: &str) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        let credential = credential.to_owned();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE credentials SET bearer_token = ?1
                     WHERE id = (SELECT credential_id FROM providers WHERE id = ?2)",
                    (credential, provider_id.to_string()),
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
                    "UPDATE providers SET credential_id = NULL WHERE id = ?1",
                    [provider_id.to_string()],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn set_provider_routing_requirement(&self, provider_id: Uuid, requirement: &str) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        let requirement = requirement.to_owned();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE providers SET routing_requirement = ?1 WHERE id = ?2",
                    (requirement, provider_id.to_string()),
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn set_recovery_required(&self) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        database
            .call(|connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE target_route_state SET recovery_state = 'recovery-required' WHERE target = 'codex'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn overwrite_route_state(
        &self,
        takeover_state: &str,
        route_port: Option<u16>,
        routing_credential: Option<&str>,
        snapshot_id: Option<Uuid>,
    ) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        let takeover_state = takeover_state.to_owned();
        let routing_credential = routing_credential.map(str::to_owned);
        let snapshot_id = snapshot_id.map(|id| id.to_string());
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                connection.execute(
                    "UPDATE target_route_state SET takeover_state = ?1, route_port = ?2,
                            routing_credential = ?3, activated_snapshot_id = ?4
                     WHERE target = 'codex'",
                    (takeover_state, route_port, routing_credential, snapshot_id),
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn corrupt_committed_target(&self, target: Target, missing: &'static str) {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                match missing {
                    "snapshot" => {
                        connection.execute(
                            "DELETE FROM activated_snapshots WHERE target = ?1",
                            [target.as_str()],
                        )?;
                    }
                    "routing-credential" => {
                        connection.execute(
                            "UPDATE target_route_state SET routing_credential = NULL WHERE target = ?1",
                            [target.as_str()],
                        )?;
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn mutation_fingerprint(&self) -> MutationFingerprint {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        let sqlite = database
            .call(|connection| -> tokio_rusqlite::rusqlite::Result<Vec<u8>> {
                let queries = [
                    "SELECT id,target,position,provider_revision,name,base_url,model,protocol,credential_id,provenance_kind,provenance_key,generated_owner_id,routing_requirement FROM providers ORDER BY id",
                    "SELECT id,target,bearer_token FROM credentials ORDER BY id",
                    "SELECT target,management_revision,view_sequence,current_provider_id,serving_provider_id,takeover_state,route_port,routing_credential,activated_snapshot_id,managed_config_path,managed_config_version,recovery_intent_id,recovery_state FROM target_route_state ORDER BY target",
                    "SELECT id,target,provider_id,base_url,model,provider_bearer_token,epoch FROM activated_snapshots ORDER BY id",
                    "SELECT action_id,action_kind,committed_revision,outcome_json FROM action_receipts ORDER BY action_id",
                    "SELECT id,target,action_id,config_path,file_identity_json,payload_json,state,created_revision FROM activation_recovery ORDER BY id",
                    "SELECT target,code,message FROM target_problems ORDER BY target,code",
                ];
                let mut fingerprint = Vec::new();
                for query in queries {
                    fingerprint.extend_from_slice(query.as_bytes());
                    let mut statement = connection.prepare(query)?;
                    let column_count = statement.column_count();
                    let mut rows = statement.query([])?;
                    while let Some(row) = rows.next()? {
                        for column in 0..column_count {
                            use tokio_rusqlite::rusqlite::types::ValueRef;
                            match row.get_ref(column)? {
                                ValueRef::Null => fingerprint.extend_from_slice(b"null"),
                                ValueRef::Integer(value) => fingerprint
                                    .extend_from_slice(value.to_string().as_bytes()),
                                ValueRef::Real(value) => fingerprint
                                    .extend_from_slice(value.to_bits().to_string().as_bytes()),
                                ValueRef::Text(value) | ValueRef::Blob(value) => {
                                    fingerprint.extend_from_slice(value)
                                }
                            }
                            fingerprint.push(0);
                        }
                        fingerprint.push(b'\n');
                    }
                }
                Ok(fingerprint)
            })
            .await
            .unwrap();
        MutationFingerprint {
            sqlite,
            config: fs::read(self.home.user_home().join(".codex/config.toml")).ok(),
            auth: fs::read(self.home.user_home().join(".codex/auth.json")).ok(),
        }
    }

    async fn recovery_state_for_action(&self, action_id: Uuid) -> Option<String> {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT state FROM activation_recovery WHERE action_id = ?1",
                        [action_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
            })
            .await
            .unwrap()
    }

    async fn stored_receipt_for(&self, target: Target, action_id: Uuid) -> ActionOutcome {
        let database = tokio_rusqlite::Connection::open(self.home.database_path())
            .await
            .unwrap();
        let json = database
            .call(move |connection| {
                connection.query_row(
                    "SELECT outcome_json FROM action_receipts
                     WHERE target = ?1 AND action_id = ?2",
                    (target.as_str(), action_id.to_string()),
                    |row| row.get::<_, String>(0),
                )
            })
            .await
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }
}

fn command(provider_id: Uuid, revision: u64, action_id: Uuid) -> ActivateProviderCommand {
    ActivateProviderCommand {
        action_id,
        expected_revision: revision,
        provider_id,
        mode: ActivationMode::Takeover,
    }
}

fn direct_action(provider_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "kind": "activate-provider",
        "providerId": provider_id,
        "mode": "direct",
    })
}

fn takeover_action(provider_id: Uuid) -> serde_json::Value {
    serde_json::json!({
        "kind": "activate-provider",
        "providerId": provider_id,
        "mode": "takeover",
    })
}

fn claude_context(home: &MuxviaHome) -> ClaudePreflightContext {
    ClaudePreflightContext {
        claude_config_dir: None,
        selector_state: ClaudeSelectorState::Unset,
        blocking_selector: None,
        host_managed_state: ClaudeHostManagedState::Unmanaged,
        cwd: home.user_home().to_string_lossy().into_owned(),
    }
}

#[tokio::test]
async fn claude_takeover_commits_one_target_native_snapshot_and_leaves_codex_unchanged() {
    let fixture = Fixture::new().await;
    let codex_before = fixture.store.target_view().await.unwrap();
    let codex_file_before = fs::read(fixture.home.user_home().join(".codex/config.toml")).ok();
    let (provider_id, revision) = fixture
        .save_claude("Claude", "claude-sonnet-test", "provider-secret")
        .await;
    let probe = Arc::new(GoodClaudeProbe(AtomicUsize::new(0)));
    let service = fixture.dual_service(ActivationHooks::default(), probe.clone());
    let mut updates = fixture.store.subscribe_target_views();

    let outcome = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            takeover_action(provider_id),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();

    assert_eq!(outcome.status, ActionStatus::Applied);
    assert_eq!(outcome.view.target, Target::Claude);
    assert_eq!(outcome.view.mode, "takeover");
    assert_eq!(
        outcome.view.current_provider_id,
        Some(provider_id.to_string())
    );
    assert_eq!(
        outcome.view.activated_snapshot.as_ref().unwrap().model,
        "claude-sonnet-test"
    );
    assert_eq!(
        outcome
            .view
            .activated_snapshot
            .as_ref()
            .unwrap()
            .protocol
            .to_string(),
        "anthropic-messages"
    );
    assert_eq!(probe.0.load(Ordering::SeqCst), 1);
    assert!(service.model_endpoint_for(Target::Claude).await.is_some());
    assert!(service.model_endpoint().await.is_none());
    expect_push_redacted(&mut updates, &outcome.view).await;
    assert!(updates.try_recv().is_err());
    assert_eq!(fixture.store.target_view().await.unwrap(), codex_before);
    assert_eq!(
        fs::read(fixture.home.user_home().join(".codex/config.toml")).ok(),
        codex_file_before
    );
}

#[tokio::test]
async fn claude_direct_commits_both_authentication_profiles_without_model_runtime() {
    for (authentication, active_key, inactive_key) in [
        (
            "anthropic-api-key",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
        ),
        (
            "anthropic-bearer",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ),
    ] {
        let fixture = Fixture::new().await;
        let settings = fixture.home.user_home().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"prior-token","ANTHROPIC_API_KEY":"prior-key"},"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();
        let (provider_id, revision) = fixture
            .save_claude_with_auth(
                "Claude Direct",
                "claude-sonnet-test",
                "provider-secret",
                authentication,
            )
            .await;
        let probe = Arc::new(GoodClaudeProbe(AtomicUsize::new(0)));
        let steps = Arc::new(Steps::default());
        let service = fixture.dual_service(ActivationHooks::observed(steps.clone()), probe.clone());
        let codex_endpoint_before = service.model_endpoint().await;
        let claude_endpoint_before = service.model_endpoint_for(Target::Claude).await;
        let action_id = Uuid::new_v4();
        let mut updates = fixture.store.subscribe_target_views();

        let outcome = service
            .apply_raw_for_with_context(
                Target::Claude,
                action_id,
                revision,
                direct_action(provider_id),
                Some(&claude_context(&fixture.home)),
            )
            .await
            .expect_applied_redacted();

        assert!(
            public_surface_is_secret_free(
                &outcome,
                &["provider-secret", "prior-token", "prior-key"]
            )
            .is_ok(),
            "Claude Direct outcome exposed a credential"
        );
        assert_eq!(outcome.status, ActionStatus::Applied);
        assert_eq!(outcome.view.target, Target::Claude);
        assert_eq!(outcome.view.mode, "direct");
        assert_eq!(outcome.view.management_revision, revision + 1);
        assert_eq!(outcome.view.view_sequence, revision + 1);
        assert_eq!(
            outcome.view.current_provider_id,
            Some(provider_id.to_string())
        );
        assert!(outcome.view.serving_provider_id.is_none());
        let snapshot = outcome.view.activated_snapshot.as_ref().unwrap();
        assert_eq!(snapshot.provider_id, provider_id);
        assert_eq!(snapshot.model, "claude-sonnet-test");
        assert_eq!(snapshot.authentication.to_string(), authentication);
        assert_eq!(outcome.view.managed_configuration.state, "applied");
        assert_eq!(
            outcome.view.managed_configuration.path.as_deref(),
            settings.to_str()
        );
        assert!(outcome.view.managed_configuration.restart_required);
        assert_eq!(outcome.view.takeover.state, "inactive");
        assert!(outcome.view.takeover.endpoint.is_none());
        assert_eq!(fixture.managed_config_version_for(Target::Claude).await, 2);
        assert_eq!(fixture.count("activated_snapshots").await, 1);
        assert_eq!(fixture.count("activation_recovery").await, 1);
        assert_eq!(fixture.count("action_receipts").await, 2);
        assert_eq!(
            fixture
                .recovery_state_for_action(action_id)
                .await
                .as_deref(),
            Some("committed")
        );
        assert!(
            fixture
                .store
                .routing_credential_for(Target::Claude)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(service.model_endpoint().await, codex_endpoint_before);
        assert_eq!(
            service.model_endpoint_for(Target::Claude).await,
            claude_endpoint_before
        );
        let document: serde_json::Value =
            serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        let env = document["env"].as_object().unwrap();
        assert_eq!(
            document["env"]["ANTHROPIC_BASE_URL"],
            "https://api.anthropic.test/"
        );
        assert_eq!(document["env"]["ANTHROPIC_MODEL"], "claude-sonnet-test");
        assert!(
            redacted_match(
                &document["env"][active_key],
                &serde_json::json!("provider-secret")
            )
            .is_ok(),
            "Claude Direct credential did not match"
        );
        assert!(!env.contains_key(inactive_key));
        assert_eq!(document["permissions"]["allow"][0], "Read");
        expect_push_redacted(&mut updates, &outcome.view).await;
        assert!(updates.try_recv().is_err());
        let reached = steps.0.lock().unwrap().clone();
        assert!(!reached.contains(&ActivationStep::BindListener));
        assert!(!reached.contains(&ActivationStep::PersistRoutingCredential));
        assert!(!reached.contains(&ActivationStep::RuntimeHandoff));

        let replay = service
            .apply_raw_for_with_context(
                Target::Claude,
                action_id,
                0,
                serde_json::json!({"malformed": true}),
                None,
            )
            .await
            .expect_applied_redacted();
        assert!(
            public_surface_is_secret_free(
                &replay,
                &["provider-secret", "prior-token", "prior-key"]
            )
            .is_ok(),
            "Claude Direct replay exposed a credential"
        );
        assert_eq!(replay.status, ActionStatus::Replayed);
        assert_eq!(replay.view, outcome.view);
        assert_eq!(probe.0.load(Ordering::SeqCst), 1);
        assert!(updates.try_recv().is_err());
    }
}

#[tokio::test]
async fn claude_direct_switches_authentication_profiles_against_the_immutable_snapshot() {
    let fixture = Fixture::new().await;
    let settings = fixture.home.user_home().join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"operator-token","ANTHROPIC_API_KEY":"operator-key"},"permissions":{"deny":["Bash"]}}"#,
    )
    .unwrap();
    let (api_provider, _) = fixture
        .save_claude_with_auth(
            "API Direct",
            "claude-api",
            "api-provider-secret",
            "anthropic-api-key",
        )
        .await;
    let (bearer_provider, revision) = fixture
        .save_claude_with_auth(
            "Bearer Direct",
            "claude-bearer",
            "bearer-provider-secret",
            "anthropic-bearer",
        )
        .await;
    let service = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );

    let first = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            direct_action(api_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&first, &["api-provider-secret", "bearer-provider-secret"])
            .is_ok(),
        "Claude Direct outcome exposed a credential"
    );
    fixture
        .mutate_provider(
            api_provider,
            "https://edited-after-activation.invalid",
            "edited-model",
        )
        .await;
    fixture
        .mutate_provider_credential(api_provider, "edited-provider-secret")
        .await;
    let second = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            first.view.management_revision,
            direct_action(bearer_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(
            &second,
            &[
                "api-provider-secret",
                "bearer-provider-secret",
                "edited-provider-secret"
            ]
        )
        .is_ok(),
        "Claude Direct transition exposed a credential"
    );
    let after_bearer: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(after_bearer["env"]["ANTHROPIC_MODEL"], "claude-bearer");
    assert!(
        redacted_match(
            &after_bearer["env"]["ANTHROPIC_AUTH_TOKEN"],
            &serde_json::json!("bearer-provider-secret")
        )
        .is_ok(),
        "Claude bearer credential did not match"
    );
    assert!(
        !after_bearer["env"]
            .as_object()
            .unwrap()
            .contains_key("ANTHROPIC_API_KEY")
    );
    assert_eq!(after_bearer["permissions"]["deny"][0], "Bash");

    let third = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            second.view.management_revision,
            direct_action(api_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(
            &third,
            &[
                "api-provider-secret",
                "bearer-provider-secret",
                "edited-provider-secret"
            ]
        )
        .is_ok(),
        "Claude Direct transition exposed a credential"
    );
    let after_api: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(after_api["env"]["ANTHROPIC_MODEL"], "edited-model");
    assert!(
        redacted_match(
            &after_api["env"]["ANTHROPIC_API_KEY"],
            &serde_json::json!("edited-provider-secret")
        )
        .is_ok(),
        "Claude API credential did not match"
    );
    assert!(
        !after_api["env"]
            .as_object()
            .unwrap()
            .contains_key("ANTHROPIC_AUTH_TOKEN")
    );
    assert_eq!(third.view.mode, "direct");
    assert_eq!(
        third.view.current_provider_id,
        Some(api_provider.to_string())
    );
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
}

#[tokio::test]
async fn claude_corrupt_committed_expectation_fails_closed_as_recovery_required() {
    let fixture = Fixture::new().await;
    let (first_provider, revision) = fixture
        .save_claude_with_auth("First", "claude-first", "first-secret", "anthropic-api-key")
        .await;
    let service = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let first = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            direct_action(first_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let (second_provider, second_revision) = fixture
        .save_claude_with_auth(
            "Second",
            "claude-second",
            "second-secret",
            "anthropic-bearer",
        )
        .await;
    assert!(second_revision > first.view.management_revision);
    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(|connection| -> tokio_rusqlite::rusqlite::Result<()> {
            connection.execute(
                "UPDATE activation_recovery SET payload_json = '{\"corrupt\":true}'
                 WHERE id = (SELECT recovery_intent_id FROM target_route_state
                             WHERE target = 'claude')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let settings = fixture.home.user_home().join(".claude/settings.json");
    let file_before = fs::read(&settings).unwrap();

    let failure = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            second_revision,
            direct_action(second_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();

    assert!(
        public_surface_is_secret_free(&failure, &["first-secret", "second-secret"]).is_ok(),
        "corrupt-expectation failure exposed a credential"
    );
    assert_eq!(failure.problem.code, "recovery-required");
    assert_eq!(
        failure.authoritative_view.recovery.state,
        "recovery-required"
    );
    assert_eq!(
        failure.authoritative_view.managed_configuration.state,
        "recovery-required"
    );
    assert!(
        redacted_match(&fs::read(&settings).unwrap(), &file_before).is_ok(),
        "corrupt-expectation failure changed Claude settings"
    );
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
}

#[tokio::test]
async fn claude_retried_rolled_back_action_rebinds_the_current_recovery_expectation() {
    let fixture = Fixture::new().await;
    let (provider_a, _) = fixture
        .save_claude_with_auth("A", "claude-a", "secret-a", "anthropic-api-key")
        .await;
    let (provider_b, _) = fixture
        .save_claude_with_auth("B", "claude-b", "secret-b", "anthropic-bearer")
        .await;
    let (provider_c, revision) = fixture
        .save_claude_with_auth("C", "claude-c", "secret-c", "anthropic-api-key")
        .await;
    let action_a = Uuid::new_v4();

    let failed = fixture.dual_service(
        ActivationHooks::failing(ActivationFailpoint::FinalCommit),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let first_a = failed
        .apply_raw_for_with_context(
            Target::Claude,
            action_a,
            revision,
            direct_action(provider_a),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();
    assert!(
        public_surface_is_secret_free(&first_a, &["secret-a", "secret-b", "secret-c"]).is_ok(),
        "failed Claude activation exposed a credential"
    );
    assert_eq!(first_a.problem.code, "internal-failure");
    assert_eq!(
        fixture.recovery_state_for_action(action_a).await.as_deref(),
        Some("rolled-back")
    );

    let service = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let b = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            direct_action(provider_b),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&b, &["secret-a", "secret-b", "secret-c"]).is_ok(),
        "Claude activation outcome exposed a credential"
    );
    let retried_a = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_a,
            b.view.management_revision,
            direct_action(provider_a),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&retried_a, &["secret-a", "secret-b", "secret-c"]).is_ok(),
        "retried Claude outcome exposed a credential"
    );
    assert_eq!(retried_a.status, ActionStatus::Applied);
    assert_eq!(
        retried_a.view.current_provider_id,
        Some(provider_a.to_string())
    );

    let c = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            retried_a.view.management_revision,
            direct_action(provider_c),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&c, &["secret-a", "secret-b", "secret-c"]).is_ok(),
        "Claude activation outcome exposed a credential"
    );
    assert_eq!(c.status, ActionStatus::Applied);
    assert_eq!(c.view.current_provider_id, Some(provider_c.to_string()));
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
}

#[tokio::test]
async fn claude_legal_committed_payload_mismatch_persists_recovery_required_and_blocks_writes() {
    let fixture = Fixture::new().await;
    let (provider_a, revision) = fixture
        .save_claude_with_auth("A", "claude-a", "secret-a", "anthropic-api-key")
        .await;
    let service = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let action_a = Uuid::new_v4();
    let a = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_a,
            revision,
            direct_action(provider_a),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let (provider_b, next_revision) = fixture
        .save_claude_with_auth("B", "claude-b", "secret-b", "anthropic-bearer")
        .await;
    assert!(next_revision > a.view.management_revision);

    let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
        .await
        .unwrap();
    database
        .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
            let payload_json: String = connection.query_row(
                "SELECT payload_json FROM activation_recovery
                 WHERE target = 'claude' AND action_id = ?1 AND state = 'committed'",
                [action_a.to_string()],
                |row| row.get(0),
            )?;
            let mut payload: serde_json::Value = serde_json::from_str(&payload_json)
                .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
            payload["desired"]["owned"]["model"] = serde_json::json!("mismatched-model");
            payload["before"]["unrelated_fingerprint"] =
                serde_json::json!("mismatched-fingerprint");
            connection.execute(
                "UPDATE activation_recovery SET payload_json = ?1
                 WHERE target = 'claude' AND action_id = ?2 AND state = 'committed'",
                tokio_rusqlite::rusqlite::params![payload.to_string(), action_a.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut updates = fixture.store.subscribe_target_views();

    let failure = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            next_revision,
            direct_action(provider_b),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();
    assert!(
        public_surface_is_secret_free(&failure, &["secret-a", "secret-b"]).is_ok(),
        "recovery-required failure exposed a credential"
    );
    assert_eq!(failure.problem.code, "recovery-required");
    assert_eq!(
        failure.authoritative_view.recovery.state,
        "recovery-required"
    );
    assert_eq!(
        fixture.recovery_state_for_action(action_a).await.as_deref(),
        Some("recovery-required")
    );
    assert_eq!(
        fixture
            .store
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .recovery
            .state,
        "recovery-required"
    );
    let blocked = fixture
        .store
        .apply_provider_action_for(
            Target::Claude,
            Uuid::new_v4(),
            next_revision,
            serde_json::json!({"kind": "delete-provider", "providerId": provider_b}),
        )
        .await
        .expect_failure_redacted();
    assert!(
        public_surface_is_secret_free(&blocked, &["secret-a", "secret-b"]).is_ok(),
        "blocked managed write exposed a credential"
    );
    assert_eq!(blocked.problem.code, "recovery-required");
    assert_eq!(
        fixture.store.target_view().await.unwrap().recovery.state,
        "clean"
    );
    assert!(updates.try_recv().is_err());
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
}

#[tokio::test]
async fn claude_direct_final_revision_race_restores_auth_profile_and_same_action_retries() {
    let fixture = Fixture::new().await;
    let (provider_a, revision) = fixture
        .save_claude_with_auth("A", "claude-a", "secret-a", "anthropic-api-key")
        .await;
    let setup = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let a = setup
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            direct_action(provider_a),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&a, &["secret-a"]).is_ok(),
        "Claude Direct setup outcome exposed a credential"
    );
    let settings_path = fixture.home.user_home().join(".claude/settings.json");
    let settings_before = fs::read(&settings_path).unwrap();
    let snapshot_before = a.view.activated_snapshot.clone();
    let (provider_b, revision_b) = fixture
        .save_claude_with_auth("B", "claude-b", "secret-b", "anthropic-bearer")
        .await;
    let action_b = Uuid::new_v4();
    let pause = Arc::new(ActivationPause::default());
    let paused = Arc::new(fixture.dual_service(
        ActivationHooks::pausing_final_commit(pause.clone()),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    ));
    let context = claude_context(&fixture.home);
    let activation = {
        let paused = Arc::clone(&paused);
        tokio::spawn(async move {
            paused
                .apply_raw_for_with_context(
                    Target::Claude,
                    action_b,
                    revision_b,
                    direct_action(provider_b),
                    Some(&context),
                )
                .await
        })
    };
    pause.wait_until_reached().await;
    let (_, concurrent_revision) = fixture
        .save_claude_with_auth("Concurrent", "claude-c", "secret-c", "anthropic-api-key")
        .await;
    let mut updates = fixture.store.subscribe_target_views();
    pause.release();

    let failure = activation.await.unwrap().expect_failure_redacted();
    assert!(
        public_surface_is_secret_free(&failure, &["secret-a", "secret-b", "secret-c"]).is_ok(),
        "stale Claude activation exposed a credential"
    );
    assert_eq!(failure.problem.code, "stale-revision");
    assert!(
        redacted_match(&fs::read(&settings_path).unwrap(), &settings_before).is_ok(),
        "stale Claude activation did not restore the prior auth profile"
    );
    let current = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(current.management_revision, concurrent_revision);
    assert_eq!(current.current_provider_id, Some(provider_a.to_string()));
    assert_eq!(current.activated_snapshot, snapshot_before);
    assert!(fixture.store.receipt(action_b).await.unwrap().is_none());
    assert_eq!(
        fixture.recovery_state_for_action(action_b).await.as_deref(),
        Some("rolled-back")
    );
    assert!(updates.try_recv().is_err());
    assert!(paused.model_endpoint_for(Target::Claude).await.is_none());

    let retry = setup
        .apply_raw_for_with_context(
            Target::Claude,
            action_b,
            concurrent_revision,
            direct_action(provider_b),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&retry, &["secret-a", "secret-b", "secret-c"]).is_ok(),
        "retried Claude outcome exposed a credential"
    );
    assert_eq!(retry.status, ActionStatus::Applied);
    assert_eq!(retry.view.current_provider_id, Some(provider_b.to_string()));
    assert!(setup.model_endpoint_for(Target::Claude).await.is_none());
}

#[tokio::test]
async fn claude_direct_to_takeover_uses_v2_and_takeover_to_direct_fails_before_intent() {
    let fixture = Fixture::new().await;
    let settings = fixture.home.user_home().join(".claude/settings.json");
    let (direct_provider, _) = fixture
        .save_claude_with_auth(
            "API Direct",
            "claude-direct",
            "direct-api-secret",
            "anthropic-api-key",
        )
        .await;
    let (takeover_provider, revision) = fixture
        .save_claude_with_auth(
            "Takeover",
            "claude-route",
            "upstream-secret",
            "anthropic-bearer",
        )
        .await;
    let service = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let direct = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            direct_action(direct_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&direct, &["direct-api-secret", "upstream-secret"]).is_ok(),
        "Claude Direct outcome exposed a credential"
    );

    let takeover = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            direct.view.management_revision,
            takeover_action(takeover_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&takeover, &["direct-api-secret", "upstream-secret"]).is_ok(),
        "Claude Takeover outcome exposed a credential"
    );

    assert_eq!(takeover.view.mode, "takeover");
    assert_eq!(fixture.managed_config_version_for(Target::Claude).await, 2);
    assert!(takeover.view.takeover.endpoint.is_some());
    assert!(service.model_endpoint_for(Target::Claude).await.is_some());
    let applied: serde_json::Value = serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert_eq!(applied["env"]["ANTHROPIC_MODEL"], "claude-route");
    assert!(applied["env"]["ANTHROPIC_AUTH_TOKEN"].is_string());
    assert!(
        !applied["env"]
            .as_object()
            .unwrap()
            .contains_key("ANTHROPIC_API_KEY")
    );
    let intent_count = fixture.count("activation_recovery").await;
    let file_before = fs::read(&settings).unwrap();
    let failure = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            takeover.view.management_revision,
            direct_action(direct_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();
    assert!(
        public_surface_is_secret_free(&failure, &["direct-api-secret", "upstream-secret"]).is_ok(),
        "takeover-active failure exposed a credential"
    );
    assert_eq!(failure.problem.code, "takeover-active");
    assert_eq!(fixture.count("activation_recovery").await, intent_count);
    assert!(
        redacted_match(&fs::read(&settings).unwrap(), &file_before).is_ok(),
        "takeover-active failure changed Claude settings"
    );
    service.shutdown_model_for(Target::Claude).await.unwrap();
}

#[tokio::test]
async fn claude_direct_post_intent_failpoints_restore_exact_state_without_runtime_or_publication() {
    for failpoint in [
        ActivationFailpoint::RecoveryIntent,
        ActivationFailpoint::AtomicConfigWrite,
        ActivationFailpoint::ConfigVerify,
        ActivationFailpoint::FinalCommit,
    ] {
        let fixture = Fixture::new().await;
        let settings = fixture.home.user_home().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        let before = br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"prior-token","ANTHROPIC_API_KEY":"prior-key"},"permissions":{"allow":["Read"]}}"#;
        fs::write(&settings, before).unwrap();
        let before_value: serde_json::Value = serde_json::from_slice(before).unwrap();
        let (provider_id, revision) = fixture
            .save_claude_with_auth(
                "Claude Direct",
                "claude-direct",
                "provider-secret",
                "anthropic-bearer",
            )
            .await;
        let service = fixture.dual_service(
            ActivationHooks::failing(failpoint),
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
        );
        let mut updates = fixture.store.subscribe_target_views();
        let action_id = Uuid::new_v4();

        let failure = service
            .apply_raw_for_with_context(
                Target::Claude,
                action_id,
                revision,
                direct_action(provider_id),
                Some(&claude_context(&fixture.home)),
            )
            .await
            .expect_failure_redacted();

        assert!(
            public_surface_is_secret_free(
                &failure,
                &["provider-secret", "prior-token", "prior-key"]
            )
            .is_ok(),
            "Claude failpoint failure exposed a credential"
        );
        assert!(matches!(
            failure.problem.code.as_str(),
            "configuration-write-failed" | "internal-failure"
        ));
        assert!(
            redacted_match(
                &serde_json::from_slice::<serde_json::Value>(&fs::read(&settings).unwrap())
                    .unwrap(),
                &before_value
            )
            .is_ok(),
            "Claude failpoint did not restore settings at {failpoint:?}"
        );
        assert_eq!(
            fixture
                .recovery_state_for_action(action_id)
                .await
                .as_deref(),
            Some("rolled-back"),
            "{failpoint:?}"
        );
        assert!(
            fixture
                .store
                .activated_snapshot_for(Target::Claude)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            fixture
                .store
                .routing_credential_for(Target::Claude)
                .await
                .unwrap()
                .is_none()
        );
        assert!(service.model_endpoint_for(Target::Claude).await.is_none());
        assert!(updates.try_recv().is_err());
    }
}

#[tokio::test]
async fn claude_real_sqlite_commit_failure_restores_first_and_direct_to_direct_configuration() {
    for prior_direct in [false, true] {
        let fixture = Fixture::new().await;
        let settings = fixture.home.user_home().join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(
            &settings,
            br#"{"env":{"ANTHROPIC_AUTH_TOKEN":"operator-token","ANTHROPIC_API_KEY":"operator-key"},"permissions":{"allow":["Read"]}}"#,
        )
        .unwrap();
        let (first_provider, first_revision) = fixture
            .save_claude_with_auth("First", "claude-first", "first-secret", "anthropic-bearer")
            .await;
        let service = fixture.dual_service(
            ActivationHooks::default(),
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
        );
        let (provider_id, revision) = if prior_direct {
            let first = service
                .apply_raw_for_with_context(
                    Target::Claude,
                    Uuid::new_v4(),
                    first_revision,
                    direct_action(first_provider),
                    Some(&claude_context(&fixture.home)),
                )
                .await
                .expect_applied_redacted();
            let (second_provider, second_revision) = fixture
                .save_claude_with_auth(
                    "Second",
                    "claude-second",
                    "second-secret",
                    "anthropic-api-key",
                )
                .await;
            assert!(second_revision > first.view.management_revision);
            (second_provider, second_revision)
        } else {
            (first_provider, first_revision)
        };
        let view_before = fixture.store.target_view_for(Target::Claude).await.unwrap();
        let file_before: serde_json::Value =
            serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        let snapshot_count = fixture.count("activated_snapshots").await;
        let receipt_count = fixture.count("action_receipts").await;
        let action_id = Uuid::new_v4();
        let abort = ActivationCommitAbort::install(&fixture.home);

        let failure = service
            .apply_raw_for_with_context(
                Target::Claude,
                action_id,
                revision,
                direct_action(provider_id),
                Some(&claude_context(&fixture.home)),
            )
            .await
            .expect_failure_redacted();
        drop(abort);

        assert!(
            public_surface_is_secret_free(&failure, &["first-secret", "second-secret"]).is_ok(),
            "SQLite commit failure exposed a credential"
        );
        assert_eq!(failure.problem.code, "internal-failure");
        assert!(
            redacted_match(
                &serde_json::from_slice::<serde_json::Value>(&fs::read(&settings).unwrap())
                    .unwrap(),
                &file_before
            )
            .is_ok(),
            "SQLite commit failure did not restore Claude settings"
        );
        let view_after = fixture.store.target_view_for(Target::Claude).await.unwrap();
        assert_eq!(
            view_after.management_revision,
            view_before.management_revision
        );
        assert_eq!(
            view_after.current_provider_id,
            view_before.current_provider_id
        );
        assert_eq!(
            view_after.activated_snapshot,
            view_before.activated_snapshot
        );
        assert_eq!(fixture.count("activated_snapshots").await, snapshot_count);
        assert_eq!(fixture.count("action_receipts").await, receipt_count);
        assert_eq!(
            fixture
                .recovery_state_for_action(action_id)
                .await
                .as_deref(),
            Some("rolled-back")
        );
        assert!(
            fixture
                .store
                .routing_credential_for(Target::Claude)
                .await
                .unwrap()
                .is_none()
        );
        assert!(service.model_endpoint_for(Target::Claude).await.is_none());
    }
}

#[tokio::test]
async fn claude_real_sqlite_commit_failure_cleans_direct_to_takeover_candidate() {
    let fixture = Fixture::new().await;
    let (direct_provider, direct_revision) = fixture
        .save_claude_with_auth(
            "Direct",
            "claude-direct",
            "direct-secret",
            "anthropic-api-key",
        )
        .await;
    let service = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let direct = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            direct_revision,
            direct_action(direct_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let (takeover_provider, revision) = fixture
        .save_claude_with_auth(
            "Takeover",
            "claude-route",
            "upstream-secret",
            "anthropic-bearer",
        )
        .await;
    assert!(revision > direct.view.management_revision);
    let settings = fixture.home.user_home().join(".claude/settings.json");
    let file_before: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    let view_before = fixture.store.target_view_for(Target::Claude).await.unwrap();
    let action_id = Uuid::new_v4();
    let abort = ActivationCommitAbort::install(&fixture.home);

    let failure = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_id,
            revision,
            takeover_action(takeover_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();
    drop(abort);

    assert!(
        public_surface_is_secret_free(&failure, &["direct-secret", "upstream-secret"]).is_ok(),
        "SQLite transition failure exposed a credential"
    );
    assert_eq!(failure.problem.code, "internal-failure");
    assert!(
        redacted_match(
            &serde_json::from_slice::<serde_json::Value>(&fs::read(&settings).unwrap()).unwrap(),
            &file_before
        )
        .is_ok(),
        "SQLite transition failure did not restore Claude settings"
    );
    let view_after = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(
        view_after.management_revision,
        view_before.management_revision
    );
    assert_eq!(
        view_after.current_provider_id,
        view_before.current_provider_id
    );
    assert_eq!(
        view_after.activated_snapshot,
        view_before.activated_snapshot
    );
    assert!(
        fixture
            .store
            .routing_credential_for(Target::Claude)
            .await
            .unwrap()
            .is_none()
    );
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
    assert_eq!(
        fixture
            .recovery_state_for_action(action_id)
            .await
            .as_deref(),
        Some("rolled-back")
    );
}

#[tokio::test]
async fn claude_v1_takeover_sqlite_failure_restores_legacy_api_key_and_retry_upgrades_to_v2() {
    let fixture = Fixture::new().await;
    let settings = fixture.home.user_home().join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(
        &settings,
        br#"{"env":{"ANTHROPIC_API_KEY":"legacy-api-key"},"permissions":{"allow":["Read"]}}"#,
    )
    .unwrap();
    let (first_provider, first_revision) = fixture
        .save_claude_with_auth(
            "Legacy",
            "claude-legacy",
            "legacy-upstream-secret",
            "anthropic-api-key",
        )
        .await;
    let first_service = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let first = first_service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            first_revision,
            takeover_action(first_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(&first, &["legacy-upstream-secret", "legacy-api-key"])
            .is_ok(),
        "legacy Claude activation exposed a credential"
    );
    let endpoint = first_service
        .model_endpoint_for(Target::Claude)
        .await
        .unwrap();
    fixture
        .downgrade_latest_claude_takeover_to_v1("legacy-api-key")
        .await;
    assert_eq!(fixture.managed_config_version_for(Target::Claude).await, 1);
    first_service
        .shutdown_model_for(Target::Claude)
        .await
        .unwrap();
    let restarted_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let service = ActivationService::new(
        Arc::clone(&restarted_store),
        fixture.home.clone(),
        fixture.probe.clone(),
        "/usr/bin/codex".into(),
        Arc::new(NoopUpstream),
    )
    .with_claude_runtime(
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
        "/usr/bin/claude".into(),
    );
    service
        .bootstrap_committed_takeover_for(Target::Claude)
        .await
        .unwrap();
    assert_eq!(
        service.model_endpoint_for(Target::Claude).await,
        Some(endpoint)
    );
    let restarted_settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert!(
        redacted_match(
            &restarted_settings["env"]["ANTHROPIC_API_KEY"],
            &serde_json::json!("legacy-api-key")
        )
        .is_ok(),
        "legacy Claude API key changed during bootstrap"
    );
    let (second_provider, revision) = fixture
        .save_claude_with_auth(
            "Upgraded",
            "claude-upgraded",
            "upgraded-upstream-secret",
            "anthropic-bearer",
        )
        .await;
    assert!(revision > first.view.management_revision);
    let action_id = Uuid::new_v4();
    let abort = ActivationCommitAbort::install(&fixture.home);

    let failure = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_id,
            revision,
            takeover_action(second_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();
    drop(abort);

    assert!(
        public_surface_is_secret_free(
            &failure,
            &[
                "legacy-upstream-secret",
                "upgraded-upstream-secret",
                "legacy-api-key"
            ]
        )
        .is_ok(),
        "legacy Claude failure exposed a credential"
    );
    assert_eq!(failure.problem.code, "internal-failure");
    assert_eq!(fixture.managed_config_version_for(Target::Claude).await, 1);
    let restored: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert!(
        redacted_match(
            &restored["env"]["ANTHROPIC_API_KEY"],
            &serde_json::json!("legacy-api-key")
        )
        .is_ok(),
        "legacy Claude API key was not restored"
    );
    assert_eq!(restored["env"]["ANTHROPIC_MODEL"], "claude-legacy");
    assert_eq!(
        service.model_endpoint_for(Target::Claude).await,
        Some(endpoint)
    );
    assert!(tokio::net::TcpStream::connect(endpoint).await.is_ok());
    assert_eq!(
        fixture
            .recovery_state_for_action(action_id)
            .await
            .as_deref(),
        Some("rolled-back")
    );

    let upgraded = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_id,
            revision,
            takeover_action(second_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert!(
        public_surface_is_secret_free(
            &upgraded,
            &[
                "legacy-upstream-secret",
                "upgraded-upstream-secret",
                "legacy-api-key"
            ]
        )
        .is_ok(),
        "upgraded Claude activation exposed a credential"
    );
    assert_eq!(upgraded.status, ActionStatus::Applied);
    assert_eq!(fixture.managed_config_version_for(Target::Claude).await, 2);
    let upgraded_settings: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
    assert!(
        !upgraded_settings["env"]
            .as_object()
            .unwrap()
            .contains_key("ANTHROPIC_API_KEY")
    );
    assert_eq!(
        upgraded_settings["env"]["ANTHROPIC_MODEL"],
        "claude-upgraded"
    );
    assert_eq!(
        service.model_endpoint_for(Target::Claude).await,
        Some(endpoint)
    );
    service.shutdown_model_for(Target::Claude).await.unwrap();
}

#[tokio::test]
async fn inconsistent_claude_context_never_projects_provider_mode_without_a_selector() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save_claude("Claude", "claude-test", "provider-secret")
        .await;
    let probe = Arc::new(GoodClaudeProbe(AtomicUsize::new(0)));
    let service = fixture
        .service(ActivationHooks::default())
        .with_claude_runtime(probe.clone(), "/usr/bin/claude".into());
    let mut invalid = claude_context(&fixture.home);
    invalid.selector_state = ClaudeSelectorState::Enabled;

    let failure = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            takeover_action(provider_id),
            Some(&invalid),
        )
        .await
        .expect_failure_redacted();

    assert_eq!(failure.problem.code, "preflight-context-required");
    assert!(failure.problem.selector.is_none());
    assert_eq!(probe.0.load(Ordering::SeqCst), 0);
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
}

#[tokio::test]
async fn claude_provisional_and_post_intent_faults_release_runtime_and_restore_exact_settings() {
    for (failpoint, expects_intent) in [
        (ActivationFailpoint::BindListener, false),
        (ActivationFailpoint::PersistRoutingCredential, false),
        (ActivationFailpoint::Snapshot, false),
        (ActivationFailpoint::RecoveryIntent, true),
        (ActivationFailpoint::AtomicConfigWrite, true),
        (ActivationFailpoint::ConfigVerify, true),
        (ActivationFailpoint::FinalCommit, true),
    ] {
        let fixture = Fixture::new().await;
        let settings_home = fixture.home.user_home().join(".claude");
        fs::create_dir_all(&settings_home).unwrap();
        let settings_path = settings_home.join("settings.json");
        let before = br#"{"permissions":{"allow":["Read"]}}"#;
        let before_value: serde_json::Value = serde_json::from_slice(before).unwrap();
        fs::write(&settings_path, before).unwrap();
        let (provider_id, revision) = fixture
            .save_claude("Claude", "claude-sonnet-test", "provider-secret")
            .await;
        let codex_before = fixture.store.target_view().await.unwrap();
        let service = fixture.dual_service(
            ActivationHooks::failing(failpoint),
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
        );
        let action_id = Uuid::new_v4();

        let failure = service
            .apply_raw_for_with_context(
                Target::Claude,
                action_id,
                revision,
                takeover_action(provider_id),
                Some(&claude_context(&fixture.home)),
            )
            .await
            .expect_failure_redacted();

        assert!(
            public_surface_is_secret_free(&failure, &["provider-secret"]).is_ok(),
            "Claude takeover failpoint exposed a credential"
        );
        assert!(matches!(
            failure.problem.code.as_str(),
            "configuration-write-failed" | "internal-failure"
        ));
        assert!(
            redacted_match(
                &serde_json::from_slice::<serde_json::Value>(&fs::read(&settings_path).unwrap())
                    .unwrap(),
                &before_value
            )
            .is_ok(),
            "Claude takeover failpoint did not restore settings at {failpoint:?}"
        );
        assert!(service.model_endpoint_for(Target::Claude).await.is_none());
        assert!(
            fixture
                .store
                .routing_credential_for(Target::Claude)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            fixture
                .store
                .activated_snapshot_for(Target::Claude)
                .await
                .unwrap()
                .is_none()
        );
        let intent = fixture
            .store
            .recovery_intent_for(Target::Claude, action_id)
            .await
            .unwrap();
        assert_eq!(intent.is_some(), expects_intent, "{failpoint:?}");
        if let Some(intent) = intent {
            assert_eq!(
                intent.state(),
                muxvia_routing::state::RecoveryState::RolledBack
            );
        }
        assert_eq!(fixture.store.target_view().await.unwrap(), codex_before);
    }
}

#[tokio::test]
async fn claude_unverifiable_restore_marks_only_claude_recovery_required() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save_claude("Claude", "claude-test", "provider-secret")
        .await;
    let service = fixture.dual_service(
        ActivationHooks::failing(ActivationFailpoint::RestoreVerify),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );

    let failure = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            takeover_action(provider_id),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();

    assert_eq!(failure.problem.code, "recovery-required");
    assert_eq!(
        fixture
            .store
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .recovery
            .state,
        "recovery-required"
    );
    assert_eq!(
        fixture.store.target_view().await.unwrap().recovery.state,
        "clean"
    );
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
}

#[tokio::test]
async fn claude_unverifiable_hot_switch_drains_only_the_claude_runtime() {
    let fixture = Fixture::new().await;
    let (codex_provider, codex_revision) = fixture
        .save("Codex", "gpt-peer", "codex-provider-secret")
        .await;
    let (first_claude, first_revision) = fixture
        .save_claude("Claude One", "claude-one", "first-provider-secret")
        .await;
    let first = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let codex_applied = first
        .activate(command(codex_provider, codex_revision, Uuid::new_v4()))
        .await
        .expect_applied_redacted();
    let claude_applied = first
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            first_revision,
            takeover_action(first_claude),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let codex_endpoint: std::net::SocketAddr = codex_applied
        .view
        .takeover
        .endpoint
        .as_deref()
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    let claude_endpoint: std::net::SocketAddr = claude_applied
        .view
        .takeover
        .endpoint
        .as_deref()
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    first.shutdown_models().await.unwrap();

    let faulting = fixture.dual_service(
        ActivationHooks::failing(ActivationFailpoint::RestoreVerify),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    faulting.bootstrap_committed_takeovers().await.unwrap();
    assert_eq!(faulting.model_endpoint().await, Some(codex_endpoint));
    assert_eq!(
        faulting.model_endpoint_for(Target::Claude).await,
        Some(claude_endpoint)
    );
    let codex_before = fixture.store.target_view().await.unwrap();
    let (second_claude, second_revision) = fixture
        .save_claude("Claude Two", "claude-two", "second-provider-secret")
        .await;

    let failure = faulting
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            second_revision,
            takeover_action(second_claude),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_failure_redacted();

    assert_eq!(failure.problem.code, "recovery-required");
    assert_eq!(
        failure.authoritative_view.recovery.state,
        "recovery-required"
    );
    assert!(faulting.model_endpoint_for(Target::Claude).await.is_none());
    assert!(
        tokio::net::TcpStream::connect(claude_endpoint)
            .await
            .is_err()
    );
    assert_eq!(faulting.model_endpoint().await, Some(codex_endpoint));
    assert!(tokio::net::TcpStream::connect(codex_endpoint).await.is_ok());
    assert_eq!(fixture.store.target_view().await.unwrap(), codex_before);
    faulting.shutdown_models().await.unwrap();
}

#[tokio::test]
async fn claude_publication_failure_keeps_the_commit_and_replay_is_side_effect_free() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save_claude("Claude", "claude-sonnet-test", "provider-secret")
        .await;
    let service = fixture.dual_service(
        ActivationHooks::failing(ActivationFailpoint::PublishView),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let mut updates = fixture.store.subscribe_target_views();
    let action_id = Uuid::new_v4();

    let applied = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_id,
            revision,
            takeover_action(provider_id),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();

    assert_eq!(applied.status, ActionStatus::Applied);
    assert!(updates.try_recv().is_err());
    assert_eq!(
        fixture.store.target_view_for(Target::Claude).await.unwrap(),
        applied.view
    );
    assert!(service.model_endpoint_for(Target::Claude).await.is_some());
    let replay = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_id,
            0,
            serde_json::json!({"malformed": true}),
            None,
        )
        .await
        .expect_applied_redacted();
    assert_eq!(replay.status, ActionStatus::Replayed);
    assert_eq!(replay.view, applied.view);
    assert!(updates.try_recv().is_err());
}

#[tokio::test]
async fn claude_post_commit_runtime_handoff_recovery_updates_applied_receipt_and_replays_coherently()
 {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save_claude("Claude", "claude-handoff-test", "provider-secret")
        .await;
    let codex_before = fixture.store.target_view().await.unwrap();
    let service = fixture.dual_service(
        ActivationHooks::failing(ActivationFailpoint::RuntimeHandoff),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let mut updates = fixture.store.subscribe_target_views();
    let action_id = Uuid::new_v4();

    let initial = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_id,
            revision,
            takeover_action(provider_id),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();

    assert_eq!(initial.status, ActionStatus::Applied);
    let current = fixture.store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(initial.view, current);
    assert_eq!(current.recovery.state, "recovery-required");
    assert_eq!(current.managed_configuration.state, "recovery-required");
    let stored_receipt = fixture.stored_receipt_for(Target::Claude, action_id).await;
    assert_eq!(stored_receipt.status, ActionStatus::Applied);
    assert_eq!(stored_receipt.view, current);
    let replay = service
        .apply_raw_for_with_context(
            Target::Claude,
            action_id,
            0,
            serde_json::json!({"malformed": true}),
            None,
        )
        .await
        .expect_applied_redacted();
    assert_eq!(replay.status, ActionStatus::Replayed);
    assert_eq!(replay.view, current);
    let receipt = fixture
        .store
        .receipt_for(Target::Claude, action_id)
        .await
        .unwrap()
        .expect("the database commit must remain authoritative");
    assert_eq!(receipt.status, ActionStatus::Replayed);
    assert_eq!(receipt.view, current);
    assert_eq!(
        receipt.view.current_provider_id,
        Some(provider_id.to_string())
    );
    assert_eq!(
        receipt.view.activated_snapshot.as_ref().unwrap().model,
        "claude-handoff-test"
    );
    let endpoint: std::net::SocketAddr = receipt
        .view
        .takeover
        .endpoint
        .as_deref()
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    assert_eq!(current.takeover.endpoint, receipt.view.takeover.endpoint);
    assert_eq!(fixture.count("activated_snapshots").await, 1);
    assert_eq!(
        fixture
            .recovery_state_for_action(action_id)
            .await
            .as_deref(),
        Some("recovery-required")
    );
    let settings: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.home.user_home().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["env"]["ANTHROPIC_MODEL"], "claude-handoff-test");
    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
    assert!(tokio::net::TcpStream::connect(endpoint).await.is_err());
    expect_push_redacted(&mut updates, &current).await;
    assert!(updates.try_recv().is_err());
    assert_eq!(fixture.store.target_view().await.unwrap(), codex_before);
}

#[tokio::test]
async fn claude_hot_switch_reuses_runtime_and_pins_in_flight_then_next_request_snapshots() {
    let fixture = Fixture::new().await;
    let upstream = Arc::new(PinningUpstream::default());
    let service = ActivationService::new(
        Arc::clone(&fixture.store),
        fixture.home.clone(),
        fixture.probe.clone(),
        "/usr/bin/codex".into(),
        upstream.clone(),
    )
    .with_claude_runtime(
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
        "/usr/bin/claude".into(),
    );
    let (first_id, revision) = fixture
        .save_claude("First", "claude-first", "first-provider-secret")
        .await;
    let first = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            revision,
            takeover_action(first_id),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let endpoint = first.view.takeover.endpoint.clone().unwrap();
    let credential = fixture
        .store
        .routing_credential_for(Target::Claude)
        .await
        .unwrap()
        .unwrap();
    let credential_value = secrecy::ExposeSecret::expose_secret(&credential).to_owned();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let old_request = {
        let client = client.clone();
        let endpoint = endpoint.clone();
        let credential = credential_value.clone();
        tokio::spawn(async move {
            client
                .post(format!("{endpoint}/v1/messages"))
                .bearer_auth(credential)
                .header("content-type", "application/json")
                .body(r#"{"model":"client-old","messages":[]}"#)
                .send()
                .await
                .unwrap()
        })
    };
    upstream.first_started.notified().await;

    let (second_id, second_revision) = fixture
        .save_claude("Second", "claude-second", "second-provider-secret")
        .await;
    let second = service
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            second_revision,
            takeover_action(second_id),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    assert_eq!(
        second.view.takeover.endpoint.as_deref(),
        Some(endpoint.as_str())
    );
    let credential_was_reused = secrecy::ExposeSecret::expose_secret(
        &fixture
            .store
            .routing_credential_for(Target::Claude)
            .await
            .unwrap()
            .unwrap(),
    ) == credential_value;
    assert!(
        credential_was_reused,
        "Claude hot switch changed its stable Routing Credential"
    );
    let new_response = client
        .post(format!("{endpoint}/v1/messages"))
        .bearer_auth(&credential_value)
        .header("content-type", "application/json")
        .body(r#"{"model":"client-new","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(new_response.status(), StatusCode::OK);
    upstream.release_first.notify_one();
    assert_eq!(old_request.await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        upstream.models.lock().unwrap().as_slice(),
        ["claude-first", "claude-second"]
    );
    assert_eq!(fixture.count("activated_snapshots").await, 2);
}

#[tokio::test]
async fn claude_listener_credential_and_snapshot_remain_provisional_until_database_commit() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save_claude("Claude", "claude-test", "provider-secret")
        .await;
    let pause = Arc::new(ActivationPause::default());
    let service = Arc::new(fixture.dual_service(
        ActivationHooks::pausing_final_commit(pause.clone()),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    ));
    let context = claude_context(&fixture.home);
    let activation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .apply_raw_for_with_context(
                    Target::Claude,
                    Uuid::new_v4(),
                    revision,
                    takeover_action(provider_id),
                    Some(&context),
                )
                .await
        })
    };
    pause.wait_until_reached().await;

    assert!(service.model_endpoint_for(Target::Claude).await.is_none());
    assert!(
        fixture
            .store
            .routing_credential_for(Target::Claude)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .store
            .activated_snapshot_for(Target::Claude)
            .await
            .unwrap()
            .is_none()
    );
    let settings: serde_json::Value = serde_json::from_slice(
        &fs::read(fixture.home.user_home().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    let endpoint = settings["env"]["ANTHROPIC_BASE_URL"].as_str().unwrap();
    let credential = settings["env"]["ANTHROPIC_AUTH_TOKEN"].as_str().unwrap();
    let attempted = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{endpoint}/v1/messages"))
        .bearer_auth(credential)
        .header("content-type", "application/json")
        .body(r#"{"messages":[]}"#)
        .send();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), attempted)
            .await
            .is_err(),
        "a provisional Claude listener served before the database commit"
    );

    pause.release();
    let applied = activation.await.unwrap().expect_applied_redacted();
    assert_eq!(applied.status, ActionStatus::Applied);
    assert!(service.model_endpoint_for(Target::Claude).await.is_some());
}

#[tokio::test]
async fn claude_payload_ownership_mismatch_rolls_back_without_commit_or_publication() {
    for current_version in [1_u32, 2_u32] {
        let fixture = Fixture::new().await;
        let (provider_id, mut revision) = fixture
            .save_claude("Claude", "claude-test", "provider-secret")
            .await;
        let mut snapshot_count = 0;
        if current_version == 2 {
            let setup = fixture.dual_service(
                ActivationHooks::default(),
                Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
            );
            let applied = setup
                .apply_raw_for_with_context(
                    Target::Claude,
                    Uuid::new_v4(),
                    revision,
                    takeover_action(provider_id),
                    Some(&claude_context(&fixture.home)),
                )
                .await
                .expect_applied_redacted();
            revision = applied.view.management_revision;
            snapshot_count = 1;
            setup.shutdown_models().await.unwrap();
            let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
                .await
                .unwrap();
            database
                .call(|connection| {
                    connection.execute(
                        "UPDATE target_route_state SET managed_config_version = 2
                         WHERE target = 'claude'",
                        [],
                    )?;
                    Ok::<(), tokio_rusqlite::rusqlite::Error>(())
                })
                .await
                .unwrap();
        }

        let settings_path = fixture.home.user_home().join(".claude/settings.json");
        let settings_before = fs::read(&settings_path).ok();
        let view_before = fixture.store.target_view_for(Target::Claude).await.unwrap();
        let action_id = Uuid::new_v4();
        let pause = Arc::new(ActivationPause::default());
        let service = Arc::new(fixture.dual_service(
            ActivationHooks::pausing_final_commit(pause.clone()),
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
        ));
        let mut updates = fixture.store.subscribe_target_views();
        let context = claude_context(&fixture.home);
        let activation = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                service
                    .apply_raw_for_with_context(
                        Target::Claude,
                        action_id,
                        revision,
                        takeover_action(provider_id),
                        Some(&context),
                    )
                    .await
            })
        };
        pause.wait_until_reached().await;

        // Every newly authorized Claude activation writes the T06 four-field contract,
        // including an unmanaged row that still carries the migrated v1 marker.
        let mismatched_version = 1;
        let database = tokio_rusqlite::Connection::open(fixture.home.database_path())
            .await
            .unwrap();
        database
            .call(move |connection| {
                let payload_json: String = connection.query_row(
                    "SELECT payload_json FROM activation_recovery
                     WHERE target = 'claude' AND action_id = ?1 AND state = 'pending'",
                    [action_id.to_string()],
                    |row| row.get(0),
                )?;
                let mut payload: serde_json::Value = serde_json::from_str(&payload_json)
                    .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                payload["ownership_version"] = serde_json::json!(mismatched_version);
                payload["before"]["ownership_version"] = serde_json::json!(mismatched_version);
                payload["desired"]["ownership_version"] = serde_json::json!(mismatched_version);
                for field in ["before", "desired"] {
                    let owned = payload[field]["owned"]
                        .as_object_mut()
                        .ok_or(tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                    if mismatched_version == 2 {
                        owned.insert("api_key".to_owned(), serde_json::Value::Null);
                    } else {
                        owned.remove("api_key");
                    }
                }
                connection.execute(
                    "UPDATE activation_recovery SET payload_json = ?1
                     WHERE target = 'claude' AND action_id = ?2 AND state = 'pending'",
                    tokio_rusqlite::rusqlite::params![payload.to_string(), action_id.to_string()],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        pause.release();

        let failure = activation.await.unwrap().expect_failure_redacted();
        assert!(
            public_surface_is_secret_free(&failure, &["provider-secret"]).is_ok(),
            "ownership mismatch failure exposed a credential"
        );
        assert_eq!(failure.problem.code, "internal-failure");
        assert!(
            redacted_match(&fs::read(&settings_path).ok(), &settings_before).is_ok(),
            "ownership mismatch rollback did not restore exact Claude settings"
        );
        let view = fixture.store.target_view_for(Target::Claude).await.unwrap();
        assert_eq!(view.management_revision, view_before.management_revision);
        assert_eq!(view.view_sequence, view_before.view_sequence);
        assert_eq!(view.mode, view_before.mode);
        assert_eq!(view.current_provider_id, view_before.current_provider_id);
        assert_eq!(view.activated_snapshot, view_before.activated_snapshot);
        assert_eq!(view.takeover, view_before.takeover);
        assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
        assert_eq!(fixture.count("activated_snapshots").await, snapshot_count);
        assert_eq!(
            fixture
                .recovery_state_for_action(action_id)
                .await
                .as_deref(),
            Some("rolled-back")
        );
        assert!(service.model_endpoint_for(Target::Claude).await.is_none());
        assert!(updates.try_recv().is_err());
    }
}

async fn assert_direct_pre_mutation_failure(
    fixture: &Fixture,
    service: &ActivationService,
    provider_id: Uuid,
    revision: u64,
    expected_code: &str,
) {
    assert_activation_pre_mutation_failure(
        fixture,
        service,
        revision,
        direct_action(provider_id),
        expected_code,
    )
    .await;
}

async fn assert_activation_pre_mutation_failure(
    fixture: &Fixture,
    service: &ActivationService,
    revision: u64,
    action: serde_json::Value,
    expected_code: &str,
) {
    let action_id = Uuid::new_v4();
    let before = fixture.mutation_fingerprint().await;
    let endpoint_before = service.model_endpoint().await;
    let mut updates = fixture.store.subscribe_target_views();
    let failure = service
        .apply_raw(action_id, revision, action)
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, expected_code);
    assert_eq!(
        fixture.mutation_fingerprint().await,
        before,
        "pre-mutation Direct failure changed protected state"
    );
    assert_eq!(service.model_endpoint().await, endpoint_before);
    assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    assert!(updates.try_recv().is_err());
}

async fn assert_invalid_committed_state_failure(
    fixture: &Fixture,
    service: &ActivationService,
    revision: u64,
    action: serde_json::Value,
) {
    let action_id = Uuid::new_v4();
    let config_path = fixture.home.user_home().join(".codex/config.toml");
    let auth_path = fixture.home.user_home().join(".codex/auth.json");
    let config_before = fs::read(&config_path).ok();
    let auth_before = fs::read(&auth_path).ok();
    let view_before = fixture.store.target_view().await.unwrap();
    let snapshot_count = fixture.count("activated_snapshots").await;
    let endpoint_before = service.model_endpoint().await;
    let mut updates = fixture.store.subscribe_target_views();

    let failure = service
        .apply_raw(action_id, revision, action)
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "recovery-required");
    assert!(
        fs::read(&config_path).ok() == config_before,
        "invalid committed state handling changed Codex configuration"
    );
    assert!(
        fs::read(&auth_path).ok() == auth_before,
        "invalid committed state handling changed Codex authentication"
    );
    let view = fixture.store.target_view().await.unwrap();
    assert_eq!(view.management_revision, view_before.management_revision);
    assert_eq!(view.current_provider_id, view_before.current_provider_id);
    assert_eq!(view.activated_snapshot, view_before.activated_snapshot);
    assert_eq!(view.recovery.state, "recovery-required");
    assert_eq!(view.managed_configuration.state, "recovery-required");
    assert_eq!(service.model_endpoint().await, endpoint_before);
    assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    assert_eq!(fixture.count("activated_snapshots").await, snapshot_count);
    assert!(updates.try_recv().is_err());
}

#[tokio::test]
async fn inactive_dangling_snapshot_pointer_requires_recovery_before_activation() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save("Direct", "gpt-direct", "provider-secret")
        .await;
    fixture
        .overwrite_route_state("inactive", None, None, Some(Uuid::new_v4()))
        .await;
    let service = fixture.service(ActivationHooks::default());

    assert_invalid_committed_state_failure(
        &fixture,
        &service,
        revision,
        direct_action(provider_id),
    )
    .await;
}

#[tokio::test]
async fn active_dangling_snapshot_pointer_requires_recovery_before_activation() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save("Direct", "gpt-direct", "provider-secret")
        .await;
    fixture
        .overwrite_route_state(
            "active",
            Some(43123),
            Some("routing-secret"),
            Some(Uuid::new_v4()),
        )
        .await;
    let service = fixture.service(ActivationHooks::default());

    assert_invalid_committed_state_failure(
        &fixture,
        &service,
        revision,
        takeover_action(provider_id),
    )
    .await;
}

#[tokio::test]
async fn incomplete_takeover_runtime_requires_recovery_before_activation() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save("Direct", "gpt-direct", "provider-secret")
        .await;
    let service = fixture.service(ActivationHooks::default());
    let activated = service
        .apply_raw(Uuid::new_v4(), revision, direct_action(provider_id))
        .await
        .unwrap();
    let activated_revision = activated.view.management_revision;
    let snapshot_id = activated.view.activated_snapshot.unwrap().id;
    fixture
        .overwrite_route_state("active", Some(43123), None, Some(snapshot_id))
        .await;

    assert_invalid_committed_state_failure(
        &fixture,
        &service,
        activated_revision,
        takeover_action(provider_id),
    )
    .await;
}

#[tokio::test]
async fn managed_direct_drift_blocks_direct_and_takeover_transitions_before_mutation() {
    for mode in [ActivationMode::Direct, ActivationMode::Takeover] {
        let fixture = Fixture::new().await;
        let config_home = fixture.home.user_home().join(".codex");
        fs::create_dir_all(&config_home).unwrap();
        let auth_before = br#"{"tokens":"drift-auth-sentinel"}"#;
        fs::write(config_home.join("auth.json"), auth_before).unwrap();
        let (first_id, revision) = fixture.save("First", "gpt-first", "first-secret").await;
        let service = fixture.service(ActivationHooks::default());
        service
            .apply_raw(Uuid::new_v4(), revision, direct_action(first_id))
            .await
            .unwrap();
        let (second_id, second_revision) =
            fixture.save("Second", "gpt-second", "second-secret").await;
        let config_path = config_home.join("config.toml");
        let managed = fs::read_to_string(&config_path).unwrap();
        let drifted = managed.replacen("model = \"gpt-first\"", "model = \"drifted\"", 1);
        assert!(
            managed != drifted,
            "test failed to mutate a managed Codex field"
        );
        fs::write(&config_path, drifted).unwrap();
        let action = match mode {
            ActivationMode::Direct => direct_action(second_id),
            ActivationMode::Takeover => takeover_action(second_id),
        };

        assert_activation_pre_mutation_failure(
            &fixture,
            &service,
            second_revision,
            action,
            "recovery-required",
        )
        .await;
        assert!(
            fs::read(config_home.join("auth.json")).unwrap() == auth_before,
            "managed drift handling changed Codex authentication state"
        );
    }
}

#[tokio::test]
async fn direct_activation_commits_config_snapshot_and_receipt_without_model_route() {
    let fixture = Fixture::new().await;
    let config_home = fixture.home.user_home().join(".codex");
    fs::create_dir_all(&config_home).unwrap();
    fs::write(
        config_home.join("config.toml"),
        "# operator-owned\napproval_policy = \"never\"\n",
    )
    .unwrap();
    let auth_path = config_home.join("auth.json");
    let auth_before = br#"{"tokens":"auth-sentinel-must-not-change"}"#;
    fs::write(&auth_path, auth_before).unwrap();
    let provider_secret = "provider-secret-must-not-escape";
    let (provider_id, revision) = fixture.save("Direct", "gpt-direct", provider_secret).await;
    let observer = Arc::new(Steps::default());
    let service = fixture.service(ActivationHooks::observed(observer.clone()));
    let action_id = Uuid::new_v4();
    let mut updates = fixture.store.subscribe_target_views();

    let outcome = service
        .apply_raw(action_id, revision, direct_action(provider_id))
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
    assert_eq!(outcome.view.mode, "direct");
    assert_eq!(outcome.view.takeover.state, "inactive");
    assert!(outcome.view.takeover.endpoint.is_none());
    assert_eq!(outcome.view.managed_configuration.state, "applied");
    assert_eq!(
        outcome.view.managed_configuration.path.as_deref(),
        Some(config_home.join("config.toml").to_string_lossy().as_ref())
    );
    assert!(outcome.view.managed_configuration.restart_required);
    let snapshot = outcome.view.activated_snapshot.as_ref().unwrap();
    assert_eq!(snapshot.provider_id, provider_id);
    assert_eq!(snapshot.model, "gpt-direct");
    assert_eq!(snapshot.epoch, fixture.store.service_epoch());
    assert_eq!(
        observer.0.lock().unwrap().as_slice(),
        &[
            ActivationStep::Validate,
            ActivationStep::Snapshot,
            ActivationStep::RecoveryIntent,
            ActivationStep::AtomicConfigWrite,
            ActivationStep::ConfigVerify,
            ActivationStep::StateAndReceiptCommit,
            ActivationStep::PublishView,
        ]
    );

    let config_before_replay = fs::read_to_string(config_home.join("config.toml")).unwrap();
    let config = config_before_replay.parse::<DocumentMut>().unwrap();
    assert_eq!(config["model"].as_str(), Some("gpt-direct"));
    assert_eq!(config["model_provider"].as_str(), Some("muxvia_codex"));
    assert_eq!(config["approval_policy"].as_str(), Some("never"));
    let provider = &config["model_providers"]["muxvia_codex"];
    assert_eq!(provider["name"].as_str(), Some("Muxvia Direct"));
    assert_eq!(
        provider["base_url"].as_str(),
        Some("https://upstream.example/v1")
    );
    assert_eq!(provider["wire_api"].as_str(), Some("responses"));
    assert_eq!(provider["supports_websockets"].as_bool(), Some(false));
    assert!(
        provider["http_headers"]["Authorization"].as_str()
            == Some("Bearer provider-secret-must-not-escape"),
        "Direct Authorization did not match the saved Provider credential"
    );
    assert!(
        fs::read(&auth_path).unwrap() == auth_before,
        "Direct activation changed Codex authentication state"
    );
    assert!(fixture.store.routing_credential().await.unwrap().is_none());
    assert!(service.model_endpoint().await.is_none());
    assert_eq!(fixture.count("activated_snapshots").await, 1);
    assert_eq!(fixture.count("activation_recovery").await, 1);
    assert_eq!(fixture.count("action_receipts").await, 2);

    let published = updates.recv().await.unwrap();
    assert_eq!(published, outcome.view);
    assert!(updates.try_recv().is_err());
    let steps_before_replay = observer.0.lock().unwrap().len();
    let probe_before_replay = fixture.probe.0.load(Ordering::SeqCst);
    let replay = service
        .apply_raw(action_id, u64::MAX, serde_json::json!({"malformed": true}))
        .await
        .unwrap();
    assert_eq!(replay.status, ActionStatus::Replayed);
    assert_eq!(replay.view, outcome.view);
    assert_eq!(observer.0.lock().unwrap().len(), steps_before_replay);
    assert_eq!(fixture.probe.0.load(Ordering::SeqCst), probe_before_replay);
    assert!(
        fs::read_to_string(config_home.join("config.toml")).unwrap() == config_before_replay,
        "receipt-first replay changed Managed Configuration"
    );
    assert_eq!(fixture.count("activated_snapshots").await, 1);
    assert_eq!(fixture.count("activation_recovery").await, 1);
    assert_eq!(fixture.count("action_receipts").await, 2);
    assert!(updates.try_recv().is_err());
}

#[tokio::test]
async fn direct_activation_validation_failures_are_authoritative_and_pre_mutation() {
    for case in [
        "takeover-required",
        "incomplete",
        "stale",
        "recovery",
        "unsupported-home",
        "incompatible",
        "collision",
        "symlink",
    ] {
        let fixture = Fixture::new().await;
        let config_home = fixture.home.user_home().join(".codex");
        fs::create_dir_all(&config_home).unwrap();
        if case != "symlink" {
            fs::write(
                config_home.join("config.toml"),
                "approval_policy = \"never\"\n",
            )
            .unwrap();
        }
        fs::write(
            config_home.join("auth.json"),
            br#"{"tokens":"pre-mutation-auth-sentinel"}"#,
        )
        .unwrap();
        let (provider_id, revision) = fixture.save("Direct", "gpt", "provider-secret").await;
        match case {
            "takeover-required" => {
                fixture
                    .set_provider_routing_requirement(provider_id, "takeover-required")
                    .await;
            }
            "incomplete" => fixture.remove_provider_credential(provider_id).await,
            "recovery" => fixture.set_recovery_required().await,
            "collision" => fs::write(
                config_home.join("config.toml"),
                "[model_providers.muxvia_codex]\nname = \"operator-owned\"\n",
            )
            .unwrap(),
            "symlink" => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink;
                    let outside = fixture.home.user_home().join("outside.toml");
                    fs::write(&outside, "model = \"outside\"\n").unwrap();
                    symlink(&outside, config_home.join("config.toml")).unwrap();
                }
            }
            _ => {}
        }
        let service = match case {
            "unsupported-home" => fixture
                .service(ActivationHooks::default())
                .with_configuration_home_override(Some(fixture.home.user_home().join("elsewhere"))),
            "incompatible" => ActivationService::new(
                Arc::clone(&fixture.store),
                fixture.home.clone(),
                Arc::new(BadProbe),
                "/usr/bin/codex".into(),
                Arc::new(NoopUpstream),
            ),
            _ => fixture.service(ActivationHooks::default()),
        };
        let expected_revision = if case == "stale" {
            revision - 1
        } else {
            revision
        };
        let expected_code = match case {
            "takeover-required" => "takeover-required",
            "incomplete" => "incomplete-provider",
            "stale" => "stale-revision",
            "recovery" => "recovery-required",
            "unsupported-home" => "unsupported-configuration-home",
            "incompatible" => "incompatible-target-cli",
            "collision" => "configuration-collision",
            "symlink" => "configuration-write-failed",
            _ => unreachable!(),
        };

        assert_direct_pre_mutation_failure(
            &fixture,
            &service,
            provider_id,
            expected_revision,
            expected_code,
        )
        .await;
    }
}

#[tokio::test]
async fn direct_activation_rejects_active_takeover_before_mutation() {
    let fixture = Fixture::new().await;
    let (takeover_id, revision) = fixture
        .save("Takeover", "gpt-route", "route-provider")
        .await;
    let service = fixture.service(ActivationHooks::default());
    service
        .activate(command(takeover_id, revision, Uuid::new_v4()))
        .await
        .unwrap();
    let (direct_id, direct_revision) = fixture
        .save("Direct", "gpt-direct", "direct-provider")
        .await;

    assert_direct_pre_mutation_failure(
        &fixture,
        &service,
        direct_id,
        direct_revision,
        "takeover-active",
    )
    .await;
}

#[tokio::test]
async fn direct_transitions_verify_the_committed_snapshot_instead_of_the_editable_provider() {
    let fixture = Fixture::new().await;
    let config_home = fixture.home.user_home().join(".codex");
    fs::create_dir_all(&config_home).unwrap();
    fs::write(
        config_home.join("config.toml"),
        "approval_policy = \"never\"\n",
    )
    .unwrap();
    let (first_id, revision) = fixture.save("First", "gpt-first", "first-secret").await;
    let service = fixture.service(ActivationHooks::default());
    let first = service
        .apply_raw(Uuid::new_v4(), revision, direct_action(first_id))
        .await
        .unwrap();
    let first_snapshot_id = first.view.activated_snapshot.unwrap().id;
    fixture
        .mutate_provider(
            first_id,
            "https://edited-after-activation.invalid/v1",
            "edited-after-activation",
        )
        .await;
    fixture
        .mutate_provider_credential(first_id, "edited-first-secret")
        .await;

    let (second_id, second_revision) = fixture.save("Second", "gpt-second", "second-secret").await;
    let second = service
        .apply_raw(Uuid::new_v4(), second_revision, direct_action(second_id))
        .await
        .unwrap();

    assert_eq!(second.view.mode, "direct");
    assert_eq!(
        second.view.current_provider_id.as_deref(),
        Some(second_id.to_string().as_str())
    );
    assert!(second.view.serving_provider_id.is_none());
    assert!(second.view.takeover.endpoint.is_none());
    assert!(fixture.store.routing_credential().await.unwrap().is_none());
    assert!(service.model_endpoint().await.is_none());
    assert_eq!(fixture.count("activated_snapshots").await, 2);
    assert_ne!(
        second.view.activated_snapshot.as_ref().unwrap().id,
        first_snapshot_id
    );
    let direct_config = fs::read_to_string(config_home.join("config.toml")).unwrap();
    assert!(direct_config.contains("gpt-second"));
    assert!(direct_config.contains("https://upstream.example/v1"));
    assert!(!direct_config.contains("edited-after-activation.invalid"));
    fixture
        .mutate_provider(
            second_id,
            "https://second-edited.invalid/v1",
            "second-edited",
        )
        .await;
    fixture
        .mutate_provider_credential(second_id, "edited-second-secret")
        .await;

    let (takeover_id, takeover_revision) = fixture
        .save("Takeover", "gpt-takeover", "takeover-secret")
        .await;
    let takeover = service
        .apply_raw(
            Uuid::new_v4(),
            takeover_revision,
            takeover_action(takeover_id),
        )
        .await
        .unwrap();

    assert_eq!(takeover.view.mode, "takeover");
    assert_eq!(takeover.view.takeover.state, "active");
    assert!(takeover.view.takeover.endpoint.is_some());
    assert_eq!(
        takeover.view.current_provider_id.as_deref(),
        Some(takeover_id.to_string().as_str())
    );
    assert!(fixture.store.routing_credential().await.unwrap().is_some());
    assert!(service.model_endpoint().await.is_some());
    assert_eq!(fixture.count("activated_snapshots").await, 3);
}

#[tokio::test]
async fn direct_post_intent_failures_restore_exact_prior_state_without_runtime_or_publication() {
    for (failpoint, expected_code) in [
        (
            ActivationFailpoint::AtomicConfigWrite,
            "configuration-write-failed",
        ),
        (
            ActivationFailpoint::ConfigVerify,
            "configuration-write-failed",
        ),
        (ActivationFailpoint::FinalCommit, "internal-failure"),
    ] {
        let fixture = Fixture::new().await;
        let config_home = fixture.home.user_home().join(".codex");
        fs::create_dir_all(&config_home).unwrap();
        let config_before = "# keep\nmodel = \"operator\"\n[features]\nfoo = true\n";
        fs::write(config_home.join("config.toml"), config_before).unwrap();
        let auth_before = br#"{"tokens":"rollback-auth-sentinel"}"#;
        fs::write(config_home.join("auth.json"), auth_before).unwrap();
        let (provider_id, revision) = fixture
            .save("Direct", "gpt-direct", "provider-secret")
            .await;
        let action_id = Uuid::new_v4();
        let mut updates = fixture.store.subscribe_target_views();
        let service = fixture.service(ActivationHooks::failing(failpoint));

        let failure = service
            .apply_raw(action_id, revision, direct_action(provider_id))
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, expected_code);
        assert!(
            fs::read_to_string(config_home.join("config.toml")).unwrap() == config_before,
            "Direct rollback did not restore exact prior Managed Configuration"
        );
        assert!(
            fs::read(config_home.join("auth.json")).unwrap() == auth_before,
            "Direct rollback changed Codex authentication state"
        );
        let view = fixture.store.target_view().await.unwrap();
        assert_eq!(view.management_revision, revision);
        assert_eq!(view.view_sequence, revision);
        assert_eq!(view.mode, "unmanaged");
        assert!(view.current_provider_id.is_none());
        assert!(view.serving_provider_id.is_none());
        assert!(view.activated_snapshot.is_none());
        assert_eq!(view.takeover.state, "inactive");
        assert!(view.takeover.endpoint.is_none());
        assert!(fixture.store.routing_credential().await.unwrap().is_none());
        assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
        assert_eq!(fixture.count("activated_snapshots").await, 0);
        assert_eq!(
            fixture
                .recovery_state_for_action(action_id)
                .await
                .as_deref(),
            Some("rolled-back")
        );
        assert!(service.model_endpoint().await.is_none());
        assert!(updates.try_recv().is_err());
    }
}

#[tokio::test]
async fn direct_final_stale_rollback_and_rolled_back_action_retry_preserve_ordering() {
    let fixture = Fixture::new().await;
    let config_home = fixture.home.user_home().join(".codex");
    fs::create_dir_all(&config_home).unwrap();
    let config_before = "# keep\napproval_policy = \"never\"\n";
    fs::write(config_home.join("config.toml"), config_before).unwrap();
    let (provider_id, revision) = fixture
        .save("Direct", "gpt-direct", "provider-secret")
        .await;
    let action_id = Uuid::new_v4();
    let pause = Arc::new(ActivationPause::default());
    let service = Arc::new(fixture.service(ActivationHooks::pausing_final_commit(pause.clone())));
    let mut updates = fixture.store.subscribe_target_views();
    let activation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .apply_raw(action_id, revision, direct_action(provider_id))
                .await
        })
    };
    pause.wait_until_reached().await;
    let (_, saved_revision) = fixture
        .save("Concurrent", "gpt-other", "other-secret")
        .await;
    pause.release();

    let failure = activation.await.unwrap().unwrap_err();
    assert_eq!(failure.problem.code, "stale-revision");
    assert!(
        fs::read_to_string(config_home.join("config.toml")).unwrap() == config_before,
        "stale Direct final commit did not restore prior Managed Configuration"
    );
    let view = fixture.store.target_view().await.unwrap();
    assert_eq!(view.management_revision, saved_revision);
    assert_eq!(view.mode, "unmanaged");
    assert!(view.current_provider_id.is_none());
    assert!(view.activated_snapshot.is_none());
    assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    assert_eq!(
        fixture
            .recovery_state_for_action(action_id)
            .await
            .as_deref(),
        Some("rolled-back")
    );
    assert!(service.model_endpoint().await.is_none());
    assert!(updates.try_recv().is_err());

    let retry = fixture.service(ActivationHooks::default());
    let applied = retry
        .apply_raw(action_id, saved_revision, direct_action(provider_id))
        .await
        .unwrap();
    assert_eq!(applied.status, ActionStatus::Applied);
    assert_eq!(applied.view.mode, "direct");
    assert_eq!(fixture.count("activation_recovery").await, 1);
    assert_eq!(fixture.count("activated_snapshots").await, 1);
    assert!(retry.model_endpoint().await.is_none());
    assert_eq!(updates.recv().await.unwrap(), applied.view);
    assert!(updates.try_recv().is_err());
}

#[tokio::test]
async fn paused_direct_final_commit_does_not_block_model_control() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save("Direct", "gpt-direct", "provider-secret")
        .await;
    let pause = Arc::new(ActivationPause::default());
    let service = Arc::new(fixture.service(ActivationHooks::pausing_final_commit(pause.clone())));
    let activation = {
        let service = Arc::clone(&service);
        tokio::spawn(async move {
            service
                .apply_raw(Uuid::new_v4(), revision, direct_action(provider_id))
                .await
        })
    };
    pause.wait_until_reached().await;

    let endpoint =
        tokio::time::timeout(std::time::Duration::from_secs(1), service.model_endpoint()).await;
    let shutdown =
        tokio::time::timeout(std::time::Duration::from_secs(1), service.shutdown_model()).await;
    pause.release();
    let applied = activation.await.unwrap().unwrap();

    assert!(
        endpoint.is_ok(),
        "Direct final commit blocked Model Server inspection"
    );
    assert!(endpoint.unwrap().is_none());
    assert!(
        shutdown.is_ok(),
        "Direct final commit blocked Model Server shutdown"
    );
    assert!(shutdown.unwrap().is_ok());
    assert_eq!(applied.status, ActionStatus::Applied);
    assert_eq!(applied.view.mode, "direct");
}

#[tokio::test]
async fn direct_restore_verification_failure_enters_recovery_required_without_advertising_direct() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save("Direct", "gpt-direct", "provider-secret")
        .await;
    let action_id = Uuid::new_v4();
    let mut updates = fixture.store.subscribe_target_views();
    let service = fixture.service(ActivationHooks::failing(ActivationFailpoint::RestoreVerify));

    let failure = service
        .apply_raw(action_id, revision, direct_action(provider_id))
        .await
        .unwrap_err();

    assert_eq!(failure.problem.code, "recovery-required");
    let view = fixture.store.target_view().await.unwrap();
    assert_eq!(view.mode, "unmanaged");
    assert_eq!(view.recovery.state, "recovery-required");
    assert_eq!(view.managed_configuration.state, "recovery-required");
    assert!(view.current_provider_id.is_none());
    assert!(view.activated_snapshot.is_none());
    assert_eq!(view.takeover.state, "inactive");
    assert!(view.takeover.endpoint.is_none());
    assert!(fixture.store.routing_credential().await.unwrap().is_none());
    assert!(fixture.store.receipt(action_id).await.unwrap().is_none());
    assert_eq!(
        fixture
            .recovery_state_for_action(action_id)
            .await
            .as_deref(),
        Some("recovery-required")
    );
    assert!(service.model_endpoint().await.is_none());
    assert!(updates.try_recv().is_err());
}

#[tokio::test]
async fn committed_direct_restart_opens_control_only_and_exits_after_last_session() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture
        .save("Direct", "gpt-direct", "provider-secret")
        .await;
    let first = fixture.service(ActivationHooks::default());
    first
        .apply_raw(Uuid::new_v4(), revision, direct_action(provider_id))
        .await
        .unwrap();
    assert!(first.model_endpoint().await.is_none());
    drop(first);

    let reopened = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let activation = Arc::new(ActivationService::new(
        Arc::clone(&reopened),
        fixture.home.clone(),
        fixture.probe.clone(),
        "/usr/bin/codex".into(),
        Arc::new(SuccessfulUpstream),
    ));
    let mut handle = ControlServer::bind_process(
        &fixture.home,
        Arc::clone(&reopened),
        "test",
        Arc::clone(&activation),
    )
    .await
    .unwrap();
    assert!(activation.model_endpoint().await.is_none());
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
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(1), handle.wait_for_exit())
        .await
        .unwrap()
        .unwrap();
    assert!(!handle.socket_path().exists());
    assert!(activation.model_endpoint().await.is_none());
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
    assert!(outcome.view.problems.is_empty());
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
            ActivationStep::RuntimeHandoff,
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
async fn unknown_compatible_warning_is_committed_replayed_and_projected_after_reopen() {
    let fixture = Fixture::new().await;
    let provider_secret = "provider-secret-must-not-appear-in-warning";
    let (provider_id, revision) = fixture.save("First", "gpt", provider_secret).await;
    let service = ActivationService::new(
        Arc::clone(&fixture.store),
        fixture.home.clone(),
        Arc::new(UnknownCompatibleProbe),
        "/usr/bin/codex".into(),
        Arc::new(NoopUpstream),
    );
    let action_id = Uuid::new_v4();

    let applied = service
        .activate(command(provider_id, revision, action_id))
        .await
        .unwrap();

    assert_eq!(applied.view.problems.len(), 1);
    assert_eq!(applied.view.problems[0].code, "untested-target-cli");
    assert!(applied.view.problems[0].message.contains("99.0.0"));
    assert!(
        !serde_json::to_string(&applied)
            .unwrap()
            .contains(provider_secret)
    );

    let replayed = service
        .activate(command(Uuid::new_v4(), u64::MAX, action_id))
        .await
        .unwrap();
    assert_eq!(replayed.status, ActionStatus::Replayed);
    assert_eq!(replayed.view.problems, applied.view.problems);

    let reopened = StateStore::open(&fixture.home).await.unwrap();
    assert_eq!(
        reopened.target_view().await.unwrap().problems,
        applied.view.problems
    );
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

    let save = fixture.store.apply_provider_action(
        Uuid::new_v4(), revision, serde_json::json!({"kind":"create-provider","name":"x","baseUrl":"https://x.test/v1","model":"m","credential":{"kind":"replace","value":"s"},"presetKey":null})
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
              "expectedRevision":0,"action":{"kind":"create-provider","name":"First",
              "baseUrl":"https://upstream.example/v1","model":"gpt","credential":{"kind":"replace","value":"provider-secret"},"presetKey":null}}
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

#[tokio::test]
async fn committed_takeover_bootstraps_exact_endpoint_in_a_new_service_epoch_without_activate() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt", "provider-secret").await;
    let first = fixture.service(ActivationHooks::default());
    let activated = first
        .activate(command(provider_id, revision, Uuid::new_v4()))
        .await
        .unwrap();
    let endpoint: std::net::SocketAddr = activated
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    first.shutdown_model().await.unwrap();
    drop(first);

    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    assert_ne!(second_store.service_epoch(), fixture.store.service_epoch());
    let second = ActivationService::new(
        Arc::clone(&second_store),
        fixture.home.clone(),
        fixture.probe.clone(),
        "/usr/bin/codex".into(),
        Arc::new(SuccessfulUpstream),
    );
    second.bootstrap_committed_takeover().await.unwrap();
    assert_eq!(second.model_endpoint().await, Some(endpoint));
    let credential = second_store.routing_credential().await.unwrap().unwrap();

    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://{endpoint}/v1/responses"))
        .header(
            "X-Muxvia-Routing-Credential",
            secrecy::ExposeSecret::expose_secret(&credential),
        )
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), "ok");
}

#[tokio::test]
async fn occupied_committed_port_fails_before_opening_the_control_socket() {
    let fixture = Fixture::new().await;
    let (provider_id, revision) = fixture.save("First", "gpt", "secret").await;
    let first = fixture.service(ActivationHooks::default());
    let activated = first
        .activate(command(provider_id, revision, Uuid::new_v4()))
        .await
        .unwrap();
    let endpoint: std::net::SocketAddr = activated
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    first.shutdown_model().await.unwrap();
    let occupied = tokio::net::TcpListener::bind(endpoint).await.unwrap();
    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let second = Arc::new(ActivationService::new(
        Arc::clone(&second_store),
        fixture.home.clone(),
        fixture.probe.clone(),
        "/usr/bin/codex".into(),
        Arc::new(NoopUpstream),
    ));

    let error = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&second_store),
        "test",
        Arc::clone(&second),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(error, ControlServerError::State));
    assert!(!fixture.home.root().join("run/control.sock").exists());
    assert!(second.model_endpoint().await.is_none());
    second.shutdown_models().await.unwrap();
    drop(occupied);
}

#[tokio::test]
async fn occupied_claude_port_fails_before_uds_and_drains_the_resumed_codex_peer() {
    let fixture = Fixture::new().await;
    let (codex_provider, codex_revision) = fixture.save("Codex", "gpt-test", "secret").await;
    let (claude_provider, claude_revision) =
        fixture.save_claude("Claude", "claude-test", "secret").await;
    let first = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let codex = first
        .activate(command(codex_provider, codex_revision, Uuid::new_v4()))
        .await
        .unwrap();
    let claude = first
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            claude_revision,
            takeover_action(claude_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let codex_endpoint: std::net::SocketAddr = codex
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    let claude_endpoint: std::net::SocketAddr = claude
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    first.shutdown_models().await.unwrap();
    let occupied = tokio::net::TcpListener::bind(claude_endpoint)
        .await
        .unwrap();
    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let second = Arc::new(
        ActivationService::new(
            Arc::clone(&second_store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
            "/usr/bin/claude".into(),
        ),
    );

    let error = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&second_store),
        "test",
        Arc::clone(&second),
    )
    .await
    .err()
    .unwrap();
    assert!(matches!(error, ControlServerError::State));
    assert!(!fixture.home.root().join("run/control.sock").exists());
    assert!(second.model_endpoint().await.is_none());
    assert!(second.model_endpoint_for(Target::Claude).await.is_none());
    second.shutdown_models().await.unwrap();
    let rebound_codex = tokio::net::TcpListener::bind(codex_endpoint).await.unwrap();
    drop(rebound_codex);
    drop(occupied);
}

#[tokio::test]
async fn malformed_claude_committed_state_is_target_local_while_codex_and_uds_resume() {
    for missing in ["snapshot", "routing-credential"] {
        let fixture = Fixture::new().await;
        let (codex_provider, codex_revision) =
            fixture.save("Codex", "gpt-test", "codex-secret").await;
        let (claude_provider, claude_revision) = fixture
            .save_claude("Claude", "claude-test", "claude-secret")
            .await;
        let first = fixture.dual_service(
            ActivationHooks::default(),
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
        );
        let codex = first
            .activate(command(codex_provider, codex_revision, Uuid::new_v4()))
            .await
            .unwrap();
        first
            .apply_raw_for_with_context(
                Target::Claude,
                Uuid::new_v4(),
                claude_revision,
                takeover_action(claude_provider),
                Some(&claude_context(&fixture.home)),
            )
            .await
            .expect_applied_redacted();
        let codex_endpoint: std::net::SocketAddr = codex
            .view
            .takeover
            .endpoint
            .unwrap()
            .trim_start_matches("http://")
            .parse()
            .unwrap();
        first.shutdown_models().await.unwrap();
        fixture
            .corrupt_committed_target(Target::Claude, missing)
            .await;

        let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
        let second = Arc::new(
            ActivationService::new(
                Arc::clone(&second_store),
                fixture.home.clone(),
                fixture.probe.clone(),
                "/usr/bin/codex".into(),
                Arc::new(NoopUpstream),
            )
            .with_claude_runtime(
                Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
                "/usr/bin/claude".into(),
            ),
        );
        let handle = ControlServer::bind_with_activation(
            &fixture.home,
            Arc::clone(&second_store),
            "test",
            Arc::clone(&second),
        )
        .await
        .unwrap();

        assert!(handle.socket_path().exists(), "missing {missing}");
        assert_eq!(second.model_endpoint().await, Some(codex_endpoint));
        assert!(second.model_endpoint_for(Target::Claude).await.is_none());
        let claude_view = second_store.target_view_for(Target::Claude).await.unwrap();
        assert_eq!(claude_view.recovery.state, "recovery-required");
        assert_eq!(claude_view.managed_configuration.state, "recovery-required");
        assert_eq!(claude_view.takeover.state, "unavailable");
        assert!(claude_view.takeover.endpoint.is_none());

        handle.shutdown().await.unwrap();
        second.shutdown_models().await.unwrap();
    }
}

#[tokio::test]
async fn unconstructible_claude_configuration_home_is_target_local_at_startup() {
    let fixture = Fixture::new().await;
    let (codex_provider, codex_revision) = fixture.save("Codex", "gpt", "codex-secret").await;
    let (claude_provider, claude_revision) = fixture
        .save_claude("Claude", "claude", "claude-secret")
        .await;
    let first = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let codex = first
        .activate(command(codex_provider, codex_revision, Uuid::new_v4()))
        .await
        .unwrap();
    first
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            claude_revision,
            takeover_action(claude_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let codex_endpoint: std::net::SocketAddr = codex
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    first.shutdown_models().await.unwrap();
    let claude_home = fixture.home.user_home().join(".claude");
    fs::rename(
        &claude_home,
        fixture.home.user_home().join(".claude-preserved"),
    )
    .unwrap();
    fs::write(&claude_home, b"not-a-directory").unwrap();

    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let second = Arc::new(
        ActivationService::new(
            Arc::clone(&second_store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
            "/usr/bin/claude".into(),
        ),
    );
    let handle = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&second_store),
        "test",
        Arc::clone(&second),
    )
    .await
    .unwrap();

    assert!(handle.socket_path().exists());
    assert_eq!(second.model_endpoint().await, Some(codex_endpoint));
    assert!(second.model_endpoint_for(Target::Claude).await.is_none());
    let claude_view = second_store.target_view_for(Target::Claude).await.unwrap();
    assert_eq!(claude_view.takeover.state, "unavailable");
    assert!(claude_view.takeover.endpoint.is_none());
    assert!(
        claude_view
            .problems
            .iter()
            .any(|problem| problem.code == "model-route-unavailable")
    );

    handle.shutdown().await.unwrap();
    second.shutdown_models().await.unwrap();
}

#[tokio::test]
async fn startup_marks_only_claude_configuration_drift_and_resumes_clean_codex() {
    let fixture = Fixture::new().await;
    let (codex_provider, codex_revision) = fixture
        .save("Codex", "gpt-test", "codex-provider-secret")
        .await;
    let (claude_provider, claude_revision) = fixture
        .save_claude("Claude", "claude-test", "claude-provider-secret")
        .await;
    let first = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let codex_applied = first
        .activate(command(codex_provider, codex_revision, Uuid::new_v4()))
        .await
        .unwrap();
    let claude_applied = first
        .apply_raw_for_with_context(
            Target::Claude,
            Uuid::new_v4(),
            claude_revision,
            takeover_action(claude_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let codex_endpoint: std::net::SocketAddr = codex_applied
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    let claude_endpoint: std::net::SocketAddr = claude_applied
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    assert_ne!(codex_endpoint, claude_endpoint);
    let routing_credentials_are_distinct = secrecy::ExposeSecret::expose_secret(
        &fixture.store.routing_credential().await.unwrap().unwrap(),
    ) != secrecy::ExposeSecret::expose_secret(
        &fixture
            .store
            .routing_credential_for(Target::Claude)
            .await
            .unwrap()
            .unwrap(),
    );
    assert!(
        routing_credentials_are_distinct,
        "Codex and Claude Routing Credentials were not isolated"
    );
    first.shutdown_models().await.unwrap();

    let settings_path = fixture.home.user_home().join(".claude/settings.json");
    let mut drifted: serde_json::Value =
        serde_json::from_slice(&fs::read(&settings_path).unwrap()).unwrap();
    drifted["env"]["ANTHROPIC_MODEL"] = serde_json::json!("operator-drift");
    fs::write(&settings_path, serde_json::to_vec_pretty(&drifted).unwrap()).unwrap();
    let drifted_before = fs::read(&settings_path).unwrap();

    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let second = Arc::new(
        ActivationService::new(
            Arc::clone(&second_store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
            "/usr/bin/claude".into(),
        ),
    );
    let handle = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&second_store),
        "test",
        Arc::clone(&second),
    )
    .await
    .unwrap();

    assert_eq!(second.model_endpoint().await, Some(codex_endpoint));
    assert!(second.model_endpoint_for(Target::Claude).await.is_none());
    assert_eq!(
        second_store
            .target_view_for(Target::Claude)
            .await
            .unwrap()
            .managed_configuration
            .state,
        "configuration-drift"
    );
    assert!(
        redacted_match(&fs::read(&settings_path).unwrap(), &drifted_before).is_ok(),
        "startup drift handling changed Claude settings"
    );
    let unbound = tokio::net::TcpListener::bind(claude_endpoint)
        .await
        .unwrap();
    drop(unbound);
    handle.shutdown().await.unwrap();
    second.shutdown_models().await.unwrap();
}

#[tokio::test]
async fn startup_keeps_claude_recovery_control_only_while_resuming_clean_codex() {
    let fixture = Fixture::new().await;
    let (codex_provider, codex_revision) = fixture
        .save("Codex", "gpt-test", "codex-provider-secret")
        .await;
    let (claude_provider, claude_revision) = fixture
        .save_claude("Claude", "claude-test", "claude-provider-secret")
        .await;
    let first = fixture.dual_service(
        ActivationHooks::default(),
        Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
    );
    let codex = first
        .activate(command(codex_provider, codex_revision, Uuid::new_v4()))
        .await
        .unwrap();
    let claude_action = Uuid::new_v4();
    let claude = first
        .apply_raw_for_with_context(
            Target::Claude,
            claude_action,
            claude_revision,
            takeover_action(claude_provider),
            Some(&claude_context(&fixture.home)),
        )
        .await
        .expect_applied_redacted();
    let codex_endpoint: std::net::SocketAddr = codex
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    let claude_endpoint: std::net::SocketAddr = claude
        .view
        .takeover
        .endpoint
        .unwrap()
        .trim_start_matches("http://")
        .parse()
        .unwrap();
    first.shutdown_models().await.unwrap();
    let claude_intent = fixture
        .store
        .recovery_intent_for(Target::Claude, claude_action)
        .await
        .unwrap()
        .unwrap();
    fixture
        .store
        .set_recovery_state(
            claude_intent.id(),
            muxvia_routing::state::RecoveryState::RecoveryRequired,
        )
        .await
        .unwrap();

    let second_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
    let second = Arc::new(
        ActivationService::new(
            Arc::clone(&second_store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(
            Arc::new(GoodClaudeProbe(AtomicUsize::new(0))),
            "/usr/bin/claude".into(),
        ),
    );
    let handle = ControlServer::bind_with_activation(
        &fixture.home,
        Arc::clone(&second_store),
        "test",
        Arc::clone(&second),
    )
    .await
    .unwrap();

    assert_eq!(second.model_endpoint().await, Some(codex_endpoint));
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
    let unbound = tokio::net::TcpListener::bind(claude_endpoint)
        .await
        .unwrap();
    drop(unbound);
    handle.shutdown().await.unwrap();
    second.shutdown_models().await.unwrap();
}

#[tokio::test]
async fn dead_model_handle_is_not_reused_or_silently_rebound_before_activation_intent() {
    let fixture = Fixture::new().await;
    let (first_id, revision) = fixture.save("First", "gpt-1", "secret-1").await;
    let service = fixture.service(ActivationHooks::default());
    service
        .activate(command(first_id, revision, Uuid::new_v4()))
        .await
        .unwrap();
    let (second_id, second_revision) = fixture.save("Second", "gpt-2", "secret-2").await;
    let intent_count = fixture.count("activation_recovery").await;
    service.abort_model().await;

    let failure = service
        .activate(command(second_id, second_revision, Uuid::new_v4()))
        .await
        .unwrap_err();
    assert_eq!(failure.problem.code, "configuration-write-failed");
    assert_eq!(fixture.count("activation_recovery").await, intent_count);
    assert!(service.model_endpoint().await.is_none());
}
