use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio_rusqlite::rusqlite::params;
use uuid::Uuid;

use crate::codex::{ConfigSnapshot, DesiredCodexState};

use super::{StateError, StateStore};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryState {
    Pending,
    Committed,
    RolledBack,
    RecoveryRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagedWriteStatus {
    Allowed,
    RecoveryRequired,
}

impl RecoveryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Committed => "committed",
            Self::RolledBack => "rolled-back",
            Self::RecoveryRequired => "recovery-required",
        }
    }

    fn parse(value: &str) -> Result<Self, StateError> {
        match value {
            "pending" => Ok(Self::Pending),
            "committed" => Ok(Self::Committed),
            "rolled-back" => Ok(Self::RolledBack),
            "recovery-required" => Ok(Self::RecoveryRequired),
            _ => Err(StateError::InvalidRecoveryState),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct RecoveryIntent {
    id: Uuid,
    action_id: Uuid,
    config_path: PathBuf,
    before: ConfigSnapshot,
    desired: DesiredCodexState,
    state: RecoveryState,
    created_revision: u64,
}

impl fmt::Debug for RecoveryIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveryIntent")
            .field("id", &self.id)
            .field("action_id", &self.action_id)
            .field("config_path", &self.config_path)
            .field("state", &self.state)
            .field("created_revision", &self.created_revision)
            .finish()
    }
}

