use std::sync::{Arc, Mutex};

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
        activation::ActivatedSnapshot, provider::has_valid_provider_declaration,
        view::project_target_view_for,
    },
    home::MuxviaHome,
};

use super::recovery::RecoveryPayload;

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
    #[error("state store contains invalid compatibility state")]
    InvalidCompatibilityState,
    #[error("state store has no compatibility state for this Target CLI")]
    MissingCompatibility,
}

pub struct StateStore {
    pub(super) connection: Connection,
    service_epoch: String,
    target_views: broadcast::Sender<TargetView>,
    published_view_sequences: [Arc<Mutex<Option<u64>>>; 2],
}

type ActivationPreparationRow = (
    u64,
    i64,
    String,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    u64,
);

pub struct ActivationPreparation {
    pub managed_config_version: u32,
    pub provider_id: Uuid,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub authentication: ProviderAuthentication,
    pub provider_credential: SecretString,
    pub routing_requirement: ProviderRoutingRequirement,
    pub prior_snapshot: Option<CommittedActivationSnapshot>,
    pub prior_route_runtime: Option<CommittedRouteRuntime>,
    pub prior_recovery_payload: Option<RecoveryPayload>,
    pub prior_recovery_id: Option<Uuid>,
    pub preferred_route_port: Option<u16>,
}

pub struct CommittedActivationSnapshot {
    pub base_url: String,
    pub model: String,
    pub authentication: ProviderAuthentication,
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
    pub managed_config_version: u32,
    pub(crate) recovery_expectation: Option<CommittedRecoveryExpectation>,
}

pub(crate) struct CommittedRecoveryExpectation {
    pub id: Uuid,
    pub payload: RecoveryPayload,
}

pub struct RoutingSnapshot {
    id: Uuid,
    provider_id: Uuid,
    base_url: String,
    model: String,
    provider_credential: SecretString,
    protocol: ProviderProtocol,
    authentication: ProviderAuthentication,
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

    pub fn protocol(&self) -> ProviderProtocol {
        self.protocol
    }

