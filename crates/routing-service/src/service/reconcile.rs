use std::{
    collections::HashMap,
    fmt,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
};

use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    claude::{ClaudeConfigCodec, ClaudeProbe},
    codex::{CodexConfigCodec, CodexProbe, FileIdentity},
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
    configuration_home_override: Option<PathBuf>,
}

#[derive(Clone)]
pub(crate) struct ReconciliationRuntime {
    pub(crate) home: MuxviaHome,
    pub(crate) codex_probe: Arc<dyn CodexProbe>,
    pub(crate) claude_probe: Arc<dyn ClaudeProbe>,
    pub(crate) codex_executable: PathBuf,
    pub(crate) claude_executable: PathBuf,
    pub(crate) configuration_home_override: Option<PathBuf>,
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
    registration_locks: HashMap<ObservationKey, Arc<Mutex<()>>>,
    codex: CodexRuntimeContext,
    claude: ClaudeRuntimeContext,
}

pub(crate) struct PreviewRegistration {
    pub(crate) preview: ReconciliationPreview,
    key: ObservationKey,
    inserted_token: Uuid,
    previous: Option<ObservationRecord>,
    _guard: OwnedMutexGuard<()>,
}

impl ReconciliationService {
    pub(crate) fn from_runtime(state: Arc<StateStore>, runtime: ReconciliationRuntime) -> Self {
        let home = &runtime.home;
        let mut registration_locks = HashMap::new();
        for target in [Target::Codex, Target::Claude] {
            for strategy in [
                ReconciliationStrategy::Adopt,
                ReconciliationStrategy::Reapply,
                ReconciliationStrategy::Restore,
            ] {
                registration_locks.insert(ObservationKey(target, strategy), Arc::default());
            }
        }
        Self {
            state,
            tokens: Mutex::new(HashMap::new()),
            registration_locks,
            codex: CodexRuntimeContext {
                probe: runtime.codex_probe,
                executable: runtime.codex_executable,
                user_home: home.user_home().to_path_buf(),
                configuration_home_override: runtime.configuration_home_override,
            },
            claude: ClaudeRuntimeContext {
                probe: runtime.claude_probe,
                executable: runtime.claude_executable,
                user_home: home.user_home().to_path_buf(),
            },
        }
    }

    #[cfg(test)]
    pub(crate) async fn preview(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        context: ReconciliationContext,
    ) -> Result<ReconciliationPreview, ControlProblem> {
        self.preview_cancellable(target, strategy, context, CancellationToken::new())
            .await
    }

    #[cfg(test)]
    pub(crate) async fn preview_cancellable(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        context: ReconciliationContext,
        cancellation: CancellationToken,
    ) -> Result<ReconciliationPreview, ControlProblem> {
        Ok(self
            .preview_registered_cancellable(target, strategy, context, cancellation)
            .await?
            .preview)
    }

