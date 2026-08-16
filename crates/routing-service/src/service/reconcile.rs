use std::{
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
};

use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    claude::{ClaudeConfigCodec, ClaudeProbe, CommandClaudeProbe},
    codex::{CodexConfigCodec, CodexProbe, CommandCodexProbe, FileIdentity},
    control::protocol::{
        CompatibilityClassification, CompatibilityView, ControlProblem, ProviderEffect,
        ReconciliationPreview, ReconciliationStrategy, ShadowSource, Target,
    },
    home::MuxviaHome,
    state::{StateError, StateStore},
};

use super::reconciliation_adapter::{
    CommittedConfiguration, ObservedReconciliation, PreparedConfiguration, ProbedCompatibility,
    ReconciliationContext, TargetReconciliationAdapter,
};

pub(crate) struct CodexRuntimeContext {
    probe: Arc<dyn CodexProbe>,
    executable: PathBuf,
    user_home: PathBuf,
}

pub(crate) struct ClaudeRuntimeContext {
    probe: Arc<dyn ClaudeProbe>,
    executable: PathBuf,
    user_home: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ObservationKey(Target, ReconciliationStrategy);

impl Hash for ObservationKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let target = match self.0 {
            Target::Codex => 0_u8,
            Target::Claude => 1,
        };
        let strategy = match self.1 {
            ReconciliationStrategy::Adopt => 0_u8,
            ReconciliationStrategy::Reapply => 1,
            ReconciliationStrategy::Restore => 2,
        };
        target.hash(state);
        strategy.hash(state);
    }
}

#[derive(Clone, PartialEq)]
struct ObservationRecord {
    token: Uuid,
    target: Target,
    strategy: ReconciliationStrategy,
    management_revision: u64,
    compatibility: CompatibilityView,
    shadows: Vec<ShadowSource>,
    canonical_home: PathBuf,
    file_identity: FileIdentity,
    owned_fingerprint: String,
    unrelated_fingerprint: String,
    snapshot_id: Option<Uuid>,
    recovery_intent_id: Option<Uuid>,
    service_epoch: Uuid,
}

impl fmt::Debug for ObservationRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObservationRecord")
            .field("token", &self.token)
            .field("target", &self.target)
            .field("strategy", &self.strategy)
            .field("management_revision", &self.management_revision)
            .field("compatibility", &self.compatibility)
            .field("shadows", &self.shadows)
            .field("canonical_home", &"<redacted>")
            .field("file_identity", &"<redacted>")
            .field("owned_fingerprint", &"<opaque>")
            .field("unrelated_fingerprint", &"<opaque>")
            .field("snapshot_id", &self.snapshot_id)
            .field("recovery_intent_id", &self.recovery_intent_id)
            .field("service_epoch", &self.service_epoch)
            .finish()
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct ValidatedReconciliation {
    pub(crate) prepared: PreparedConfiguration,
}

pub(crate) struct ReconciliationService {
    state: Arc<StateStore>,
    tokens: Mutex<HashMap<ObservationKey, ObservationRecord>>,
    codex: CodexRuntimeContext,
    claude: ClaudeRuntimeContext,
}

impl ReconciliationService {
    pub(crate) fn for_home(
        state: Arc<StateStore>,
        home: &MuxviaHome,
    ) -> Result<Self, ControlProblem> {
        let codex_executable = find_executable("codex");
        let claude_executable = find_executable("claude");
        Self::with_runtimes(
            state,
            home,
            Arc::new(CommandCodexProbe),
            codex_executable,
            Arc::new(CommandClaudeProbe),
            claude_executable,
        )
    }

    pub(crate) fn with_runtimes(
        state: Arc<StateStore>,
        home: &MuxviaHome,
        codex_probe: Arc<dyn CodexProbe>,
        codex_executable: PathBuf,
        claude_probe: Arc<dyn ClaudeProbe>,
        claude_executable: PathBuf,
    ) -> Result<Self, ControlProblem> {
        let codex_codec = CodexConfigCodec::for_user_home(home.user_home())
            .map_err(|problem| stable_problem(problem.code()))?;
        let claude_codec = ClaudeConfigCodec::for_user_home(home.user_home())
            .map_err(|problem| stable_problem(problem.code()))?;
        let _ = (codex_codec, claude_codec);
        Ok(Self {
            state,
            tokens: Mutex::new(HashMap::new()),
            codex: CodexRuntimeContext {
                probe: codex_probe,
                executable: codex_executable,
                user_home: home.user_home().to_path_buf(),
            },
            claude: ClaudeRuntimeContext {
                probe: claude_probe,
                executable: claude_executable,
                user_home: home.user_home().to_path_buf(),
            },
        })
    }

