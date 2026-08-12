use secrecy::{ExposeSecret, SecretString};
use tokio_rusqlite::{Connection, rusqlite::params};
use uuid::Uuid;

use crate::{
    control::protocol::{ActionOutcome, ActionStatus, ControlProblem, TargetAction, TargetView},
    domain::{
        provider::normalize_provider_base_url,
        view::{empty_target_view, project_target_view},
    },
    home::MuxviaHome,
};

const SCHEMA: &str = include_str!("schema.sql");

struct SaveProviderCommand {
    pub action_id: Uuid,
    pub expected_revision: u64,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub credential: SecretString,
}

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
}

pub struct StateStore {
    connection: Connection,
    service_epoch: String,
}

impl StateStore {
    pub async fn open(home: &MuxviaHome) -> Result<Self, StateError> {
        home.prepare_database()?;
        let connection = Connection::open(home.database_path()).await?;
        connection
            .call(|connection| {
                connection.execute_batch("PRAGMA foreign_keys = ON;")?;
                connection.execute_batch(SCHEMA)
            })
            .await
            .map_err(map_call_error)?;

        Ok(Self {
            connection,
            service_epoch: Uuid::new_v4().to_string(),
        })
    }

    pub async fn target_view(&self) -> Result<TargetView, StateError> {
        let service_epoch = self.service_epoch.clone();
        self.connection
            .call(move |connection| project_target_view(connection, &service_epoch))
            .await
            .map_err(map_call_error)
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

    pub async fn apply_save_provider_action(
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

        match serde_json::from_value(raw_action) {
            Ok(TargetAction::SaveProvider {
                name,
                base_url,
                model,
                credential,
            }) => {
                self.save_provider(SaveProviderCommand {
                    action_id,
                    expected_revision,
                    name,
                    base_url,
                    model,
                    credential: SecretString::from(credential),
                })
                .await
            }
            Ok(TargetAction::ActivateProvider { .. }) => Err(self
                .failure(
                    "unsupported-operation",
                    "Provider activation is not supported yet",
                )
                .await),
            Err(_) => Err(self
                .failure("invalid-provider", "Provider action is malformed")
                .await),
        }
    }

    async fn save_provider(
        &self,
        command: SaveProviderCommand,
    ) -> Result<ActionOutcome, ActionFailure> {
        match self.receipt(command.action_id).await {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) => {
                return Err(self
                    .failure("state-store-error", "State store operation failed")
                    .await);
            }
        }

        let service_epoch = self.service_epoch.clone();
        let action_id = command.action_id.to_string();
        let provider_id = Uuid::new_v4().to_string();
        let credential = command.credential.expose_secret().to_owned();
        let attempt = self
            .connection
            .call(move |connection| -> Result<SaveAttempt, StateError> {
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
                        return Ok(SaveAttempt::Applied(outcome));
                    }
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(error) => return Err(StateError::Sqlite(error)),
                }
                let current_revision: u64 = transaction.query_row(
                    "SELECT management_revision FROM target_route_state WHERE target = 'codex'",
                    [],
                    |row| row.get(0),
                )?;
                if current_revision != command.expected_revision {
                    let authoritative_view = project_target_view(&transaction, &service_epoch)?;
                    return Ok(SaveAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: "stale-revision".to_owned(),
                            message: "Target state changed; refresh and retry".to_owned(),
                        },
                        authoritative_view,
                    }));
                }
                if command.name.trim().is_empty()
                    || command.model.trim().is_empty()
                    || credential.trim().is_empty()
                {
                    let authoritative_view = project_target_view(&transaction, &service_epoch)?;
                    return Ok(SaveAttempt::Failure(ActionFailure {
                        problem: ControlProblem {
                            code: "incomplete-provider".to_owned(),
                            message: "Provider name, model, and credential are required".to_owned(),
                        },
                        authoritative_view,
                    }));
                }
                let normalized_url = match normalize_provider_base_url(&command.base_url) {
                    Ok(url) => url,
                    Err(_) => {
                        let authoritative_view = project_target_view(&transaction, &service_epoch)?;
                        return Ok(SaveAttempt::Failure(ActionFailure {
                            problem: ControlProblem {
                                code: "invalid-provider".to_owned(),
                                message: "Provider URL is not allowed".to_owned(),
                            },
                            authoritative_view,
                        }));
                    }
                };
                transaction.execute(
                    "INSERT INTO providers (id, target, name, base_url, model)
                     VALUES (?1, 'codex', ?2, ?3, ?4)",
                    params![provider_id, command.name, normalized_url, command.model],
                )?;
                transaction.execute(
                    "INSERT INTO provider_credentials (provider_id, bearer_token) VALUES (?1, ?2)",
                    params![provider_id, credential],
                )?;
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
                     VALUES (?1, 'save-provider', ?2, ?3)",
                    params![action_id, outcome.view.management_revision, outcome_json],
                )?;
                transaction.commit()?;
                Ok(SaveAttempt::Applied(outcome))
            })
            .await;

        match attempt {
            Ok(SaveAttempt::Applied(outcome)) => Ok(outcome),
            Ok(SaveAttempt::Failure(failure)) => Err(failure),
            Err(_) => Err(self
                .failure("state-store-error", "State store operation failed")
                .await),
        }
    }

    async fn failure(&self, code: &str, message: &str) -> ActionFailure {
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

enum SaveAttempt {
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

fn map_state_call_error(error: tokio_rusqlite::Error<StateError>) -> StateError {
    match error {
        tokio_rusqlite::Error::ConnectionClosed => StateError::Unavailable,
        tokio_rusqlite::Error::Error(error) => error,
        _ => StateError::Unavailable,
    }
}
