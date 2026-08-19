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
        ActionOutcome, ActionStatus, CompatibilityClassification, CompatibilityProbe,
        CompatibilityView, ControlProblem, ProviderEffect, ReconciliationField,
        ReconciliationFieldState, ReconciliationPreview, ReconciliationStrategy, ShadowSource,
        Target,
    },
    domain::activation::ActivatedSnapshot,
    home::MuxviaHome,
    model::{ModelServerError, ModelServerHandle, auth::routing_credential_value_matches},
    state::{
        ActionFailure, AdoptReconciliation, ManagedWriteStatus, ReconciliationCommit,
        ReconciliationCommitFailpoint, ReconciliationCommitInput, StateError, StateStore,
    },
};

use super::reconciliation_adapter::{
    CommittedConfiguration, ObservedReconciliation, PreparedConfiguration, ProbedCompatibility,
    ReconciliationContext, TargetReconciliationAdapter, recover_pending_material,
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
    pub(crate) target_runtime: ReconciliationTargetRuntime,
}

#[derive(Clone)]
pub(crate) struct ReconciliationTargetRuntime {
    codex: Arc<Mutex<Option<ModelServerHandle>>>,
    claude: Arc<Mutex<Option<ModelServerHandle>>>,
    codex_gate: Arc<Mutex<()>>,
    claude_gate: Arc<Mutex<()>>,
}

impl ReconciliationTargetRuntime {
    pub(crate) fn new(
        codex: Arc<Mutex<Option<ModelServerHandle>>>,
        claude: Arc<Mutex<Option<ModelServerHandle>>>,
        codex_gate: Arc<Mutex<()>>,
        claude_gate: Arc<Mutex<()>>,
    ) -> Self {
        Self {
            codex,
            claude,
            codex_gate,
            claude_gate,
        }
    }

    #[cfg(test)]
    fn empty() -> Self {
        Self::new(
            Arc::default(),
            Arc::default(),
            Arc::default(),
            Arc::default(),
        )
    }

    fn slot(&self, target: Target) -> &Mutex<Option<ModelServerHandle>> {
        match target {
            Target::Codex => &self.codex,
            Target::Claude => &self.claude,
        }
    }

    fn gate(&self, target: Target) -> &Mutex<()> {
        match target {
            Target::Codex => &self.codex_gate,
            Target::Claude => &self.claude_gate,
        }
    }

    fn gate_arc(&self, target: Target) -> Arc<Mutex<()>> {
        match target {
            Target::Codex => Arc::clone(&self.codex_gate),
            Target::Claude => Arc::clone(&self.claude_gate),
        }
    }

    #[cfg(test)]
    async fn active_request_count(&self, target: Target) -> usize {
        self.slot(target)
            .lock()
            .await
            .as_ref()
            .map_or(0, ModelServerHandle::active_request_count)
    }

    #[cfg(test)]
    async fn set_reservation_attempt_hook(
        &self,
        target: Target,
        hook: Arc<dyn Fn() + Send + Sync>,
    ) {
        if let Some(handle) = self.slot(target).lock().await.as_mut() {
            handle.set_reservation_attempt_hook(hook);
        }
    }

    pub(crate) async fn reserve_if_idle(
        &self,
        target: Target,
    ) -> Result<Option<ModelServerHandle>, ModelServerError> {
        let mut slot = self.slot(target).lock().await;
        if let Some(handle) = slot.as_ref()
            && !handle.try_reserve_idle()
        {
            return Err(ModelServerError::Task);
        }
        Ok(slot.take())
    }

    pub(crate) async fn restore_reserved(&self, target: Target, handle: Option<ModelServerHandle>) {
        if let Some(handle) = handle {
            handle.release_idle_reservation();
            *self.slot(target).lock().await = Some(handle);
        }
    }

    pub(crate) async fn shutdown_reserved(
        &self,
        handle: Option<ModelServerHandle>,
    ) -> Result<(), ModelServerError> {
        match handle {
            Some(handle) => handle.shutdown().await,
            None => Ok(()),
        }
    }
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
    compatibility: CompatibilityView,
    management_revision: u64,
    managed_config_path: PathBuf,
}

pub(crate) struct ReconciliationService {
    state: Arc<StateStore>,
    tokens: Mutex<HashMap<ObservationKey, ObservationRecord>>,
    registration_locks: HashMap<ObservationKey, Arc<Mutex<()>>>,
    codex: CodexRuntimeContext,
    claude: ClaudeRuntimeContext,
    target_runtime: ReconciliationTargetRuntime,
    hooks: ReconciliationHooks,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ReconciliationFailpoint {
    #[default]
    None,
    AfterIntent,
    AtomicWrite,
    Verify,
    ListenerStop,
    CredentialInsert,
    ProviderInsert,
    SnapshotInsert,
    FinalRevision,
    FinalTransaction,
    RollbackVerify,
}

#[derive(Clone, Default)]
pub(crate) struct ReconciliationHooks {
    pub(crate) failpoint: ReconciliationFailpoint,
    #[cfg(test)]
    pause_after_verify: Option<Arc<ReconciliationPause>>,
    #[cfg(test)]
    pause_after_reserve: Option<Arc<ReconciliationPause>>,
    #[cfg(test)]
    pause_before_reserve: Option<Arc<ReconciliationPause>>,
}

#[cfg(test)]
#[derive(Default)]
struct ReconciliationPause {
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

pub(crate) struct PreviewRegistration {
    pub(crate) preview: ReconciliationPreview,
    key: ObservationKey,
    inserted_token: Uuid,
    previous: Option<ObservationRecord>,
    _guard: OwnedMutexGuard<()>,
}

pub(crate) struct DeferredPublication<T> {
    pub(crate) result: T,
    pub(crate) publication: Option<crate::control::protocol::TargetView>,
}

impl<T> DeferredPublication<T> {
    fn none(result: T) -> Self {
        Self {
            result,
            publication: None,
        }
    }
}

#[cfg(test)]
impl<T, E> DeferredPublication<Result<T, E>> {
    fn unwrap(self) -> T
    where
        E: std::fmt::Debug,
    {
        self.result.unwrap()
    }