    pub(crate) async fn preview_registered_cancellable(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        context: ReconciliationContext,
        cancellation: CancellationToken,
    ) -> Result<PreviewRegistration, ControlProblem> {
        let (record, observed) = self
            .observe_cancellable(target, strategy, &context, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            return Err(stable_problem("inspection-cancelled"));
        }
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
        let key = ObservationKey(target, strategy);
        let registration_lock = Arc::clone(
            self.registration_locks
                .get(&key)
                .expect("every closed reconciliation key has a registration lock"),
        );
        let guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(stable_problem("inspection-cancelled"));
            }
            guard = registration_lock.lock_owned() => guard,
        };
        if cancellation.is_cancelled() {
            return Err(stable_problem("inspection-cancelled"));
        }
        let inserted_token = record.token;
        let previous = self.tokens.lock().await.insert(key, record);
        Ok(PreviewRegistration {
            preview,
            key,
            inserted_token,
            previous,
            _guard: guard,
        })
    }

    pub(crate) async fn rollback_preview(&self, registration: PreviewRegistration) {
        let mut tokens = self.tokens.lock().await;
        if tokens
            .get(&registration.key)
            .is_none_or(|record| record.token != registration.inserted_token)
        {
            return;
        }
        match registration.previous {
            Some(previous) => {
                tokens.insert(registration.key, previous);
            }
            None => {
                tokens.remove(&registration.key);
            }
        }
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
        self.observe_cancellable(target, strategy, context, CancellationToken::new())
            .await
    }

    async fn observe_cancellable(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        context: &ReconciliationContext,
        cancellation: CancellationToken,
    ) -> Result<(ObservationRecord, ObservedReconciliation), ControlProblem> {
        if cancellation.is_cancelled() {
            return Err(stable_problem("inspection-cancelled"));
        }
        if target == Target::Codex
            && self
                .codex
                .configuration_home_override
                .as_ref()
                .is_some_and(|configured| configured != &self.codex.user_home.join(".codex"))
        {
            return Err(stable_problem("unsupported-configuration-home"));
        }
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
                let compatibility = match self
                    .codex
                    .probe
                    .probe_cancellable(&self.codex.executable, cancellation.clone())
                    .await
                {
                    Ok(capability) => ProbedCompatibility::from(capability),
                    Err(problem) => ProbedCompatibility::new(
                        problem.version().unwrap_or("unavailable").to_owned(),
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
                let compatibility = match self
                    .claude
                    .probe
                    .probe_cancellable(&self.claude.executable, cancellation.clone())
                    .await
                {
                    Ok(capability) => ProbedCompatibility::from(capability),
                    Err(problem) => ProbedCompatibility::new(
                        problem.version().unwrap_or("unavailable").to_owned(),
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
        if cancellation.is_cancelled() {
            return Err(stable_problem("inspection-cancelled"));
        }
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

    pub(crate) async fn tracks_token(
        &self,
        target: Target,
        strategy: ReconciliationStrategy,
        token: Uuid,
    ) -> bool {
        self.tokens
            .lock()
            .await
            .get(&ObservationKey(target, strategy))
            .is_some_and(|record| record.token == token)
    }
}

impl ObservationRecord {
    #[allow(dead_code)]
    fn with_token(mut self, token: Uuid) -> Self {
        self.token = token;
        self
    }
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
        future::Future,
        os::unix::fs::symlink,
        path::Path,
        pin::Pin,
        sync::{Arc, Mutex as StdMutex},
    };

    use async_trait::async_trait;
    use axum::{body::Bytes, http::StatusCode};
    use futures_util::stream;
    use tempfile::TempDir;
    use tokio_rusqlite::rusqlite::Connection;
    use tokio_util::sync::CancellationToken;

    use super::{ReconciliationRuntime, ReconciliationService, ReconciliationStrategy, Target};
    use crate::{
        claude::{ClaudeCapability, ClaudeProbe, ClaudeProblem},
        codex::{CodexCapability, CodexProbe, CodexProblem},
        control::protocol::{
            ClaudeHostManagedState, ClaudePreflightContext, ClaudeSelectorState,
            CompatibilityClassification, ProviderAuthentication,
        },
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

        fn probe_cancellable<'a>(
            &'a self,
            executable: &'a Path,
            cancellation: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<CodexCapability, CodexProblem>> + Send + 'a>>
        {
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(CodexProblem::new("probe-cancelled", Some(executable))),
                    result = async { self.probe(executable) } => result,
                }
            })
        }
    }

    struct TestedClaude;

    impl ClaudeProbe for TestedClaude {
        fn probe(&self, _: &Path) -> Result<ClaudeCapability, ClaudeProblem> {
            Ok(ClaudeCapability::Tested {
                version: "claude-test".into(),
            })
        }

        fn probe_cancellable<'a>(
            &'a self,
            executable: &'a Path,
            cancellation: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<ClaudeCapability, ClaudeProblem>> + Send + 'a>>
        {
            Box::pin(async move {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(ClaudeProblem::new("probe-cancelled", Some(executable))),
                    result = async { self.probe(executable) } => result,
                }
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

    fn reconciliation_service(
        store: Arc<StateStore>,
        home: &MuxviaHome,
        probe: Arc<MutableCodexProbe>,
    ) -> ReconciliationService {
        ReconciliationService::from_runtime(
            store,
            ReconciliationRuntime {
                home: home.clone(),
                codex_probe: probe,
                claude_probe: Arc::new(TestedClaude),
                codex_executable: "/usr/bin/codex".into(),
                claude_executable: "/usr/bin/claude".into(),
                configuration_home_override: None,
            },
        )
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
            let service = reconciliation_service(Arc::clone(&store), &home, probe.clone());
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

        async fn preview_record(&self) -> (Uuid, super::ObservationRecord) {
            let preview = self.preview().await;
            let record = self
                .service
                .tokens
                .lock()
                .await
                .get(&super::ObservationKey(
                    Target::Codex,
                    ReconciliationStrategy::Reapply,
                ))
                .unwrap()
                .clone();
            (preview.observation_token, record)
        }

        async fn reobserved_record(&self, token: Uuid) -> super::ObservationRecord {
            self.service
                .observe(
                    Target::Codex,
                    ReconciliationStrategy::Reapply,
                    &ReconciliationContext::Codex,
                )
                .await
                .unwrap()
                .0
                .with_token(token)
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

    fn rewrite_same_file_preserving_identity(path: &Path, before: &str, after: &str) {
        assert_eq!(before.len(), after.len());
        let metadata = fs::metadata(path).unwrap();
        let modified = metadata.modified().unwrap();
        let permissions = metadata.permissions();
        fs::write(path, after).unwrap();
        fs::set_permissions(path, permissions).unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
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
    async fn cancelled_registration_restores_only_the_token_it_replaced() {
        let fixture = Fixture::new().await;
        let first = fixture.preview().await;
        let cancelled = fixture
            .service
            .preview_registered_cancellable(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                ReconciliationContext::Codex,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        fixture.assert_stale(first.observation_token).await;
        fixture.service.rollback_preview(cancelled).await;
        fixture
            .service
            .validate_preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                first.observation_token,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();

        let cancelled_b = fixture
            .service
            .preview_registered_cancellable(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                ReconciliationContext::Codex,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let mut nested = Box::pin(fixture.service.preview_registered_cancellable(
            Target::Codex,
            ReconciliationStrategy::Reapply,
            ReconciliationContext::Codex,
            cancellation.clone(),
        ));
        let blocked = tokio::time::timeout(std::time::Duration::from_millis(50), &mut nested).await;
        assert!(blocked.is_err(), "same-key registration was not serialized");
        cancellation.cancel();
        let cancelled_wait = tokio::time::timeout(std::time::Duration::from_millis(50), nested)
            .await
            .expect("cancelled registration lock wait did not complete");
        let failure = match cancelled_wait {
            Ok(_) => panic!("cancelled registration lock wait succeeded"),
            Err(failure) => failure,
        };
        assert_eq!(failure.code, "inspection-cancelled");
        fixture.service.rollback_preview(cancelled_b).await;
        let cancelled_c = fixture
            .service
            .preview_registered_cancellable(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                ReconciliationContext::Codex,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        fixture.service.rollback_preview(cancelled_c).await;
        fixture
            .service
            .validate_preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                first.observation_token,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn preview_rejects_the_same_nondefault_codex_home_as_activation() {
        let fixture = Fixture::new().await;
        let service = ReconciliationService::from_runtime(
            Arc::clone(&fixture.store),
            ReconciliationRuntime {
                home: fixture.home.clone(),
                codex_probe: fixture.probe.clone(),
                claude_probe: Arc::new(TestedClaude),
                codex_executable: "/usr/bin/codex".into(),
                claude_executable: "/usr/bin/claude".into(),
                configuration_home_override: Some(
                    fixture.home.user_home().join("nondefault-codex-home"),
                ),
            },
        );
        let failure = service
            .preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "unsupported-configuration-home");
        assert_eq!(service.token_count().await, 0);
    }

    #[tokio::test]
    async fn validation_binds_target_and_strategy_lookup() {
        let fixture = Fixture::new().await;
        let preview = fixture.preview().await;
        for (target, strategy) in [
            (Target::Codex, ReconciliationStrategy::Adopt),
            (Target::Claude, ReconciliationStrategy::Reapply),
        ] {
            let failure = fixture
                .service
                .validate_preview(
                    target,
                    strategy,
                    preview.observation_token,
                    ReconciliationContext::Codex,
                )
                .await
                .unwrap_err();
            assert_eq!(failure.code, "stale-reconciliation-preview");
        }
    }

    #[tokio::test]
    async fn validation_binds_management_revision_only() {
        let fixture = Fixture::new().await;
        let (token, before) = fixture.preview_record().await;
        fixture
            .store
            .apply_provider_action_for(
                Target::Codex,
                Uuid::new_v4(),
                before.management_revision,
                serde_json::json!({
                    "kind": "create-provider", "name": "Revision mutation",
                    "baseUrl": "https://other.test/v1", "model": "other",
                    "credential": { "kind": "replace", "value": "other-secret" },
                    "authentication": "openai-bearer", "presetKey": null
                }),
            )
            .await
            .unwrap();
        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected.management_revision = after.management_revision;
        assert_ne!(before.management_revision, after.management_revision);
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_exact_version_without_classification_change() {
        let fixture = Fixture::new().await;
        fixture.probe.set(ProbeState::Unknown("codex-a".into()));
        let (token, before) = fixture.preview_record().await;
        fixture.probe.set(ProbeState::Unknown("codex-b".into()));
        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected
            .compatibility
            .version
            .clone_from(&after.compatibility.version);
        assert_eq!(
            before.compatibility.classification,
            after.compatibility.classification
        );
        assert_ne!(before.compatibility.version, after.compatibility.version);
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_acknowledgement_requirement_only() {
        let fixture = Fixture::new().await;
        fixture.probe.set(ProbeState::Unknown("codex-next".into()));
        let (token, before) = fixture.preview_record().await;
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
        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected.compatibility.acknowledgement_required =
            after.compatibility.acknowledgement_required;
        assert!(before.compatibility.acknowledgement_required);
        assert!(!after.compatibility.acknowledgement_required);
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_classification_only() {
        let fixture = Fixture::new().await;
        fixture
            .probe
            .set(ProbeState::Unknown("same-version".into()));
        fixture
            .store
            .record_compatibility(
                Target::Codex,
                "same-version".into(),
                CompatibilityClassification::UnknownCompatible,
            )
            .await
            .unwrap();
        fixture
            .store
            .acknowledge_compatibility(Target::Codex, "same-version")
            .await
            .unwrap();
        let (token, before) = fixture.preview_record().await;
        fixture.probe.set(ProbeState::Tested("same-version".into()));
        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected.compatibility.classification = after.compatibility.classification;
        assert_eq!(before.compatibility.version, after.compatibility.version);
        assert!(!before.compatibility.acknowledgement_required);
        assert!(!after.compatibility.acknowledgement_required);
        assert_ne!(
            before.compatibility.classification,
            after.compatibility.classification
        );
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_owned_fingerprint_only() {
        let fixture = Fixture::new().await;
        let config = fixture.home.user_home().join(".codex/config.toml");
        let original = fs::read_to_string(&config).unwrap();
        let (token, before) = fixture.preview_record().await;
        rewrite_same_file_preserving_identity(
            &config,
            &original,
            &original.replace("gpt-test", "gpt-next"),
        );
        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected
            .owned_fingerprint
            .clone_from(&after.owned_fingerprint);
        assert_ne!(before.owned_fingerprint, after.owned_fingerprint);
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_unrelated_fingerprint_only() {
        let fixture = Fixture::new().await;
        let config = fixture.home.user_home().join(".codex/config.toml");
        let original = format!(
            "{}\nunrelated = \"AAAA\"\n",
            fs::read_to_string(&config).unwrap()
        );
        fs::write(&config, &original).unwrap();
        let (token, before) = fixture.preview_record().await;
        rewrite_same_file_preserving_identity(
            &config,
            &original,
            &original.replace("AAAA", "BBBB"),
        );
        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected
            .unrelated_fingerprint
            .clone_from(&after.unrelated_fingerprint);
        assert_ne!(before.unrelated_fingerprint, after.unrelated_fingerprint);
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_file_identity_only() {
        let fixture = Fixture::new().await;
        let config = fixture.home.user_home().join(".codex/config.toml");
        let bytes = fs::read(&config).unwrap();
        let metadata = fs::metadata(&config).unwrap();
        let (token, before) = fixture.preview_record().await;
        let replacement = config.with_extension("replacement");
        fs::write(&replacement, bytes).unwrap();
        fs::set_permissions(&replacement, metadata.permissions()).unwrap();
        fs::File::options()
            .write(true)
            .open(&replacement)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(metadata.modified().unwrap()))
            .unwrap();
        fs::rename(replacement, &config).unwrap();
        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected.file_identity.clone_from(&after.file_identity);
        assert_ne!(before.file_identity, after.file_identity);
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_shadow_sources_only() {
        let fixture = Fixture::new().await;
        let created = fixture
            .store
            .apply_provider_action_for(
                Target::Claude,
                Uuid::new_v4(),
                0,
                serde_json::json!({
                    "kind": "create-provider", "name": "Claude",
                    "baseUrl": "https://api.anthropic.test", "model": "claude-test",
                    "credential": { "kind": "replace", "value": "claude-secret" },
                    "authentication": "anthropic-api-key", "presetKey": null
                }),
            )
            .await
            .unwrap();
        let provider_id = created.view.providers[0].id;
        let project = fixture.home.user_home().join("project");
        fs::create_dir(&project).unwrap();
        let context = ClaudePreflightContext {
            claude_config_dir: None,
            selector_state: ClaudeSelectorState::Unset,
            blocking_selector: None,
            host_managed_state: ClaudeHostManagedState::Unmanaged,
            cwd: project.to_string_lossy().into_owned(),
        };
        let activation = ActivationService::new(
            Arc::clone(&fixture.store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        )
        .with_claude_runtime(Arc::new(TestedClaude), "/usr/bin/claude".into());
        activation
            .apply_raw_for_with_context(
                Target::Claude,
                Uuid::new_v4(),
                1,
                serde_json::json!({
                    "kind": "activate-provider",
                    "providerId": provider_id,
                    "mode": "direct"
                }),
                Some(&context),
            )
            .await
            .unwrap();
        let service = ReconciliationService::from_runtime(
            Arc::clone(&fixture.store),
            activation.reconciliation_runtime(),
        );
        let preview = service
            .preview(
                Target::Claude,
                ReconciliationStrategy::Reapply,
                ReconciliationContext::Claude(context.clone()),
            )
            .await
            .unwrap();
        let before = service
            .tokens
            .lock()
            .await
            .get(&super::ObservationKey(
                Target::Claude,
                ReconciliationStrategy::Reapply,
            ))
            .unwrap()
            .clone();
        fs::create_dir(project.join(".claude")).unwrap();
        fs::write(
            project.join(".claude/settings.json"),
            r#"{"env":{"ANTHROPIC_MODEL":"shadow-model"}}"#,
        )
        .unwrap();
        let after = service
            .observe(
                Target::Claude,
                ReconciliationStrategy::Reapply,
                &ReconciliationContext::Claude(context.clone()),
            )
            .await
            .unwrap()
            .0
            .with_token(preview.observation_token);
        let mut expected = before.clone();
        expected.shadows.clone_from(&after.shadows);
        assert_eq!(
            before.shadows,
            Vec::<crate::control::protocol::ShadowSource>::new()
        );
        assert_eq!(
            after.shadows,
            vec![crate::control::protocol::ShadowSource::ClaudeShared]
        );
        assert_eq!(expected, after);
        let failure = service
            .validate_preview(
                Target::Claude,
                ReconciliationStrategy::Reapply,
                preview.observation_token,
                ReconciliationContext::Claude(context),
            )
            .await
            .unwrap_err();
        assert_eq!(failure.code, "stale-reconciliation-preview");
    }

    #[tokio::test]
    async fn validation_binds_canonical_home_only() {
        let fixture = Fixture::new().await;
        let user_home = fixture.home.user_home();
        let configured = user_home.join(".codex");
        let first_home = user_home.join("codex-first");
        let second_home = user_home.join("codex-second");
        fs::rename(&configured, &first_home).unwrap();
        fs::create_dir(&second_home).unwrap();
        fs::hard_link(
            first_home.join("config.toml"),
            second_home.join("config.toml"),
        )
        .unwrap();
        symlink(&first_home, &configured).unwrap();
        let service = reconciliation_service(
            Arc::clone(&fixture.store),
            &fixture.home,
            fixture.probe.clone(),
        );
        let preview = service
            .preview(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        let before = service
            .tokens
            .lock()
            .await
            .get(&super::ObservationKey(
                Target::Codex,
                ReconciliationStrategy::Reapply,
            ))
            .unwrap()
            .clone();
        fs::remove_file(&configured).unwrap();
        symlink(&second_home, &configured).unwrap();
        let after = service
            .observe(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                &ReconciliationContext::Codex,
            )
            .await
            .unwrap()
            .0
            .with_token(preview.observation_token);
        let mut expected = before.clone();
        expected.canonical_home.clone_from(&after.canonical_home);
        assert_ne!(before.canonical_home, after.canonical_home);
        assert_eq!(expected, after);
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

    #[tokio::test]
    async fn validation_binds_exact_existing_alternate_snapshot_only() {
        let fixture = Fixture::new().await;
        let (token, before) = fixture.preview_record().await;
        let original_id = before.snapshot_id.unwrap();
        let alternate_id = Uuid::new_v4();
        let connection = Connection::open(fixture.home.database_path()).unwrap();
        connection
            .execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, protocol, authentication,
                  provider_bearer_token, epoch)
                 SELECT ?1, target, provider_id, base_url, model, protocol, authentication,
                        provider_bearer_token, epoch
                 FROM activated_snapshots WHERE id = ?2",
                [alternate_id.to_string(), original_id.to_string()],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE target_route_state SET activated_snapshot_id = ?1
                 WHERE target = 'codex'",
                [alternate_id.to_string()],
            )
            .unwrap();
        drop(connection);

        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected.snapshot_id = after.snapshot_id;
        assert_eq!(after.snapshot_id, Some(alternate_id));
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_exact_existing_alternate_recovery_intent_only() {
        let fixture = Fixture::new().await;
        let (token, before) = fixture.preview_record().await;
        let original_id = before.recovery_intent_id.unwrap();
        let alternate_id = Uuid::new_v4();
        let alternate_action_id = Uuid::new_v4();
        let connection = Connection::open(fixture.home.database_path()).unwrap();
        connection
            .execute(
                "INSERT INTO activation_recovery
                 (id, target, action_id, config_path, file_identity_json, payload_json,
                  state, created_revision)
                 SELECT ?1, target, ?2, config_path, file_identity_json, payload_json,
                        state, created_revision
                 FROM activation_recovery WHERE id = ?3",
                [
                    alternate_id.to_string(),
                    alternate_action_id.to_string(),
                    original_id.to_string(),
                ],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE target_route_state SET recovery_intent_id = ?1
                 WHERE target = 'codex'",
                [alternate_id.to_string()],
            )
            .unwrap();
        drop(connection);

        let after = fixture.reobserved_record(token).await;
        let mut expected = before.clone();
        expected.recovery_intent_id = after.recovery_intent_id;
        assert_eq!(after.recovery_intent_id, Some(alternate_id));
        assert_eq!(expected, after);
        fixture.assert_stale(token).await;
    }

    #[tokio::test]
    async fn validation_binds_service_epoch_with_nonempty_restarted_registry_only() {
        let fixture = Fixture::new().await;
        let (token, before) = fixture.preview_record().await;
        let restarted_store = Arc::new(StateStore::open(&fixture.home).await.unwrap());
        let restarted =
            reconciliation_service(restarted_store, &fixture.home, fixture.probe.clone());
        restarted.tokens.lock().await.insert(
            super::ObservationKey(Target::Codex, ReconciliationStrategy::Reapply),
            before.clone(),
        );
        assert_eq!(restarted.token_count().await, 1);
        let after = restarted
            .observe(
                Target::Codex,
                ReconciliationStrategy::Reapply,
                &ReconciliationContext::Codex,
            )
            .await
            .unwrap()
            .0
            .with_token(token);
        let mut expected = before.clone();
        expected.service_epoch = after.service_epoch;
        assert_ne!(before.service_epoch, after.service_epoch);
        assert_eq!(expected, after);
        let failure = restarted
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