    pub(crate) async fn preview(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        context: ReconciliationContext,
    ) -> Result<ReconciliationPreview, ControlProblem> {
        let (record, observed) = self.observe(target, strategy, &context).await?;
        let preview = ReconciliationPreview {
            observation_token: record.token,
            target,
            strategy,
            management_revision: record.management_revision,
            compatibility: record.compatibility.clone(),
            shadow_sources: record.shadows.clone(),
            changes: observed.observation.changes,
            provider_effect: match strategy {
                ReconciliationStrategy::Adopt => ProviderEffect::CreateNew,
                ReconciliationStrategy::Reapply => ProviderEffect::KeepCurrent,
                ReconciliationStrategy::Restore => ProviderEffect::ExitManaged,
            },
            restart_required: strategy != ReconciliationStrategy::Adopt,
            unobservable_runtime_boundary: true,
        };
        self.tokens
            .lock()
            .await
            .insert(ObservationKey(target, strategy), record);
        Ok(preview)
    }

    #[allow(dead_code)]
    pub(crate) async fn validate_preview(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        token: Uuid,
        context: ReconciliationContext,
    ) -> Result<ValidatedReconciliation, ControlProblem> {
        let expected = self
            .tokens
            .lock()
            .await
            .get(&ObservationKey(target, strategy))
            .filter(|record| record.token == token)
            .cloned()
            .ok_or_else(stale_preview)?;
        let (actual, observed) = self
            .observe(target, strategy, &context)
            .await
            .map_err(|_| stale_preview())?;
        if expected != actual.with_token(expected.token) {
            return Err(stale_preview());
        }
        let is_still_current = self
            .tokens
            .lock()
            .await
            .get(&ObservationKey(target, strategy))
            .is_some_and(|record| record.token == expected.token);
        if !is_still_current {
            return Err(stale_preview());
        }
        Ok(ValidatedReconciliation {
            prepared: observed.prepared,
        })
    }

    async fn observe(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        context: &ReconciliationContext,
    ) -> Result<(ObservationRecord, ObservedReconciliation), ControlProblem> {
        let view = self
            .state
            .target_view_for(target)
            .await
            .map_err(map_state_problem)?;
        let snapshot_id = view.activated_snapshot.as_ref().map(|snapshot| snapshot.id);
        let recovery_intent_id = view
            .recovery
            .intent_id
            .as_deref()
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| stable_problem("recovery-required"))?;
        let recovery_id = recovery_intent_id.ok_or_else(|| stable_problem("recovery-required"))?;
        let recovery = self
            .state
            .recovery_intent(recovery_id)
            .await
            .map_err(map_state_problem)?
            .ok_or_else(|| stable_problem("recovery-required"))?;
        if recovery.target() != target {
            return Err(stable_problem("recovery-required"));
        }

