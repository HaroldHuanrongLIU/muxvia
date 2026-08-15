use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use secrecy::{ExposeSecret, SecretString};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify},
};
use uuid::Uuid;

use crate::{
    claude::{
        ClaudeCapability, ClaudeConfigCodec, ClaudeConfigSnapshot, ClaudeProbe, CommandClaudeProbe,
        DesiredClaudeState,
    },
    codex::config::ManagedCodexState,
};
use crate::{
    codex::{CodexCapability, CodexConfigCodec, CodexProbe, ConfigSnapshot, DesiredCodexState},
    control::protocol::{
        ActionOutcome, ActionStatus, ActivationMode, ClaudeHostManagedState,
        ClaudePreflightContext, ClaudeSelectorState, ControlProblem, Target, TargetAction,
    },
    domain::activation::ActivatedSnapshot,
    home::MuxviaHome,
    model::{
        ModelServer, ModelServerError, ModelServerHandle, ReservedListener, UpstreamTransport,
    },
    state::{
        ActionFailure, ActivationCommit, ActivationPreparation, ActivationRuntime,
        ManagedWriteStatus, RecoveryIntent, RecoveryState, StateStore,
    },
};

enum ConfigurationPreflight {
    Codex {
        codec: CodexConfigCodec,
        before: Box<ConfigSnapshot>,
    },
    Claude {
        codec: ClaudeConfigCodec,
        before: Box<ClaudeConfigSnapshot>,
    },
}

enum ActivationConfiguration {
    Codex {
        codec: CodexConfigCodec,
        before: Box<ConfigSnapshot>,
        desired: DesiredCodexState,
    },
    Claude {
        codec: ClaudeConfigCodec,
        before: ClaudeConfigSnapshot,
        desired: DesiredClaudeState,
    },
}

impl ActivationConfiguration {
    fn config_path(&self) -> &std::path::Path {
        match self {
            Self::Codex { codec, .. } => codec.config_path(),
            Self::Claude { codec, .. } => codec.settings_path(),
        }
    }

    fn pending_intent(&self, id: Uuid, action_id: Uuid, revision: u64) -> RecoveryIntent {
        match self {
            Self::Codex {
                codec,
                before,
                desired,
            } => RecoveryIntent::pending(
                id,
                action_id,
                codec.config_path().to_owned(),
                before.as_ref().clone(),
                desired.clone(),
                revision,
            ),
            Self::Claude {
                codec,
                before,
                desired,
            } => RecoveryIntent::pending_claude(
                id,
                action_id,
                codec.settings_path().to_owned(),
                before.clone(),
                desired.clone(),
                revision,
            ),
        }
    }

