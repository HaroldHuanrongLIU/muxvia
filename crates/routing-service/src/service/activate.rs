use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use secrecy::{ExposeSecret, SecretString};
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify},
};
use uuid::Uuid;

use crate::{
    codex::{CodexConfigCodec, CodexProbe},
    control::protocol::{ActionOutcome, ActionStatus, TakeoverMode, Target, TargetAction},
    domain::activation::ActivatedSnapshot,
    home::MuxviaHome,
    model::{
        ModelServer, ModelServerError, ModelServerHandle, ReservedListener, UpstreamTransport,
    },
    state::{
        ActionFailure, ActivationCommit, ActivationPreparation, RecoveryIntent, RecoveryState,
        StateStore,
    },
};

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
    AtomicConfigWrite,
    ConfigVerify,
    FinalCommit,
    RestoreVerify,
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
    pub mode: TakeoverMode,
}

pub struct ActivationService {
    store: Arc<StateStore>,
    home: MuxviaHome,
    probe: Arc<dyn CodexProbe>,
    codex_executable: PathBuf,
    upstream: Arc<dyn UpstreamTransport>,
    gate: Mutex<()>,
    model: Mutex<Option<ModelServerHandle>>,
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
            probe,
            codex_executable,
            upstream,
            gate: Mutex::new(()),
            model: Mutex::new(None),
            hooks: ActivationHooks::default(),
            configuration_home_override: None,
        }
    }

    pub fn with_hooks(mut self, hooks: ActivationHooks) -> Self {
        self.hooks = hooks;
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
        if let Some(outcome) = self.receipt_or_failure(action_id).await? {
            return Ok(outcome);
        }
        match serde_json::from_value(raw_action.clone()) {
            Ok(TargetAction::SaveProvider { .. }) => {
                let outcome = self
                    .store
                    .apply_save_provider_action(action_id, expected_revision, raw_action)
                    .await?;
                if outcome.status == ActionStatus::Applied {
                    self.store.publish_target_view(outcome.view.clone());
                }
                Ok(outcome)
            }
            Ok(TargetAction::ActivateProvider { provider_id, mode }) => {
                let provider_id = match Uuid::parse_str(&provider_id) {
                    Ok(provider_id) => provider_id,
                    Err(_) => {
                        return Err(self
                            .store
                            .failure("incomplete-provider", "Provider is missing or incomplete")
                            .await);
                    }
                };
                self.activate(ActivateProviderCommand {
                    action_id,
                    expected_revision,
                    provider_id,
                    mode,
                })
                .await
            }
            Err(_) => Err(self
                .store
                .failure("invalid-provider", "Provider action is malformed")
                .await),
        }
    }

    pub async fn activate(
        &self,
        command: ActivateProviderCommand,
    ) -> Result<ActionOutcome, ActionFailure> {
        if let Some(outcome) = self.receipt_or_failure(command.action_id).await? {
            return Ok(outcome);
        }
        let _gate = self.gate.lock().await;
        if let Some(outcome) = self.receipt_or_failure(command.action_id).await? {
            return Ok(outcome);
        }
        if command.mode != TakeoverMode::Takeover {
            return Err(self
                .activation_failure("internal-failure", "Activation failed")
                .await);
        }
        self.hooks.observer.reached(ActivationStep::Validate);
        if self
            .configuration_home_override
            .as_ref()
            .is_some_and(|configured| configured != &self.home.user_home().join(".codex"))
        {
            return Err(self
                .store
                .failure(
                    "unsupported-configuration-home",
                    "Only the default Codex configuration home is supported",
                )
                .await);
        }
        let preparation = match self
            .store
            .prepare_activation(command.provider_id, command.expected_revision)
            .await
        {
            Ok(Ok(preparation)) => preparation,
            Ok(Err(failure)) => return Err(failure),
            Err(_) => {
                return Err(self
                    .activation_failure("internal-failure", "Activation failed")
                    .await);
            }
        };
        if self.probe.probe(&self.codex_executable).is_err() {
            return Err(self
                .store
                .failure("incompatible-target-cli", "Codex CLI is not compatible")
                .await);
        }
        let codec = match CodexConfigCodec::for_user_home(self.home.user_home()) {
            Ok(codec) => codec,
            Err(problem) => return Err(self.codex_failure(problem.code()).await),
        };
        let before = match self.inspect_config(&codec, &preparation) {
            Ok(before) => before,
            Err(code) => return Err(self.codex_failure(code).await),
        };

        let mut model = self.model.lock().await;
        let (port, started_new) = match self
            .ensure_model_listener(&mut model, preparation.route_port)
            .await
        {
            Ok(result) => result,
            Err(()) => {
                return Err(self
                    .store
                    .failure(
                        "configuration-write-failed",
                        "Could not reserve the local model route",
                    )
                    .await);
            }
        };
        self.hooks.observer.reached(ActivationStep::BindListener);
        let routing_credential = match preparation.routing_credential.clone() {
            Some(credential) => credential,
            None => match random_credential() {
                Ok(credential) => credential,
                Err(()) => {
                    self.stop_new_listener(&mut model, started_new).await;
                    return Err(self
                        .activation_failure("internal-failure", "Activation failed")
                        .await);
                }
            },
        };
        self.hooks
            .observer
            .reached(ActivationStep::PersistRoutingCredential);
        let snapshot = ActivatedSnapshot {
            id: Uuid::new_v4(),
            target: Target::Codex,
            provider_id: preparation.provider_id,
            base_url: preparation.base_url,
            model: preparation.model.clone(),
            provider_credential: preparation.provider_credential,
            epoch: self.store.service_epoch(),
        };
        self.hooks.observer.reached(ActivationStep::Snapshot);
        let route_base = format!("http://127.0.0.1:{port}/v1");
        let desired = codec.desired(
            &preparation.model,
            &route_base,
            routing_credential.expose_secret(),
        );
        let recovery_id = Uuid::new_v4();
        let intent = RecoveryIntent::pending(
            recovery_id,
            command.action_id,
            codec.config_path().to_owned(),
            before.clone(),
            desired.clone(),
            command.expected_revision,
        );
        if self.store.insert_recovery_intent(&intent).await.is_err() {
            self.stop_new_listener(&mut model, started_new).await;
            return Err(self
                .activation_failure("internal-failure", "Activation failed")
                .await);
        }
        self.hooks.observer.reached(ActivationStep::RecoveryIntent);

        let activation = async {
            self.hooks
                .reached(ActivationStep::AtomicConfigWrite)
                .map_err(|_| "configuration-write-failed")?;
            codec
                .atomic_apply(&before, &desired)
                .map_err(|problem| problem.code())?;
            self.hooks
                .reached(ActivationStep::ConfigVerify)
                .map_err(|_| "configuration-write-failed")?;
            codec
                .verify(&before, &desired)
                .map_err(|problem| problem.code())?;
            self.hooks
                .reached(ActivationStep::StateAndReceiptCommit)
                .map_err(|_| "internal-failure")?;
            if let Some(pause) = &self.hooks.final_commit_pause {
                pause.reached.notify_one();
                pause.release.notified().await;
            }
            self.store
                .commit_activation(
                    command.action_id,
                    command.expected_revision,
                    snapshot,
                    port,
                    routing_credential,
                    recovery_id,
                    codec.config_path().to_string_lossy().into_owned(),
                )
                .await
                .map_err(|_| "internal-failure")
        }
        .await;

        match activation {
            Ok(ActivationCommit::Applied(outcome)) => {
                self.store.publish_target_view(outcome.view.clone());
                self.hooks.observer.reached(ActivationStep::PublishView);
                Ok(outcome)
            }
            Ok(ActivationCommit::Replayed(outcome)) => {
                self.rollback(&codec, &intent, started_new, &mut model)
                    .await?;
                Ok(outcome)
            }
            Ok(ActivationCommit::Stale(view)) => {
                self.rollback(&codec, &intent, started_new, &mut model)
                    .await?;
                Err(ActionFailure {
                    problem: crate::control::protocol::ControlProblem {
                        code: "stale-revision".into(),
                        message: "Target state changed; refresh and retry".into(),
                    },
                    authoritative_view: view,
                })
            }
            Ok(ActivationCommit::RecoveryRequired(view)) => {
                self.mark_required(&intent, started_new, &mut model).await;
                Err(ActionFailure {
                    problem: crate::control::protocol::ControlProblem {
                        code: "recovery-required".into(),
                        message: "Managed writes are blocked until recovery is resolved".into(),
                    },
                    authoritative_view: view,
                })
            }
            Err(code) => {
                self.rollback(&codec, &intent, started_new, &mut model)
                    .await?;
                Err(self.codex_failure(code).await)
            }
        }
    }

    pub async fn model_endpoint(&self) -> Option<std::net::SocketAddr> {
        self.model
            .lock()
            .await
            .as_ref()
            .and_then(|handle| handle.is_running().then_some(handle.endpoint()))
    }

    pub async fn bootstrap_committed_takeover(&self) -> Result<(), ModelServerError> {
        let _gate = self.gate.lock().await;
        let takeover = self
            .store
            .committed_takeover()
            .await
            .map_err(|_| ModelServerError::State)?;
        let Some(takeover) = takeover else {
            return Ok(());
        };
        let mut model = self.model.lock().await;
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
        let handle = ModelServer::bind_reserved(
            reserved,
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
        let model = self.model.lock().await.take();
        if let Some(model) = model {
            model.shutdown().await
        } else {
            Ok(())
        }
    }

    #[doc(hidden)]
    pub async fn abort_model(&self) {
        if let Some(model) = self.model.lock().await.as_mut() {
            model.abort();
        }
        tokio::task::yield_now().await;
    }

    fn inspect_config(
        &self,
        codec: &CodexConfigCodec,
        preparation: &ActivationPreparation,
    ) -> Result<crate::codex::ConfigSnapshot, &'static str> {
        match (
            &preparation.active_model,
            preparation.route_port,
            &preparation.routing_credential,
        ) {
            (Some(model), Some(port), Some(credential)) => {
                let expected = codec.desired(
                    model,
                    &format!("http://127.0.0.1:{port}/v1"),
                    credential.expose_secret(),
                );
                codec
                    .inspect_managed(&expected)
                    .map_err(|problem| problem.code())
            }
            (None, _, _) => codec.inspect().map_err(|problem| problem.code()),
            _ => Err("recovery-required"),
        }
    }

    async fn ensure_model_listener(
        &self,
        slot: &mut Option<ModelServerHandle>,
        persisted_port: Option<u16>,
    ) -> Result<(u16, bool), ()> {
        if let Some(handle) = slot.as_ref() {
            if !handle.is_running() {
                return Err(());
            }
            let port = handle.endpoint().port();
            return if persisted_port.is_none_or(|expected| expected == port) {
                Ok((port, false))
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
        let handle = ModelServer::bind_reserved(
            reserved,
            Arc::clone(&self.store),
            Arc::clone(&self.upstream),
        )
        .await
        .map_err(|_| ())?;
        *slot = Some(handle);
        Ok((endpoint, true))
    }

    async fn rollback(
        &self,
        codec: &CodexConfigCodec,
        intent: &RecoveryIntent,
        started_new: bool,
        model: &mut Option<ModelServerHandle>,
    ) -> Result<(), ActionFailure> {
        let restored = self.hooks.failpoint != Some(ActivationFailpoint::RestoreVerify)
            && codec
                .restore_or_confirm_before(intent.before(), intent.desired())
                .is_ok();
        if restored
            && self
                .store
                .set_recovery_state(intent.id(), RecoveryState::RolledBack)
                .await
                .is_ok()
        {
            self.stop_new_listener(model, started_new).await;
            return Ok(());
        }
        self.mark_required(intent, started_new, model).await;
        Err(self
            .store
            .failure(
                "recovery-required",
                "Managed configuration requires recovery",
            )
            .await)
    }

    async fn mark_required(
        &self,
        intent: &RecoveryIntent,
        started_new: bool,
        model: &mut Option<ModelServerHandle>,
    ) {
        let _ = self
            .store
            .set_recovery_state(intent.id(), RecoveryState::RecoveryRequired)
            .await;
        self.stop_new_listener(model, started_new).await;
    }

    async fn stop_new_listener(&self, slot: &mut Option<ModelServerHandle>, started_new: bool) {
        if started_new && let Some(handle) = slot.take() {
            let _ = handle.shutdown().await;
        }
    }

    async fn receipt_or_failure(
        &self,
        action_id: Uuid,
    ) -> Result<Option<ActionOutcome>, ActionFailure> {
        match self.store.receipt(action_id).await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Err(self
                .activation_failure("internal-failure", "Activation failed")
                .await),
        }
    }

    async fn codex_failure(&self, code: &str) -> ActionFailure {
        let stable = match code {
            "stale-revision"
            | "incomplete-provider"
            | "incompatible-target-cli"
            | "unsupported-configuration-home"
            | "configuration-collision"
            | "configuration-write-failed"
            | "recovery-required" => code,
            _ => "internal-failure",
        };
        self.activation_failure(stable, "Activation failed").await
    }

    async fn activation_failure(&self, code: &str, message: &str) -> ActionFailure {
        if code == "internal-failure" {
            let message = format!("Activation failed (correlation {})", Uuid::new_v4());
            self.store.failure(code, &message).await
        } else {
            self.store.failure(code, message).await
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