        let (adapter, compatibility, canonical_home, committed) = match target {
            Target::Codex => {
                let codec = CodexConfigCodec::for_user_home(&self.codex.user_home)
                    .map_err(|problem| stable_problem(problem.code()))?;
                let canonical_home = codec
                    .config_path()
                    .parent()
                    .expect("managed Codex configuration has a parent")
                    .to_path_buf();
                let compatibility = match self.codex.probe.probe(&self.codex.executable) {
                    Ok(capability) => ProbedCompatibility::from(capability),
                    Err(_) => ProbedCompatibility::new(
                        "unavailable".to_owned(),
                        CompatibilityClassification::Incompatible,
                    ),
                };
                (
                    TargetReconciliationAdapter::Codex(codec),
                    compatibility,
                    canonical_home,
                    CommittedConfiguration::Codex {
                        desired: recovery.desired().clone(),
                        recovery_before: recovery.before().clone(),
                    },
                )
            }
            Target::Claude => {
                let codec = ClaudeConfigCodec::for_user_home(&self.claude.user_home)
                    .map_err(|problem| stable_problem(problem.code()))?;
                let canonical_home = codec
                    .settings_path()
                    .parent()
                    .expect("managed Claude configuration has a parent")
                    .to_path_buf();
                let before = recovery
                    .claude_before()
                    .ok_or_else(|| stable_problem("recovery-required"))?;
                let desired = recovery
                    .claude_desired()
                    .ok_or_else(|| stable_problem("recovery-required"))?;
                let compatibility = match self.claude.probe.probe(&self.claude.executable) {
                    Ok(capability) => ProbedCompatibility::from(capability),
                    Err(_) => ProbedCompatibility::new(
                        "unavailable".to_owned(),
                        CompatibilityClassification::Incompatible,
                    ),
                };
                (
                    TargetReconciliationAdapter::Claude(codec),
                    compatibility,
                    canonical_home,
                    CommittedConfiguration::Claude {
                        desired: desired.clone(),
                        recovery_before: before.clone(),
                    },
                )
            }
        };
        let observed = adapter
            .observe(strategy, &committed, context, compatibility.clone())
            .map_err(|problem| stable_problem(problem.code()))?;
        let compatibility = self.compatibility_view(target, &compatibility).await?;
        let record = ObservationRecord {
            token: Uuid::new_v4(),
            target,
            strategy,
            management_revision: view.management_revision,
            compatibility,
            shadows: observed.observation.shadows.clone(),
            canonical_home,
            file_identity: observed.observation.file_identity.clone(),
            owned_fingerprint: observed.observation.owned_fingerprint.clone(),
            unrelated_fingerprint: observed.observation.unrelated_fingerprint.clone(),
            snapshot_id,
            recovery_intent_id,
            service_epoch: self.state.service_epoch(),
        };
        Ok((record, observed))
    }

    async fn compatibility_view(
        &self,
        target: Target,
        observed: &ProbedCompatibility,
    ) -> Result<CompatibilityView, ControlProblem> {
        let acknowledgement_required = if observed.classification()
            == CompatibilityClassification::UnknownCompatible
        {
            match self.state.compatibility_for(target).await {
                Ok(saved) => {
                    saved.version != observed.version()
                        || saved.classification != CompatibilityClassification::UnknownCompatible
                        || saved.acknowledgement_required
                }
                Err(StateError::MissingCompatibility) => true,
                Err(error) => return Err(map_state_problem(error)),
            }
        } else {
            false
        };
        Ok(CompatibilityView {
            version: observed.version().to_owned(),
            classification: observed.classification(),
            acknowledgement_required,
        })
    }

    pub(crate) async fn token_count(&self) -> usize {
        self.tokens.lock().await.len()
    }
}

impl ObservationRecord {
    #[allow(dead_code)]
    fn with_token(mut self, token: Uuid) -> Self {
        self.token = token;
        self
    }
}

fn find_executable(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join(name))
                .find(|candidate| candidate.is_file())
        })
        .and_then(|path| std::fs::canonicalize(path).ok())
        .unwrap_or_else(|| PathBuf::from(format!("/usr/bin/{name}")))
}

fn map_state_problem(_: StateError) -> ControlProblem {
    stable_problem("state-store-error")
}

#[allow(dead_code)]
fn stale_preview() -> ControlProblem {
    stable_problem("stale-reconciliation-preview")
}

