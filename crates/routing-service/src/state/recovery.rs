use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize, de::Error as _};
use tokio_rusqlite::rusqlite::params;
use uuid::Uuid;

use crate::control::protocol::Target;
use crate::{
    claude::{ClaudeConfigOwnership, ClaudeConfigSnapshot, DesiredClaudeState},
    codex::{ConfigSnapshot, DesiredCodexState},
};

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
    ConfigurationDrift,
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

#[derive(Clone, Serialize)]
pub struct RecoveryIntent {
    id: Uuid,
    target: Target,
    action_id: Uuid,
    config_path: PathBuf,
    payload: RecoveryPayload,
    state: RecoveryState,
    created_revision: u64,
}

#[derive(Clone)]
pub enum RecoveryPayload {
    Codex {
        before: Box<ConfigSnapshot>,
        desired: Box<DesiredCodexState>,
    },
    Claude {
        before: Box<ClaudeConfigSnapshot>,
        desired: Box<DesiredClaudeState>,
    },
    ClaudeLegacy {
        payload: serde_json::Value,
    },
}

impl Serialize for RecoveryPayload {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            Self::Codex { before, desired } => serde_json::json!({
                "target": "codex",
                "before": before,
                "desired": desired,
            }),
            Self::Claude { before, desired } => serde_json::json!({
                "target": "claude",
                "ownership_version": before.ownership(),
                "before": before,
                "desired": desired,
            }),
            Self::ClaudeLegacy { payload } => serde_json::json!({
                "target": "claude",
                "payload": payload,
            }),
        };
        value.serialize(serializer)
    }
}

impl fmt::Debug for RecoveryPayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex { .. } => formatter.write_str("RecoveryPayload::Codex(<redacted>)"),
            Self::Claude { .. } => formatter.write_str("RecoveryPayload::Claude(<redacted>)"),
            Self::ClaudeLegacy { .. } => {
                formatter.write_str("RecoveryPayload::ClaudeLegacy(<redacted>)")
            }
        }
    }
}

impl<'de> Deserialize<'de> for RecoveryPayload {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = serde_json::Value::deserialize(deserializer)?;
        let target = value
            .get("target")
            .and_then(|value| value.as_str())
            .ok_or_else(|| D::Error::custom("missing recovery target"))?
            .to_owned();
        let object = value
            .as_object_mut()
            .ok_or_else(|| D::Error::custom("invalid recovery payload"))?;
        object.remove("target");
        match target.as_str() {
            "codex" => Ok(Self::Codex {
                before: Box::new(
                    serde_json::from_value(
                        object
                            .remove("before")
                            .ok_or_else(|| D::Error::custom("missing Codex before"))?,
                    )
                    .map_err(D::Error::custom)?,
                ),
                desired: Box::new(
                    serde_json::from_value(
                        object
                            .remove("desired")
                            .ok_or_else(|| D::Error::custom("missing Codex desired"))?,
                    )
                    .map_err(D::Error::custom)?,
                ),
            }),
            "claude" => {
                let declared_ownership = object
                    .remove("ownership_version")
                    .map(serde_json::from_value::<ClaudeConfigOwnership>)
                    .transpose()
                    .map_err(D::Error::custom)?;
                let claude = object
                    .remove("payload")
                    .unwrap_or_else(|| serde_json::Value::Object(object.clone()));
                let claude_object = claude
                    .as_object()
                    .ok_or_else(|| D::Error::custom("invalid Claude recovery payload"))?;
                let typed = claude_object
                    .get("before")
                    .cloned()
                    .zip(claude_object.get("desired").cloned())
                    .and_then(|(before, desired)| {
                        Some((
                            serde_json::from_value::<ClaudeConfigSnapshot>(before).ok()?,
                            serde_json::from_value::<DesiredClaudeState>(desired).ok()?,
                        ))
                    });
                match typed {
                    Some((before, desired))
                        if before.ownership() == desired.ownership()
                            && declared_ownership.unwrap_or(ClaudeConfigOwnership::LegacyThree)
                                == before.ownership() =>
                    {
                        Ok(Self::Claude {
                            before: Box::new(before),
                            desired: Box::new(desired),
                        })
                    }
                    Some(_) => Err(D::Error::custom(
                        "inconsistent Claude configuration ownership version",
                    )),
                    None => Ok(Self::ClaudeLegacy { payload: claude }),
                }
            }
            _ => Err(D::Error::custom("invalid recovery target")),
        }
    }
}

