use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use secrecy::{ExposeSecret, SecretString};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify},
};
use uuid::Uuid;

use crate::{
    claude::{
        ClaudeCapability, ClaudeConfigCodec, ClaudeConfigOwnership, ClaudeConfigSnapshot,
        ClaudeProbe, CommandClaudeProbe, DesiredClaudeState, ManagedClaudeState,
    },
    codex::config::ManagedCodexState,
};
use crate::{
    codex::{CodexCapability, CodexConfigCodec, CodexProbe, ConfigSnapshot, DesiredCodexState},
    control::protocol::{
        ActionOutcome, ActionStatus, ActivationMode, ClaudeBlockingSelector,
        ClaudeHostManagedState, ClaudePreflightContext, ClaudeSelectorState,
        CompatibilityClassification, CompatibilityView, ControlProblem, Target, TargetAction,
    },
    domain::activation::ActivatedSnapshot,
    home::MuxviaHome,
    model::{
        ModelServer, ModelServerError, ModelServerHandle, ReservedListener, RouteHealthRuntime,
        UpstreamTransport,
    },
    state::{
        ActionFailure, ActivationCommit, ActivationPreparation, ActivationRuntime,
        ManagedWriteStatus, RecoveryIntent, RecoveryPayload, RecoveryState, StateError, StateStore,
    },
};

use super::reconcile::{ReconciliationRuntime, ReconciliationTargetRuntime};

enum ConfigurationPreflight {
    Codex {
        codec: CodexConfigCodec,
        before: Box<ConfigSnapshot>,
        recovery_before: Box<ConfigSnapshot>,
        ownership: Option<Box<DesiredCodexState>>,
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
        recovery_before: Box<ConfigSnapshot>,
        desired: DesiredCodexState,
    },
    Claude {
        codec: ClaudeConfigCodec,
        before: ClaudeConfigSnapshot,
        desired: DesiredClaudeState,
    },
}

enum CommittedConfigurationStatus {
    Matches,
    ConfigurationDrift,
    RecoveryRequired(Uuid),
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
                ..
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