impl RecoveryIntent {
    pub fn pending(
        id: Uuid,
        action_id: Uuid,
        config_path: PathBuf,
        before: ConfigSnapshot,
        desired: DesiredCodexState,
        created_revision: u64,
    ) -> Self {
        Self {
            id,
            action_id,
            config_path,
            before,
            desired,
            state: RecoveryState::Pending,
            created_revision,
        }
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn before(&self) -> &ConfigSnapshot {
        &self.before
    }

    pub fn desired(&self) -> &DesiredCodexState {
        &self.desired
    }

    pub fn state(&self) -> RecoveryState {
        self.state
    }
}

impl StateStore {
    pub async fn insert_recovery_intent(&self, intent: &RecoveryIntent) -> Result<(), StateError> {
        let intent = intent.clone();
        self.connection
            .call(move |connection| -> Result<(), StateError> {
                let identity_json = serde_json::to_string(intent.before.identity())?;
                let before_json = serde_json::to_string(&intent.before)?;
                let desired_json = serde_json::to_string(&intent.desired)?;
                connection.execute(
                    "INSERT INTO activation_recovery
                     (id, target, action_id, config_path, file_identity_json,
                      before_owned_json, desired_owned_json, state, created_revision)
                     VALUES (?1, 'codex', ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(action_id) DO UPDATE SET
                       id = excluded.id,
                       config_path = excluded.config_path,
                       file_identity_json = excluded.file_identity_json,
                       before_owned_json = excluded.before_owned_json,
                       desired_owned_json = excluded.desired_owned_json,
                       state = excluded.state,
                       created_revision = excluded.created_revision
                     WHERE activation_recovery.state = 'rolled-back'",
                    params![
                        intent.id.to_string(),
                        intent.action_id.to_string(),
                        intent.config_path.to_string_lossy(),
                        identity_json,
                        before_json,
                        desired_json,
                        intent.state.as_str(),
                        intent.created_revision,
                    ],
                )?;
                let stored_id: String = connection.query_row(
                    "SELECT id FROM activation_recovery WHERE action_id = ?1",
                    [intent.action_id.to_string()],
                    |row| row.get(0),
                )?;
                if stored_id != intent.id.to_string() {
                    return Err(StateError::MissingRecoveryIntent);
                }
                Ok(())
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub async fn recovery_intent(&self, id: Uuid) -> Result<Option<RecoveryIntent>, StateError> {
        self.connection
            .call(move |connection| load_intent(connection, &id.to_string()))
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub async fn pending_recovery_intents(&self) -> Result<Vec<RecoveryIntent>, StateError> {
        self.connection
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id, action_id, config_path, before_owned_json,
                            desired_owned_json, state, created_revision
                     FROM activation_recovery WHERE state = 'pending' ORDER BY rowid",
                )?;
                let rows = statement.query_map([], parse_intent_row)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StateError::from)
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub async fn set_recovery_state(
        &self,
        id: Uuid,
        state: RecoveryState,
    ) -> Result<(), StateError> {
        self.connection
            .call(move |connection| -> Result<(), StateError> {
                let transaction = connection.transaction()?;
                let changed = transaction.execute(
                    "UPDATE activation_recovery SET state = ?1 WHERE id = ?2",
                    params![state.as_str(), id.to_string()],
                )?;
                if changed == 0 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                if state == RecoveryState::RecoveryRequired {
                    transaction.execute(
                        "UPDATE target_route_state
                         SET recovery_state = 'recovery-required',
                             view_sequence = view_sequence + CASE
                               WHEN recovery_state = 'recovery-required' THEN 0 ELSE 1 END
                         WHERE target = 'codex'",
                        [],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub async fn ensure_managed_writes_allowed(&self) -> Result<(), crate::codex::CodexProblem> {
        match self.managed_write_status().await {
            Ok(ManagedWriteStatus::Allowed) => Ok(()),
            Ok(ManagedWriteStatus::RecoveryRequired) | Err(_) => {
                Err(crate::codex::CodexProblem::new("recovery-required", None))
            }
        }
    }

    pub async fn managed_write_status(&self) -> Result<ManagedWriteStatus, StateError> {
        self.connection
            .call(|connection| -> Result<ManagedWriteStatus, StateError> {
                let state: String = connection.query_row(
                    "SELECT recovery_state FROM target_route_state WHERE target = 'codex'",
                    [],
                    |row| row.get(0),
                )?;
                match state.as_str() {
                    "clean" => Ok(ManagedWriteStatus::Allowed),
                    "recovery-required" => Ok(ManagedWriteStatus::RecoveryRequired),
                    _ => Err(StateError::InvalidRecoveryState),
                }
            })
            .await
            .map_err(super::store::map_state_call_error)
    }
}

fn load_intent(
    connection: &tokio_rusqlite::rusqlite::Connection,
    id: &str,
) -> Result<Option<RecoveryIntent>, StateError> {
    let result = connection.query_row(
        "SELECT id, action_id, config_path, before_owned_json,
                desired_owned_json, state, created_revision
         FROM activation_recovery WHERE id = ?1",
        [id],
        parse_intent_row,
    );
    match result {
        Ok(intent) => Ok(Some(intent)),
        Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(StateError::Sqlite(error)),
    }
}

fn parse_intent_row(
    row: &tokio_rusqlite::rusqlite::Row<'_>,
) -> tokio_rusqlite::rusqlite::Result<RecoveryIntent> {
    let id: String = row.get(0)?;
    let action_id: String = row.get(1)?;
    let before_json: String = row.get(3)?;
    let desired_json: String = row.get(4)?;
    let state: String = row.get(5)?;
    Ok(RecoveryIntent {
        id: Uuid::parse_str(&id).map_err(conversion_error)?,
        action_id: Uuid::parse_str(&action_id).map_err(conversion_error)?,
        config_path: PathBuf::from(row.get::<_, String>(2)?),
        before: serde_json::from_str(&before_json).map_err(conversion_error)?,
        desired: serde_json::from_str(&desired_json).map_err(conversion_error)?,
        state: RecoveryState::parse(&state).map_err(conversion_error)?,
        created_revision: row.get(6)?,
    })
}

fn conversion_error(
    error: impl std::error::Error + Send + Sync + 'static,
) -> tokio_rusqlite::rusqlite::Error {
    tokio_rusqlite::rusqlite::Error::FromSqlConversionFailure(
        0,
        tokio_rusqlite::rusqlite::types::Type::Text,
        Box::new(error),
    )
}