impl RecoveryPayload {
    pub fn target(&self) -> Target {
        match self {
            Self::Codex { .. } => Target::Codex,
            Self::Claude { .. } | Self::ClaudeLegacy { .. } => Target::Claude,
        }
    }
}

impl fmt::Debug for RecoveryIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveryIntent")
            .field("id", &self.id)
            .field("target", &self.target)
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
        Self::pending_for_target(
            Target::Codex,
            id,
            action_id,
            config_path,
            RecoveryPayload::Codex {
                before: Box::new(before),
                desired: Box::new(desired),
            },
            created_revision,
        )
        .expect("Codex payload has the Codex target")
    }

    pub fn pending_for_target(
        target: Target,
        id: Uuid,
        action_id: Uuid,
        config_path: PathBuf,
        payload: RecoveryPayload,
        created_revision: u64,
    ) -> Result<Self, StateError> {
        if target != payload.target() {
            return Err(StateError::InvalidRecoveryPayload);
        }
        Ok(Self {
            id,
            target,
            action_id,
            config_path,
            payload,
            state: RecoveryState::Pending,
            created_revision,
        })
    }

    pub fn pending_claude(
        id: Uuid,
        action_id: Uuid,
        config_path: PathBuf,
        before: ClaudeConfigSnapshot,
        desired: DesiredClaudeState,
        created_revision: u64,
    ) -> Self {
        Self::pending_for_target(
            Target::Claude,
            id,
            action_id,
            config_path,
            RecoveryPayload::Claude {
                before: Box::new(before),
                desired: Box::new(desired),
            },
            created_revision,
        )
        .expect("Claude payload has the Claude target")
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn target(&self) -> Target {
        self.target
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn before(&self) -> &ConfigSnapshot {
        match &self.payload {
            RecoveryPayload::Codex { before, .. } => before,
            RecoveryPayload::Claude { .. } | RecoveryPayload::ClaudeLegacy { .. } => {
                unreachable!("Claude recovery is reconciled by its adapter")
            }
        }
    }

    pub fn desired(&self) -> &DesiredCodexState {
        match &self.payload {
            RecoveryPayload::Codex { desired, .. } => desired,
            RecoveryPayload::Claude { .. } | RecoveryPayload::ClaudeLegacy { .. } => {
                unreachable!("Claude recovery is reconciled by its adapter")
            }
        }
    }

    pub fn claude_before(&self) -> Option<&ClaudeConfigSnapshot> {
        match &self.payload {
            RecoveryPayload::Claude { before, .. } => Some(before),
            _ => None,
        }
    }

    pub fn claude_desired(&self) -> Option<&DesiredClaudeState> {
        match &self.payload {
            RecoveryPayload::Claude { desired, .. } => Some(desired),
            _ => None,
        }
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
                let identity_json = match &intent.payload {
                    RecoveryPayload::Codex { before, .. } => {
                        serde_json::to_string(before.identity())?
                    }
                    RecoveryPayload::Claude { before, .. } => {
                        serde_json::to_string(before.identity())?
                    }
                    RecoveryPayload::ClaudeLegacy { .. } => "null".to_owned(),
                };
                let payload_json = serde_json::to_string(&intent.payload)?;
                connection.execute(
                    "INSERT INTO activation_recovery
                     (id, target, action_id, config_path, file_identity_json,
                      payload_json, state, created_revision)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(target, action_id) DO UPDATE SET
                       id = excluded.id,
                       config_path = excluded.config_path,
                       file_identity_json = excluded.file_identity_json,
                       payload_json = excluded.payload_json,
                       state = excluded.state,
                       created_revision = excluded.created_revision
                     WHERE activation_recovery.target = excluded.target
                       AND activation_recovery.state = 'rolled-back'",
                    params![
                        intent.id.to_string(),
                        intent.target.as_str(),
                        intent.action_id.to_string(),
                        intent.config_path.to_string_lossy(),
                        identity_json,
                        payload_json,
                        intent.state.as_str(),
                        intent.created_revision,
                    ],
                )?;
                let stored_id: String = connection.query_row(
                    "SELECT id FROM activation_recovery WHERE target = ?1 AND action_id = ?2",
                    params![intent.target.as_str(), intent.action_id.to_string()],
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

    pub async fn recovery_intent_for(
        &self,
        target: Target,
        action_id: Uuid,
    ) -> Result<Option<RecoveryIntent>, StateError> {
        self.connection
            .call(move |connection| {
                let result = connection.query_row(
                    "SELECT id, target, action_id, config_path, payload_json, state, created_revision
                     FROM activation_recovery WHERE target = ?1 AND action_id = ?2",
                    params![target.as_str(), action_id.to_string()],
                    parse_intent_row,
                );
                match result {
                    Ok(intent) => Ok(Some(intent)),
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(error) => Err(StateError::Sqlite(error)),
                }
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub async fn pending_recovery_intents(&self) -> Result<Vec<RecoveryIntent>, StateError> {
        self.connection
            .call(|connection| {
                let mut statement = connection.prepare(
                    "SELECT id, target, action_id, config_path, payload_json, state, created_revision
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
                let target: String = transaction.query_row(
                    "SELECT target FROM activation_recovery WHERE id = ?1",
                    [id.to_string()],
                    |row| row.get(0),
                )?;
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
                         WHERE target = ?1",
                        [target],
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
            Ok(ManagedWriteStatus::ConfigurationDrift) => {
                Err(crate::codex::CodexProblem::new("configuration-drift", None))
            }
            Ok(ManagedWriteStatus::RecoveryRequired) | Err(_) => {
                Err(crate::codex::CodexProblem::new("recovery-required", None))
            }
        }
    }

    pub async fn managed_write_status(&self) -> Result<ManagedWriteStatus, StateError> {
        self.managed_write_status_for(Target::Codex).await
    }

    pub async fn managed_write_status_for(
        &self,
        target: Target,
    ) -> Result<ManagedWriteStatus, StateError> {
        self.connection
            .call(
                move |connection| -> Result<ManagedWriteStatus, StateError> {
                    let (state, drifted): (String, bool) = connection.query_row(
                        "SELECT r.recovery_state,
                                EXISTS(SELECT 1 FROM target_problems p
                                  WHERE p.target = r.target AND p.code = 'configuration-drift')
                         FROM target_route_state r WHERE r.target = ?1",
                        [target.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?;
                    match state.as_str() {
                        "clean" if drifted => Ok(ManagedWriteStatus::ConfigurationDrift),
                        "clean" => Ok(ManagedWriteStatus::Allowed),
                        "recovery-required" => Ok(ManagedWriteStatus::RecoveryRequired),
                        _ => Err(StateError::InvalidRecoveryState),
                    }
                },
            )
            .await
            .map_err(super::store::map_state_call_error)
    }
}

fn load_intent(
    connection: &tokio_rusqlite::rusqlite::Connection,
    id: &str,
) -> Result<Option<RecoveryIntent>, StateError> {
    let result = connection.query_row(
        "SELECT id, target, action_id, config_path, payload_json, state, created_revision
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
    let target: String = row.get(1)?;
    let action_id: String = row.get(2)?;
    let payload_json: String = row.get(4)?;
    let state: String = row.get(5)?;
    let target = match target.as_str() {
        "codex" => Target::Codex,
        "claude" => Target::Claude,
        _ => return Err(conversion_error(StateError::InvalidRecoveryState)),
    };
    let payload: RecoveryPayload = serde_json::from_str(&payload_json).map_err(conversion_error)?;
    if payload.target() != target {
        return Err(conversion_error(StateError::InvalidRecoveryPayload));
    }
    Ok(RecoveryIntent {
        id: Uuid::parse_str(&id).map_err(conversion_error)?,
        target,
        action_id: Uuid::parse_str(&action_id).map_err(conversion_error)?,
        config_path: PathBuf::from(row.get::<_, String>(3)?),
        payload,
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