    fn unwrap_err(self) -> E
    where
        T: std::fmt::Debug,
    {
        self.result.unwrap_err()
    }
}

impl ReconciliationService {
    pub(crate) async fn recover_pending_intents(&self) -> Result<(), StateError> {
        for intent in self.state.pending_reconciliation_intents().await? {
            let _strategy = intent.strategy;
            if recover_pending_material(
                intent.target,
                &self.codex.user_home,
                &intent.before_json,
                &intent.desired_json,
            )
            .is_ok()
            {
                self.state
                    .set_reconciliation_intent_state(intent.target, intent.action_id, "rolled-back")
                    .await?;
            } else {
                self.state
                    .mark_reconciliation_recovery_required(intent.target, intent.action_id)
                    .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn lock_target_mutation(&self, target: Target) -> OwnedMutexGuard<()> {
        self.target_runtime.gate_arc(target).lock_owned().await
    }
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
            target_runtime: runtime.target_runtime,
            hooks: ReconciliationHooks::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_hooks(mut self, hooks: ReconciliationHooks) -> Self {
        self.hooks = hooks;
        self
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

    pub(crate) async fn probe_compatibility_cancellable(
        &self,
        target: Target,
        cancellation: CancellationToken,
    ) -> Result<CompatibilityProbe, ControlProblem> {
        let view = self
            .state
            .target_view_for(target)
            .await
            .map_err(map_state_problem)?;
        let compatibility = self
            .probe_target_compatibility_cancellable(target, cancellation)
            .await?;
        Ok(CompatibilityProbe {
            target,
            management_revision: view.management_revision,
            compatibility,
        })
    }

    pub(crate) async fn resolve_compatibility(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        version: String,
    ) -> DeferredPublication<Result<ActionOutcome, ActionFailure>> {
        match self.state.receipt_for(target, action_id).await {
            Ok(Some(outcome)) => return DeferredPublication::none(Ok(outcome)),
            Ok(None) => {}
            Err(_) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "state-store-error")
                    .await));
            }
        }
        let _gate = self.target_runtime.gate(target).lock().await;
        match self.state.receipt_for(target, action_id).await {
            Ok(Some(outcome)) => return DeferredPublication::none(Ok(outcome)),
            Ok(None) => {}
            Err(_) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "state-store-error")
                    .await));
            }
        }
        let current_view = match self.state.target_view_for(target).await {
            Ok(view) => view,
            Err(_) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "state-store-error")
                    .await));
            }
        };
        if current_view.management_revision != expected_revision {
            return DeferredPublication::none(Err(ActionFailure {
                problem: stable_problem("stale-revision"),
                authoritative_view: current_view,
            }));
        }
        let compatibility = match self.probe_compatibility(target).await {
            Ok(compatibility) => compatibility,
            Err(problem) => {
                let mut failure = self.failure(target, &problem.code).await;
                failure.problem = problem;
                return DeferredPublication::none(Err(failure));
            }
        };
        if compatibility.classification == CompatibilityClassification::Incompatible {
            let blocked = self
                .project_ordinary_problem(target, "incompatible-target-cli", None, compatibility)
                .await;
            return DeferredPublication {
                result: match blocked.result {
                    Ok(()) => Err(self.failure(target, "incompatible-target-cli").await),
                    Err(failure) => Err(failure),
                },
                publication: blocked.publication,
            };
        }
        if compatibility.version != version {
            return DeferredPublication::none(Err(self
                .failure(target, "stale-compatibility-probe")
                .await));
        }
        match self
            .state
            .apply_compatibility_resolution(
                target,
                action_id,
                expected_revision,
                version,
                compatibility,
            )
            .await
        {
            Ok(result) => {
                let publication = result.as_ref().ok().and_then(|outcome| {
                    (outcome.status == ActionStatus::Applied).then(|| outcome.view.clone())
                });
                DeferredPublication {
                    result,
                    publication,
                }
            }
            Err(_) => {
                DeferredPublication::none(Err(self.failure(target, "state-store-error").await))
            }
        }
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
            compatibility: expected.compatibility,
            management_revision: expected.management_revision,
            managed_config_path: expected.canonical_home.join(match target {
                Target::Codex => "config.toml",
                Target::Claude => "settings.json",
            }),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn apply(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        strategy: ReconciliationStrategy,
        observation_token: Uuid,
        acknowledge_version: Option<String>,
        context: ReconciliationContext,
    ) -> DeferredPublication<Result<ActionOutcome, ActionFailure>> {
        let result = self
            .apply_result(
                target,
                action_id,
                expected_revision,
                strategy,
                observation_token,
                acknowledge_version,
                context,
            )
            .await;
        let publication = result.as_ref().ok().and_then(|outcome| {
            (outcome.status == ActionStatus::Applied).then(|| outcome.view.clone())
        });
        DeferredPublication {
            result,
            publication,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_result(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        strategy: ReconciliationStrategy,
        observation_token: Uuid,
        acknowledge_version: Option<String>,
        context: ReconciliationContext,
    ) -> Result<ActionOutcome, ActionFailure> {
        match self.state.receipt_for(target, action_id).await {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) => return Err(self.failure(target, "state-store-error").await),
        }
        let _gate = self.target_runtime.gate(target).lock().await;
        let key = ObservationKey(target, strategy);
        let registration_lock = Arc::clone(
            self.registration_locks
                .get(&key)
                .expect("every closed reconciliation key has a registration lock"),
        );
        let _registration_guard = registration_lock.lock_owned().await;
        match self.state.receipt_for(target, action_id).await {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) => return Err(self.failure(target, "state-store-error").await),
        }
        match self.state.managed_write_status_for(target).await {
            Ok(ManagedWriteStatus::RecoveryRequired) => {
                return Err(self.failure(target, "recovery-required").await);
            }
            Ok(ManagedWriteStatus::Allowed | ManagedWriteStatus::ConfigurationDrift) => {}
            Err(_) => return Err(self.failure(target, "state-store-error").await),
        }
        let validated = match self
            .validate_preview(target, strategy, observation_token, context)
            .await
        {
            Ok(validated) => validated,
            Err(problem) => {
                let mut failure = self.failure(target, &problem.code).await;
                failure.problem = problem;
                return Err(failure);
            }
        };
        if validated.management_revision != expected_revision {
            return Err(self.failure(target, "stale-revision").await);
        }
        let current_view = match self.state.target_view_for(target).await {
            Ok(view) => view,
            Err(_) => return Err(self.failure(target, "state-store-error").await),
        };
        let adopt_takeover =
            strategy == ReconciliationStrategy::Adopt && current_view.takeover.state == "active";
        let mut compatibility = validated.compatibility;
        match compatibility.classification {
            CompatibilityClassification::Tested => {}
            CompatibilityClassification::UnknownCompatible => {
                if compatibility.acknowledgement_required {
                    if acknowledge_version.as_deref() != Some(compatibility.version.as_str()) {
                        return Err(self
                            .failure(target, "compatibility-acknowledgement-required")
                            .await);
                    }
                    compatibility.acknowledgement_required = false;
                }
            }
            CompatibilityClassification::Incompatible => {
                return Err(self.failure(target, "incompatible-target-cli").await);
            }
        }
        let expected = match self
            .tokens
            .lock()
            .await
            .get(&key)
            .filter(|record| record.token == observation_token)
            .cloned()
        {
            Some(expected) => expected,
            None => return Err(self.failure(target, "stale-reconciliation-preview").await),
        };
        if !expected.shadows.is_empty() {
            return Err(self.failure(target, "shadowing-configuration").await);
        }
        let adopt = if strategy == ReconciliationStrategy::Adopt {
            let provider = validated
                .prepared
                .adopted_provider()
                .map_err(|problem| problem.code());
            let provider = match provider {
                Ok(provider) => provider,
                Err(code) => return Err(self.failure(target, code).await),
            };
            if adopt_takeover
                && current_view
                    .takeover
                    .endpoint
                    .as_deref()
                    .is_some_and(|endpoint| {
                        points_to_managed_endpoint(&provider.base_url, endpoint)
                    })
            {
                return Err(self.failure(target, "stale-reconciliation-preview").await);
            }
            let route_credential = match self.state.routing_credential_for(target).await {
                Ok(credential) => credential,
                Err(_) => return Err(self.failure(target, "state-store-error").await),
            };
            if route_credential.as_ref().is_some_and(|route_credential| {
                routing_credential_value_matches(&provider.credential, route_credential)
            }) {
                return Err(self.failure(target, "invalid-provider-credential").await);
            }
            let recovery_payload_json =
                match serde_json::to_string(&validated.prepared.adopted_recovery_payload()) {
                    Ok(payload) => payload,
                    Err(_) => return Err(self.failure(target, "recovery-required").await),
                };
            let file_identity_json = match validated.prepared.file_identity_json() {
                Ok(identity) => identity,
                Err(problem) => return Err(self.failure(target, problem.code()).await),
            };
            let provider_id = Uuid::new_v4();
            Some(AdoptReconciliation {
                provider_id,
                credential_id: Uuid::new_v4(),
                snapshot: ActivatedSnapshot {
                    id: Uuid::new_v4(),
                    target,
                    provider_id,
                    base_url: provider.base_url,
                    model: provider.model,
                    protocol: provider.protocol,
                    authentication: provider.authentication,
                    provider_credential: provider.credential,
                    epoch: self.state.service_epoch(),
                },
                name: provider.name,
                recovery_id: Uuid::new_v4(),
                recovery_payload_json,
                file_identity_json,
                config_path: validated.managed_config_path.to_string_lossy().into_owned(),
                managed_config_version: validated.prepared.managed_config_version(),
                exit_takeover: adopt_takeover,
            })
        } else {
            None
        };
        let refreshed_recovery_payload_json = if strategy == ReconciliationStrategy::Reapply {
            match serde_json::to_string(&validated.prepared.adopted_recovery_payload()) {
                Ok(payload) => Some(payload),
                Err(_) => return Err(self.failure(target, "recovery-required").await),
            }
        } else {
            None
        };
        let refreshed_file_identity_json = if strategy == ReconciliationStrategy::Reapply {
            match validated.prepared.file_identity_json() {
                Ok(identity) => Some(identity),
                Err(problem) => return Err(self.failure(target, problem.code()).await),
            }
        } else {
            None
        };
        let (before_json, desired_json) = match validated.prepared.durable_material() {
            Ok(material) => material,
            Err(problem) => return Err(self.failure(target, problem.code()).await),
        };
        #[cfg(test)]
        if let Some(pause) = &self.hooks.pause_before_reserve {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
        let mut reserved_runtime = None;
        if strategy == ReconciliationStrategy::Restore || adopt_takeover {
            reserved_runtime = match self.target_runtime.reserve_if_idle(target).await {
                Ok(runtime) => runtime,
                Err(_) => return Err(self.failure(target, "target-busy").await),
            };
            #[cfg(test)]
            if let Some(pause) = &self.hooks.pause_after_reserve {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
        }
        if self
            .state
            .insert_reconciliation_intent(
                target,
                action_id,
                strategy,
                expected_revision,
                before_json,
                desired_json,
            )
            .await
            .is_err()
        {
            self.target_runtime
                .restore_reserved(target, reserved_runtime.take())
                .await;
            return Err(self.failure(target, "state-store-error").await);
        }

        if self.hooks.failpoint == ReconciliationFailpoint::AfterIntent {
            let failure = self
                .rollback_after_intent(
                    target,
                    action_id,
                    &validated.prepared,
                    key,
                    observation_token,
                )
                .await;
            self.target_runtime
                .restore_reserved(target, reserved_runtime.take())
                .await;
            return failure;
        }

        let applied = if self.hooks.failpoint == ReconciliationFailpoint::AtomicWrite {
            Err(super::reconciliation_adapter::ReconciliationProblem::fixed(
                "configuration-write-failed",
            ))
        } else {
            validated.prepared.atomic_apply(&self.codex.user_home)
        };
        let verified = applied.and_then(|()| {
            if self.hooks.failpoint == ReconciliationFailpoint::Verify {
                Err(super::reconciliation_adapter::ReconciliationProblem::fixed(
                    "configuration-write-failed",
                ))
            } else {
                validated.prepared.verify(&self.codex.user_home)
            }
        });
        if verified.is_err() {
            let failure = self
                .rollback_after_intent(
                    target,
                    action_id,
                    &validated.prepared,
                    key,
                    observation_token,
                )
                .await;
            self.target_runtime
                .restore_reserved(target, reserved_runtime.take())
                .await;
            return failure;
        }
        if self.hooks.failpoint == ReconciliationFailpoint::RollbackVerify {
            let failure = self
                .rollback_after_intent(
                    target,
                    action_id,
                    &validated.prepared,
                    key,
                    observation_token,
                )
                .await;
            self.target_runtime
                .restore_reserved(target, reserved_runtime.take())
                .await;
            return failure;
        }
        #[cfg(test)]
        if let Some(pause) = &self.hooks.pause_after_verify {
            pause.reached.notify_one();
            pause.release.notified().await;
        }
        if (strategy == ReconciliationStrategy::Restore || adopt_takeover)
            && self.hooks.failpoint == ReconciliationFailpoint::ListenerStop
        {
            let failure = self
                .rollback_after_intent(
                    target,
                    action_id,
                    &validated.prepared,
                    key,
                    observation_token,
                )
                .await;
            self.target_runtime
                .restore_reserved(target, reserved_runtime.take())
                .await;
            return failure;
        }
        let commit = self
            .state
            .commit_reconciliation(ReconciliationCommitInput {
                target,
                action_id,
                expected_revision,
                strategy,
                compatibility,
                adopt,
                refreshed_recovery_payload_json,
                refreshed_file_identity_json,
                failpoint: match self.hooks.failpoint {
                    ReconciliationFailpoint::CredentialInsert => {
                        ReconciliationCommitFailpoint::CredentialInsert
                    }
                    ReconciliationFailpoint::ProviderInsert => {
                        ReconciliationCommitFailpoint::ProviderInsert
                    }
                    ReconciliationFailpoint::SnapshotInsert => {
                        ReconciliationCommitFailpoint::SnapshotInsert
                    }
                    ReconciliationFailpoint::FinalRevision => {
                        ReconciliationCommitFailpoint::FinalRevision
                    }
                    ReconciliationFailpoint::FinalTransaction => {
                        ReconciliationCommitFailpoint::FinalTransaction
                    }
                    _ => ReconciliationCommitFailpoint::None,
                },
            })
            .await;
        match commit {
            Ok(ReconciliationCommit::Applied(outcome)) => {
                if self
                    .target_runtime
                    .shutdown_reserved(reserved_runtime.take())
                    .await
                    .is_err()
                {
                    let recovery = match self
                        .state
                        .mark_committed_reconciliation_recovery_required(target, action_id)
                        .await
                    {
                        Ok(recovery) => recovery,
                        Err(_) => return Err(self.failure(target, "state-store-error").await),
                    };
                    self.consume_token(key, observation_token).await;
                    return Ok(recovery);
                }
                self.consume_token(key, observation_token).await;
                Ok(outcome)
            }
            Ok(ReconciliationCommit::Replayed(outcome)) => {
                let _ = self
                    .target_runtime
                    .shutdown_reserved(reserved_runtime.take())
                    .await;
                Ok(outcome)
            }
            Ok(ReconciliationCommit::Stale) => {
                let failure = self
                    .rollback_after_intent_with_code(
                        target,
                        action_id,
                        &validated.prepared,
                        "stale-revision",
                        key,
                        observation_token,
                    )
                    .await;
                self.target_runtime
                    .restore_reserved(target, reserved_runtime.take())
                    .await;
                failure
            }
            Err(_) => {
                let failure = self
                    .rollback_after_intent(
                        target,
                        action_id,
                        &validated.prepared,
                        key,
                        observation_token,
                    )
                    .await;
                self.target_runtime
                    .restore_reserved(target, reserved_runtime.take())
                    .await;
                failure
            }
        }
    }

    pub(crate) async fn ensure_ordinary_write_allowed(
        &self,
        target: Target,
        context: Option<ReconciliationContext>,
        probe_unmanaged_provider_write: bool,
    ) -> DeferredPublication<Result<(), ActionFailure>> {
        self.ensure_write_allowed(target, context, probe_unmanaged_provider_write, false)
            .await
    }

    pub(crate) async fn ensure_synchronization_write_allowed(
        &self,
        target: Target,
        context: Option<ReconciliationContext>,
    ) -> DeferredPublication<Result<(), ActionFailure>> {
        self.ensure_write_allowed(target, context, true, true).await
    }

    async fn ensure_write_allowed(
        &self,
        target: Target,
        context: Option<ReconciliationContext>,
        probe_unmanaged_provider_write: bool,
        require_unmanaged_acknowledgement: bool,
    ) -> DeferredPublication<Result<(), ActionFailure>> {
        match self.state.managed_write_status_for(target).await {
            Ok(ManagedWriteStatus::Allowed) => {}
            Ok(ManagedWriteStatus::ConfigurationDrift) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "configuration-drift")
                    .await));
            }
            Ok(ManagedWriteStatus::RecoveryRequired) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "recovery-required")
                    .await));
            }
            Err(_) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "state-store-error")
                    .await));
            }
        }
        let view = match self.state.target_view_for(target).await {
            Ok(view) => view,
            Err(_) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "state-store-error")
                    .await));
            }
        };
        if view.managed_configuration.state == "unmanaged" {
            if !probe_unmanaged_provider_write {
                return DeferredPublication::none(Ok(()));
            }
            let compatibility = match self.probe_compatibility(target).await {
                Ok(compatibility) => compatibility,
                Err(problem) => {
                    let mut failure = self.failure(target, &problem.code).await;
                    failure.problem = problem;
                    return DeferredPublication::none(Err(failure));
                }
            };
            return match self
                .ensure_compatibility_allowed(
                    target,
                    &compatibility,
                    require_unmanaged_acknowledgement,
                )
                .await
            {
                Ok(()) => DeferredPublication::none(Ok(())),
                Err(failure) => {
                    self.project_ordinary_problem(
                        target,
                        failure.problem.code.as_str(),
                        None,
                        compatibility,
                    )
                    .await
                }
            };
        }
        let Some(context) = context else {
            return DeferredPublication::none(Err(self
                .failure(target, "preflight-context-required")
                .await));
        };
        let (record, observed) = match self
            .observe(target, ReconciliationStrategy::Reapply, &context)
            .await
        {
            Ok(observed) => observed,
            Err(problem) => {
                let mut failure = self.failure(target, &problem.code).await;
                failure.problem = problem;
                return DeferredPublication::none(Err(failure));
            }
        };
        if let Some(source) = record.shadows.first().cloned() {
            return self
                .project_ordinary_problem(
                    target,
                    "shadowing-configuration",
                    Some(source),
                    record.compatibility,
                )
                .await;
        }
        if observed.observation.owned_drifted {
            if self
                .state
                .mark_configuration_drift_for(target)
                .await
                .is_err()
            {
                return DeferredPublication::none(Err(self
                    .failure(target, "state-store-error")
                    .await));
            }
            let failure = self.failure(target, "configuration-drift").await;
            return DeferredPublication {
                publication: Some(failure.authoritative_view.clone()),
                result: Err(failure),
            };
        }
        match self
            .ensure_compatibility_allowed(target, &record.compatibility, true)
            .await
        {
            Ok(()) => DeferredPublication::none(Ok(())),
            Err(failure) => {
                self.project_ordinary_problem(
                    target,
                    failure.problem.code.as_str(),
                    None,
                    record.compatibility,
                )
                .await
            }
        }
    }

    async fn project_ordinary_problem(
        &self,
        target: Target,
        code: &str,
        source: Option<ShadowSource>,
        compatibility: CompatibilityView,
    ) -> DeferredPublication<Result<(), ActionFailure>> {
        let stable_code = match code {
            "shadowing-configuration" => "shadowing-configuration",
            "compatibility-acknowledgement-required" => "compatibility-acknowledgement-required",
            "incompatible-target-cli" => "incompatible-target-cli",
            _ => return DeferredPublication::none(Err(self.failure(target, code).await)),
        };
        let publication = match self
            .state
            .project_managed_write_problem(target, stable_code, source, compatibility)
            .await
        {
            Ok(publication) => publication,
            Err(_) => {
                return DeferredPublication::none(Err(self
                    .failure(target, "state-store-error")
                    .await));
            }
        };
        let mut failure = self.failure(target, stable_code).await;
        if let Some(problem) = failure
            .authoritative_view
            .problems
            .iter()
            .find(|problem| problem.code == stable_code)
        {
            failure.problem.source.clone_from(&problem.source);
            failure.problem.selector = problem.selector;
        }
        DeferredPublication {
            result: Err(failure),
            publication,
        }
    }

    async fn ensure_compatibility_allowed(
        &self,
        target: Target,
        compatibility: &CompatibilityView,
        require_acknowledgement: bool,
    ) -> Result<(), ActionFailure> {
        match compatibility.classification {
            CompatibilityClassification::Tested => Ok(()),
            CompatibilityClassification::UnknownCompatible
                if !require_acknowledgement || !compatibility.acknowledgement_required =>
            {
                Ok(())
            }
            CompatibilityClassification::UnknownCompatible => Err(self
                .failure(target, "compatibility-acknowledgement-required")
                .await),
            CompatibilityClassification::Incompatible => {
                Err(self.failure(target, "incompatible-target-cli").await)
            }
        }
    }

    async fn probe_compatibility(
        &self,
        target: Target,
    ) -> Result<CompatibilityView, ControlProblem> {
        self.probe_target_compatibility_cancellable(target, CancellationToken::new())
            .await
    }

    async fn probe_target_compatibility_cancellable(
        &self,
        target: Target,
        cancellation: CancellationToken,
    ) -> Result<CompatibilityView, ControlProblem> {
        let observed = match target {
            Target::Codex => match self
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
            },
            Target::Claude => match self
                .claude
                .probe
                .probe_cancellable(&self.claude.executable, cancellation)
                .await
            {
                Ok(capability) => ProbedCompatibility::from(capability),
                Err(problem) => ProbedCompatibility::new(
                    problem.version().unwrap_or("unavailable").to_owned(),
                    CompatibilityClassification::Incompatible,
                ),
            },
        };
        self.compatibility_view(target, &observed).await
    }

    async fn rollback_after_intent(
        &self,
        target: Target,
        action_id: Uuid,
        prepared: &PreparedConfiguration,
        key: ObservationKey,
        observation_token: Uuid,
    ) -> Result<ActionOutcome, ActionFailure> {
        self.rollback_after_intent_with_code(
            target,
            action_id,
            prepared,
            "configuration-write-failed",
            key,
            observation_token,
        )
        .await
    }

    async fn rollback_after_intent_with_code(
        &self,
        target: Target,
        action_id: Uuid,
        prepared: &PreparedConfiguration,
        failure_code: &'static str,
        key: ObservationKey,
        observation_token: Uuid,
    ) -> Result<ActionOutcome, ActionFailure> {
        let restored = prepared.exact_rollback(&self.codex.user_home);
        if self.hooks.failpoint != ReconciliationFailpoint::RollbackVerify
            && restored.is_ok()
            && self
                .state
                .set_reconciliation_intent_state(target, action_id, "rolled-back")
                .await
                .is_ok()
        {
            return Err(self.failure(target, failure_code).await);
        }
        match self
            .state
            .mark_reconciliation_recovery_required(target, action_id)
            .await
        {
            Ok(outcome) => {
                self.consume_token(key, observation_token).await;
                Ok(outcome)
            }
            Err(_) => Err(self.failure(target, "state-store-error").await),
        }
    }

    async fn consume_token(&self, key: ObservationKey, token: Uuid) {
        let mut tokens = self.tokens.lock().await;
        if tokens.get(&key).is_some_and(|record| record.token == token) {
            tokens.remove(&key);
        }
    }

    async fn failure(&self, target: Target, code: &str) -> ActionFailure {
        self.state
            .failure_for(target, code, stable_message(code))
            .await
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
        let mut observed = adapter
            .observe(strategy, &committed, context, compatibility.clone())
            .map_err(|problem| stable_problem(problem.code()))?;
        if strategy == ReconciliationStrategy::Adopt
            && view.takeover.state == "active"
            && let Some(takeover) = observed
                .observation
                .changes
                .iter_mut()
                .find(|change| change.field == ReconciliationField::Takeover)
        {
            takeover.state = ReconciliationFieldState::Absent;
        }
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
    let message = stable_message(code);
    ControlProblem {
        code: code.to_owned(),
        message: message.to_owned(),
        source: None,
        selector: None,
    }
}

