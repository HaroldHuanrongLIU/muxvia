use tokio_rusqlite::rusqlite::{OptionalExtension, params};

use crate::control::protocol::{CompatibilityClassification, CompatibilityView, Target};

use super::{StateError, StateStore};

impl StateStore {
    pub async fn record_compatibility(
        &self,
        target: Target,
        version: String,
        classification: CompatibilityClassification,
    ) -> Result<CompatibilityView, StateError> {
        self.connection
            .call(move |connection| {
                let classification = classification.as_str();
                connection.execute(
                    "INSERT INTO target_compatibility
                       (target, observed_version, classification, acknowledged_version)
                     VALUES (?1, ?2, ?3, NULL)
                     ON CONFLICT(target) DO UPDATE SET
                       acknowledged_version = CASE
                         WHEN target_compatibility.observed_version = excluded.observed_version
                           AND excluded.classification = 'unknown-compatible'
                           AND target_compatibility.classification = 'unknown-compatible'
                         THEN target_compatibility.acknowledged_version
                         ELSE NULL
                       END,
                       observed_version = excluded.observed_version,
                       classification = excluded.classification",
                    params![target.as_str(), version, classification],
                )?;
                read_compatibility(connection, target)
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub async fn compatibility_for(&self, target: Target) -> Result<CompatibilityView, StateError> {
        self.connection
            .call(move |connection| read_compatibility(connection, target))
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub async fn acknowledge_compatibility(
        &self,
        target: Target,
        version: &str,
    ) -> Result<CompatibilityView, StateError> {
        let version = version.to_owned();
        self.connection
            .call(move |connection| {
                let state = read_compatibility(connection, target)?;
                if state.classification != CompatibilityClassification::UnknownCompatible
                    || state.version != version
                {
                    return Err(StateError::InvalidCompatibilityState);
                }
                connection.execute(
                    "UPDATE target_compatibility SET acknowledged_version = ?1 WHERE target = ?2",
                    params![version, target.as_str()],
                )?;
                read_compatibility(connection, target)
            })
            .await
            .map_err(super::store::map_state_call_error)
    }
}

fn read_compatibility(
    connection: &tokio_rusqlite::rusqlite::Connection,
    target: Target,
) -> Result<CompatibilityView, StateError> {
    let row = connection
        .query_row(
            "SELECT observed_version, classification, acknowledged_version
             FROM target_compatibility WHERE target = ?1",
            [target.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    let (version, classification, acknowledged_version) =
        row.ok_or(StateError::MissingCompatibility)?;
    let classification = CompatibilityClassification::from_db(&classification)
        .ok_or(StateError::InvalidCompatibilityState)?;
    match (classification, acknowledged_version.as_deref()) {
        (
            CompatibilityClassification::Tested | CompatibilityClassification::Incompatible,
            Some(_),
        ) => {
            return Err(StateError::InvalidCompatibilityState);
        }
        (CompatibilityClassification::UnknownCompatible, Some(acknowledged))
            if acknowledged != version.as_str() =>
        {
            return Err(StateError::InvalidCompatibilityState);
        }
        _ => {}
    }
    Ok(CompatibilityView {
        acknowledgement_required: classification == CompatibilityClassification::UnknownCompatible
            && acknowledged_version.is_none(),
        version,
        classification,
    })
}

impl CompatibilityClassification {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tested => "tested",
            Self::UnknownCompatible => "unknown-compatible",
            Self::Incompatible => "incompatible",
        }
    }

    fn from_db(value: &str) -> Option<Self> {
        match value {
            "tested" => Some(Self::Tested),
            "unknown-compatible" => Some(Self::UnknownCompatible),
            "incompatible" => Some(Self::Incompatible),
            _ => None,
        }
    }
}