fn stable_problem(code: &str) -> ControlProblem {
    let message = match code {
        "stale-reconciliation-preview" => "Target state changed; preview again",
        "recovery-required" => "Managed configuration requires recovery",
        "state-store-error" => "State store unavailable",
        "configuration-drift" => "Managed configuration drift must be reconciled",
        "shadowing-configuration" => "A higher-priority configuration source is active",
        "unsupported-configuration-home" => "Configuration Home is unsupported",
        "unsafe-managed-file" => "Managed configuration is unsafe",
        _ => "Reconciliation preview failed",
    };
    ControlProblem {
        code: code.to_owned(),
        message: message.to_owned(),
        source: None,
        selector: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::symlink,
        path::Path,
        sync::{Arc, Mutex as StdMutex},
    };

    use async_trait::async_trait;
    use axum::{body::Bytes, http::StatusCode};
    use futures_util::stream;
    use tempfile::TempDir;
    use tokio_rusqlite::rusqlite::Connection;

    use super::{ReconciliationService, ReconciliationStrategy, Target};
    use crate::{
        claude::{ClaudeCapability, ClaudeProbe, ClaudeProblem},
        codex::{CodexCapability, CodexProbe, CodexProblem},
        control::protocol::{CompatibilityClassification, ProviderAuthentication},
        home::MuxviaHome,
        model::{UpstreamError, UpstreamRequest, UpstreamResponse, UpstreamTransport},
        service::{activate::ActivationService, reconciliation_adapter::ReconciliationContext},
        state::StateStore,
    };
    use uuid::Uuid;

    #[derive(Clone)]
    enum ProbeState {
        Tested(String),
        Unknown(String),
        Incompatible,
    }

    struct MutableCodexProbe(StdMutex<ProbeState>);

    impl MutableCodexProbe {
        fn set(&self, state: ProbeState) {
            *self.0.lock().unwrap() = state;
        }
    }

    impl CodexProbe for MutableCodexProbe {
        fn probe(&self, _: &Path) -> Result<CodexCapability, CodexProblem> {
            match self.0.lock().unwrap().clone() {
                ProbeState::Tested(version) => Ok(CodexCapability::Tested { version }),
                ProbeState::Unknown(version) => Ok(CodexCapability::UnknownCompatible {
                    warning: "untested".into(),
                    version,
                }),
                ProbeState::Incompatible => Err(CodexProblem::new(
                    "incompatible-target-cli",
                    Some(Path::new("/usr/bin/codex")),
                )),
            }
        }
    }

    struct TestedClaude;

    impl ClaudeProbe for TestedClaude {
        fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
            Ok(ClaudeCapability::Tested {
                version: "claude-test".into(),
            })
        }
    }

    struct NoopUpstream;

    #[async_trait]
    impl UpstreamTransport for NoopUpstream {
        async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
            Ok(UpstreamResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: Box::pin(stream::once(async { Ok(Bytes::new()) })),
            })
        }
    }

    struct Fixture {
        _temp: TempDir,
        home: MuxviaHome,
        store: Arc<StateStore>,
        probe: Arc<MutableCodexProbe>,
        service: ReconciliationService,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp = TempDir::new().unwrap();
            let user_home = temp.path().join("home");
            fs::create_dir_all(&user_home).unwrap();
            let home = MuxviaHome::from_user_home(&user_home);
            let store = Arc::new(StateStore::open(&home).await.unwrap());
            let created = store
                .apply_provider_action_for(
                    Target::Codex,
                    Uuid::new_v4(),
                    0,
                    serde_json::json!({
                        "kind": "create-provider",
                        "name": "Codex",
                        "baseUrl": "https://api.openai.test/v1",
                        "model": "gpt-test",
                        "credential": { "kind": "replace", "value": "token-binding-secret" },
                        "authentication": ProviderAuthentication::OpenaiBearer,
                        "presetKey": null
                    }),
                )
                .await
                .unwrap();
            let provider_id = created.view.providers[0].id;
            let activation_probe = Arc::new(MutableCodexProbe(StdMutex::new(ProbeState::Tested(
                "codex-test".into(),
            ))));
            let activation = ActivationService::new(
                Arc::clone(&store),
                home.clone(),
                activation_probe,
                "/usr/bin/codex".into(),
                Arc::new(NoopUpstream),
            );
            activation
                .apply_raw_for(
                    Target::Codex,
                    Uuid::new_v4(),
                    1,
                    serde_json::json!({
                        "kind": "activate-provider",
                        "providerId": provider_id,
                        "mode": "direct"
                    }),
                )
                .await
                .unwrap();
            let probe = Arc::new(MutableCodexProbe(StdMutex::new(ProbeState::Tested(
                "codex-test".into(),
            ))));
            let service = ReconciliationService::with_runtimes(
                Arc::clone(&store),
                &home,
                probe.clone(),
                "/usr/bin/codex".into(),
                Arc::new(TestedClaude),
                "/usr/bin/claude".into(),
            )
            .unwrap();
            Self {
                _temp: temp,
                home,
                store,
                probe,
                service,
            }
        }

        async fn preview(&self) -> crate::control::protocol::ReconciliationPreview {
            self.service
                .preview(
                    Target::Codex,
                    ReconciliationStrategy::Reapply,
                    ReconciliationContext::Codex,
                )
                .await
                .unwrap()
        }

        async fn assert_stale(&self, token: Uuid) {
            let failure = self
                .service
                .validate_preview(
                    Target::Codex,
                    ReconciliationStrategy::Reapply,
                    token,
                    ReconciliationContext::Codex,
                )
                .await
                .unwrap_err();
            assert_eq!(failure.code, "stale-reconciliation-preview");
        }
    }

    #[tokio::test]
    async fn preview_projects_tested_unknown_acknowledged_and_incompatible_exactly() {
        let fixture = Fixture::new().await;
        let tested = fixture.preview().await;
        assert_eq!(tested.observation_token.get_version_num(), 4);
        assert_eq!(
            tested.compatibility.classification,
            CompatibilityClassification::Tested
        );
        assert_eq!(tested.compatibility.version, "codex-test");
        assert!(!tested.compatibility.acknowledgement_required);
        let record_debug = format!(
            "{:?}",
            fixture.service.tokens.lock().await.values().next().unwrap()
        );
        assert!(!record_debug.contains(fixture.home.user_home().to_str().unwrap()));
        assert!(!record_debug.contains("token-binding-secret"));

        fixture.probe.set(ProbeState::Unknown("codex-next".into()));
        let unknown = fixture.preview().await;
        assert_eq!(
            unknown.compatibility.classification,
            CompatibilityClassification::UnknownCompatible
        );
        assert_eq!(unknown.compatibility.version, "codex-next");
        assert!(unknown.compatibility.acknowledgement_required);
        fixture
            .store
            .record_compatibility(
                Target::Codex,
                "codex-next".into(),
                CompatibilityClassification::UnknownCompatible,
            )
            .await
            .unwrap();
        fixture
            .store
            .acknowledge_compatibility(Target::Codex, "codex-next")
            .await
            .unwrap();
        let acknowledged = fixture.preview().await;
        assert!(!acknowledged.compatibility.acknowledgement_required);

        fixture.probe.set(ProbeState::Incompatible);
        let incompatible = fixture.preview().await;
        assert_eq!(
            incompatible.compatibility.classification,
            CompatibilityClassification::Incompatible
        );
        assert_eq!(incompatible.compatibility.version, "unavailable");
        assert!(!incompatible.compatibility.acknowledgement_required);
        assert_eq!(fixture.service.token_count().await, 1);
    }

    #[tokio::test]
    async fn newer_preview_replaces_only_its_target_strategy_token() {
        let fixture = Fixture::new().await;
        let first = fixture.preview().await;
        let second = fixture.preview().await;
        assert_ne!(first.observation_token, second.observation_token);
        fixture.assert_stale(first.observation_token).await;
        fixture
            .service
            .validate_preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                second.observation_token,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        assert_eq!(fixture.service.token_count().await, 1);
    }

    #[tokio::test]
    async fn validation_reobserves_owned_unrelated_shadow_identity_version_and_acknowledgement() {
        let fixture = Fixture::new().await;
        let config = fixture.home.user_home().join(".codex/config.toml");

        let preview = fixture.preview().await;
        let original = fs::read_to_string(&config).unwrap();
        fs::write(&config, original.replace("gpt-test", "gpt-mutated")).unwrap();
        fixture.assert_stale(preview.observation_token).await;
        fs::write(&config, &original).unwrap();

        let preview = fixture.preview().await;
        fs::write(&config, format!("{original}\nunrelated = true\n")).unwrap();
        fixture.assert_stale(preview.observation_token).await;
        fs::write(&config, &original).unwrap();

        let preview = fixture.preview().await;
        fs::write(&config, format!("profile = \"shadow\"\n{original}")).unwrap();
        fixture.assert_stale(preview.observation_token).await;
        fs::write(&config, &original).unwrap();

        let preview = fixture.preview().await;
        let replacement = config.with_extension("replacement");
        fs::write(&replacement, &original).unwrap();
        fs::rename(&replacement, &config).unwrap();
        fixture.assert_stale(preview.observation_token).await;

        let preview = fixture.preview().await;
        fixture.probe.set(ProbeState::Unknown("codex-next".into()));
        fixture.assert_stale(preview.observation_token).await;

        let preview = fixture.preview().await;
        fixture
            .store
            .record_compatibility(
                Target::Codex,
                "codex-next".into(),
                CompatibilityClassification::UnknownCompatible,
            )
            .await
            .unwrap();
        fixture
            .store
            .acknowledge_compatibility(Target::Codex, "codex-next")
            .await
            .unwrap();
        fixture.assert_stale(preview.observation_token).await;
    }

    #[tokio::test]
    async fn validation_binds_target_strategy_revision_snapshot_recovery_home_and_epoch() {
        let mut fixture = Fixture::new().await;
        let preview = fixture.preview().await;
        let wrong_strategy = fixture
            .service
            .validate_preview(
                Target::Codex,
                ReconciliationStrategy::Adopt,
                preview.observation_token,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(wrong_strategy.code, "stale-reconciliation-preview");
        let wrong_target = fixture
            .service
            .validate_preview(
                Target::Claude,
                ReconciliationStrategy::Reapply,
                preview.observation_token,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(wrong_target.code, "stale-reconciliation-preview");

        let preview = fixture.preview().await;
        fixture
            .store
            .apply_provider_action_for(
                Target::Codex,
                Uuid::new_v4(),
                preview.management_revision,
                serde_json::json!({
                    "kind": "create-provider",
                    "name": "Revision mutation",
                    "baseUrl": "https://other.test/v1",
                    "model": "other",
                    "credential": { "kind": "replace", "value": "other-secret" },
                    "authentication": "openai-bearer",
                    "presetKey": null
                }),
            )
            .await
            .unwrap();
        fixture.assert_stale(preview.observation_token).await;

        let current = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let snapshot = current.activated_snapshot.unwrap().id;
        let recovery = current.recovery.intent_id.unwrap();
        let preview = fixture.preview().await;
        let connection = Connection::open(fixture.home.database_path()).unwrap();
        connection
            .execute(
                "UPDATE target_route_state SET activated_snapshot_id = ?1 WHERE target = 'codex'",
                [Uuid::new_v4().to_string()],
            )
            .unwrap();
        fixture.assert_stale(preview.observation_token).await;
        connection
            .execute(
                "UPDATE target_route_state SET activated_snapshot_id = ?1 WHERE target = 'codex'",
                [snapshot.to_string()],
            )
            .unwrap();

        let preview = fixture.preview().await;
        connection
            .execute(
                "UPDATE target_route_state SET recovery_intent_id = ?1 WHERE target = 'codex'",
                [Uuid::new_v4().to_string()],
            )
            .unwrap();
        fixture.assert_stale(preview.observation_token).await;
        connection
            .execute(
                "UPDATE target_route_state SET recovery_intent_id = ?1 WHERE target = 'codex'",
                [recovery],
            )
            .unwrap();
        drop(connection);

        let preview = fixture.preview().await;
        fixture.service.codex.user_home.push("changed");
        fixture.assert_stale(preview.observation_token).await;
        fixture.service.codex.user_home.pop();

        let preview = fixture.preview().await;
        let restarted_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
        let restarted = ReconciliationService::with_runtimes(
            restarted_store,
            &fixture.home,
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(TestedClaude),
            "/usr/bin/claude".into(),
        )
        .unwrap();
        let failure = restarted
            .validate_preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                preview.observation_token,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "stale-reconciliation-preview");
    }

    #[tokio::test]
    async fn validation_recanonicalizes_configuration_home_directory_symlinks() {
        let fixture = Fixture::new().await;
        let user_home = fixture.home.user_home();
        let configured = user_home.join(".codex");
        let first_home = user_home.join("codex-first");
        let second_home = user_home.join("codex-second");
        fs::rename(&configured, &first_home).unwrap();
        fs::create_dir(&second_home).unwrap();
        fs::copy(
            first_home.join("config.toml"),
            second_home.join("config.toml"),
        )
        .unwrap();
        symlink(&first_home, &configured).unwrap();
        let service = ReconciliationService::with_runtimes(
            Arc::clone(&fixture.store),
            &fixture.home,
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(TestedClaude),
            "/usr/bin/claude".into(),
        )
        .unwrap();
        let preview = service
            .preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        fs::remove_file(&configured).unwrap();
        symlink(&second_home, &configured).unwrap();

        let failure = service
            .validate_preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                preview.observation_token,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "stale-reconciliation-preview");
    }
}
