use secrecy::{ExposeSecret, SecretString};
use tokio::sync::broadcast;
use tokio_rusqlite::{Connection, rusqlite::params};
use uuid::Uuid;

use crate::{
    control::protocol::{
        ActionOutcome, ActionStatus, ControlProblem, ProviderAuthentication, ProviderProtocol,
        ProviderRoutingRequirement, Target, TargetAction, TargetView,
    },
    domain::{
        activation::ActivatedSnapshot,
        provider::has_valid_provider_declaration,
        view::{empty_target_view, project_target_view, project_target_view_for},
    },
    home::MuxviaHome,
};

#[derive(Debug, thiserror::Error)]
#[error("{problem:?}")]
pub struct ActionFailure {
    pub problem: ControlProblem,
    pub authoritative_view: TargetView,
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("state store unavailable")]
    Unavailable,
    #[error("state store I/O failed")]
    Io(#[from] std::io::Error),
    #[error("state store operation failed")]
    Sqlite(#[from] tokio_rusqlite::rusqlite::Error),
    #[error("state store serialization failed")]
    Serialization(#[from] serde_json::Error),
    #[error("state store contains an invalid recovery state")]
    InvalidRecoveryState,
    #[error("state store contains an invalid recovery payload")]
    InvalidRecoveryPayload,
    #[error("recovery intent does not exist")]
    MissingRecoveryIntent,
    #[error("state store contains an invalid activated snapshot")]
    InvalidActivatedSnapshot,
    #[error("state store contains an invalid Provider routing requirement")]
    InvalidProviderRoutingRequirement,
}

pub struct StateStore {
    pub(super) connection: Connection,
    service_epoch: String,
    target_views: broadcast::Sender<TargetView>,
}

type ActivationPreparationRow = (
    u64,
    String,
    String,
    Option<u16>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

pub struct ActivationPreparation {
    pub provider_id: Uuid,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub authentication: ProviderAuthentication,
    pub provider_credential: SecretString,
    pub routing_requirement: ProviderRoutingRequirement,
    pub prior_snapshot: Option<CommittedActivationSnapshot>,
    pub prior_route_runtime: Option<CommittedRouteRuntime>,
    pub preferred_route_port: Option<u16>,
}

pub struct CommittedActivationSnapshot {
    pub base_url: String,
    pub model: String,
    pub provider_credential: SecretString,
}

pub struct CommittedRouteRuntime {
    pub route_port: u16,
    pub routing_credential: SecretString,
}

pub enum ActivationRuntime {
    Direct,
    Takeover {
        route_port: u16,
        routing_credential: SecretString,
    },
}

pub enum ActivationCommit {
    Applied(ActionOutcome),
    Replayed(ActionOutcome),
    Stale(TargetView),
    RecoveryRequired(TargetView),
}

pub struct CommittedTakeover {
    pub route_port: u16,
}

pub struct RoutingSnapshot {
    id: Uuid,
    provider_id: Uuid,
    base_url: String,
    model: String,
    provider_credential: SecretString,
    epoch: Uuid,
}

impl RoutingSnapshot {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn provider_id(&self) -> Uuid {
        self.provider_id
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn provider_credential(&self) -> &SecretString {
        &self.provider_credential
    }

    pub fn epoch(&self) -> Uuid {
        self.epoch
    }
}

impl StateStore {
    pub async fn open(home: &MuxviaHome) -> Result<Self, StateError> {
        home.prepare_database()?;
        let connection = Connection::open(home.database_path()).await?;
        connection
            .call(|connection| {
                super::migrations::migrate(connection)?;
                connection.execute_batch("PRAGMA foreign_keys = ON;")?;
                let mut foreign_key_check = connection.prepare("PRAGMA foreign_key_check")?;
                if foreign_key_check.query([])?.next()?.is_some() {
                    return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
                }
                Ok(())
            })
            .await
            .map_err(map_call_error)?;

        let (target_views, _) = broadcast::channel(32);
        Ok(Self {
            connection,
            service_epoch: Uuid::new_v4().to_string(),
            target_views,
        })
    }

    pub async fn target_view(&self) -> Result<TargetView, StateError> {
        self.target_view_for(Target::Codex).await
    }

    pub async fn target_view_for(&self, target: Target) -> Result<TargetView, StateError> {
        let service_epoch = self.service_epoch.clone();
        self.connection
            .call(move |connection| project_target_view_for(connection, &service_epoch, target))
            .await
            .map_err(map_call_error)
    }

    pub(crate) async fn provider_for_inspection(
        &self,
        provider_id: Uuid,
        provider_revision: u64,
    ) -> Result<super::providers::ProviderInspectionRead, StateError> {
        self.connection
            .call(move |connection| {
                super::providers::read_provider_for_inspection(
                    connection,
                    provider_id,
                    provider_revision,
                )
            })
            .await
            .map_err(map_call_error)
    }

    pub fn subscribe_target_views(&self) -> broadcast::Receiver<TargetView> {
        self.target_views.subscribe()
    }

    pub fn service_epoch(&self) -> Uuid {
        Uuid::parse_str(&self.service_epoch).expect("service epoch is generated as a UUID")
    }

    pub async fn committed_takeover(&self) -> Result<Option<CommittedTakeover>, StateError> {
        self.connection
            .call(
                |connection| -> Result<Option<CommittedTakeover>, StateError> {
                    let row = connection.query_row(
                        "SELECT route_port, routing_credential, activated_snapshot_id
                     FROM target_route_state
                     WHERE target = 'codex' AND takeover_state = 'active'",
                        [],
                        |row| {
                            Ok((
                                row.get::<_, Option<u16>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    );
                    match row {
                        Ok((Some(route_port), Some(credential), Some(snapshot)))
                            if !credential.is_empty() && !snapshot.is_empty() =>
                        {
                            let exists: bool = connection.query_row(
                                "SELECT EXISTS(SELECT 1 FROM activated_snapshots WHERE id = ?1)",
                                [snapshot],
                                |row| row.get(0),
                            )?;
                            if exists {
                                Ok(Some(CommittedTakeover { route_port }))
                            } else {
                                Err(StateError::InvalidActivatedSnapshot)
                            }
                        }
                        Ok(_) => Err(StateError::InvalidActivatedSnapshot),
                        Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                        Err(error) => Err(StateError::Sqlite(error)),
                    }
                },
            )
            .await
            .map_err(map_state_call_error)
    }

    pub async fn prepare_activation(
        &self,
        provider_id: Uuid,
        expected_revision: u64,
    ) -> Result<Result<ActivationPreparation, ActionFailure>, StateError> {
        let service_epoch = self.service_epoch.clone();
        self.connection
            .call(move |connection| -> Result<Result<ActivationPreparation, ActionFailure>, StateError> {
                let (revision, recovery_state, takeover_state, route_port, routing_credential,
                    raw_snapshot_id, joined_snapshot_id, prior_base_url, prior_model,
                    prior_provider_credential):
                    ActivationPreparationRow = connection.query_row(
                        "SELECT r.management_revision, r.recovery_state, r.takeover_state,
                                r.route_port, r.routing_credential, r.activated_snapshot_id,
                                s.id, s.base_url, s.model, s.provider_bearer_token
                         FROM target_route_state r
                         LEFT JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
                         WHERE r.target = 'codex'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                            row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?,
                            row.get(9)?)),
                    )?;
                let failure = |code: &str, message: &str| -> ActionFailure {
                    ActionFailure {
                        problem: ControlProblem { code: code.to_owned(), message: message.to_owned() },
                        authoritative_view: project_target_view(connection, &service_epoch)
                            .unwrap_or_else(|_| empty_target_view(&service_epoch)),
                    }
                };
                if recovery_state == "recovery-required" {
                    return Ok(Err(failure("recovery-required", "Managed writes are blocked until recovery is resolved")));
                }
                if revision != expected_revision {
                    return Ok(Err(failure("stale-revision", "Target state changed; refresh and retry")));
                }
                if raw_snapshot_id != joined_snapshot_id {
                    return Ok(Err(failure(
                        "recovery-required",
                        "Managed configuration requires recovery",
                    )));
                }
                let (prior_snapshot, prior_route_runtime) = match (
                    takeover_state.as_str(),
                    joined_snapshot_id,
                    prior_base_url,
                    prior_model,
                    prior_provider_credential,
                    route_port,
                    routing_credential,
                ) {
                    ("inactive", None, None, None, None, None, None) => (None, None),
                    ("inactive", None, None, None, None, Some(_), None) => (None, None),
                    ("inactive", Some(_), Some(base_url), Some(model), Some(credential), None, None) => (
                        Some(CommittedActivationSnapshot {
                            base_url,
                            model,
                            provider_credential: SecretString::from(credential),
                        }),
                        None,
                    ),
                    ("active", Some(_), Some(base_url), Some(model), Some(provider_credential),
                        Some(route_port), Some(routing_credential)) => (
                            Some(CommittedActivationSnapshot {
                                base_url,
                                model,
                                provider_credential: SecretString::from(provider_credential),
                            }),
                            Some(CommittedRouteRuntime {
                                route_port,
                                routing_credential: SecretString::from(routing_credential),
                            }),
                        ),
                    _ => return Ok(Err(failure(
                        "recovery-required",
                        "Managed configuration requires recovery",
                    ))),
                };
                let provider = connection.query_row(
                    "SELECT p.base_url, p.model, c.bearer_token, p.protocol, p.authentication, p.routing_requirement
                     FROM providers p LEFT JOIN credentials c ON c.id = p.credential_id
                     WHERE p.id = ?1 AND p.target = 'codex'",
                    [provider_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?, row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?, row.get::<_, String>(5)?)),
                );
                let (base_url, model, credential, protocol, authentication, routing_requirement) = match provider {
                    Ok(values) => values,
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {
                        return Ok(Err(failure("incomplete-provider", "Provider is missing or incomplete")));
                    }
                    Err(error) => return Err(StateError::Sqlite(error)),
                };
                if base_url.is_empty() || model.is_empty() || credential.is_none() {
                    return Ok(Err(failure("incomplete-provider", "Provider is missing or incomplete")));
                }
                let credential = credential.expect("credential is present after completeness check");
                let routing_requirement = match routing_requirement.as_str() {
                    "direct-compatible" => ProviderRoutingRequirement::DirectCompatible,
                    "takeover-required" => ProviderRoutingRequirement::TakeoverRequired,
                    _ => return Err(StateError::InvalidProviderRoutingRequirement),
                };
                let protocol = match protocol.as_str() {
                    "openai-responses" => ProviderProtocol::OpenaiResponses,
                    "anthropic-messages" => ProviderProtocol::AnthropicMessages,
                    _ => return Err(StateError::InvalidActivatedSnapshot),
                };
                let authentication = match authentication.as_str() {
                    "openai-bearer" => ProviderAuthentication::OpenaiBearer,
                    "anthropic-api-key" => ProviderAuthentication::AnthropicApiKey,
                    "anthropic-bearer" => ProviderAuthentication::AnthropicBearer,
                    _ => return Err(StateError::InvalidActivatedSnapshot),
                };
                if !has_valid_provider_declaration(Target::Codex, protocol, authentication) {
                    return Ok(Err(failure(
                        "incomplete-provider",
                        "Provider is missing or incomplete",
                    )));
                }
                Ok(Ok(ActivationPreparation {
                    provider_id,
                    base_url,
                    model,
                    protocol,
                    authentication,
                    provider_credential: SecretString::from(credential),
                    routing_requirement,
                    prior_snapshot,
                    prior_route_runtime,
                    preferred_route_port: route_port,
                }))
            })
            .await
            .map_err(map_state_call_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn commit_activation(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        snapshot: ActivatedSnapshot,
        runtime: ActivationRuntime,
        recovery_id: Uuid,
        config_path: String,
        capability_problem: Option<ControlProblem>,
    ) -> Result<ActivationCommit, StateError> {
        let service_epoch = self.service_epoch.clone();
        let (takeover_state, route_port, routing_credential) = match runtime {
            ActivationRuntime::Direct => ("inactive", None, None),
            ActivationRuntime::Takeover {
                route_port,
                routing_credential,
            } => (
                "active",
                Some(route_port),
                Some(routing_credential.expose_secret().to_owned()),
            ),
        };
        let provider_credential = snapshot.provider_credential.expose_secret().to_owned();
        self.connection.call(move |connection| -> Result<ActivationCommit, StateError> {
            let transaction = connection.transaction_with_behavior(
                tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
            )?;
            let recorded = transaction.query_row(
                "SELECT outcome_json FROM action_receipts WHERE target = 'codex' AND action_id = ?1",
                [action_id.to_string()],
                |row| row.get::<_, String>(0),
            );
            match recorded {
                Ok(json) => {
                    let mut outcome: ActionOutcome = serde_json::from_str(&json)?;
                    outcome.status = ActionStatus::Replayed;
                    return Ok(ActivationCommit::Replayed(outcome));
                }
                Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {}
                Err(error) => return Err(StateError::Sqlite(error)),
            }
            let (revision, recovery_state): (u64, String) = transaction.query_row(
                "SELECT management_revision, recovery_state FROM target_route_state WHERE target = 'codex'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if recovery_state == "recovery-required" {
                return Ok(ActivationCommit::RecoveryRequired(project_target_view(&transaction, &service_epoch)?));
            }
            if revision != expected_revision {
                return Ok(ActivationCommit::Stale(project_target_view(&transaction, &service_epoch)?));
            }
            transaction.execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, protocol, authentication, provider_bearer_token, epoch)
                 VALUES (?1, 'codex', ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![snapshot.id.to_string(), snapshot.provider_id.to_string(), snapshot.base_url,
                    snapshot.model, snapshot.protocol.to_string(), snapshot.authentication.to_string(),
                    provider_credential, snapshot.epoch.to_string()],
            )?;
            let changed = transaction.execute(
                "UPDATE activation_recovery SET state = 'committed'
                 WHERE id = ?1 AND state = 'pending'",
                [recovery_id.to_string()],
            )?;
            if changed != 1 {
                return Err(StateError::MissingRecoveryIntent);
            }
            transaction.execute(
                "UPDATE target_route_state SET
                    management_revision = management_revision + 1,
                    view_sequence = view_sequence + 1,
                    current_provider_id = ?1,
                    serving_provider_id = NULL,
                    takeover_state = ?2, route_port = ?3,
                    routing_credential = ?4, activated_snapshot_id = ?5,
                    managed_config_path = ?6, recovery_state = 'clean'
                 WHERE target = 'codex'",
                params![snapshot.provider_id.to_string(), takeover_state, route_port,
                    routing_credential, snapshot.id.to_string(), config_path],
            )?;
            transaction.execute(
                "DELETE FROM target_problems
                 WHERE target = 'codex' AND code = 'untested-target-cli'",
                [],
            )?;
            if let Some(problem) = capability_problem {
                transaction.execute(
                    "INSERT INTO target_problems (target, code, message)
                     VALUES ('codex', ?1, ?2)",
                    params![problem.code, problem.message],
                )?;
            }
            let view = project_target_view(&transaction, &service_epoch)?;
            let outcome = ActionOutcome { status: ActionStatus::Applied, view };
            let json = serde_json::to_string(&outcome)?;
            transaction.execute(
                "INSERT INTO action_receipts (target, action_id, action_kind, committed_revision, outcome_json)
                 VALUES ('codex', ?1, 'activate-provider', ?2, ?3)",
                params![action_id.to_string(), outcome.view.management_revision, json],
            )?;
            transaction.commit()?;
            Ok(ActivationCommit::Applied(outcome))
        }).await.map_err(map_state_call_error)
    }

    pub(crate) fn publish_target_view(&self, view: TargetView) {
        let _ = self.target_views.send(view);
    }

    pub async fn routing_credential(&self) -> Result<Option<SecretString>, StateError> {
        self.connection
            .call(|connection| {
                let credential = connection.query_row(
                    "SELECT routing_credential FROM target_route_state WHERE target = 'codex'",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )?;
                Ok(credential.map(SecretString::from))
            })
            .await
            .map_err(map_call_error)
    }

    pub async fn activated_snapshot(&self) -> Result<Option<RoutingSnapshot>, StateError> {
        self.connection
            .call(|connection| {
                let row = connection.query_row(
                    "SELECT s.id, s.provider_id, s.base_url, s.model,
                            s.provider_bearer_token, s.epoch
                     FROM target_route_state r
                     JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
                     WHERE r.target = 'codex'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    },
                );
                match row {
                    Ok((id, provider_id, base_url, model, credential, epoch)) => {
                        let id = Uuid::parse_str(&id)
                            .map_err(|_| StateError::InvalidActivatedSnapshot)?;
                        let provider_id = Uuid::parse_str(&provider_id)
                            .map_err(|_| StateError::InvalidActivatedSnapshot)?;
                        let epoch = Uuid::parse_str(&epoch)
                            .map_err(|_| StateError::InvalidActivatedSnapshot)?;
                        Ok(Some(RoutingSnapshot {
                            id,
                            provider_id,
                            base_url,
                            model,
                            provider_credential: SecretString::from(credential),
                            epoch,
                        }))
                    }
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(error) => Err(StateError::Sqlite(error)),
                }
            })
            .await
            .map_err(map_state_call_error)
    }

    pub async fn record_serving(&self, snapshot_id: Uuid) -> Result<TargetView, StateError> {
        let service_epoch = self.service_epoch.clone();
        let view = self
            .connection
            .call(move |connection| -> Result<TargetView, StateError> {
                let transaction = connection.transaction_with_behavior(
                    tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                )?;
                let provider_id = transaction
                    .query_row(
                        "SELECT provider_id FROM activated_snapshots WHERE id = ?1",
                        [snapshot_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|error| match error {
                        tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows => {
                            StateError::InvalidActivatedSnapshot
                        }
                        error => StateError::Sqlite(error),
                    })?;
                transaction.execute(
                    "UPDATE target_route_state
                     SET serving_provider_id = ?1,
                         view_sequence = view_sequence + 1
                     WHERE target = 'codex'",
                    [provider_id],
                )?;
                let view = project_target_view(&transaction, &service_epoch)?;
                transaction.commit()?;
                Ok(view)
            })
            .await
            .map_err(map_state_call_error)?;
        self.publish_target_view(view.clone());
        Ok(view)
    }

    pub async fn receipt(&self, action_id: Uuid) -> Result<Option<ActionOutcome>, StateError> {
        self.receipt_for(Target::Codex, action_id).await
    }

    pub async fn receipt_for(
        &self,
        target: Target,
        action_id: Uuid,
    ) -> Result<Option<ActionOutcome>, StateError> {
        self.connection
            .call(move |connection| {
                let outcome = connection.query_row(
                    "SELECT outcome_json FROM action_receipts WHERE target = ?1 AND action_id = ?2",
                    params![target.as_str(), action_id.to_string()],
                    |row| row.get::<_, String>(0),
                );
                match outcome {
                    Ok(json) => {
                        let mut outcome: ActionOutcome = serde_json::from_str(&json)?;
                        outcome.status = ActionStatus::Replayed;
                        Ok(Some(outcome))
                    }
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(error) => Err(StateError::Sqlite(error)),
                }
            })
            .await
            .map_err(map_state_call_error)
    }

    pub async fn apply_provider_action(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
    ) -> Result<ActionOutcome, ActionFailure> {
        self.apply_provider_action_for(Target::Codex, action_id, expected_revision, raw_action)
            .await
    }

    pub async fn apply_provider_action_for(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
    ) -> Result<ActionOutcome, ActionFailure> {
        match self.receipt_for(target, action_id).await {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) => {
                return Err(self
                    .failure("state-store-error", "State store operation failed")
                    .await);
            }
        }

        if target == Target::Codex && self.ensure_managed_writes_allowed().await.is_err() {
            return Err(self
                .failure(
                    "recovery-required",
                    "Managed writes are blocked until recovery is resolved",
                )
                .await);
        }

        let (action, action_kind) = match serde_json::from_value(raw_action) {
            Ok(TargetAction::CreateProvider {
                name,
                base_url,
                model,
                credential,
                authentication,
                preset_key,
            }) => (
                super::providers::ProviderAction::Create {
                    name,
                    base_url,
                    model,
                    credential,
                    authentication,
                    preset_key,
                },
                "create-provider",
            ),
            Ok(TargetAction::UpdateProvider {
                provider_id,
                provider_revision,
                name,
                base_url,
                model,
                credential,
                authentication,
            }) => (
                super::providers::ProviderAction::Update {
                    provider_id,
                    provider_revision,
                    name,
                    base_url,
                    model,
                    credential,
                    authentication,
                },
                "update-provider",
            ),
            Ok(TargetAction::ReorderProviders { provider_ids }) => (
                super::providers::ProviderAction::Reorder { provider_ids },
                "reorder-providers",
            ),
            Ok(TargetAction::DeleteProvider {
                provider_id,
                provider_revision,
            }) => (
                super::providers::ProviderAction::Delete {
                    provider_id,
                    provider_revision,
                },
                "delete-provider",
            ),
            Ok(TargetAction::DuplicateProvider {
                source_provider_id,
                source_provider_revision,
                name,
                base_url,
                model,
                credential,
            }) => (
                super::providers::ProviderAction::Duplicate {
                    source_provider_id,
                    source_provider_revision,
                    name,
                    base_url,
                    model,
                    credential,
                },
                "duplicate-provider",
            ),
            Ok(_) => {
                return Err(self
                    .failure("unsupported-operation", "Provider action is not supported")
                    .await);
            }
            Err(_) => {
                return Err(self
                    .failure("invalid-provider", "Provider action is malformed")
                    .await);
            }
        };
        let service_epoch = self.service_epoch.clone();
        let action_id = action_id.to_string();
        let attempt = self
            .connection
            .call(move |connection| -> Result<ProviderAttempt, StateError> {
                let transaction = connection.transaction_with_behavior(
                    tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                )?;
                let recorded = transaction.query_row(
                    "SELECT outcome_json FROM action_receipts WHERE target = ?1 AND action_id = ?2",
                    params![target.as_str(), action_id],
                    |row| row.get::<_, String>(0),
                );
                match recorded {
                    Ok(json) => {
                        let mut outcome: ActionOutcome = serde_json::from_str(&json)?;
                        outcome.status = ActionStatus::Replayed;
                        return Ok(ProviderAttempt::Applied(outcome));
                    }
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(error) => return Err(StateError::Sqlite(error)),
                }
                let current_revision: u64 = transaction.query_row(
                    "SELECT management_revision FROM target_route_state WHERE target = ?1",
                    [target.as_str()],
                    |row| row.get(0),
                )?;
                let recovery_state: String = transaction.query_row(
                    "SELECT recovery_state FROM target_route_state WHERE target = ?1",
                    [target.as_str()],
                    |row| row.get(0),
                )?;
                if recovery_state == "recovery-required" {
                    return Ok(ProviderAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: "recovery-required".to_owned(),
                            message: "Managed writes are blocked until recovery is resolved"
                                .to_owned(),
                        },
                        authoritative_view: project_target_view_for(
                            &transaction,
                            &service_epoch,
                            target,
                        )?,
                    }));
                }
                if current_revision != expected_revision {
                    let authoritative_view =
                        project_target_view_for(&transaction, &service_epoch, target)?;
                    return Ok(ProviderAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: "stale-revision".to_owned(),
                            message: "Target state changed; refresh and retry".to_owned(),
                        },
                        authoritative_view,
                    }));
                }
                if let Err(error) = super::providers::mutate_provider(&transaction, target, action)
                {
                    let (code, message) = match error {
                        super::providers::ProviderMutationError::Invalid => {
                            ("invalid-provider", "Provider declaration is invalid")
                        }
                        super::providers::ProviderMutationError::InvalidOrder => (
                            "invalid-provider-order",
                            "Provider order must contain every Provider exactly once",
                        ),
                        super::providers::ProviderMutationError::ProviderReferenced => (
                            "provider-referenced",
                            "Provider is referenced by Current or an Activated Snapshot",
                        ),
                        super::providers::ProviderMutationError::StaleProviderRevision => (
                            "stale-provider-revision",
                            "Provider changed; refresh and retry",
                        ),
                        super::providers::ProviderMutationError::NoProviderChange => {
                            ("no-provider-change", "Provider declaration is unchanged")
                        }
                    };
                    return Ok(ProviderAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: code.to_owned(),
                            message: message.to_owned(),
                        },
                        authoritative_view: project_target_view_for(
                            &transaction,
                            &service_epoch,
                            target,
                        )?,
                    }));
                }
                transaction.execute(
                    "UPDATE target_route_state
                     SET management_revision = management_revision + 1,
                         view_sequence = view_sequence + 1
                     WHERE target = ?1",
                    [target.as_str()],
                )?;
                let view = project_target_view_for(&transaction, &service_epoch, target)?;
                let outcome = ActionOutcome {
                    status: ActionStatus::Applied,
                    view,
                };
                let outcome_json = serde_json::to_string(&outcome)?;
                transaction.execute(
                    "INSERT INTO action_receipts
                     (target, action_id, action_kind, committed_revision, outcome_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        target.as_str(),
                        action_id,
                        action_kind,
                        outcome.view.management_revision,
                        outcome_json
                    ],
                )?;
                transaction.commit()?;
                Ok(ProviderAttempt::Applied(outcome))
            })
            .await;

        match attempt {
            Ok(ProviderAttempt::Applied(outcome)) => Ok(outcome),
            Ok(ProviderAttempt::Failure(failure)) => Err(failure),
            Err(_) => Err(self
                .failure("state-store-error", "State store operation failed")
                .await),
        }
    }

    #[doc(hidden)]
    pub async fn apply_save_provider_action(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
    ) -> Result<ActionOutcome, ActionFailure> {
        if let Ok(Some(outcome)) = self.receipt(action_id).await {
            return Ok(outcome);
        }
        let raw_action = match raw_action {
            serde_json::Value::Object(mut action)
                if action.get("kind") == Some(&serde_json::json!("save-provider")) =>
            {
                action.insert("kind".into(), serde_json::json!("create-provider"));
                if let Some(serde_json::Value::String(value)) = action.remove("credential") {
                    action.insert(
                        "credential".into(),
                        serde_json::json!({ "kind": "replace", "value": value }),
                    );
                }
                action.insert("presetKey".into(), serde_json::Value::Null);
                serde_json::Value::Object(action)
            }
            value => value,
        };
        self.apply_provider_action(action_id, expected_revision, raw_action)
            .await
    }

    pub(crate) async fn failure(&self, code: &str, message: &str) -> ActionFailure {
        let authoritative_view = self
            .target_view()
            .await
            .unwrap_or_else(|_| empty_target_view(&self.service_epoch));
        ActionFailure {
            problem: ControlProblem {
                code: code.to_owned(),
                message: message.to_owned(),
            },
            authoritative_view,
        }
    }
}

enum ProviderAttempt {
    Applied(ActionOutcome),
    Failure(ActionFailure),
}

fn map_call_error(error: tokio_rusqlite::Error<tokio_rusqlite::rusqlite::Error>) -> StateError {
    match error {
        tokio_rusqlite::Error::ConnectionClosed => StateError::Unavailable,
        tokio_rusqlite::Error::Error(error) => StateError::Sqlite(error),
        _ => StateError::Unavailable,
    }
}

pub(super) fn map_state_call_error(error: tokio_rusqlite::Error<StateError>) -> StateError {
    match error {
        tokio_rusqlite::Error::ConnectionClosed => StateError::Unavailable,
        tokio_rusqlite::Error::Error(error) => error,
        _ => StateError::Unavailable,
    }
}