    fn atomic_apply(&self) -> Result<(), &'static str> {
        match self {
            Self::Codex {
                codec,
                before,
                desired,
            } => codec
                .atomic_apply(before, desired)
                .map_err(|problem| problem.code()),
            Self::Claude {
                codec,
                before,
                desired,
            } => codec
                .atomic_apply(before, desired)
                .map_err(|problem| problem.code()),
        }
    }

    fn verify(&self) -> Result<(), &'static str> {
        match self {
            Self::Codex {
                codec,
                before,
                desired,
            } => codec
                .verify(before, desired)
                .map_err(|problem| problem.code()),
            Self::Claude {
                codec,
                before,
                desired,
            } => codec
                .verify(before, desired)
                .map_err(|problem| problem.code()),
        }
    }

    fn restore_or_confirm_before(&self) -> Result<(), ()> {
        match self {
            Self::Codex {
                codec,
                before,
                desired,
            } => codec
                .restore_or_confirm_before(before, desired)
                .map_err(|_| ()),
            Self::Claude {
                codec,
                before,
                desired,
            } => codec
                .restore_or_confirm_before(before, desired)
                .map_err(|_| ()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationStep {
    Validate,
    BindListener,
    PersistRoutingCredential,
    Snapshot,
    RecoveryIntent,
    AtomicConfigWrite,
    ConfigVerify,
    StateAndReceiptCommit,
    RuntimeHandoff,
    PublishView,
}

pub trait ActivationObserver: Send + Sync {
    fn reached(&self, step: ActivationStep);
}

struct NoopObserver;

impl ActivationObserver for NoopObserver {
    fn reached(&self, _: ActivationStep) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationFailpoint {
    BindListener,
    PersistRoutingCredential,
    Snapshot,
    RecoveryIntent,
    AtomicConfigWrite,
    ConfigVerify,
    FinalCommit,
    RestoreVerify,
    RuntimeHandoff,
    PublishView,
}

#[derive(Clone)]
pub struct ActivationHooks {
    observer: Arc<dyn ActivationObserver>,
    failpoint: Option<ActivationFailpoint>,
    final_commit_pause: Option<Arc<ActivationPause>>,
}

impl Default for ActivationHooks {
    fn default() -> Self {
        Self {
            observer: Arc::new(NoopObserver),
            failpoint: None,
            final_commit_pause: None,
        }
    }
}

impl ActivationHooks {
    pub fn observed(observer: Arc<dyn ActivationObserver>) -> Self {
        Self {
            observer,
            failpoint: None,
            final_commit_pause: None,
        }
    }

    pub fn failing(failpoint: ActivationFailpoint) -> Self {
        Self {
            observer: Arc::new(NoopObserver),
            failpoint: Some(failpoint),
            final_commit_pause: None,
        }
    }

    pub fn pausing_final_commit(pause: Arc<ActivationPause>) -> Self {
        Self {
            observer: Arc::new(NoopObserver),
            failpoint: None,
            final_commit_pause: Some(pause),
        }
    }

    fn reached(&self, step: ActivationStep) -> Result<(), ()> {
        self.observer.reached(step);
        let fails = matches!(
            (self.failpoint, step),
            (
                Some(ActivationFailpoint::BindListener),
                ActivationStep::BindListener
            ) | (
                Some(ActivationFailpoint::PersistRoutingCredential),
                ActivationStep::PersistRoutingCredential
            ) | (
                Some(ActivationFailpoint::Snapshot),
                ActivationStep::Snapshot
            ) | (
                Some(ActivationFailpoint::RecoveryIntent),
                ActivationStep::RecoveryIntent
            ) | (
                Some(ActivationFailpoint::AtomicConfigWrite),
                ActivationStep::AtomicConfigWrite
            ) | (
                Some(ActivationFailpoint::ConfigVerify),
                ActivationStep::ConfigVerify
            ) | (
                Some(ActivationFailpoint::RestoreVerify),
                ActivationStep::ConfigVerify
            ) | (
                Some(ActivationFailpoint::FinalCommit),
                ActivationStep::StateAndReceiptCommit
            ) | (
                Some(ActivationFailpoint::RuntimeHandoff),
                ActivationStep::RuntimeHandoff
            ) | (
                Some(ActivationFailpoint::PublishView),
                ActivationStep::PublishView
            )
        );
        if fails { Err(()) } else { Ok(()) }
    }
}

#[derive(Default)]
pub struct ActivationPause {
    reached: Notify,
    release: Notify,
}

impl ActivationPause {
    pub async fn wait_until_reached(&self) {
        self.reached.notified().await;
    }

    pub fn release(&self) {
        self.release.notify_one();
    }
}

pub struct ActivateProviderCommand {
    pub action_id: Uuid,
    pub expected_revision: u64,
    pub provider_id: Uuid,
    pub mode: ActivationMode,
}

pub struct ActivationService {
    store: Arc<StateStore>,
    home: MuxviaHome,
    codex_probe: Arc<dyn CodexProbe>,
    claude_probe: Arc<dyn ClaudeProbe>,
    codex_executable: PathBuf,
    claude_executable: PathBuf,
    upstream: Arc<dyn UpstreamTransport>,
    codex_gate: Mutex<()>,
    claude_gate: Mutex<()>,
    codex_model: Mutex<Option<ModelServerHandle>>,
    claude_model: Mutex<Option<ModelServerHandle>>,
    hooks: ActivationHooks,
    configuration_home_override: Option<PathBuf>,
}

impl ActivationService {
    pub fn new(
        store: Arc<StateStore>,
        home: MuxviaHome,
        probe: Arc<dyn CodexProbe>,
        codex_executable: PathBuf,
        upstream: Arc<dyn UpstreamTransport>,
    ) -> Self {
        Self {
            store,
            home,
            codex_probe: probe,
            claude_probe: Arc::new(CommandClaudeProbe),
            codex_executable,
            claude_executable: PathBuf::from("/usr/bin/claude"),
            upstream,
            codex_gate: Mutex::new(()),
            claude_gate: Mutex::new(()),
            codex_model: Mutex::new(None),
            claude_model: Mutex::new(None),
            hooks: ActivationHooks::default(),
            configuration_home_override: None,
        }
    }

    pub fn with_hooks(mut self, hooks: ActivationHooks) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_claude_runtime(mut self, probe: Arc<dyn ClaudeProbe>, executable: PathBuf) -> Self {
        self.claude_probe = probe;
        self.claude_executable = executable;
        self
    }

    pub fn with_configuration_home_override(mut self, home: Option<PathBuf>) -> Self {
        self.configuration_home_override = home;
        self
    }

    pub async fn apply_raw(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
    ) -> Result<ActionOutcome, ActionFailure> {
        self.apply_raw_for(Target::Codex, action_id, expected_revision, raw_action)
            .await
    }

    pub async fn apply_raw_for(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
    ) -> Result<ActionOutcome, ActionFailure> {
        self.apply_raw_for_with_context(target, action_id, expected_revision, raw_action, None)
            .await
    }

    pub async fn apply_raw_for_with_context(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
        claude_context: Option<&ClaudePreflightContext>,
    ) -> Result<ActionOutcome, ActionFailure> {
        if let Some(outcome) = self.receipt_for_or_failure(target, action_id).await? {
            return Ok(outcome);
        }
        match serde_json::from_value(raw_action.clone()) {
            Ok(
                TargetAction::CreateProvider { .. }
                | TargetAction::UpdateProvider { .. }
                | TargetAction::ReorderProviders { .. }
                | TargetAction::DeleteProvider { .. }
                | TargetAction::DuplicateProvider { .. },
            ) => {
                let outcome = self
                    .store
                    .apply_provider_action_for(target, action_id, expected_revision, raw_action)
                    .await?;
                if outcome.status == ActionStatus::Applied {
                    self.store.publish_target_view(outcome.view.clone());
                }
                Ok(outcome)
            }
            Ok(TargetAction::ActivateProvider { provider_id, mode }) => {
                if target == Target::Claude && mode == ActivationMode::Direct {
                    return Err(self
                        .store
                        .failure_for(
                            target,
                            "unsupported-activation-mode",
                            "Claude Direct Activation is not available",
                        )
                        .await);
                }
                let provider_id = match Uuid::parse_str(&provider_id) {
                    Ok(provider_id) => provider_id,
                    Err(_) => {
                        return Err(self
                            .store
                            .failure_for(
                                target,
                                "incomplete-provider",
                                "Provider is missing or incomplete",
                            )
                            .await);
                    }
                };
                self.activate_for(
                    target,
                    ActivateProviderCommand {
                        action_id,
                        expected_revision,
                        provider_id,
                        mode,
                    },
                    claude_context,
                )
                .await
            }
            Err(_) => Err(self
                .store
                .failure_for(target, "invalid-provider", "Provider action is malformed")
                .await),
        }
    }

    pub async fn activate(
        &self,
        command: ActivateProviderCommand,
    ) -> Result<ActionOutcome, ActionFailure> {
        self.activate_for(Target::Codex, command, None).await
    }

    async fn activate_for(
        &self,
        target: Target,
        command: ActivateProviderCommand,
        claude_context: Option<&ClaudePreflightContext>,
    ) -> Result<ActionOutcome, ActionFailure> {
        if let Some(outcome) = self
            .receipt_for_or_failure(target, command.action_id)
            .await?
        {
            return Ok(outcome);
        }
        let _gate = self.gate_for(target).lock().await;
        if let Some(outcome) = self
            .receipt_for_or_failure(target, command.action_id)
            .await?
        {
            return Ok(outcome);
        }
        self.hooks.observer.reached(ActivationStep::Validate);
        if target == Target::Codex
            && self
                .configuration_home_override
                .as_ref()
                .is_some_and(|configured| configured != &self.home.user_home().join(".codex"))
        {
            return Err(self
                .store
                .failure_for(
                    target,
                    "unsupported-configuration-home",
                    "Only the default Codex configuration home is supported",
                )
                .await);
        }
        if target == Target::Claude {
            let Some(context) = claude_context else {
                return Err(self
                    .store
                    .failure_for(
                        target,
                        "preflight-context-required",
                        "Claude preflight context is required",
                    )
                    .await);
            };
            if matches!(
                context.selector_state,
                ClaudeSelectorState::Enabled | ClaudeSelectorState::UnknownNonempty
            ) || matches!(
                context.host_managed_state,
                ClaudeHostManagedState::Managed | ClaudeHostManagedState::Unknown
            ) {
                return Err(self
                    .store
                    .failure_for(
                        target,
                        "provider-mode-active",
                        "Claude provider mode prevents Target Takeover",
                    )
                    .await);
            }
        }
        let preparation = match self
            .store
            .prepare_activation_for(target, command.provider_id, command.expected_revision)
            .await
        {
            Ok(Ok(preparation)) => preparation,
            Ok(Err(failure)) => return Err(failure),
            Err(_) => {
                return Err(self.target_failure(target, "internal-failure").await);
            }
        };
        if command.mode == ActivationMode::Direct {
            if preparation.routing_requirement
                == crate::control::protocol::ProviderRoutingRequirement::TakeoverRequired
            {
                return Err(self
                    .store
                    .failure_for(
                        target,
                        "takeover-required",
                        "Provider requires Target Takeover for activation",
                    )
                    .await);
            }
            if preparation.prior_route_runtime.is_some() {
                return Err(self
                    .store
                    .failure_for(
                        target,
                        "takeover-active",
                        "Direct Activation is unavailable while Target Takeover is active",
                    )
                    .await);
            }
        }
        let (configuration, capability_problem) = match self
            .preflight_configuration(target, &preparation, claude_context)
            .await
        {
            Ok(result) => result,
            Err(code) => return Err(self.target_failure(target, code).await),
        };

        let (runtime, configuration, mut candidate) = match command.mode {
            ActivationMode::Direct => {
                let ConfigurationPreflight::Codex { codec, before } = configuration else {
                    return Err(self
                        .target_failure(target, "unsupported-activation-mode")
                        .await);
                };
                let desired = codec.desired_direct(
                    &preparation.model,
                    &preparation.base_url,
                    preparation.provider_credential.expose_secret(),
                );
                (
                    ActivationRuntime::Direct,
                    ActivationConfiguration::Codex {
                        codec,
                        before,
                        desired,
                    },
                    None,
                )
            }
            ActivationMode::Takeover => {
                let persisted_port = preparation.preferred_route_port;
                let model_slot = self.model_for(target).lock().await;
                let reserved = self
                    .ensure_model_candidate(target, &model_slot, persisted_port)
                    .await;
                drop(model_slot);
                let (route_port, candidate) = match reserved {
                    Ok(result) => result,
                    Err(()) => {
                        return Err(self
                            .store
                            .failure_for(
                                target,
                                "configuration-write-failed",
                                "Could not reserve the local model route",
                            )
                            .await);
                    }
                };
                if self.hooks.reached(ActivationStep::BindListener).is_err() {
                    self.shutdown_candidate(candidate).await;
                    return Err(self.target_failure(target, "internal-failure").await);
                }
                let routing_credential = match preparation
                    .prior_route_runtime
                    .as_ref()
                    .map(|runtime| runtime.routing_credential.clone())
                {
                    Some(credential) => credential,
                    None => match random_credential() {
                        Ok(credential) => credential,
                        Err(()) => {
                            self.shutdown_candidate(candidate).await;
                            return Err(self.target_failure(target, "internal-failure").await);
                        }
                    },
                };
                if self
                    .hooks
                    .reached(ActivationStep::PersistRoutingCredential)
                    .is_err()
                {
                    self.shutdown_candidate(candidate).await;
                    return Err(self.target_failure(target, "internal-failure").await);
                }
                let configuration = match configuration {
                    ConfigurationPreflight::Codex { codec, before } => {
                        let desired = codec.desired_takeover(
                            &preparation.model,
                            &format!("http://127.0.0.1:{route_port}/v1"),
                            routing_credential.expose_secret(),
                        );
                        ActivationConfiguration::Codex {
                            codec,
                            before,
                            desired,
                        }
                    }
                    ConfigurationPreflight::Claude { codec, before } => {
                        let desired = codec.desired_takeover(
                            &preparation.model,
                            &format!("http://127.0.0.1:{route_port}"),
                            routing_credential.expose_secret(),
                        );
                        ActivationConfiguration::Claude {
                            codec,
                            before: *before,
                            desired,
                        }
                    }
                };
                (
                    ActivationRuntime::Takeover {
                        route_port,
                        routing_credential,
                    },
                    configuration,
                    candidate,
                )
            }
        };
        let snapshot = ActivatedSnapshot {
            id: Uuid::new_v4(),
            target,
            provider_id: preparation.provider_id,
            base_url: preparation.base_url,
            model: preparation.model.clone(),
            protocol: preparation.protocol,
            authentication: preparation.authentication,
            provider_credential: preparation.provider_credential,
            epoch: self.store.service_epoch(),
        };
        if self.hooks.reached(ActivationStep::Snapshot).is_err() {
            self.shutdown_candidate(candidate).await;
            return Err(self.target_failure(target, "internal-failure").await);
        }
        let recovery_id = Uuid::new_v4();
        let intent =
            configuration.pending_intent(recovery_id, command.action_id, command.expected_revision);
        if self.store.insert_recovery_intent(&intent).await.is_err() {
            self.shutdown_candidate(candidate).await;
            return Err(self.target_failure(target, "internal-failure").await);
        }
        if self.hooks.reached(ActivationStep::RecoveryIntent).is_err() {
            self.rollback(&configuration, &intent, candidate).await?;
            return Err(self.target_failure(target, "internal-failure").await);
        }

        let activation = async {
            configuration.atomic_apply()?;
            self.hooks
                .reached(ActivationStep::AtomicConfigWrite)
                .map_err(|_| "configuration-write-failed")?;
            configuration.verify()?;
            self.hooks
                .reached(ActivationStep::ConfigVerify)
                .map_err(|_| "configuration-write-failed")?;
            self.hooks
                .reached(ActivationStep::StateAndReceiptCommit)
                .map_err(|_| "internal-failure")?;
            if let Some(pause) = &self.hooks.final_commit_pause {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
            self.store
                .commit_activation_for(
                    target,
                    command.action_id,
                    command.expected_revision,
                    snapshot,
                    runtime,
                    recovery_id,
                    configuration.config_path().to_string_lossy().into_owned(),
                    capability_problem,
                )
                .await
                .map_err(|_| "internal-failure")
        }
        .await;

        match activation {
            Ok(ActivationCommit::Applied(outcome)) => {
                if let Some(mut handle) = candidate.take() {
                    if self.hooks.reached(ActivationStep::RuntimeHandoff).is_err() {
                        handle.abort();
                        tokio::task::yield_now().await;
                    }
                    if handle.activate().await.is_err() {
                        return Err(self.committed_handoff_failure(&intent, Some(handle)).await);
                    }
                    *self.model_for(target).lock().await = Some(handle);
                }
                if self.hooks.reached(ActivationStep::PublishView).is_ok() {
                    self.store.publish_target_view(outcome.view.clone());
                }
                Ok(outcome)
            }
            Ok(ActivationCommit::Replayed(outcome)) => {
                self.rollback(&configuration, &intent, candidate).await?;
                Ok(outcome)
            }
            Ok(ActivationCommit::Stale(view)) => {
                self.rollback(&configuration, &intent, candidate).await?;
                Err(ActionFailure {
                    problem: crate::control::protocol::ControlProblem {
                        code: "stale-revision".into(),
                        message: "Target state changed; refresh and retry".into(),
                    },
                    authoritative_view: view,
                })
            }
            Ok(ActivationCommit::RecoveryRequired(view)) => {
                self.mark_required(&intent, candidate).await;
                Err(ActionFailure {
                    problem: crate::control::protocol::ControlProblem {
                        code: "recovery-required".into(),
                        message: "Managed writes are blocked until recovery is resolved".into(),
                    },
                    authoritative_view: view,
                })
            }
            Err(code) => {
                self.rollback(&configuration, &intent, candidate).await?;
                Err(self.target_failure(target, code).await)
            }
        }
    }

    pub async fn model_endpoint(&self) -> Option<std::net::SocketAddr> {
        self.model_endpoint_for(Target::Codex).await
    }

    pub async fn model_endpoint_for(&self, target: Target) -> Option<std::net::SocketAddr> {
        self.model_for(target)
            .lock()
            .await
            .as_ref()
            .and_then(|handle| handle.is_running().then_some(handle.endpoint()))
    }

    pub async fn bootstrap_committed_takeover(&self) -> Result<(), ModelServerError> {
        self.bootstrap_committed_takeover_for(Target::Codex).await
    }

    pub async fn bootstrap_committed_takeovers(&self) -> Result<(), ModelServerError> {
        for target in [Target::Codex, Target::Claude] {
            if let Err(error) = self.bootstrap_committed_takeover_for(target).await {
                let _ = self.shutdown_models().await;
                return Err(error);
            }
        }
        Ok(())
    }

    pub async fn bootstrap_committed_takeover_for(
        &self,
        target: Target,
    ) -> Result<(), ModelServerError> {
        let _gate = self.gate_for(target).lock().await;
        match self
            .store
            .managed_write_status_for(target)
            .await
            .map_err(|_| ModelServerError::State)?
        {
            ManagedWriteStatus::RecoveryRequired | ManagedWriteStatus::ConfigurationDrift => {
                return Ok(());
            }
            ManagedWriteStatus::Allowed => {}
        }
        let takeover = self
            .store
            .committed_takeover_for(target)
            .await
            .map_err(|_| ModelServerError::State)?;
        let Some(takeover) = takeover else {
            return Ok(());
        };
        if !self
            .configuration_matches_committed_takeover(target, takeover.route_port)
            .await?
        {
            self.store
                .mark_configuration_drift_for(target)
                .await
                .map_err(|_| ModelServerError::State)?;
            return Ok(());
        }
        let mut model = self.model_for(target).lock().await;
        if model.as_ref().is_some_and(ModelServerHandle::is_running) {
            return if model
                .as_ref()
                .is_some_and(|handle| handle.endpoint().port() == takeover.route_port)
            {
                Ok(())
            } else {
                Err(ModelServerError::Task)
            };
        }
        if model.is_some() {
            return Err(ModelServerError::Task);
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, takeover.route_port)).await?;
        let reserved = ReservedListener::new(listener)?;
        let handle = ModelServer::bind_reserved_for(
            reserved,
            target,
            Arc::clone(&self.store),
            Arc::clone(&self.upstream),
        )
        .await?;
        if handle.endpoint().port() != takeover.route_port || !handle.is_running() {
            return Err(ModelServerError::Task);
        }
        *model = Some(handle);
        Ok(())
    }

    pub async fn shutdown_model(&self) -> Result<(), ModelServerError> {
        self.shutdown_model_for(Target::Codex).await
    }

    pub async fn shutdown_model_for(&self, target: Target) -> Result<(), ModelServerError> {
        let model = self.model_for(target).lock().await.take();
        if let Some(model) = model {
            model.shutdown().await
        } else {
            Ok(())
        }
    }

    pub async fn shutdown_models(&self) -> Result<(), ModelServerError> {
        let codex = self.shutdown_model_for(Target::Codex).await;
        let claude = self.shutdown_model_for(Target::Claude).await;
        codex.and(claude)
    }

    async fn configuration_matches_committed_takeover(
        &self,
        target: Target,
        route_port: u16,
    ) -> Result<bool, ModelServerError> {
        let snapshot = self
            .store
            .activated_snapshot_for(target)
            .await
            .map_err(|_| ModelServerError::State)?
            .ok_or(ModelServerError::State)?;
        let routing_credential = self
            .store
            .routing_credential_for(target)
            .await
            .map_err(|_| ModelServerError::State)?
            .ok_or(ModelServerError::State)?;
        match target {
            Target::Codex => {
                let codec = CodexConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|_| ModelServerError::State)?;
                let expected = codec.desired_takeover(
                    snapshot.model(),
                    &format!("http://127.0.0.1:{route_port}/v1"),
                    routing_credential.expose_secret(),
                );
                Ok(matches!(
                    codec.inspect_managed_state(&expected),
                    Ok(ManagedCodexState::Takeover { .. })
                ))
            }
            Target::Claude => {
                let codec = ClaudeConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|_| ModelServerError::State)?;
                let expected = codec.desired_takeover(
                    snapshot.model(),
                    &format!("http://127.0.0.1:{route_port}"),
                    routing_credential.expose_secret(),
                );
                Ok(codec.inspect_takeover(&expected).is_ok())
            }
        }
    }

    #[doc(hidden)]
    pub async fn abort_model(&self) {
        if let Some(model) = self.codex_model.lock().await.as_mut() {
            model.abort();
        }
        tokio::task::yield_now().await;
    }

    #[doc(hidden)]
    pub async fn abort_model_for(&self, target: Target) {
        if let Some(model) = self.model_for(target).lock().await.as_mut() {
            model.abort();
        }
        tokio::task::yield_now().await;
    }

    fn gate_for(&self, target: Target) -> &Mutex<()> {
        match target {
            Target::Codex => &self.codex_gate,
            Target::Claude => &self.claude_gate,
        }
    }

    fn model_for(&self, target: Target) -> &Mutex<Option<ModelServerHandle>> {
        match target {
            Target::Codex => &self.codex_model,
            Target::Claude => &self.claude_model,
        }
    }

    async fn preflight_configuration(
        &self,
        target: Target,
        preparation: &ActivationPreparation,
        claude_context: Option<&ClaudePreflightContext>,
    ) -> Result<(ConfigurationPreflight, Option<ControlProblem>), &'static str> {
        match target {
            Target::Codex => {
                let capability_problem = match self.codex_probe.probe(&self.codex_executable) {
                    Ok(CodexCapability::Tested { .. }) => None,
                    Ok(CodexCapability::UnknownCompatible { warning, .. }) => {
                        Some(ControlProblem {
                            code: "untested-target-cli".into(),
                            message: warning,
                        })
                    }
                    Err(_) => return Err("incompatible-target-cli"),
                };
                let codec = CodexConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|problem| problem.code())?;
                let before = self.inspect_config(&codec, preparation)?;
                Ok((
                    ConfigurationPreflight::Codex {
                        codec,
                        before: Box::new(before),
                    },
                    capability_problem,
                ))
            }
            Target::Claude => {
                let context = claude_context.ok_or("preflight-context-required")?;
                let codec = ClaudeConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|problem| problem.code())?;
                let expected = match (
                    &preparation.prior_snapshot,
                    &preparation.prior_route_runtime,
                ) {
                    (Some(snapshot), Some(runtime)) => Some(codec.desired_takeover(
                        &snapshot.model,
                        &format!("http://127.0.0.1:{}", runtime.route_port),
                        runtime.routing_credential.expose_secret(),
                    )),
                    (None, None) => None,
                    (Some(_), None) | (None, Some(_)) => return Err("recovery-required"),
                };
                let (_, before) = codec
                    .preflight_snapshot(context, expected.as_ref())
                    .map_err(|problem| match problem.code() {
                        "configuration-collision" if expected.is_some() => "recovery-required",
                        code => code,
                    })?;
                let capability_problem = match self.claude_probe.probe(&self.claude_executable) {
                    Ok(ClaudeCapability::Tested { .. }) => None,
                    Ok(ClaudeCapability::UnknownCompatible { warning, .. }) => {
                        Some(ControlProblem {
                            code: "untested-target-cli".into(),
                            message: warning,
                        })
                    }
                    Err(_) => return Err("incompatible-target-cli"),
                };
                Ok((
                    ConfigurationPreflight::Claude {
                        codec,
                        before: Box::new(before),
                    },
                    capability_problem,
                ))
            }
        }
    }

    fn inspect_config(
        &self,
        codec: &CodexConfigCodec,
        preparation: &ActivationPreparation,
    ) -> Result<crate::codex::ConfigSnapshot, &'static str> {
        match (
            &preparation.prior_snapshot,
            &preparation.prior_route_runtime,
        ) {
            (Some(snapshot), Some(runtime)) => {
                let expected = codec.desired_takeover(
                    &snapshot.model,
                    &format!("http://127.0.0.1:{}/v1", runtime.route_port),
                    runtime.routing_credential.expose_secret(),
                );
                match codec.inspect_managed_state(&expected).map_err(|problem| {
                    match problem.code() {
                        "configuration-collision" => "recovery-required",
                        code => code,
                    }
                })? {
                    ManagedCodexState::Takeover { snapshot } => Ok(snapshot),
                    _ => Err("recovery-required"),
                }
            }
            (Some(snapshot), None) => {
                let expected = codec.desired_direct(
                    &snapshot.model,
                    &snapshot.base_url,
                    snapshot.provider_credential.expose_secret(),
                );
                match codec.inspect_managed_state(&expected).map_err(|problem| {
                    match problem.code() {
                        "configuration-collision" => "recovery-required",
                        code => code,
                    }
                })? {
                    ManagedCodexState::Direct { snapshot } => Ok(snapshot),
                    _ => Err("recovery-required"),
                }
            }
            (None, None) => codec.inspect().map_err(|problem| problem.code()),
            (None, Some(_)) => Err("recovery-required"),
        }
    }

    async fn ensure_model_candidate(
        &self,
        target: Target,
        slot: &Option<ModelServerHandle>,
        persisted_port: Option<u16>,
    ) -> Result<(u16, Option<ModelServerHandle>), ()> {
        if let Some(handle) = slot.as_ref() {
            if !handle.is_running() {
                return Err(());
            }
            let port = handle.endpoint().port();
            return if persisted_port.is_none_or(|expected| expected == port) {
                Ok((port, None))
            } else {
                Err(())
            };
        }
        let port = persisted_port.unwrap_or(0);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| ())?;
        let reserved = ReservedListener::new(listener).map_err(|_| ())?;
        let endpoint = reserved.endpoint().port();
        let handle = ModelServer::bind_reserved_staged_for(
            reserved,
            target,
            Arc::clone(&self.store),
            Arc::clone(&self.upstream),
        )
        .await
        .map_err(|_| ())?;
        Ok((endpoint, Some(handle)))
    }

    async fn rollback(
        &self,
        configuration: &ActivationConfiguration,
        intent: &RecoveryIntent,
        candidate: Option<ModelServerHandle>,
    ) -> Result<(), ActionFailure> {
        let restored = self.hooks.failpoint != Some(ActivationFailpoint::RestoreVerify)
            && configuration.restore_or_confirm_before().is_ok();
        if restored
            && self
                .store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .is_ok()
        {
            self.shutdown_candidate(candidate).await;
            return Ok(());
        }
        self.mark_required(intent, candidate).await;
        Err(self
            .store
            .failure_for(
                intent.target(),
                "recovery-required",
                "Managed configuration requires recovery",
            )
            .await)
    }

    async fn mark_required(&self, intent: &RecoveryIntent, candidate: Option<ModelServerHandle>) {
        let _ = self
            .store
            .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
            .await;
        self.shutdown_candidate(candidate).await;
        let _ = self.shutdown_model_for(intent.target()).await;
    }

    async fn committed_handoff_failure(
        &self,
        intent: &RecoveryIntent,
        candidate: Option<ModelServerHandle>,
    ) -> ActionFailure {
        self.mark_required(intent, candidate).await;
        let failure = self
            .store
            .failure_for(
                intent.target(),
                "recovery-required",
                "Committed model runtime requires recovery",
            )
            .await;
        self.store
            .publish_target_view(failure.authoritative_view.clone());
        failure
    }

    async fn shutdown_candidate(&self, candidate: Option<ModelServerHandle>) {
        if let Some(handle) = candidate {
            let _ = handle.shutdown().await;
        }
    }

    async fn receipt_for_or_failure(
        &self,
        target: Target,
        action_id: Uuid,
    ) -> Result<Option<ActionOutcome>, ActionFailure> {
        match self.store.receipt_for(target, action_id).await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(self
                .store
                .failure_for(target, "state-store-error", "State store operation failed")
                .await),
        }
    }

    async fn target_failure(&self, target: Target, code: &str) -> ActionFailure {
        let stable = match code {
            "stale-revision"
            | "incomplete-provider"
            | "incompatible-target-cli"
            | "unsupported-configuration-home"
            | "preflight-context-required"
            | "provider-mode-active"
            | "shadowing-configuration"
            | "invalid-configuration"
            | "unsupported-activation-mode"
            | "takeover-required"
            | "takeover-active"
            | "configuration-drift"
            | "configuration-collision"
            | "configuration-write-failed"
            | "recovery-required" => code,
            _ => "internal-failure",
        };
        if stable == "internal-failure" {
            let message = format!("Activation failed (correlation {})", Uuid::new_v4());
            self.store.failure_for(target, stable, &message).await
        } else {
            self.store
                .failure_for(target, stable, "Activation failed")
                .await
        }
    }
}

fn random_credential() -> Result<SecretString, ()> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ())?;
    let mut encoded = String::with_capacity(64);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    Ok(SecretString::from(encoded))
}