    pub fn authentication(&self) -> ProviderAuthentication {
        self.authentication
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
                drop(foreign_key_check);
                mark_invalid_managed_configurations(connection, None)?;
                Ok(())
            })
            .await
            .map_err(map_call_error)?;

        let (target_views, _) = broadcast::channel(32);
        Ok(Self {
            connection,
            service_epoch: Uuid::new_v4().to_string(),
            target_views,
            published_view_sequences: [Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None))],
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
        target: Target,
        provider_id: Uuid,
        provider_revision: u64,
    ) -> Result<super::providers::ProviderInspectionRead, StateError> {
        self.connection
            .call(move |connection| {
                super::providers::read_provider_for_inspection(
                    connection,
                    target,
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
        self.committed_takeover_for(Target::Codex).await
    }

    pub async fn committed_takeover_for(
        &self,
        target: Target,
    ) -> Result<Option<CommittedTakeover>, StateError> {
        self.connection
            .call(
                move |connection| -> Result<Option<CommittedTakeover>, StateError> {
                    let row = connection.query_row(
                        "SELECT r.route_port, r.routing_credential, r.activated_snapshot_id,
                                r.managed_config_version, r.recovery_intent_id, a.id, a.state,
                                a.payload_json,
                                (SELECT COUNT(*) FROM activation_recovery history
                                 WHERE history.target = r.target
                                   AND history.state = 'committed')
                         FROM target_route_state r
                         LEFT JOIN activation_recovery a
                           ON a.id = r.recovery_intent_id AND a.target = r.target
                         WHERE r.target = ?1 AND r.takeover_state = 'active'",
                        [target.as_str()],
                        |row| {
                            Ok((
                                row.get::<_, Option<i64>>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, Option<String>>(4)?,
                                row.get::<_, Option<String>>(5)?,
                                row.get::<_, Option<String>>(6)?,
                                row.get::<_, Option<String>>(7)?,
                                row.get::<_, u64>(8)?,
                            ))
                        },
                    );
                    match row {
                        Ok((
                            Some(route_port),
                            Some(credential),
                            Some(snapshot),
                            managed_config_version,
                            raw_recovery_id,
                            joined_recovery_id,
                            recovery_intent_state,
                            recovery_payload_json,
                            committed_recovery_count,
                        )) if !credential.is_empty() && !snapshot.is_empty() => {
                            if !valid_committed_managed_configuration(
                                target,
                                managed_config_version,
                                "active",
                                JoinedValue {
                                    raw: Some(&snapshot),
                                    joined: Some(&snapshot),
                                },
                                Some(route_port),
                                Some(&credential),
                                RecoveryBinding {
                                    id: JoinedValue {
                                        raw: raw_recovery_id.as_deref(),
                                        joined: joined_recovery_id.as_deref(),
                                    },
                                    state: recovery_intent_state.as_deref(),
                                },
                            ) {
                                return Err(StateError::InvalidActivatedSnapshot);
                            }
                            let exists: bool = connection.query_row(
                                "SELECT EXISTS(SELECT 1 FROM activated_snapshots
                                 WHERE id = ?1 AND target = ?2)",
                                [snapshot, target.as_str().to_owned()],
                                |row| row.get(0),
                            )?;
                            if exists {
                                let recovery_expectation =
                                    match validate_committed_recovery_binding(
                                        target,
                                        u32::try_from(managed_config_version).map_err(|_| {
                                            StateError::InvalidActivatedSnapshot
                                        })?,
                                        raw_recovery_id.as_deref(),
                                        joined_recovery_id.as_deref(),
                                        recovery_intent_state.as_deref(),
                                        recovery_payload_json.as_deref(),
                                        committed_recovery_count,
                                    ) {
                                        Ok(expectation) => expectation,
                                        Err(_) => {
                                            mark_invalid_current_recovery_binding(
                                                connection,
                                                target,
                                                raw_recovery_id.as_deref(),
                                            )?;
                                            return Ok(None);
                                        }
                                    };
                                Ok(Some(CommittedTakeover {
                                    route_port: u16::try_from(route_port)
                                        .expect("validated route port"),
                                    managed_config_version: u32::try_from(managed_config_version)
                                        .expect("validated managed configuration version"),
                                    recovery_expectation,
                                }))
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
        self.prepare_activation_for(Target::Codex, provider_id, expected_revision)
            .await
    }

    pub async fn prepare_activation_for(
        &self,
        target: Target,
        provider_id: Uuid,
        expected_revision: u64,
    ) -> Result<Result<ActivationPreparation, ActionFailure>, StateError> {
        let service_epoch = self.service_epoch.clone();
        self.connection
            .call(move |connection| -> Result<Result<ActivationPreparation, ActionFailure>, StateError> {
                let (revision, managed_config_version, recovery_state, takeover_state, route_port, routing_credential,
                    raw_snapshot_id, joined_snapshot_id, prior_base_url, prior_model,
                    prior_authentication, prior_provider_credential, raw_recovery_id,
                    joined_recovery_id, recovery_intent_state, recovery_payload_json,
                    committed_recovery_count):
                    ActivationPreparationRow = connection.query_row(
                        "SELECT r.management_revision, r.managed_config_version,
                                r.recovery_state, r.takeover_state,
                                r.route_port, r.routing_credential, r.activated_snapshot_id,
                                s.id, s.base_url, s.model, s.authentication,
                                s.provider_bearer_token, r.recovery_intent_id,
                                a.id, a.state, a.payload_json,
                                (SELECT COUNT(*) FROM activation_recovery history
                                 WHERE history.target = r.target
                                   AND history.state = 'committed')
                         FROM target_route_state r
                         LEFT JOIN activated_snapshots s
                           ON s.id = r.activated_snapshot_id AND s.target = r.target
                         LEFT JOIN activation_recovery a
                           ON a.id = r.recovery_intent_id AND a.target = r.target
                         WHERE r.target = ?1",
                        [target.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                            row.get(4)?, row.get(5)?, row.get(6)?, row.get(7)?, row.get(8)?,
                            row.get(9)?, row.get(10)?, row.get(11)?, row.get(12)?,
                            row.get(13)?, row.get(14)?, row.get(15)?, row.get(16)?)),
                    )?;
                let failure = |connection: &tokio_rusqlite::rusqlite::Connection,
                               code: &str,
                               message: &str|
                 -> ActionFailure {
                    ActionFailure {
                        problem: ControlProblem {
                            code: code.to_owned(), message: message.to_owned(),
                            source: None, selector: None,
                        },
                        authoritative_view: project_target_view_for(connection, &service_epoch, target)
                            .unwrap_or_else(|_| crate::domain::view::empty_target_view_for(&service_epoch, target)),
                    }
                };
                if recovery_state == "recovery-required" {
                    return Ok(Err(failure(connection, "recovery-required", "Managed writes are blocked until recovery is resolved")));
                }
                if !valid_committed_managed_configuration(
                    target,
                    managed_config_version,
                    &takeover_state,
                    JoinedValue {
                        raw: raw_snapshot_id.as_deref(),
                        joined: joined_snapshot_id.as_deref(),
                    },
                    route_port,
                    routing_credential.as_deref(),
                    RecoveryBinding {
                        id: JoinedValue {
                            raw: raw_recovery_id.as_deref(),
                            joined: joined_recovery_id.as_deref(),
                        },
                        state: recovery_intent_state.as_deref(),
                    },
                ) {
                    mark_invalid_managed_configurations(connection, Some(target))?;
                    return Ok(Err(failure(connection,
                        "recovery-required",
                        "Managed configuration requires recovery",
                    )));
                }
                let drifted: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM target_problems
                     WHERE target = ?1 AND code = 'configuration-drift')",
                    [target.as_str()],
                    |row| row.get(0),
                )?;
                if drifted {
                    return Ok(Err(failure(connection,
                        "configuration-drift",
                        "Managed configuration drift must be reconciled",
                    )));
                }
                if revision != expected_revision {
                    return Ok(Err(failure(connection, "stale-revision", "Target state changed; refresh and retry")));
                }
                let route_port = route_port.map(|route_port| {
                    u16::try_from(route_port).expect("validated route port")
                });
                let (prior_snapshot, prior_route_runtime) = match (
                    takeover_state.as_str(),
                    joined_snapshot_id,
                    prior_base_url,
                    prior_model,
                    prior_authentication,
                    prior_provider_credential,
                    route_port,
                    routing_credential,
                ) {
                    ("inactive", None, None, None, None, None, None, None) => (None, None),
                    ("inactive", None, None, None, None, None, Some(_), None) => (None, None),
                    ("inactive", Some(_), Some(base_url), Some(model), Some(authentication),
                        Some(credential), None, None) => (
                        Some(CommittedActivationSnapshot {
                            base_url,
                            model,
                            authentication: parse_provider_authentication(&authentication)?,
                            provider_credential: SecretString::from(credential),
                        }),
                        None,
                    ),
                    ("active", Some(_), Some(base_url), Some(model), Some(authentication),
                        Some(provider_credential), Some(route_port), Some(routing_credential)) => (
                            Some(CommittedActivationSnapshot {
                                base_url,
                                model,
                                authentication: parse_provider_authentication(&authentication)?,
                                provider_credential: SecretString::from(provider_credential),
                            }),
                            Some(CommittedRouteRuntime {
                                route_port,
                                routing_credential: SecretString::from(routing_credential),
                            }),
                        ),
                    _ => unreachable!("validated committed managed configuration shape"),
                };
                let committed_recovery = if prior_snapshot.is_some() {
                    let version = u32::try_from(managed_config_version)
                        .expect("validated managed configuration version");
                    match validate_committed_recovery_binding(
                        target,
                        version,
                        raw_recovery_id.as_deref(),
                        joined_recovery_id.as_deref(),
                        recovery_intent_state.as_deref(),
                        recovery_payload_json.as_deref(),
                        committed_recovery_count,
                    ) {
                        Ok(expectation) => expectation,
                        Err(_) => {
                            mark_invalid_current_recovery_binding(
                                connection,
                                target,
                                raw_recovery_id.as_deref(),
                            )?;
                            return Ok(Err(failure(connection,
                                "recovery-required",
                                "Managed configuration requires recovery",
                            )));
                        }
                    }
                } else {
                    None
                };
                let prior_recovery_payload =
                    committed_recovery.map(|expectation| expectation.payload);
                let provider = connection.query_row(
                    "SELECT p.base_url, p.model, c.bearer_token, p.protocol, p.authentication, p.routing_requirement
                     FROM providers p LEFT JOIN credentials c ON c.id = p.credential_id
                     WHERE p.id = ?1 AND p.target = ?2",
                    params![provider_id.to_string(), target.as_str()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?, row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?, row.get::<_, String>(5)?)),
                );
                let (base_url, model, credential, protocol, authentication, routing_requirement) = match provider {
                    Ok(values) => values,
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {
                        return Ok(Err(failure(connection, "incomplete-provider", "Provider is missing or incomplete")));
                    }
                    Err(error) => return Err(StateError::Sqlite(error)),
                };
                if base_url.is_empty() || model.is_empty() || credential.is_none() {
                    return Ok(Err(failure(connection, "incomplete-provider", "Provider is missing or incomplete")));
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
                if !has_valid_provider_declaration(target, protocol, authentication) {
                    return Ok(Err(failure(connection,
                        "incomplete-provider",
                        "Provider is missing or incomplete",
                    )));
                }
                Ok(Ok(ActivationPreparation {
                    managed_config_version: u32::try_from(managed_config_version)
                        .expect("validated managed configuration version"),
                    provider_id,
                    base_url,
                    model,
                    protocol,
                    authentication,
                    provider_credential: SecretString::from(credential),
                    routing_requirement,
                    prior_snapshot,
                    prior_route_runtime,
                    prior_recovery_payload,
                    prior_recovery_id: raw_recovery_id
                        .map(|id| Uuid::parse_str(&id))
                        .transpose()
                        .map_err(|_| StateError::InvalidRecoveryPayload)?,
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
        self.commit_activation_for(
            Target::Codex,
            action_id,
            expected_revision,
            snapshot,
            runtime,
            1,
            recovery_id,
            config_path,
            capability_problem,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn commit_activation_for(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        snapshot: ActivatedSnapshot,
        runtime: ActivationRuntime,
        managed_config_version: u32,
        recovery_id: Uuid,
        config_path: String,
        capability_problem: Option<ControlProblem>,
    ) -> Result<ActivationCommit, StateError> {
        self.commit_activation_for_with_recovery_payload(
            target,
            action_id,
            expected_revision,
            snapshot,
            runtime,
            managed_config_version,
            recovery_id,
            config_path,
            capability_problem,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn commit_activation_for_with_recovery_payload(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        snapshot: ActivatedSnapshot,
        runtime: ActivationRuntime,
        managed_config_version: u32,
        recovery_id: Uuid,
        config_path: String,
        capability_problem: Option<ControlProblem>,
        committed_recovery_payload_json: Option<String>,
    ) -> Result<ActivationCommit, StateError> {
        if snapshot.target != target
            || !has_valid_provider_declaration(target, snapshot.protocol, snapshot.authentication)
        {
            return Err(StateError::InvalidActivatedSnapshot);
        }
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
                "SELECT outcome_json FROM action_receipts WHERE target = ?1 AND action_id = ?2",
                params![target.as_str(), action_id.to_string()],
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
                "SELECT management_revision, recovery_state FROM target_route_state WHERE target = ?1",
                [target.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if recovery_state == "recovery-required" {
                return Ok(ActivationCommit::RecoveryRequired(project_target_view_for(&transaction, &service_epoch, target)?));
            }
            if revision != expected_revision {
                return Ok(ActivationCommit::Stale(project_target_view_for(&transaction, &service_epoch, target)?));
            }
            if !valid_commit_managed_config_version(
                target,
                managed_config_version,
                takeover_state,
            ) {
                let changed = transaction.execute(
                    "UPDATE activation_recovery SET state = 'recovery-required'
                     WHERE id = ?1 AND target = ?2 AND action_id = ?3 AND state = 'pending'",
                    params![
                        recovery_id.to_string(),
                        target.as_str(),
                        action_id.to_string()
                    ],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                transaction.execute(
                    "UPDATE target_route_state
                     SET recovery_intent_id = ?1, recovery_state = 'recovery-required',
                         view_sequence = view_sequence + 1
                     WHERE target = ?2",
                    [recovery_id.to_string(), target.as_str().to_owned()],
                )?;
                let view = project_target_view_for(&transaction, &service_epoch, target)?;
                transaction.commit()?;
                return Ok(ActivationCommit::RecoveryRequired(view));
            }
            let payload_json = transaction
                .query_row(
                    "SELECT payload_json FROM activation_recovery
                     WHERE id = ?1 AND target = ?2 AND action_id = ?3 AND state = 'pending'",
                    params![
                        recovery_id.to_string(),
                        target.as_str(),
                        action_id.to_string()
                    ],
                    |row| row.get::<_, String>(0),
                )
                .map_err(|error| match error {
                    tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows => {
                        StateError::MissingRecoveryIntent
                    }
                    error => StateError::Sqlite(error),
                })?;
            let payload: RecoveryPayload = serde_json::from_str(&payload_json)
                .map_err(|_| StateError::InvalidRecoveryPayload)?;
            if !payload.matches_managed_config_version(target, managed_config_version) {
                return Err(StateError::InvalidRecoveryPayload);
            }
            let committed_payload_json = match committed_recovery_payload_json {
                Some(payload_json) => {
                    let payload: RecoveryPayload = serde_json::from_str(&payload_json)
                        .map_err(|_| StateError::InvalidRecoveryPayload)?;
                    if !payload.matches_managed_config_version(target, managed_config_version) {
                        return Err(StateError::InvalidRecoveryPayload);
                    }
                    payload_json
                }
                None => payload_json,
            };
            transaction.execute(
                "INSERT INTO activated_snapshots
                 (id, target, provider_id, base_url, model, protocol, authentication, provider_bearer_token, epoch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![snapshot.id.to_string(), target.as_str(), snapshot.provider_id.to_string(), snapshot.base_url,
                    snapshot.model, snapshot.protocol.to_string(), snapshot.authentication.to_string(),
                    provider_credential, snapshot.epoch.to_string()],
            )?;
            let changed = transaction.execute(
                "UPDATE activation_recovery SET state = 'committed', payload_json = ?4
                 WHERE id = ?1 AND target = ?2 AND action_id = ?3 AND state = 'pending'",
                params![
                    recovery_id.to_string(),
                    target.as_str(),
                    action_id.to_string(),
                    committed_payload_json
                ],
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
                    managed_config_path = ?6, managed_config_version = ?7,
                    recovery_intent_id = ?8,
                    recovery_state = 'clean'
                 WHERE target = ?9",
                params![snapshot.provider_id.to_string(), takeover_state, route_port,
                    routing_credential, snapshot.id.to_string(), config_path,
                    managed_config_version, recovery_id.to_string(), target.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM target_problems
                 WHERE target = ?1 AND code IN
                   ('untested-target-cli', 'compatibility-acknowledgement-required',
                    'incompatible-target-cli', 'shadowing-configuration')",
                [target.as_str()],
            )?;
            if let Some(problem) = capability_problem {
                transaction.execute(
                    "INSERT INTO target_problems (target, code, message)
                     VALUES (?1, ?2, ?3)",
                    params![target.as_str(), problem.code, problem.message],
                )?;
            }
            let view = project_target_view_for(&transaction, &service_epoch, target)?;
            let outcome = ActionOutcome { status: ActionStatus::Applied, view };
            let json = serde_json::to_string(&outcome)?;
            transaction.execute(
                "INSERT INTO action_receipts (target, action_id, action_kind, committed_revision, outcome_json)
                 VALUES (?1, ?2, 'activate-provider', ?3, ?4)",
                params![target.as_str(), action_id.to_string(), outcome.view.management_revision, json],
            )?;
            transaction.commit()?;
            Ok(ActivationCommit::Applied(outcome))
        }).await.map_err(map_state_call_error)
    }

    pub(crate) async fn mark_committed_activation_recovery_required(
        &self,
        target: Target,
        recovery_id: Uuid,
    ) -> Result<ActionOutcome, StateError> {
        let service_epoch = self.service_epoch.clone();
        self.connection
            .call(move |connection| -> Result<ActionOutcome, StateError> {
                let transaction = connection.transaction_with_behavior(
                    tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                )?;
                let action_id = transaction
                    .query_row(
                        "SELECT action_id FROM activation_recovery
                         WHERE id = ?1 AND target = ?2 AND state = 'committed'",
                        params![recovery_id.to_string(), target.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(|error| match error {
                        tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows => {
                            StateError::MissingRecoveryIntent
                        }
                        error => StateError::Sqlite(error),
                    })?;
                let changed = transaction.execute(
                    "UPDATE activation_recovery SET state = 'recovery-required'
                     WHERE id = ?1 AND target = ?2 AND state = 'committed'",
                    params![recovery_id.to_string(), target.as_str()],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                let changed = transaction.execute(
                    "UPDATE target_route_state
                     SET recovery_state = 'recovery-required',
                         recovery_intent_id = ?1,
                         view_sequence = view_sequence + CASE
                           WHEN recovery_state = 'recovery-required' THEN 0 ELSE 1 END
                     WHERE target = ?2",
                    [recovery_id.to_string(), target.as_str().to_owned()],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                let view = project_target_view_for(&transaction, &service_epoch, target)?;
                let outcome = ActionOutcome {
                    status: ActionStatus::Applied,
                    view,
                };
                let json = serde_json::to_string(&outcome)?;
                let changed = transaction.execute(
                    "UPDATE action_receipts SET outcome_json = ?1
                     WHERE target = ?2 AND action_id = ?3
                       AND action_kind = 'activate-provider'",
                    params![json, target.as_str(), action_id],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                transaction.commit()?;
                Ok(outcome)
            })
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn mark_current_activation_recovery_required(
        &self,
        target: Target,
        recovery_id: Uuid,
    ) -> Result<TargetView, StateError> {
        let service_epoch = self.service_epoch.clone();
        self.connection
            .call(move |connection| -> Result<TargetView, StateError> {
                let transaction = connection.transaction_with_behavior(
                    tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                )?;
                let changed = transaction.execute(
                    "UPDATE activation_recovery SET state = 'recovery-required'
                     WHERE id = ?1 AND target = ?2 AND state = 'committed'",
                    params![recovery_id.to_string(), target.as_str()],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                let changed = transaction.execute(
                    "UPDATE target_route_state
                     SET recovery_state = 'recovery-required',
                         view_sequence = view_sequence + CASE
                           WHEN recovery_state = 'recovery-required' THEN 0 ELSE 1 END
                     WHERE target = ?1 AND recovery_intent_id = ?2",
                    params![target.as_str(), recovery_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                let view = project_target_view_for(&transaction, &service_epoch, target)?;
                transaction.commit()?;
                Ok(view)
            })
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn publish_target_view(&self, view: TargetView) -> Result<(), StateError> {
        self.publish_target_view_inner(view, None).await
    }

    #[cfg(test)]
    pub(crate) async fn publish_target_view_with_authoritative_read_hook(
        &self,
        view: TargetView,
        hook: Arc<dyn Fn() -> tokio_rusqlite::rusqlite::Result<()> + Send + Sync>,
    ) -> Result<(), StateError> {
        self.publish_target_view_inner(view, Some(hook)).await
    }

    async fn publish_target_view_inner(
        &self,
        view: TargetView,
        authoritative_read_hook: Option<
            Arc<dyn Fn() -> tokio_rusqlite::rusqlite::Result<()> + Send + Sync>,
        >,
    ) -> Result<(), StateError> {
        let index = match view.target {
            Target::Codex => 0,
            Target::Claude => 1,
        };
        let published = Arc::clone(&self.published_view_sequences[index]);
        let sender = self.target_views.clone();
        self.connection
            .call(move |connection| -> tokio_rusqlite::rusqlite::Result<()> {
                if let Some(hook) = authoritative_read_hook {
                    hook()?;
                }
                let authoritative: u64 = connection.query_row(
                    "SELECT view_sequence FROM target_route_state WHERE target = ?1",
                    [view.target.as_str()],
                    |row| row.get(0),
                )?;
                let mut last = published
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if view.view_sequence == authoritative
                    && last.is_none_or(|sequence| view.view_sequence > sequence)
                {
                    *last = Some(view.view_sequence);
                    let _ = sender.send(view);
                }
                Ok(())
            })
            .await
            .map_err(map_call_error)
    }

    pub async fn mark_configuration_drift_for(&self, target: Target) -> Result<(), StateError> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let inserted = transaction.execute(
                    "INSERT OR IGNORE INTO target_problems (target, code, message)
                     VALUES (?1, 'configuration-drift',
                       'Managed configuration drift must be reconciled')",
                    [target.as_str()],
                )?;
                if inserted == 1 {
                    transaction.execute(
                        "UPDATE target_route_state
                         SET view_sequence = view_sequence + 1 WHERE target = ?1",
                        [target.as_str()],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(map_call_error)
    }

    pub async fn record_startup_problem_for(
        &self,
        target: Target,
        code: &'static str,
        message: &'static str,
    ) -> Result<(), StateError> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let changed = transaction.execute(
                    "INSERT INTO target_problems (target, code, message) VALUES (?1, ?2, ?3)
                     ON CONFLICT(target, code) DO UPDATE SET message = excluded.message",
                    params![target.as_str(), code, message],
                )?;
                if changed == 1 {
                    transaction.execute(
                        "UPDATE target_route_state SET view_sequence = view_sequence + 1
                         WHERE target = ?1",
                        [target.as_str()],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(map_call_error)
    }

    pub async fn clear_startup_problems_for(&self, target: Target) -> Result<(), StateError> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let changed = transaction.execute(
                    "DELETE FROM target_problems
                     WHERE target = ?1 AND code IN
                       ('startup-reconciliation-failed', 'model-route-unavailable')",
                    [target.as_str()],
                )?;
                if changed > 0 {
                    transaction.execute(
                        "UPDATE target_route_state SET view_sequence = view_sequence + 1
                         WHERE target = ?1",
                        [target.as_str()],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(map_call_error)
    }

    pub async fn service_lifecycle_required(&self) -> Result<bool, StateError> {
        self.connection
            .call(|connection| {
                connection.query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM target_route_state
                       WHERE takeover_state = 'active' OR recovery_state = 'recovery-required'
                       UNION ALL
                       SELECT 1 FROM activation_recovery WHERE state = 'pending'
                       UNION ALL
                       SELECT 1 FROM reconciliation_intents WHERE state = 'pending'
                       UNION ALL
                       SELECT 1 FROM target_problems WHERE code = 'configuration-drift'
                     )",
                    [],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(map_call_error)
    }

    pub async fn routing_credential(&self) -> Result<Option<SecretString>, StateError> {
        self.routing_credential_for(Target::Codex).await
    }

    pub async fn routing_credential_for(
        &self,
        target: Target,
    ) -> Result<Option<SecretString>, StateError> {
        self.connection
            .call(move |connection| {
                let credential = connection.query_row(
                    "SELECT routing_credential FROM target_route_state WHERE target = ?1",
                    [target.as_str()],
                    |row| row.get::<_, Option<String>>(0),
                )?;
                Ok(credential.map(SecretString::from))
            })
            .await
            .map_err(map_call_error)
    }

    pub async fn activated_snapshot(&self) -> Result<Option<RoutingSnapshot>, StateError> {
        self.activated_snapshot_for(Target::Codex).await
    }

    pub async fn activated_snapshot_for(
        &self,
        target: Target,
    ) -> Result<Option<RoutingSnapshot>, StateError> {
        self.connection
            .call(move |connection| {
                let row = connection.query_row(
                    "SELECT s.id, s.provider_id, s.base_url, s.model,
                            s.provider_bearer_token, s.epoch, s.protocol, s.authentication
                     FROM target_route_state r
                     JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
                     WHERE r.target = ?1 AND s.target = ?1",
                    [target.as_str()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                        ))
                    },
                );
                match row {
                    Ok((
                        id,
                        provider_id,
                        base_url,
                        model,
                        credential,
                        epoch,
                        protocol,
                        authentication,
                    )) => {
                        let id = Uuid::parse_str(&id)
                            .map_err(|_| StateError::InvalidActivatedSnapshot)?;
                        let provider_id = Uuid::parse_str(&provider_id)
                            .map_err(|_| StateError::InvalidActivatedSnapshot)?;
                        let epoch = Uuid::parse_str(&epoch)
                            .map_err(|_| StateError::InvalidActivatedSnapshot)?;
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
                        Ok(Some(RoutingSnapshot {
                            id,
                            provider_id,
                            base_url,
                            model,
                            provider_credential: SecretString::from(credential),
                            protocol,
                            authentication,
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
        self.record_serving_for(Target::Codex, snapshot_id).await
    }

    pub async fn record_serving_for(
        &self,
        target: Target,
        snapshot_id: Uuid,
    ) -> Result<TargetView, StateError> {
        let service_epoch = self.service_epoch.clone();
        let view = self
            .connection
            .call(move |connection| -> Result<TargetView, StateError> {
                let transaction = connection.transaction_with_behavior(
                    tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                )?;
                let provider_id = transaction
                    .query_row(
                        "SELECT provider_id FROM activated_snapshots
                         WHERE id = ?1 AND target = ?2",
                        [snapshot_id.to_string(), target.as_str().to_owned()],
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
                     WHERE target = ?2",
                    [provider_id, target.as_str().to_owned()],
                )?;
                let view = project_target_view_for(&transaction, &service_epoch, target)?;
                transaction.commit()?;
                Ok(view)
            })
            .await
            .map_err(map_state_call_error)?;
        self.publish_target_view(view.clone()).await?;
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
                    .failure_for(target, "state-store-error", "State store operation failed")
                    .await);
            }
        }

        match self.managed_write_status_for(target).await {
            Ok(super::recovery::ManagedWriteStatus::Allowed) => {}
            Ok(super::recovery::ManagedWriteStatus::ConfigurationDrift) => {
                return Err(self
                    .failure_for(
                        target,
                        "configuration-drift",
                        "Managed configuration drift must be reconciled",
                    )
                    .await);
            }
            Ok(super::recovery::ManagedWriteStatus::RecoveryRequired) | Err(_) => {
                return Err(self
                    .failure_for(
                        target,
                        "recovery-required",
                        "Managed writes are blocked until recovery is resolved",
                    )
                    .await);
            }
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
                routing_requirement,
            }) => (
                super::providers::ProviderAction::Update {
                    provider_id,
                    provider_revision,
                    name,
                    base_url,
                    model,
                    credential,
                    authentication,
                    routing_requirement,
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
                    .failure_for(
                        target,
                        "unsupported-operation",
                        "Provider action is not supported",
                    )
                    .await);
            }
            Err(_) => {
                return Err(self
                    .failure_for(target, "invalid-provider", "Provider action is malformed")
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
                            source: None,
                            selector: None,
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
                            source: None,
                            selector: None,
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
                        super::providers::ProviderMutationError::GeneratedProviderReadOnly => (
                            "generated-provider-read-only",
                            "Universal-owned Generated Provider fields are read-only",
                        ),
                        super::providers::ProviderMutationError::GeneratedProviderDeleteForbidden => (
                            "generated-provider-delete-forbidden",
                            "Generated Provider must be disabled or deleted from its Universal Provider",
                        ),
                    };
                    return Ok(ProviderAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: code.to_owned(),
                            message: message.to_owned(),
                            source: None,
                            selector: None,
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
                .failure_for(target, "state-store-error", "State store operation failed")
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

    pub(crate) async fn failure_for(
        &self,
        target: Target,
        code: &str,
        message: &str,
    ) -> ActionFailure {
        let authoritative_view = self.target_view_for(target).await.unwrap_or_else(|_| {
            crate::domain::view::empty_target_view_for(&self.service_epoch, target)
        });
        ActionFailure {
            problem: ControlProblem {
                code: code.to_owned(),
                message: message.to_owned(),
                source: None,
                selector: None,
            },
            authoritative_view,
        }
    }
}

#[derive(Clone, Copy)]
struct JoinedValue<'a> {
    raw: Option<&'a str>,
    joined: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct RecoveryBinding<'a> {
    id: JoinedValue<'a>,
    state: Option<&'a str>,
}

fn valid_committed_managed_configuration(
    target: Target,
    version: i64,
    takeover_state: &str,
    snapshot: JoinedValue<'_>,
    route_port: Option<i64>,
    routing_credential: Option<&str>,
    recovery: RecoveryBinding<'_>,
) -> bool {
    if snapshot.raw != snapshot.joined {
        return false;
    }
    if route_port.is_some_and(|route_port| u16::try_from(route_port).is_err()) {
        return false;
    }
    let has_snapshot = snapshot.joined.is_some();
    let has_committed_recovery = recovery.id.raw.is_some()
        && recovery.id.raw == recovery.id.joined
        && recovery.state == Some("committed");
    if target == Target::Claude && recovery.id.raw.is_some() && !has_committed_recovery {
        return false;
    }
    let state_shape = match (takeover_state, has_snapshot, route_port, routing_credential) {
        ("inactive", false, _, None) => Some((false, false)),
        ("inactive", true, None, None) => Some((true, false)),
        ("active", true, Some(_), Some(credential)) if !credential.is_empty() => Some((true, true)),
        _ => None,
    };
    matches!(
        (target, version, state_shape, has_committed_recovery),
        (
            Target::Codex,
            1,
            Some((false, false) | (true, false) | (true, true)),
            _
        ) | (Target::Claude, 1, Some((false, false)), false)
            | (Target::Claude, 1, Some((true, true)), false)
            | (Target::Claude, 1, Some((true, true)), true)
            | (Target::Claude, 2, Some((true, false) | (true, true)), true)
    )
}

fn parse_provider_authentication(
    authentication: &str,
) -> Result<ProviderAuthentication, StateError> {
    match authentication {
        "openai-bearer" => Ok(ProviderAuthentication::OpenaiBearer),
        "anthropic-api-key" => Ok(ProviderAuthentication::AnthropicApiKey),
        "anthropic-bearer" => Ok(ProviderAuthentication::AnthropicBearer),
        _ => Err(StateError::InvalidActivatedSnapshot),
    }
}

fn valid_commit_managed_config_version(target: Target, version: u32, takeover_state: &str) -> bool {
    matches!(
        (target, version, takeover_state),
        (Target::Codex, 1, "inactive" | "active")
            | (Target::Claude, 1, "active")
            | (Target::Claude, 2, "inactive" | "active")
    )
}

fn validate_committed_recovery_binding(
    target: Target,
    managed_config_version: u32,
    raw_recovery_id: Option<&str>,
    joined_recovery_id: Option<&str>,
    recovery_state: Option<&str>,
    payload_json: Option<&str>,
    committed_recovery_count: u64,
) -> Result<Option<CommittedRecoveryExpectation>, StateError> {
    match (
        raw_recovery_id,
        joined_recovery_id,
        recovery_state,
        payload_json,
    ) {
        (None, None, None, None)
            if legacy_unbound_recovery_allowed(
                i64::from(managed_config_version),
                committed_recovery_count,
            ) =>
        {
            Ok(None)
        }
        (Some(raw_id), Some(joined_id), Some("committed"), Some(payload_json))
            if raw_id == joined_id =>
        {
            let id = Uuid::parse_str(raw_id).map_err(|_| StateError::InvalidRecoveryPayload)?;
            let payload: RecoveryPayload = serde_json::from_str(payload_json)
                .map_err(|_| StateError::InvalidRecoveryPayload)?;
            if !payload.matches_managed_config_version(target, managed_config_version) {
                return Err(StateError::InvalidRecoveryPayload);
            }
            Ok(Some(CommittedRecoveryExpectation { id, payload }))
        }
        _ => Err(StateError::InvalidRecoveryPayload),
    }
}

fn mark_invalid_current_recovery_binding(
    connection: &mut tokio_rusqlite::rusqlite::Connection,
    target: Target,
    recovery_id: Option<&str>,
) -> tokio_rusqlite::rusqlite::Result<()> {
    let transaction = connection
        .transaction_with_behavior(tokio_rusqlite::rusqlite::TransactionBehavior::Immediate)?;
    if let Some(recovery_id) = recovery_id {
        transaction.execute(
            "UPDATE activation_recovery SET state = 'recovery-required'
             WHERE id = ?1 AND target = ?2 AND state = 'committed'",
            [recovery_id, target.as_str()],
        )?;
    }
    transaction.execute(
        "UPDATE target_route_state
         SET recovery_state = 'recovery-required',
             view_sequence = view_sequence + CASE
               WHEN recovery_state = 'recovery-required' THEN 0 ELSE 1 END
         WHERE target = ?1",
        [target.as_str()],
    )?;
    transaction.commit()
}

fn mark_invalid_managed_configurations(
    connection: &tokio_rusqlite::rusqlite::Connection,
    target: Option<Target>,
) -> tokio_rusqlite::rusqlite::Result<()> {
    connection.execute_batch("PRAGMA ignore_check_constraints = ON;")?;
    let result = (|| {
        let startup_audit = target.is_none();
        let invalid_targets = match target {
            Some(target) => vec![target],
            None => {
                let mut statement = connection.prepare(
                    "SELECT r.target, r.managed_config_version, r.takeover_state,
                            r.route_port, r.routing_credential, r.activated_snapshot_id, s.id,
                            r.recovery_intent_id, a.id, a.state, a.payload_json,
                            (SELECT COUNT(*) FROM activation_recovery history
                             WHERE history.target = r.target
                               AND history.state = 'committed')
                     FROM target_route_state r
                     LEFT JOIN activated_snapshots s
                       ON s.id = r.activated_snapshot_id AND s.target = r.target
                     LEFT JOIN activation_recovery a
                       ON a.id = r.recovery_intent_id AND a.target = r.target",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, u64>(11)?,
                    ))
                })?;
                let mut invalid_targets = Vec::new();
                for row in rows {
                    let (
                        target,
                        version,
                        takeover_state,
                        route_port,
                        routing_credential,
                        raw_snapshot_id,
                        joined_snapshot_id,
                        raw_recovery_id,
                        joined_recovery_id,
                        recovery_intent_state,
                        recovery_payload_json,
                        committed_recovery_count,
                    ) = row?;
                    let target = match target.as_str() {
                        "codex" => Target::Codex,
                        "claude" => Target::Claude,
                        _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
                    };
                    let recovery_binding_is_invalid = raw_snapshot_id.is_some()
                        && u32::try_from(version).map_or(true, |version| {
                            validate_committed_recovery_binding(
                                target,
                                version,
                                raw_recovery_id.as_deref(),
                                joined_recovery_id.as_deref(),
                                recovery_intent_state.as_deref(),
                                recovery_payload_json.as_deref(),
                                committed_recovery_count,
                            )
                            .is_err()
                        });
                    if recovery_binding_is_invalid
                        || !valid_committed_managed_configuration(
                            target,
                            version,
                            &takeover_state,
                            JoinedValue {
                                raw: raw_snapshot_id.as_deref(),
                                joined: joined_snapshot_id.as_deref(),
                            },
                            route_port,
                            routing_credential.as_deref(),
                            RecoveryBinding {
                                id: JoinedValue {
                                    raw: raw_recovery_id.as_deref(),
                                    joined: joined_recovery_id.as_deref(),
                                },
                                state: recovery_intent_state.as_deref(),
                            },
                        )
                    {
                        invalid_targets.push(target);
                    }
                }
                invalid_targets
            }
        };
        for target in invalid_targets {
            connection.execute(
                "UPDATE target_route_state
                 SET recovery_state = 'recovery-required',
                     view_sequence = view_sequence + CASE
                       WHEN recovery_state = 'recovery-required' THEN 0 ELSE 1 END
                 WHERE target = ?1",
                [target.as_str()],
            )?;
            if startup_audit {
                connection.execute(
                    "INSERT INTO target_problems (target, code, message)
                     VALUES (?1, 'model-route-unavailable',
                       'The committed model route could not be resumed')
                     ON CONFLICT(target, code) DO UPDATE SET message = excluded.message",
                    [target.as_str()],
                )?;
            }
        }
        Ok::<(), tokio_rusqlite::rusqlite::Error>(())
    })();
    let reset = connection.execute_batch("PRAGMA ignore_check_constraints = OFF;");
    result?;
    reset?;
    Ok(())
}

fn legacy_unbound_recovery_allowed(
    managed_config_version: i64,
    committed_recovery_count: u64,
) -> bool {
    managed_config_version == 1 && committed_recovery_count == 0
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