    fn committed_recovery_payload_json(&self) -> Result<Option<String>, &'static str> {
        match self {
            Self::Codex {
                recovery_before,
                desired,
                ..
            } => serde_json::to_string(&RecoveryPayload::Codex {
                before: recovery_before.clone(),
                desired: Box::new(desired.clone()),
            })
            .map(Some)
            .map_err(|_| "internal-failure"),
            Self::Claude { .. } => Ok(None),
        }
    }

    fn atomic_apply(&self) -> Result<(), &'static str> {
        match self {
            Self::Codex {
                codec,
                before,
                desired,
                ..
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
                ..
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
                ..
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

    fn pre_intent_configuration_is_current(&self) -> bool {
        match self {
            Self::Codex { .. } => true,
            Self::Claude { codec, before, .. } => codec.matches_pre_intent_snapshot(before),
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
    codex_gate: Arc<Mutex<()>>,
    claude_gate: Arc<Mutex<()>>,
    codex_model: Arc<Mutex<Option<ModelServerHandle>>>,
    claude_model: Arc<Mutex<Option<ModelServerHandle>>>,
    codex_route_health: Arc<RouteHealthRuntime>,
    claude_route_health: Arc<RouteHealthRuntime>,
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
            codex_gate: Arc::new(Mutex::new(())),
            claude_gate: Arc::new(Mutex::new(())),
            codex_model: Arc::new(Mutex::new(None)),
            claude_model: Arc::new(Mutex::new(None)),
            codex_route_health: Arc::new(RouteHealthRuntime::default()),
            claude_route_health: Arc::new(RouteHealthRuntime::default()),
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

    pub(crate) fn reconciliation_runtime(&self) -> ReconciliationRuntime {
        ReconciliationRuntime {
            home: self.home.clone(),
            codex_probe: Arc::clone(&self.codex_probe),
            claude_probe: Arc::clone(&self.claude_probe),
            codex_executable: self.codex_executable.clone(),
            claude_executable: self.claude_executable.clone(),
            configuration_home_override: self.configuration_home_override.clone(),
            target_runtime: ReconciliationTargetRuntime::new(
                Arc::clone(&self.codex_model),
                Arc::clone(&self.claude_model),
                Arc::clone(&self.codex_gate),
                Arc::clone(&self.claude_gate),
            ),
        }
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
        let _gate = self.gate_for(target).lock().await;
        self.apply_raw_for_with_context_already_held(
            target,
            action_id,
            expected_revision,
            raw_action,
            claude_context,
        )
        .await
    }

    pub(crate) async fn apply_raw_for_with_context_already_held(
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
                if outcome.status == ActionStatus::Applied
                    && self
                        .store
                        .publish_target_view(outcome.view.clone())
                        .await
                        .is_err()
                {
                    return Err(self.target_failure(target, "state-store-error").await);
                }
                Ok(outcome)
            }
            Ok(TargetAction::ActivateProvider { provider_id, mode }) => {
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
                self.activate_for_after_gate(
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
            Ok(
                TargetAction::Reconcile { .. }
                | TargetAction::ResolveCompatibility(_)
                | TargetAction::SaveFailoverDraft(_)
                | TargetAction::ApplyFailoverChain(_),
            ) => Err(self
                .store
                .failure_for(target, "invalid-provider", "Provider action is malformed")
                .await),
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
        self.activate_for_after_gate(target, command, claude_context)
            .await
    }

    async fn activate_for_after_gate(
        &self,
        target: Target,
        command: ActivateProviderCommand,
        claude_context: Option<&ClaudePreflightContext>,
    ) -> Result<ActionOutcome, ActionFailure> {
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
            if !context.has_valid_blocking_selector() {
                return Err(self
                    .store
                    .failure_for(
                        target,
                        "preflight-context-required",
                        "Claude preflight context is incomplete",
                    )
                    .await);
            }
            if matches!(
                context.selector_state,
                ClaudeSelectorState::Enabled | ClaudeSelectorState::UnknownNonempty
            ) || matches!(
                context.host_managed_state,
                ClaudeHostManagedState::Managed | ClaudeHostManagedState::Unknown
            ) {
                let selector = context.blocking_selector;
                return Err(self
                    .target_failure_with_projection(
                        target,
                        "provider-mode-active",
                        Some("control-plane-context".to_owned()),
                        selector,
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
            Err(problem) => {
                if target == Target::Claude
                    && problem.code == "recovery-required"
                    && let Some(recovery_id) = preparation.prior_recovery_id
                {
                    let view = match self
                        .store
                        .mark_current_activation_recovery_required(target, recovery_id)
                        .await
                    {
                        Ok(view) => view,
                        Err(_) => {
                            return Err(self.target_failure(target, "internal-failure").await);
                        }
                    };
                    return Err(ActionFailure {
                        problem: ControlProblem {
                            code: "recovery-required".to_owned(),
                            message: "Managed configuration requires recovery".to_owned(),
                            source: problem.source,
                            selector: problem.selector,
                        },
                        authoritative_view: view,
                    });
                }
                return Err(self
                    .target_failure_with_projection(
                        target,
                        problem.code,
                        problem.source,
                        problem.selector,
                    )
                    .await);
            }
        };

        let (runtime, configuration, mut candidate) = match command.mode {
            ActivationMode::Direct => match configuration {
                ConfigurationPreflight::Codex {
                    codec,
                    before,
                    recovery_before,
                    ownership,
                } => {
                    let desired = match ownership.as_deref() {
                        Some(ownership) => codec.desired_direct_with_ownership(
                            &preparation.model,
                            &preparation.base_url,
                            preparation.provider_credential.expose_secret(),
                            ownership,
                        ),
                        None => codec.desired_direct(
                            &preparation.model,
                            &preparation.base_url,
                            preparation.provider_credential.expose_secret(),
                        ),
                    };
                    (
                        ActivationRuntime::Direct,
                        ActivationConfiguration::Codex {
                            codec,
                            before,
                            recovery_before,
                            desired,
                        },
                        None,
                    )
                }
                ConfigurationPreflight::Claude { codec, before } => {
                    let desired = match codec.desired_direct(
                        &preparation.model,
                        &preparation.base_url,
                        preparation.authentication,
                        preparation.provider_credential.expose_secret(),
                    ) {
                        Ok(desired) => desired,
                        Err(problem) => {
                            return Err(self.target_failure(target, problem.code()).await);
                        }
                    };
                    (
                        ActivationRuntime::Direct,
                        ActivationConfiguration::Claude {
                            codec,
                            before: *before,
                            desired,
                        },
                        None,
                    )
                }
            },
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
                    ConfigurationPreflight::Codex {
                        codec,
                        before,
                        recovery_before,
                        ownership,
                    } => {
                        let desired = match ownership.as_deref() {
                            Some(ownership) => codec.desired_takeover_with_ownership(
                                &preparation.model,
                                &format!("http://127.0.0.1:{route_port}/v1"),
                                routing_credential.expose_secret(),
                                ownership,
                            ),
                            None => codec.desired_takeover(
                                &preparation.model,
                                &format!("http://127.0.0.1:{route_port}/v1"),
                                routing_credential.expose_secret(),
                            ),
                        };
                        ActivationConfiguration::Codex {
                            codec,
                            before,
                            recovery_before,
                            desired,
                        }
                    }
                    ConfigurationPreflight::Claude { codec, before } => {
                        let desired = codec.desired_takeover_with_ownership(
                            &preparation.model,
                            &format!("http://127.0.0.1:{route_port}"),
                            routing_credential.expose_secret(),
                            before.ownership(),
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
        if !configuration.pre_intent_configuration_is_current() {
            self.shutdown_candidate(candidate).await;
            return Err(self
                .target_failure(target, "configuration-write-failed")
                .await);
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
            let committed_recovery_payload_json =
                configuration.committed_recovery_payload_json()?;
            self.store
                .commit_activation_for_with_recovery_payload(
                    target,
                    command.action_id,
                    command.expected_revision,
                    snapshot,
                    runtime,
                    match target {
                        Target::Codex => 1,
                        Target::Claude => 2,
                    },
                    recovery_id,
                    configuration.config_path().to_string_lossy().into_owned(),
                    capability_problem,
                    committed_recovery_payload_json,
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
                        return self.committed_handoff_recovery(&intent, Some(handle)).await;
                    }
                    *self.model_for(target).lock().await = Some(handle);
                }
                if self.hooks.reached(ActivationStep::PublishView).is_ok()
                    && self
                        .store
                        .publish_target_view(outcome.view.clone())
                        .await
                        .is_err()
                {
                    return Err(self.target_failure(target, "state-store-error").await);
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
                        source: None,
                        selector: None,
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
                        source: None,
                        selector: None,
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
            let view = self
                .store
                .target_view_for(target)
                .await
                .map_err(|_| ModelServerError::State)?;
            if view.problems.iter().any(|problem| {
                matches!(
                    problem.code.as_str(),
                    "startup-reconciliation-failed" | "model-route-unavailable"
                )
            }) {
                continue;
            }
            if let Err(error) = self.bootstrap_committed_takeover_for(target).await {
                if !matches!(
                    error,
                    ModelServerError::TargetState | ModelServerError::TargetConfiguration
                ) {
                    let _ = self.shutdown_models().await;
                    return Err(error);
                }
                self.store
                    .record_startup_problem_for(
                        target,
                        "model-route-unavailable",
                        "The committed model route could not be resumed",
                    )
                    .await
                    .map_err(|_| ModelServerError::State)?;
                continue;
            }
            self.store
                .clear_startup_problems_for(target)
                .await
                .map_err(|_| ModelServerError::State)?;
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
            .map_err(classify_bootstrap_state_error)?;
        let Some(takeover) = takeover else {
            return Ok(());
        };
        match self
            .configuration_matches_committed_takeover(target, &takeover)
            .await?
        {
            CommittedConfigurationStatus::Matches => {}
            CommittedConfigurationStatus::ConfigurationDrift => {
                self.store
                    .mark_configuration_drift_for(target)
                    .await
                    .map_err(|_| ModelServerError::State)?;
                return Ok(());
            }
            CommittedConfigurationStatus::RecoveryRequired(recovery_id) => {
                self.store
                    .mark_current_activation_recovery_required(target, recovery_id)
                    .await
                    .map_err(|_| ModelServerError::State)?;
                return Ok(());
            }
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
        let handle = ModelServer::bind_reserved_for_with_health(
            reserved,
            target,
            Arc::clone(&self.store),
            Arc::clone(&self.upstream),
            Arc::clone(self.route_health_for(target)),
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
        takeover: &crate::state::CommittedTakeover,
    ) -> Result<CommittedConfigurationStatus, ModelServerError> {
        let snapshot = self
            .store
            .activated_snapshot_for(target)
            .await
            .map_err(classify_bootstrap_state_error)?
            .ok_or(ModelServerError::TargetState)?;
        let routing_credential = self
            .store
            .routing_credential_for(target)
            .await
            .map_err(classify_bootstrap_state_error)?
            .ok_or(ModelServerError::TargetState)?;
        match target {
            Target::Codex => {
                let codec = CodexConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|_| ModelServerError::TargetConfiguration)?;
                let expected = match takeover.recovery_expectation.as_ref() {
                    Some(recovery) => {
                        let RecoveryPayload::Codex { desired, .. } = &recovery.payload else {
                            return Ok(CommittedConfigurationStatus::RecoveryRequired(recovery.id));
                        };
                        let expected = codec.desired_takeover_with_ownership(
                            snapshot.model(),
                            &format!("http://127.0.0.1:{}/v1", takeover.route_port),
                            routing_credential.expose_secret(),
                            desired,
                        );
                        if expected != **desired {
                            return Ok(CommittedConfigurationStatus::RecoveryRequired(recovery.id));
                        }
                        expected
                    }
                    None => codec.desired_takeover(
                        snapshot.model(),
                        &format!("http://127.0.0.1:{}/v1", takeover.route_port),
                        routing_credential.expose_secret(),
                    ),
                };
                Ok(
                    if matches!(
                        codec.inspect_managed_state(&expected),
                        Ok(ManagedCodexState::Takeover { .. })
                    ) {
                        CommittedConfigurationStatus::Matches
                    } else {
                        CommittedConfigurationStatus::ConfigurationDrift
                    },
                )
            }
            Target::Claude => {
                let codec = ClaudeConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|_| ModelServerError::TargetConfiguration)?;
                let ownership = ClaudeConfigOwnership::from_managed_config_version(
                    takeover.managed_config_version,
                )
                .ok_or(ModelServerError::TargetState)?;
                let expected = codec.desired_takeover_with_ownership(
                    snapshot.model(),
                    &format!("http://127.0.0.1:{}", takeover.route_port),
                    routing_credential.expose_secret(),
                    ownership,
                );
                if codec.inspect_takeover(&expected).is_err() {
                    return Ok(CommittedConfigurationStatus::ConfigurationDrift);
                }
                let Some(recovery) = takeover.recovery_expectation.as_ref() else {
                    return if ownership == ClaudeConfigOwnership::LegacyThree {
                        Ok(CommittedConfigurationStatus::Matches)
                    } else {
                        Err(ModelServerError::TargetState)
                    };
                };
                let RecoveryPayload::Claude { before, desired } = &recovery.payload else {
                    return Ok(CommittedConfigurationStatus::RecoveryRequired(recovery.id));
                };
                if desired.as_ref() != &expected || before.ownership() != ownership {
                    return Ok(CommittedConfigurationStatus::RecoveryRequired(recovery.id));
                }
                if matches!(
                    codec.inspect_managed_state(Some((&expected, before.as_ref()))),
                    Ok(ManagedClaudeState::Takeover { .. })
                ) {
                    Ok(CommittedConfigurationStatus::Matches)
                } else {
                    Ok(CommittedConfigurationStatus::RecoveryRequired(recovery.id))
                }
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
    ) -> Result<(ConfigurationPreflight, Option<ControlProblem>), PreflightFailure> {
        match target {
            Target::Codex => {
                let capability_problem = match self.codex_probe.probe(&self.codex_executable) {
                    Ok(CodexCapability::Tested { .. }) => None,
                    Ok(CodexCapability::UnknownCompatible { version, .. }) => {
                        self.enforce_activation_compatibility(
                            target,
                            version,
                            CompatibilityClassification::UnknownCompatible,
                        )
                        .await?;
                        Some(ControlProblem {
                            code: "untested-target-cli".into(),
                            message: "Target CLI version is untested".into(),
                            source: None,
                            selector: None,
                        })
                    }
                    Err(problem) => {
                        self.enforce_activation_compatibility(
                            target,
                            problem.version().unwrap_or("unavailable").to_owned(),
                            CompatibilityClassification::Incompatible,
                        )
                        .await?;
                        unreachable!("incompatible activation compatibility never proceeds")
                    }
                };
                let codec = CodexConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|problem| PreflightFailure::new(problem.code()))?;
                let (before, ownership) = self
                    .inspect_config(&codec, preparation)
                    .map_err(PreflightFailure::new)?;
                let recovery_before = match preparation.prior_recovery_payload.as_ref() {
                    Some(RecoveryPayload::Codex { before, .. }) => before.clone(),
                    Some(_) => return Err(PreflightFailure::new("recovery-required")),
                    None => Box::new(before.clone()),
                };
                Ok((
                    ConfigurationPreflight::Codex {
                        codec,
                        before: Box::new(before),
                        recovery_before,
                        ownership: ownership.map(Box::new),
                    },
                    capability_problem,
                ))
            }
            Target::Claude => {
                let context = claude_context
                    .ok_or_else(|| PreflightFailure::new("preflight-context-required"))?;
                let codec = ClaudeConfigCodec::for_user_home(self.home.user_home())
                    .map_err(|problem| PreflightFailure::new(problem.code()))?;
                let committed_ownership = ClaudeConfigOwnership::from_managed_config_version(
                    preparation.managed_config_version,
                )
                .ok_or_else(|| PreflightFailure::new("recovery-required"))?;
                let (expected, ownership) = match (
                    &preparation.prior_snapshot,
                    &preparation.prior_route_runtime,
                ) {
                    (Some(snapshot), Some(runtime)) => (
                        Some(codec.desired_takeover_with_ownership(
                            &snapshot.model,
                            &format!("http://127.0.0.1:{}", runtime.route_port),
                            runtime.routing_credential.expose_secret(),
                            committed_ownership,
                        )),
                        committed_ownership,
                    ),
                    (Some(snapshot), None)
                        if committed_ownership == ClaudeConfigOwnership::FourField =>
                    {
                        (
                            Some(
                                codec
                                    .desired_direct(
                                        &snapshot.model,
                                        &snapshot.base_url,
                                        snapshot.authentication,
                                        snapshot.provider_credential.expose_secret(),
                                    )
                                    .map_err(|problem| PreflightFailure::new(problem.code()))?,
                            ),
                            committed_ownership,
                        )
                    }
                    (None, None) => (None, ClaudeConfigOwnership::FourField),
                    (Some(_), None) | (None, Some(_)) => {
                        return Err(PreflightFailure::new("recovery-required"));
                    }
                };
                let (_, committed_before) = codec
                    .preflight_snapshot_with_ownership(context, expected.as_ref(), ownership)
                    .map_err(|problem| {
                        let code = match problem.code() {
                            "configuration-collision" if expected.is_some() => "recovery-required",
                            code => code,
                        };
                        PreflightFailure {
                            code,
                            source: problem.source().map(str::to_owned),
                            selector: problem.selector(),
                        }
                    })?;
                let committed_expectation = match (
                    expected.as_ref(),
                    preparation.prior_recovery_payload.as_ref(),
                ) {
                    (None, None) => None,
                    (Some(expected), Some(RecoveryPayload::Claude { before, desired }))
                        if desired.as_ref() == expected && before.ownership() == ownership =>
                    {
                        Some((expected, before.as_ref()))
                    }
                    (Some(expected), None) if ownership == ClaudeConfigOwnership::LegacyThree => {
                        Some((expected, &committed_before))
                    }
                    _ => return Err(PreflightFailure::new("recovery-required")),
                };
                let managed =
                    codec
                        .inspect_managed_state(committed_expectation)
                        .map_err(|problem| PreflightFailure {
                            code: match problem.code() {
                                "configuration-collision" if expected.is_some() => {
                                    "recovery-required"
                                }
                                code => code,
                            },
                            source: problem.source().map(str::to_owned),
                            selector: problem.selector(),
                        })?;
                let managed_shape_is_valid = matches!(
                    (
                        managed,
                        &preparation.prior_snapshot,
                        &preparation.prior_route_runtime
                    ),
                    (ManagedClaudeState::Unmanaged { .. }, None, None)
                        | (ManagedClaudeState::Direct { .. }, Some(_), None)
                        | (ManagedClaudeState::Takeover { .. }, Some(_), Some(_))
                );
                if !managed_shape_is_valid {
                    return Err(PreflightFailure::new("recovery-required"));
                }
                let before = if ownership == ClaudeConfigOwnership::LegacyThree {
                    codec
                        .preflight_snapshot_with_ownership(
                            context,
                            None,
                            ClaudeConfigOwnership::FourField,
                        )
                        .map_err(|problem| PreflightFailure {
                            code: problem.code(),
                            source: problem.source().map(str::to_owned),
                            selector: problem.selector(),
                        })?
                        .1
                } else {
                    committed_before
                };
                let capability_problem = match self.claude_probe.probe(&self.claude_executable) {
                    Ok(ClaudeCapability::Tested { .. }) => None,
                    Ok(ClaudeCapability::UnknownCompatible { version, .. }) => {
                        self.enforce_activation_compatibility(
                            target,
                            version,
                            CompatibilityClassification::UnknownCompatible,
                        )
                        .await?;
                        Some(ControlProblem {
                            code: "untested-target-cli".into(),
                            message: "Target CLI version is untested".into(),
                            source: None,
                            selector: None,
                        })
                    }
                    Err(problem) => {
                        self.enforce_activation_compatibility(
                            target,
                            problem.version().unwrap_or("unavailable").to_owned(),
                            CompatibilityClassification::Incompatible,
                        )
                        .await?;
                        unreachable!("incompatible activation compatibility never proceeds")
                    }
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

    async fn enforce_activation_compatibility(
        &self,
        target: Target,
        version: String,
        classification: CompatibilityClassification,
    ) -> Result<(), PreflightFailure> {
        let acknowledgement_required = if classification
            == CompatibilityClassification::UnknownCompatible
        {
            match self.store.compatibility_for(target).await {
                Ok(saved) => {
                    saved.version != version
                        || saved.classification != CompatibilityClassification::UnknownCompatible
                        || saved.acknowledgement_required
                }
                Err(StateError::MissingCompatibility) => true,
                Err(_) => return Err(PreflightFailure::new("state-store-error")),
            }
        } else {
            false
        };
        let compatibility = CompatibilityView {
            version,
            classification,
            acknowledgement_required,
        };
        let code = match classification {
            CompatibilityClassification::Tested => return Ok(()),
            CompatibilityClassification::UnknownCompatible if !acknowledgement_required => {
                return Ok(());
            }
            CompatibilityClassification::UnknownCompatible => {
                "compatibility-acknowledgement-required"
            }
            CompatibilityClassification::Incompatible => "incompatible-target-cli",
        };
        self.store
            .project_managed_write_problem(target, code, None, compatibility)
            .await
            .map_err(|_| PreflightFailure::new("state-store-error"))?;
        Err(PreflightFailure::new(code))
    }

    fn inspect_config(
        &self,
        codec: &CodexConfigCodec,
        preparation: &ActivationPreparation,
    ) -> Result<(crate::codex::ConfigSnapshot, Option<DesiredCodexState>), &'static str> {
        match (
            &preparation.prior_snapshot,
            &preparation.prior_route_runtime,
        ) {
            (Some(snapshot), Some(runtime)) => {
                let ownership = match preparation.prior_recovery_payload.as_ref() {
                    Some(RecoveryPayload::Codex { desired, .. }) => Some(desired.as_ref()),
                    Some(_) => return Err("recovery-required"),
                    None => None,
                };
                let expected = match ownership {
                    Some(ownership) => codec.desired_takeover_with_ownership(
                        &snapshot.model,
                        &format!("http://127.0.0.1:{}/v1", runtime.route_port),
                        runtime.routing_credential.expose_secret(),
                        ownership,
                    ),
                    None => codec.desired_takeover(
                        &snapshot.model,
                        &format!("http://127.0.0.1:{}/v1", runtime.route_port),
                        runtime.routing_credential.expose_secret(),
                    ),
                };
                if ownership.is_some_and(|ownership| expected != *ownership) {
                    return Err("recovery-required");
                }
                match codec.inspect_managed_state(&expected).map_err(|problem| {
                    match problem.code() {
                        "configuration-collision" => "recovery-required",
                        code => code,
                    }
                })? {
                    ManagedCodexState::Takeover { snapshot } => {
                        Ok((snapshot, ownership.map(|_| expected)))
                    }
                    _ => Err("recovery-required"),
                }
            }
            (Some(snapshot), None) => {
                let ownership = match preparation.prior_recovery_payload.as_ref() {
                    Some(RecoveryPayload::Codex { desired, .. }) => Some(desired.as_ref()),
                    Some(_) => return Err("recovery-required"),
                    None => None,
                };
                let expected = match ownership {
                    Some(ownership) => codec.desired_direct_with_ownership(
                        &snapshot.model,
                        &snapshot.base_url,
                        snapshot.provider_credential.expose_secret(),
                        ownership,
                    ),
                    None => codec.desired_direct(
                        &snapshot.model,
                        &snapshot.base_url,
                        snapshot.provider_credential.expose_secret(),
                    ),
                };
                if ownership.is_some_and(|ownership| expected != *ownership) {
                    return Err("recovery-required");
                }
                match codec.inspect_managed_state(&expected).map_err(|problem| {
                    match problem.code() {
                        "configuration-collision" => "recovery-required",
                        code => code,
                    }
                })? {
                    ManagedCodexState::Direct { snapshot } => {
                        Ok((snapshot, ownership.map(|_| expected)))
                    }
                    _ => Err("recovery-required"),
                }
            }
            (None, None) if preparation.prior_recovery_payload.is_none() => codec
                .inspect()
                .map(|snapshot| (snapshot, None))
                .map_err(|problem| problem.code()),
            (None, None) => Err("recovery-required"),
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
        let handle = ModelServer::bind_reserved_staged_for_with_health(
            reserved,
            target,
            Arc::clone(&self.store),
            Arc::clone(&self.upstream),
            Arc::clone(self.route_health_for(target)),
        )
        .await
        .map_err(|_| ())?;
        Ok((endpoint, Some(handle)))
    }

    fn route_health_for(&self, target: Target) -> &Arc<RouteHealthRuntime> {
        match target {
            Target::Codex => &self.codex_route_health,
            Target::Claude => &self.claude_route_health,
        }
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

    async fn committed_handoff_recovery(
        &self,
        intent: &RecoveryIntent,
        candidate: Option<ModelServerHandle>,
    ) -> Result<ActionOutcome, ActionFailure> {
        self.shutdown_candidate(candidate).await;
        let _ = self.shutdown_model_for(intent.target()).await;
        let outcome = match self
            .store
            .mark_committed_activation_recovery_required(intent.target(), intent.id())
            .await
        {
            Ok(outcome) => outcome,
            Err(_) => {
                return Err(self
                    .target_failure(intent.target(), "internal-failure")
                    .await);
            }
        };
        if self
            .store
            .publish_target_view(outcome.view.clone())
            .await
            .is_err()
        {
            return Err(self
                .target_failure(intent.target(), "state-store-error")
                .await);
        }
        Ok(outcome)
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
        self.target_failure_with_projection(target, code, None, None)
            .await
    }

    async fn target_failure_with_projection(
        &self,
        target: Target,
        code: &str,
        source: Option<String>,
        selector: Option<ClaudeBlockingSelector>,
    ) -> ActionFailure {
        let stable = match code {
            "stale-revision"
            | "incomplete-provider"
            | "compatibility-acknowledgement-required"
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
            let mut failure = self
                .store
                .failure_for(target, stable, "Activation failed")
                .await;
            failure.problem.source = source;
            failure.problem.selector = selector;
            failure
        }
    }
}

struct PreflightFailure {
    code: &'static str,
    source: Option<String>,
    selector: Option<ClaudeBlockingSelector>,
}

fn classify_bootstrap_state_error(error: StateError) -> ModelServerError {
    match error {
        StateError::InvalidActivatedSnapshot => ModelServerError::TargetState,
        StateError::Unavailable
        | StateError::Io(_)
        | StateError::Sqlite(_)
        | StateError::Serialization(_)
        | StateError::InvalidRecoveryState
        | StateError::InvalidRecoveryPayload
        | StateError::MissingRecoveryIntent
        | StateError::InvalidProviderRoutingRequirement
        | StateError::InvalidCompatibilityState
        | StateError::MissingCompatibility => ModelServerError::State,
    }
}

impl PreflightFailure {
    fn new(code: &'static str) -> Self {
        Self {
            code,
            source: None,
            selector: None,
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