fn points_to_managed_endpoint(base_url: &str, endpoint: &str) -> bool {
    let (Ok(base_url), Ok(endpoint)) = (url::Url::parse(base_url), url::Url::parse(endpoint))
    else {
        return false;
    };
    let base_is_loopback = base_url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    base_is_loopback && base_url.port_or_known_default() == endpoint.port_or_known_default()
}

fn stable_message(code: &str) -> &'static str {
    match code {
        "stale-reconciliation-preview" => "Target state changed; preview again",
        "stale-compatibility-probe" => "Target compatibility changed; probe again",
        "stale-revision" => "Target state changed; refresh and retry",
        "recovery-required" => "Managed configuration requires recovery",
        "state-store-error" => "State store unavailable",
        "configuration-drift" => "Managed configuration drift must be reconciled",
        "shadowing-configuration" => "A higher-priority configuration source is active",
        "unsupported-configuration-home" => "Configuration Home is unsupported",
        "unsafe-managed-file" => "Managed configuration is unsafe",
        "compatibility-acknowledgement-required" => {
            "Acknowledge this exact untested Target CLI version"
        }
        "incompatible-target-cli" => "Target CLI is incompatible",
        "target-busy" => "Target has an active model request",
        "invalid-provider-credential" => "Observed Provider credential is not adoptable",
        "configuration-write-failed" => "Reconciliation preview failed",
        _ => "Reconciliation preview failed",
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        future::Future,
        os::unix::fs::{MetadataExt, symlink},
        path::Path,
        pin::Pin,
        sync::{Arc, Condvar, Mutex as StdMutex, mpsc},
    };

    use async_trait::async_trait;
    use axum::{body::Bytes, http::StatusCode};
    use futures_util::stream;
    use secrecy::ExposeSecret;
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

    struct HeldUpstream {
        started: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl HeldUpstream {
        fn new() -> Self {
            Self {
                started: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
            }
        }
    }

    #[async_trait]
    impl UpstreamTransport for HeldUpstream {
        async fn send(&self, _: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
            self.started.notify_one();
            self.release.notified().await;
            Ok(UpstreamResponse {
                status: StatusCode::OK,
                headers: Default::default(),
                body: Box::pin(stream::once(async { Ok(Bytes::from_static(b"{}")) })),
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
                target_runtime: super::ReconciliationTargetRuntime::empty(),
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

    #[tokio::test]
    async fn authoritative_publication_serializes_model_path_mutation_until_sync_send() {
        let fixture = Fixture::new().await;
        fixture
            .store
            .mark_configuration_drift_for(Target::Codex)
            .await
            .unwrap();
        let candidate = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let snapshot_id = candidate.activated_snapshot.as_ref().unwrap().id;
        let mut updates = fixture.store.subscribe_target_views();
        let reached = Arc::new(tokio::sync::Notify::new());
        let hook_reached = Arc::clone(&reached);
        let release = Arc::new((StdMutex::new(false), Condvar::new()));
        let hook_release = Arc::clone(&release);
        let hook = Arc::new(move || {
            hook_reached.notify_one();
            let (released, signal) = &*hook_release;
            let mut released = released.lock().unwrap();
            while !*released {
                released = signal.wait(released).unwrap();
            }
            Ok(())
        });
        let publishing_store = Arc::clone(&fixture.store);
        let publishing = tokio::spawn(async move {
            publishing_store
                .publish_target_view_with_authoritative_read_hook(candidate, hook)
                .await
                .unwrap();
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), reached.notified())
            .await
            .unwrap();

        let mut serving = Box::pin(fixture.store.record_serving_for(Target::Codex, snapshot_id));
        let interleaved =
            match tokio::time::timeout(std::time::Duration::from_millis(50), &mut serving).await {
                Ok(result) => {
                    result.unwrap();
                    true
                }
                Err(_) => false,
            };
        {
            let (released, signal) = &*release;
            *released.lock().unwrap() = true;
            signal.notify_all();
        }
        assert!(
            !interleaved,
            "model-path mutation interleaved with authoritative publication"
        );

        publishing.await.unwrap();
        let published = updates.recv().await.unwrap();
        let served = serving.await.unwrap();
        assert!(
            published.view_sequence < served.view_sequence,
            "publication did not precede the queued model-path mutation"
        );
        let served_publication = updates.recv().await.unwrap();
        assert_eq!(served_publication.view_sequence, served.view_sequence);
    }

    #[tokio::test]
    async fn authoritative_publication_propagates_database_read_failure_without_a_push() {
        let fixture = Fixture::new().await;
        let candidate = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let mut updates = fixture.store.subscribe_target_views();
        let failure = fixture
            .store
            .publish_target_view_with_authoritative_read_hook(
                candidate,
                Arc::new(|| Err(tokio_rusqlite::rusqlite::Error::InvalidQuery)),
            )
            .await
            .unwrap_err();
        assert!(matches!(failure, crate::state::StateError::Sqlite(_)));
        assert!(matches!(
            updates.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert!(!format!("{failure:?}").contains("SQLITE_SECRET_98301"));
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

    fn reconciliation_row_counts(home: &MuxviaHome) -> [u64; 5] {
        let connection = Connection::open(home.database_path()).unwrap();
        [
            "credentials",
            "providers",
            "activated_snapshots",
            "activation_recovery",
            "action_receipts",
        ]
        .map(|table| {
            connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap()
        })
    }

    fn reconciliation_intent_count(home: &MuxviaHome) -> u64 {
        Connection::open(home.database_path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM reconciliation_intents", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[derive(PartialEq, Eq)]
    struct FileFingerprint {
        device: u64,
        inode: u64,
        mode: u32,
        modified_seconds: i64,
        modified_nanoseconds: i64,
        length: u64,
        bytes: Vec<u8>,
    }

    impl std::fmt::Debug for FileFingerprint {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("FileFingerprint")
                .field("device", &self.device)
                .field("inode", &self.inode)
                .field("mode", &self.mode)
                .field("modified_seconds", &self.modified_seconds)
                .field("modified_nanoseconds", &self.modified_nanoseconds)
                .field("length", &self.length)
                .field("bytes", &"<redacted>")
                .finish()
        }
    }

    #[test]
    fn file_fingerprint_diagnostic_redacts_raw_and_numeric_secret_bytes() {
        let home = TempDir::new().unwrap();
        let path = home.path().join("secret-config");
        let sentinel = "FINGERPRINT_SECRET_98401";
        fs::write(&path, sentinel).unwrap();
        let diagnostic = format!("{:?}", file_fingerprint(&path));
        assert!(!diagnostic.contains(sentinel));
        assert!(!diagnostic.contains(&format!("{:?}", sentinel.as_bytes())));
    }

    fn file_fingerprint(path: &Path) -> FileFingerprint {
        let metadata = fs::metadata(path).unwrap();
        FileFingerprint {
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            length: metadata.len(),
            bytes: fs::read(path).unwrap(),
        }
    }

    #[derive(PartialEq, Eq)]
    struct SecretContentFingerprint {
        length: usize,
        sha256: [u8; 32],
    }

    fn secret_content_fingerprint(path: &Path) -> SecretContentFingerprint {
        let bytes = fs::read(path).unwrap();
        SecretContentFingerprint {
            length: bytes.len(),
            sha256: ring::digest::digest(&ring::digest::SHA256, &bytes)
                .as_ref()
                .try_into()
                .unwrap(),
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
                target_runtime: super::ReconciliationTargetRuntime::empty(),
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
    async fn reconciliation_post_intent_failpoints_roll_back_or_persist_replayable_recovery() {
        for failpoint in [
            super::ReconciliationFailpoint::AfterIntent,
            super::ReconciliationFailpoint::AtomicWrite,
            super::ReconciliationFailpoint::Verify,
            super::ReconciliationFailpoint::FinalTransaction,
            super::ReconciliationFailpoint::RollbackVerify,
        ] {
            let fixture = Fixture::new().await;
            let service = reconciliation_service(
                Arc::clone(&fixture.store),
                &fixture.home,
                fixture.probe.clone(),
            )
            .with_hooks(super::ReconciliationHooks {
                failpoint,
                ..Default::default()
            });
            let config_path = fixture.home.user_home().join(".codex/config.toml");
            let before = secret_content_fingerprint(&config_path);
            let preview = service
                .preview(
                    Target::Codex,
                    ReconciliationStrategy::Reapply,
                    ReconciliationContext::Codex,
                )
                .await
                .unwrap();
            let action_id = Uuid::new_v4();
            let result = service
                .apply(
                    Target::Codex,
                    action_id,
                    preview.management_revision,
                    ReconciliationStrategy::Reapply,
                    preview.observation_token,
                    None,
                    ReconciliationContext::Codex,
                )
                .await;
            if failpoint == super::ReconciliationFailpoint::RollbackVerify {
                let outcome = result.unwrap();
                assert_eq!(outcome.view.recovery.state, "recovery-required");
                let replay = fixture
                    .store
                    .receipt_for(Target::Codex, action_id)
                    .await
                    .unwrap()
                    .expect("failed rollback must leave a replay-consistent receipt");
                assert_eq!(replay.view.recovery.state, "recovery-required");
                let replayed = service
                    .apply(
                        Target::Codex,
                        action_id,
                        preview.management_revision,
                        ReconciliationStrategy::Reapply,
                        preview.observation_token,
                        None,
                        ReconciliationContext::Codex,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    replayed.status,
                    crate::control::protocol::ActionStatus::Replayed
                );
                assert_eq!(replayed.view, outcome.view);
                let blocked_preview = service
                    .preview(
                        Target::Codex,
                        ReconciliationStrategy::Reapply,
                        ReconciliationContext::Codex,
                    )
                    .await
                    .unwrap();
                let blocked = service
                    .apply(
                        Target::Codex,
                        Uuid::new_v4(),
                        blocked_preview.management_revision,
                        ReconciliationStrategy::Reapply,
                        blocked_preview.observation_token,
                        None,
                        ReconciliationContext::Codex,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(blocked.problem.code, "recovery-required");
            } else {
                let failure = result.unwrap_err();
                assert_eq!(failure.problem.code, "configuration-write-failed");
                assert!(
                    secret_content_fingerprint(&config_path) == before,
                    "rollback changed the secret-bearing managed configuration"
                );
                assert!(
                    fixture
                        .store
                        .receipt_for(Target::Codex, action_id)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
        }
    }

    #[tokio::test]
    async fn adopt_commit_boundary_failpoints_abort_without_partial_state_or_publish() {
        for failpoint in [
            super::ReconciliationFailpoint::CredentialInsert,
            super::ReconciliationFailpoint::ProviderInsert,
            super::ReconciliationFailpoint::SnapshotInsert,
            super::ReconciliationFailpoint::FinalRevision,
            super::ReconciliationFailpoint::FinalTransaction,
        ] {
            let fixture = Fixture::new().await;
            let config_path = fixture.home.user_home().join(".codex/config.toml");
            fs::write(
                &config_path,
                r#"model = "external-model"
model_provider = "muxvia_codex"
unrelated = "preserve"

[model_providers.muxvia_codex]
name = "External"
base_url = "https://external.example/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer distinct-external-secret" }
supports_websockets = false
"#,
            )
            .unwrap();
            fixture
                .store
                .mark_configuration_drift_for(Target::Codex)
                .await
                .unwrap();
            let before_file = secret_content_fingerprint(&config_path);
            let before_counts = reconciliation_row_counts(&fixture.home);
            let before_view =
                serde_json::to_value(fixture.store.target_view_for(Target::Codex).await.unwrap())
                    .unwrap();
            let service = reconciliation_service(
                Arc::clone(&fixture.store),
                &fixture.home,
                fixture.probe.clone(),
            )
            .with_hooks(super::ReconciliationHooks {
                failpoint,
                ..Default::default()
            });
            let preview = service
                .preview(
                    Target::Codex,
                    ReconciliationStrategy::Adopt,
                    ReconciliationContext::Codex,
                )
                .await
                .unwrap();
            let action_id = Uuid::new_v4();
            let mut updates = fixture.store.subscribe_target_views();

            let failure = service
                .apply(
                    Target::Codex,
                    action_id,
                    preview.management_revision,
                    ReconciliationStrategy::Adopt,
                    preview.observation_token,
                    None,
                    ReconciliationContext::Codex,
                )
                .await
                .unwrap_err();

            assert_eq!(failure.problem.code, "configuration-write-failed");
            assert!(
                secret_content_fingerprint(&config_path) == before_file,
                "failed Adopt changed the secret-bearing managed configuration"
            );
            assert_eq!(reconciliation_row_counts(&fixture.home), before_counts);
            assert_eq!(
                serde_json::to_value(fixture.store.target_view_for(Target::Codex).await.unwrap())
                    .unwrap(),
                before_view
            );
            assert!(
                fixture
                    .store
                    .receipt_for(Target::Codex, action_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(matches!(
                updates.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ));
            assert_eq!(
                service
                    .target_runtime
                    .active_request_count(Target::Codex)
                    .await,
                0
            );
        }
    }

    #[tokio::test]
    async fn restore_listener_stop_failpoint_exactly_rolls_back_without_state_or_runtime_mutation()
    {
        let fixture = Fixture::new().await;
        let view = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let provider_id = view.current_provider_id.as_deref().unwrap();
        let activation = ActivationService::new(
            Arc::clone(&fixture.store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        );
        activation
            .apply_raw_for(
                Target::Codex,
                Uuid::new_v4(),
                view.management_revision,
                serde_json::json!({
                    "kind": "activate-provider",
                    "providerId": provider_id,
                    "mode": "takeover"
                }),
            )
            .await
            .unwrap();
        let endpoint_before = activation.model_endpoint_for(Target::Codex).await;
        let config_path = fixture.home.user_home().join(".codex/config.toml");
        let mut drifted = fs::read_to_string(&config_path).unwrap();
        drifted.push_str("\nlistener_stop_unrelated = \"preserve\"\n");
        drifted = drifted.replace("gpt-test", "listener-stop-drift");
        fs::write(&config_path, drifted).unwrap();
        fixture
            .store
            .mark_configuration_drift_for(Target::Codex)
            .await
            .unwrap();
        let before_file = secret_content_fingerprint(&config_path);
        let before_counts = reconciliation_row_counts(&fixture.home);
        let before_view =
            serde_json::to_value(fixture.store.target_view_for(Target::Codex).await.unwrap())
                .unwrap();
        let service = ReconciliationService::from_runtime(
            Arc::clone(&fixture.store),
            activation.reconciliation_runtime(),
        )
        .with_hooks(super::ReconciliationHooks {
            failpoint: super::ReconciliationFailpoint::ListenerStop,
            ..Default::default()
        });
        let preview = service
            .preview(
                Target::Codex,
                ReconciliationStrategy::Restore,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        let action_id = Uuid::new_v4();
        let mut updates = fixture.store.subscribe_target_views();

        let failure = service
            .apply(
                Target::Codex,
                action_id,
                preview.management_revision,
                ReconciliationStrategy::Restore,
                preview.observation_token,
                None,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();

        assert_eq!(failure.problem.code, "configuration-write-failed");
        assert!(
            secret_content_fingerprint(&config_path) == before_file,
            "listener-stop failure changed the secret-bearing managed configuration"
        );
        assert_eq!(reconciliation_row_counts(&fixture.home), before_counts);
        assert_eq!(
            serde_json::to_value(fixture.store.target_view_for(Target::Codex).await.unwrap())
                .unwrap(),
            before_view
        );
        assert!(
            fixture
                .store
                .receipt_for(Target::Codex, action_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            updates.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert_eq!(
            activation.model_endpoint_for(Target::Codex).await,
            endpoint_before
        );
        activation.shutdown_models().await.unwrap();
    }

    #[tokio::test]
    async fn shared_target_gate_serializes_reconcile_with_save_and_activation_but_not_peer() {
        let fixture = Fixture::new().await;
        let activation = Arc::new(ActivationService::new(
            Arc::clone(&fixture.store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        ));
        let pause = Arc::new(super::ReconciliationPause::default());
        let service = Arc::new(
            ReconciliationService::from_runtime(
                Arc::clone(&fixture.store),
                activation.reconciliation_runtime(),
            )
            .with_hooks(super::ReconciliationHooks {
                pause_after_verify: Some(Arc::clone(&pause)),
                ..Default::default()
            }),
        );
        let config_path = fixture.home.user_home().join(".codex/config.toml");
        fs::write(
            &config_path,
            fs::read_to_string(&config_path)
                .unwrap()
                .replace("gpt-test", "shared-gate-drift"),
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
        let before = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let provider_id = before.current_provider_id.clone().unwrap();
        let reconcile = tokio::spawn({
            let service = Arc::clone(&service);
            async move {
                service
                    .apply(
                        Target::Codex,
                        Uuid::new_v4(),
                        preview.management_revision,
                        ReconciliationStrategy::Reapply,
                        preview.observation_token,
                        None,
                        ReconciliationContext::Codex,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), pause.reached.notified())
            .await
            .expect("reconciliation did not pause after verified file write");
        let save = tokio::spawn({
            let activation = Arc::clone(&activation);
            async move {
                activation
                    .apply_raw_for(
                        Target::Codex,
                        Uuid::new_v4(),
                        before.management_revision,
                        serde_json::json!({
                            "kind":"create-provider","name":"same-target",
                            "baseUrl":"https://same-target.test/v1","model":"same",
                            "credential":{"kind":"replace","value":"same-target-secret"},
                            "authentication":"openai-bearer","presetKey":null
                        }),
                    )
                    .await
            }
        });
        let activate = tokio::spawn({
            let activation = Arc::clone(&activation);
            async move {
                activation
                    .apply_raw_for(
                        Target::Codex,
                        Uuid::new_v4(),
                        before.management_revision,
                        serde_json::json!({
                            "kind":"activate-provider","providerId":provider_id,"mode":"direct"
                        }),
                    )
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!save.is_finished());
        assert!(!activate.is_finished());
        let peer = activation
            .apply_raw_for(
                Target::Claude,
                Uuid::new_v4(),
                0,
                serde_json::json!({
                    "kind":"create-provider","name":"peer",
                    "baseUrl":"https://peer.test/v1","model":"peer",
                    "credential":{"kind":"replace","value":"peer-secret"},
                    "authentication":"anthropic-api-key","presetKey":null
                }),
            )
            .await
            .unwrap();
        assert_eq!(peer.view.management_revision, 1);
        pause.release.notify_one();
        reconcile.await.unwrap().unwrap();
        assert_eq!(
            save.await.unwrap().unwrap_err().problem.code,
            "stale-revision"
        );
        assert_eq!(
            activate.await.unwrap().unwrap_err().problem.code,
            "stale-revision"
        );
        let rendered = fs::read_to_string(config_path).unwrap();
        assert!(rendered.contains("gpt-test"));
        assert!(!rendered.contains("shared-gate-drift"));
    }

    #[tokio::test]
    async fn restore_busy_in_the_old_admission_window_has_zero_durable_file_or_runtime_mutation() {
        let fixture = Fixture::new().await;
        let view = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let provider_id = view.current_provider_id.clone().unwrap();
        let upstream = Arc::new(HeldUpstream::new());
        let activation = Arc::new(ActivationService::new(
            Arc::clone(&fixture.store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            upstream.clone(),
        ));
        activation
            .apply_raw_for(
                Target::Codex,
                Uuid::new_v4(),
                view.management_revision,
                serde_json::json!({
                    "kind":"activate-provider","providerId":provider_id,"mode":"takeover"
                }),
            )
            .await
            .unwrap();
        let endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
        let routing_credential = fixture
            .store
            .routing_credential_for(Target::Codex)
            .await
            .unwrap()
            .unwrap();
        let config_path = fixture.home.user_home().join(".codex/config.toml");
        fs::write(
            &config_path,
            fs::read_to_string(&config_path)
                .unwrap()
                .replace("gpt-test", "old-admission-window-drift"),
        )
        .unwrap();
        let pause = Arc::new(super::ReconciliationPause::default());
        let service = Arc::new(
            ReconciliationService::from_runtime(
                Arc::clone(&fixture.store),
                activation.reconciliation_runtime(),
            )
            .with_hooks(super::ReconciliationHooks {
                pause_before_reserve: Some(Arc::clone(&pause)),
                ..Default::default()
            }),
        );
        let preview = service
            .preview(
                Target::Codex,
                ReconciliationStrategy::Restore,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        let restore = tokio::spawn({
            let service = Arc::clone(&service);
            async move {
                service
                    .apply(
                        Target::Codex,
                        Uuid::new_v4(),
                        preview.management_revision,
                        ReconciliationStrategy::Restore,
                        preview.observation_token,
                        None,
                        ReconciliationContext::Codex,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), pause.reached.notified())
            .await
            .expect("Restore did not reach the pre-intent admission window");
        let held_request = tokio::spawn({
            let routing_credential = routing_credential.expose_secret().to_owned();
            async move {
                reqwest::Client::new()
                    .post(format!("http://{endpoint}/v1/responses"))
                    .header("X-Muxvia-Routing-Credential", routing_credential)
                    .body("{}")
                    .send()
                    .await
                    .unwrap()
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            upstream.started.notified(),
        )
        .await
        .expect("real model request did not enter the active-request set");

        let before_intents = reconciliation_intent_count(&fixture.home);
        let before_rows = reconciliation_row_counts(&fixture.home);
        let before_view = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let before_file = file_fingerprint(&config_path);
        let before_endpoint = activation.model_endpoint_for(Target::Codex).await;
        let before_active = service
            .target_runtime
            .active_request_count(Target::Codex)
            .await;

        pause.release.notify_one();
        let failure = restore.await.unwrap().unwrap_err();
        assert_eq!(failure.problem.code, "target-busy");
        assert_eq!(reconciliation_intent_count(&fixture.home), before_intents);
        assert_eq!(reconciliation_row_counts(&fixture.home), before_rows);
        assert_eq!(
            fixture.store.target_view_for(Target::Codex).await.unwrap(),
            before_view
        );
        assert_eq!(file_fingerprint(&config_path), before_file);
        assert_eq!(
            activation.model_endpoint_for(Target::Codex).await,
            before_endpoint
        );
        assert_eq!(
            service
                .target_runtime
                .active_request_count(Target::Codex)
                .await,
            before_active
        );

        upstream.release.notify_one();
        assert_eq!(held_request.await.unwrap().status(), StatusCode::OK);
        activation.shutdown_models().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_busy_reservation_does_not_transiently_reject_a_second_real_request() {
        let fixture = Fixture::new().await;
        let view = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let provider_id = view.current_provider_id.clone().unwrap();
        let upstream = Arc::new(HeldUpstream::new());
        let activation = Arc::new(ActivationService::new(
            Arc::clone(&fixture.store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            upstream.clone(),
        ));
        activation
            .apply_raw_for(
                Target::Codex,
                Uuid::new_v4(),
                view.management_revision,
                serde_json::json!({
                    "kind":"activate-provider","providerId":provider_id,"mode":"takeover"
                }),
            )
            .await
            .unwrap();
        let endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
        let routing_credential = fixture
            .store
            .routing_credential_for(Target::Codex)
            .await
            .unwrap()
            .unwrap()
            .expose_secret()
            .to_owned();
        let first = tokio::spawn({
            let routing_credential = routing_credential.clone();
            async move {
                reqwest::Client::new()
                    .post(format!("http://{endpoint}/v1/responses"))
                    .header("X-Muxvia-Routing-Credential", routing_credential)
                    .body("{}")
                    .send()
                    .await
                    .unwrap()
            }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            upstream.started.notified(),
        )
        .await
        .expect("first request did not enter the active-request set");

        let runtime = activation.reconciliation_runtime().target_runtime;
        let (reached_tx, reached_rx) = mpsc::sync_channel(1);
        let release_reservation = Arc::new((StdMutex::new(false), Condvar::new()));
        runtime
            .set_reservation_attempt_hook(Target::Codex, {
                let release_reservation = Arc::clone(&release_reservation);
                Arc::new(move || {
                    reached_tx.send(()).unwrap();
                    let (released, condition) = &*release_reservation;
                    let mut released = released.lock().unwrap();
                    while !*released {
                        released = condition.wait(released).unwrap();
                    }
                })
            })
            .await;
        let reserve = tokio::spawn({
            let runtime = runtime.clone();
            async move { runtime.reserve_if_idle(Target::Codex).await }
        });
        tokio::task::spawn_blocking(move || {
            reached_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("busy reservation did not reach its atomic admission attempt");
        })
        .await
        .unwrap();

        let second = tokio::spawn(async move {
            reqwest::Client::new()
                .post(format!("http://{endpoint}/v1/responses"))
                .header("X-Muxvia-Routing-Credential", routing_credential)
                .body("{}")
                .send()
                .await
                .unwrap()
        });
        let second_started = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            upstream.started.notified(),
        )
        .await;
        {
            let (released, condition) = &*release_reservation;
            *released.lock().unwrap() = true;
            condition.notify_one();
        }
        assert!(reserve.await.unwrap().is_err());
        upstream.release.notify_one();
        upstream.release.notify_one();
        assert!(
            second_started.is_ok(),
            "busy reservation transiently rejected the concurrent request"
        );
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        assert_eq!(second.await.unwrap().status(), StatusCode::OK);
        activation.shutdown_models().await.unwrap();
    }

    #[tokio::test]
    async fn restore_idle_reservation_rejects_a_new_real_request_before_commit() {
        let fixture = Fixture::new().await;
        let view = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let provider_id = view.current_provider_id.clone().unwrap();
        let activation = Arc::new(ActivationService::new(
            Arc::clone(&fixture.store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        ));
        activation
            .apply_raw_for(
                Target::Codex,
                Uuid::new_v4(),
                view.management_revision,
                serde_json::json!({
                    "kind":"activate-provider","providerId":provider_id,"mode":"takeover"
                }),
            )
            .await
            .unwrap();
        let endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
        let routing_credential = fixture
            .store
            .routing_credential_for(Target::Codex)
            .await
            .unwrap()
            .unwrap();
        let config_path = fixture.home.user_home().join(".codex/config.toml");
        fs::write(
            &config_path,
            fs::read_to_string(&config_path)
                .unwrap()
                .replace("gpt-test", "reservation-race-drift"),
        )
        .unwrap();
        let pause = Arc::new(super::ReconciliationPause::default());
        let service = Arc::new(
            ReconciliationService::from_runtime(
                Arc::clone(&fixture.store),
                activation.reconciliation_runtime(),
            )
            .with_hooks(super::ReconciliationHooks {
                pause_after_reserve: Some(Arc::clone(&pause)),
                ..Default::default()
            }),
        );
        let preview = service
            .preview(
                Target::Codex,
                ReconciliationStrategy::Restore,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        let restore = tokio::spawn({
            let service = Arc::clone(&service);
            async move {
                service
                    .apply(
                        Target::Codex,
                        Uuid::new_v4(),
                        preview.management_revision,
                        ReconciliationStrategy::Restore,
                        preview.observation_token,
                        None,
                        ReconciliationContext::Codex,
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), pause.reached.notified())
            .await
            .expect("Restore did not reach the idle reservation");
        let rejected = reqwest::Client::new()
            .post(format!("http://{endpoint}/v1/responses"))
            .header(
                "X-Muxvia-Routing-Credential",
                routing_credential.expose_secret(),
            )
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::SERVICE_UNAVAILABLE);
        pause.release.notify_one();
        restore.await.unwrap().unwrap();
        assert!(activation.model_endpoint_for(Target::Codex).await.is_none());
    }

    #[tokio::test]
    async fn adopt_transaction_abort_releases_idle_reservation_and_keeps_takeover_live() {
        let fixture = Fixture::new().await;
        let view = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let provider_id = view.current_provider_id.clone().unwrap();
        let activation = Arc::new(ActivationService::new(
            Arc::clone(&fixture.store),
            fixture.home.clone(),
            fixture.probe.clone(),
            "/usr/bin/codex".into(),
            Arc::new(NoopUpstream),
        ));
        activation
            .apply_raw_for(
                Target::Codex,
                Uuid::new_v4(),
                view.management_revision,
                serde_json::json!({
                    "kind":"activate-provider","providerId":provider_id,"mode":"takeover"
                }),
            )
            .await
            .unwrap();
        let endpoint = activation.model_endpoint_for(Target::Codex).await.unwrap();
        let routing_credential = fixture
            .store
            .routing_credential_for(Target::Codex)
            .await
            .unwrap()
            .unwrap();
        let config_path = fixture.home.user_home().join(".codex/config.toml");
        fs::write(
            &config_path,
            r#"model = "external-adopt"
model_provider = "muxvia_codex"
[model_providers.muxvia_codex]
name = "External"
base_url = "https://external-adopt.test/v1"
wire_api = "responses"
http_headers = { Authorization = "Bearer external-adopt-secret" }
supports_websockets = false
"#,
        )
        .unwrap();
        let before_file = secret_content_fingerprint(&config_path);
        let before_view = fixture.store.target_view_for(Target::Codex).await.unwrap();
        let service = ReconciliationService::from_runtime(
            Arc::clone(&fixture.store),
            activation.reconciliation_runtime(),
        )
        .with_hooks(super::ReconciliationHooks {
            failpoint: super::ReconciliationFailpoint::FinalTransaction,
            ..Default::default()
        });
        let preview = service
            .preview(
                Target::Codex,
                ReconciliationStrategy::Adopt,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        let action_id = Uuid::new_v4();
        let failed = service
            .apply(
                Target::Codex,
                action_id,
                preview.management_revision,
                ReconciliationStrategy::Adopt,
                preview.observation_token,
                None,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(failed.problem.code, "configuration-write-failed");
        assert!(
            secret_content_fingerprint(&config_path) == before_file,
            "final-transaction failure changed the secret-bearing managed configuration"
        );
        assert_eq!(
            fixture.store.target_view_for(Target::Codex).await.unwrap(),
            before_view
        );
        assert_eq!(
            activation.model_endpoint_for(Target::Codex).await,
            Some(endpoint)
        );
        let admitted = reqwest::Client::new()
            .post(format!("http://{endpoint}/v1/responses"))
            .header(
                "X-Muxvia-Routing-Credential",
                routing_credential.expose_secret(),
            )
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(admitted.status(), StatusCode::OK);
        assert!(
            fixture
                .store
                .receipt_for(Target::Codex, action_id)
                .await
                .unwrap()
                .is_none()
        );
        activation.shutdown_models().await.unwrap();
    }

    #[tokio::test]
    async fn reconciliation_apply_gates_exact_unknown_acknowledgement_and_incompatible_probe() {
        let fixture = Fixture::new().await;
        fixture.probe.set(ProbeState::Unknown("codex-next".into()));
        let preview = fixture.preview().await;
        let action_id = Uuid::new_v4();
        let missing = fixture
            .service
            .apply(
                Target::Codex,
                action_id,
                preview.management_revision,
                ReconciliationStrategy::Reapply,
                preview.observation_token,
                None,
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(
            missing.problem.code,
            "compatibility-acknowledgement-required"
        );
        assert!(
            fixture
                .store
                .receipt_for(Target::Codex, action_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            fixture
                .service
                .tracks_token(
                    Target::Codex,
                    ReconciliationStrategy::Reapply,
                    preview.observation_token
                )
                .await
        );
        let applied = fixture
            .service
            .apply(
                Target::Codex,
                action_id,
                preview.management_revision,
                ReconciliationStrategy::Reapply,
                preview.observation_token,
                Some("codex-next".into()),
                ReconciliationContext::Codex,
            )
            .await
            .unwrap();
        assert_eq!(
            applied.status,
            crate::control::protocol::ActionStatus::Applied
        );
        let compatibility = fixture
            .store
            .compatibility_for(Target::Codex)
            .await
            .unwrap();
        assert_eq!(compatibility.version, "codex-next");
        assert!(!compatibility.acknowledgement_required);

        let incompatible = Fixture::new().await;
        incompatible.probe.set(ProbeState::Incompatible);
        let preview = incompatible.preview().await;
        let action_id = Uuid::new_v4();
        let failure = incompatible
            .service
            .apply(
                Target::Codex,
                action_id,
                preview.management_revision,
                ReconciliationStrategy::Reapply,
                preview.observation_token,
                Some("unavailable".into()),
                ReconciliationContext::Codex,
            )
            .await
            .unwrap_err();
        assert_eq!(failure.problem.code, "incompatible-target-cli");
        assert!(
            incompatible
                .store
                .receipt_for(Target::Codex, action_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn ordinary_managed_write_projects_shadow_source_once_and_persists_it() {
        let fixture = Fixture::new().await;
        let config_path = fixture.home.user_home().join(".codex/config.toml");
        let document = format!(
            "profile = \"operator-profile\"\n{}",
            fs::read_to_string(&config_path).unwrap()
        );
        fs::write(&config_path, document).unwrap();

        let first = fixture
            .service
            .ensure_ordinary_write_allowed(Target::Codex, Some(ReconciliationContext::Codex), true)
            .await;
        let failure = first.result.unwrap_err();
        assert_eq!(failure.problem.code, "shadowing-configuration");
        assert_eq!(failure.problem.source.as_deref(), Some("codex-profile"));
        let publication = first.publication.expect("new shadow must publish once");
        assert!(publication.problems.iter().any(|problem| {
            problem.code == "shadowing-configuration"
                && problem.source.as_deref() == Some("codex-profile")
        }));
        let persisted = fixture.store.target_view_for(Target::Codex).await.unwrap();
        assert_eq!(persisted, publication);

        let duplicate = fixture
            .service
            .ensure_ordinary_write_allowed(Target::Codex, Some(ReconciliationContext::Codex), true)
            .await;
        assert_eq!(
            duplicate.result.unwrap_err().problem.code,
            "shadowing-configuration"
        );
        assert!(duplicate.publication.is_none());
    }

    #[tokio::test]
    async fn ordinary_managed_write_projects_exact_incompatible_version_target_locally() {
        let fixture = Fixture::new().await;
        let peer_before = fixture.store.target_view_for(Target::Claude).await.unwrap();
        fixture.probe.set(ProbeState::Incompatible);

        let blocked = fixture
            .service
            .ensure_ordinary_write_allowed(Target::Codex, Some(ReconciliationContext::Codex), true)
            .await;
        assert_eq!(
            blocked.result.unwrap_err().problem.code,
            "incompatible-target-cli"
        );
        let publication = blocked
            .publication
            .expect("new incompatible classification must publish once");
        assert!(
            publication
                .problems
                .iter()
                .any(|problem| problem.code == "incompatible-target-cli")
        );
        let compatibility = fixture
            .store
            .compatibility_for(Target::Codex)
            .await
            .unwrap();
        assert_eq!(compatibility.version, "unavailable");
        assert_eq!(
            compatibility.classification,
            CompatibilityClassification::Incompatible
        );
        assert_eq!(
            fixture.store.target_view_for(Target::Claude).await.unwrap(),
            peer_before
        );
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
