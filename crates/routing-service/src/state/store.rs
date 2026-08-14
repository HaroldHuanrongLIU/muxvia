use secrecy::{ExposeSecret, SecretString};
use tokio::sync::broadcast;
use tokio_rusqlite::{
    Connection,
    rusqlite::{OptionalExtension, params},
};
use uuid::Uuid;

use crate::{
    control::protocol::{ActionOutcome, ActionStatus, ControlProblem, TargetAction, TargetView},
    domain::{
        activation::ActivatedSnapshot,
        view::{empty_target_view, project_target_view},
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
    #[error("recovery intent does not exist")]
    MissingRecoveryIntent,
    #[error("state store contains an invalid activated snapshot")]
    InvalidActivatedSnapshot,
}

pub struct StateStore {
    pub(super) connection: Connection,
    service_epoch: String,
    target_views: broadcast::Sender<TargetView>,
}

pub struct ActivationPreparation {
    pub provider_id: Uuid,
    pub base_url: String,
    pub model: String,
    pub provider_credential: SecretString,
    pub route_port: Option<u16>,
    pub routing_credential: Option<SecretString>,
    pub active_model: Option<String>,
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
        let service_epoch = self.service_epoch.clone();
        self.connection
            .call(move |connection| project_target_view(connection, &service_epoch))
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
                let (revision, recovery_state, route_port, routing_credential):
                    (u64, String, Option<u16>, Option<String>) = connection.query_row(
                        "SELECT management_revision, recovery_state, route_port, routing_credential
                         FROM target_route_state WHERE target = 'codex'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
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
                let provider = connection.query_row(
                    "SELECT p.base_url, p.model, c.bearer_token
                     FROM providers p LEFT JOIN credentials c ON c.id = p.credential_id
                     WHERE p.id = ?1 AND p.target = 'codex'",
                    [provider_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)),
                );
                let (base_url, model, credential) = match provider {
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
                let active_model = connection.query_row(
                    "SELECT s.model FROM target_route_state r
                     JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
                     WHERE r.target = 'codex'",
                    [],
                    |row| row.get::<_, String>(0),
                ).optional()?;
                Ok(Ok(ActivationPreparation {
                    provider_id,
                    base_url,
                    model,
                    provider_credential: SecretString::from(credential),
                    route_port,
                    routing_credential: routing_credential.map(SecretString::from),
                    active_model,
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
        route_port: u16,
        routing_credential: SecretString,
        recovery_id: Uuid,
        config_path: String,
        capability_problem: Option<ControlProblem>,
    ) -> Result<ActivationCommit, StateError> {
        let service_epoch = self.service_epoch.clone();
        let routing_credential = routing_credential.expose_secret().to_owned();
        let provider_credential = snapshot.provider_credential.expose_secret().to_owned();
        self.connection.call(move |connection| -> Result<ActivationCommit, StateError> {
            let transaction = connection.transaction_with_behavior(
                tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
            )?;
            let recorded = transaction.query_row(
                "SELECT outcome_json FROM action_receipts WHERE action_id = ?1",
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
                 (id, target, provider_id, base_url, model, provider_bearer_token, epoch)
                 VALUES (?1, 'codex', ?2, ?3, ?4, ?5, ?6)",
                params![snapshot.id.to_string(), snapshot.provider_id.to_string(), snapshot.base_url,
                    snapshot.model, provider_credential, snapshot.epoch.to_string()],
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
                    takeover_state = 'active', route_port = ?2,
                    routing_credential = ?3, activated_snapshot_id = ?4,
                    managed_config_path = ?5, recovery_state = 'clean'
                 WHERE target = 'codex'",
                params![snapshot.provider_id.to_string(), route_port, routing_credential,
                    snapshot.id.to_string(), config_path],
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
                "INSERT INTO action_receipts (action_id, action_kind, committed_revision, outcome_json)
                 VALUES (?1, 'activate-provider', ?2, ?3)",
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
        self.connection
            .call(move |connection| {
                let outcome = connection.query_row(
                    "SELECT outcome_json FROM action_receipts WHERE action_id = ?1",
                    [action_id.to_string()],
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
        match self.receipt(action_id).await {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) => {
                return Err(self
                    .failure("state-store-error", "State store operation failed")
                    .await);
            }
        }

        if self.ensure_managed_writes_allowed().await.is_err() {
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
                preset_key,
            }) => (
                super::providers::ProviderAction::Create {
                    name,
                    base_url,
                    model,
                    credential,
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
            }) => (
                super::providers::ProviderAction::Update {
                    provider_id,
                    provider_revision,
                    name,
                    base_url,
                    model,
                    credential,
                },
                "update-provider",
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
                    "SELECT outcome_json FROM action_receipts WHERE action_id = ?1",
                    [&action_id],
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
                    "SELECT management_revision FROM target_route_state WHERE target = 'codex'",
                    [],
                    |row| row.get(0),
                )?;
                let recovery_state: String = transaction.query_row(
                    "SELECT recovery_state FROM target_route_state WHERE target = 'codex'",
                    [],
                    |row| row.get(0),
                )?;
                if recovery_state == "recovery-required" {
                    return Ok(ProviderAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: "recovery-required".to_owned(),
                            message: "Managed writes are blocked until recovery is resolved"
                                .to_owned(),
                        },
                        authoritative_view: project_target_view(&transaction, &service_epoch)?,
                    }));
                }
                if current_revision != expected_revision {
                    let authoritative_view = project_target_view(&transaction, &service_epoch)?;
                    return Ok(ProviderAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: "stale-revision".to_owned(),
                            message: "Target state changed; refresh and retry".to_owned(),
                        },
                        authoritative_view,
                    }));
                }
                if let Err(error) = super::providers::mutate_provider(&transaction, action) {
                    let (code, message) = match error {
                        super::providers::ProviderMutationError::Invalid => {
                            ("invalid-provider", "Provider declaration is invalid")
                        }
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
                        authoritative_view: project_target_view(&transaction, &service_epoch)?,
                    }));
                }
                transaction.execute(
                    "UPDATE target_route_state
                     SET management_revision = management_revision + 1,
                         view_sequence = view_sequence + 1
                     WHERE target = 'codex'",
                    [],
                )?;
                let view = project_target_view(&transaction, &service_epoch)?;
                let outcome = ActionOutcome {
                    status: ActionStatus::Applied,
                    view,
                };
                let outcome_json = serde_json::to_string(&outcome)?;
                transaction.execute(
                    "INSERT INTO action_receipts
                     (action_id, action_kind, committed_revision, outcome_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
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
