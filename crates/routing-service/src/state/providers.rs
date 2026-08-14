use std::collections::HashSet;

use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{
    control::protocol::{
        CredentialEdit, DuplicateCredential, ProviderPresetView, ProviderProtocol,
    },
    domain::provider::normalize_provider_base_url,
};

pub const OPENAI_API_RESPONSES_PRESET_KEY: &str = "openai-api-responses";
pub const OPENAI_API_RESPONSES_BASE_URL: &str = "https://api.openai.com/v1";

struct SourceDeclaration {
    position: u32,
    provider_revision: u64,
    protocol: String,
    credential_id: Option<String>,
    provenance_kind: Option<String>,
    provenance_key: Option<String>,
    generated_owner_id: Option<String>,
}

pub(crate) fn provider_presets() -> Vec<ProviderPresetView> {
    vec![ProviderPresetView {
        key: OPENAI_API_RESPONSES_PRESET_KEY.to_owned(),
        base_url: OPENAI_API_RESPONSES_BASE_URL.to_owned(),
        model: String::new(),
        protocol: ProviderProtocol::OpenaiResponses,
    }]
}

pub(super) enum ProviderAction {
    Create {
        name: String,
        base_url: String,
        model: String,
        credential: CredentialEdit,
        preset_key: Option<String>,
    },
    Update {
        provider_id: String,
        provider_revision: u64,
        name: String,
        base_url: String,
        model: String,
        credential: CredentialEdit,
    },
    Reorder {
        provider_ids: Vec<Uuid>,
    },
    Delete {
        provider_id: Uuid,
        provider_revision: u64,
    },
    Duplicate {
        source_provider_id: Uuid,
        source_provider_revision: u64,
        name: String,
        base_url: String,
        model: String,
        credential: DuplicateCredential,
    },
}

pub(super) enum ProviderMutationError {
    Invalid,
    InvalidOrder,
    ProviderReferenced,
    StaleProviderRevision,
    NoProviderChange,
}

pub(super) fn mutate_provider(
    transaction: &Transaction<'_>,
    action: ProviderAction,
) -> Result<(), ProviderMutationError> {
    match action {
        ProviderAction::Create {
            name,
            base_url,
            model,
            credential,
            preset_key,
        } => create_provider(transaction, name, base_url, model, credential, preset_key),
        ProviderAction::Update {
            provider_id,
            provider_revision,
            name,
            base_url,
            model,
            credential,
        } => update_provider(
            transaction,
            provider_id,
            provider_revision,
            name,
            base_url,
            model,
            credential,
        ),
        ProviderAction::Reorder { provider_ids } => reorder_providers(transaction, &provider_ids),
        ProviderAction::Delete {
            provider_id,
            provider_revision,
        } => delete_provider(transaction, provider_id, provider_revision),
        ProviderAction::Duplicate {
            source_provider_id,
            source_provider_revision,
            name,
            base_url,
            model,
            credential,
        } => duplicate_provider(
            transaction,
            source_provider_id,
            source_provider_revision,
            name,
            base_url,
            model,
            credential,
        ),
    }
}

pub(super) fn reorder_providers(
    transaction: &Transaction<'_>,
    provider_ids: &[Uuid],
) -> Result<(), ProviderMutationError> {
    let existing = transaction
        .prepare("SELECT id FROM providers WHERE target = 'codex' ORDER BY position")
        .map_err(|_| ProviderMutationError::Invalid)?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|_| ProviderMutationError::Invalid)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ProviderMutationError::Invalid)?
        .into_iter()
        .map(|id| Uuid::parse_str(&id).map_err(|_| ProviderMutationError::Invalid))
        .collect::<Result<Vec<_>, _>>()?;
    let requested = provider_ids.iter().copied().collect::<HashSet<_>>();
    let expected = existing.iter().copied().collect::<HashSet<_>>();
    if provider_ids.len() != existing.len()
        || requested.len() != provider_ids.len()
        || requested != expected
    {
        return Err(ProviderMutationError::InvalidOrder);
    }
    if provider_ids == existing {
        return Err(ProviderMutationError::NoProviderChange);
    }

    let temporary_start: u64 = transaction
        .query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM providers WHERE target = 'codex'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    for (position, provider_id) in provider_ids.iter().enumerate() {
        let temporary_position = temporary_start
            .checked_add(position as u64)
            .ok_or(ProviderMutationError::Invalid)?;
        transaction
            .execute(
                "UPDATE providers SET position = ?1 WHERE id = ?2 AND target = 'codex'",
                params![temporary_position, provider_id.to_string()],
            )
            .map_err(|_| ProviderMutationError::Invalid)?;
    }
    for (position, provider_id) in provider_ids.iter().enumerate() {
        transaction
            .execute(
                "UPDATE providers SET position = ?1 WHERE id = ?2 AND target = 'codex'",
                params![position as u32, provider_id.to_string()],
            )
            .map_err(|_| ProviderMutationError::Invalid)?;
    }
    Ok(())
}

pub(super) fn delete_provider(
    transaction: &Transaction<'_>,
    provider_id: Uuid,
    provider_revision: u64,
) -> Result<(), ProviderMutationError> {
    let (position, credential_id, revision, generated_owner_id): (
        u32,
        Option<String>,
        u64,
        Option<String>,
    ) = transaction
        .query_row(
            "SELECT position, credential_id, provider_revision, generated_owner_id
             FROM providers WHERE id = ?1 AND target = 'codex'",
            [provider_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|_| ProviderMutationError::Invalid)?
        .ok_or(ProviderMutationError::Invalid)?;
    if revision != provider_revision {
        return Err(ProviderMutationError::StaleProviderRevision);
    }
    if generated_owner_id.is_some() {
        return Err(ProviderMutationError::Invalid);
    }
    let referenced: bool = transaction
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM target_route_state
                 WHERE target = 'codex' AND current_provider_id = ?1
                UNION ALL
                SELECT 1 FROM target_route_state r
                 JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
                 WHERE r.target = 'codex' AND s.provider_id = ?1
             )",
            [provider_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    if referenced {
        return Err(ProviderMutationError::ProviderReferenced);
    }
    transaction
        .execute(
            "DELETE FROM providers WHERE id = ?1 AND target = 'codex'",
            [provider_id.to_string()],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    transaction
        .execute(
            "UPDATE providers SET position = position - 1
             WHERE target = 'codex' AND position > ?1",
            [position],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    if let Some(credential_id) = credential_id {
        transaction
            .execute(
                "DELETE FROM credentials
                 WHERE id = ?1
                   AND NOT EXISTS (SELECT 1 FROM providers WHERE credential_id = ?1)",
                [credential_id],
            )
            .map_err(|_| ProviderMutationError::Invalid)?;
    }
    Ok(())
}

fn create_provider(
    transaction: &Transaction<'_>,
    name: String,
    base_url: String,
    model: String,
    credential: CredentialEdit,
    preset_key: Option<String>,
) -> Result<(), ProviderMutationError> {
    let name = normalized_name(name)?;
    let base_url = normalized_base_url(base_url)?;
    let provenance = match preset_key {
        Some(key) if key == OPENAI_API_RESPONSES_PRESET_KEY => {
            (Some("preset"), Some(OPENAI_API_RESPONSES_PRESET_KEY))
        }
        Some(_) => return Err(ProviderMutationError::Invalid),
        None => (None, None),
    };
    let credential_id = match credential {
        CredentialEdit::Keep => return Err(ProviderMutationError::Invalid),
        CredentialEdit::Remove => None,
        CredentialEdit::Replace { value } => Some(insert_credential(transaction, value)?),
    };
    let position = transaction
        .query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM providers WHERE target = 'codex'",
            [],
            |row| row.get::<_, u32>(0),
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    transaction
        .execute(
            "INSERT INTO providers
             (id, target, position, provider_revision, name, base_url, model, protocol, credential_id,
              provenance_kind, provenance_key, generated_owner_id)
             VALUES (?1, 'codex', ?2, 1, ?3, ?4, ?5, 'openai-responses', ?6, ?7, ?8, NULL)",
            params![
                Uuid::new_v4().to_string(),
                position,
                name,
                base_url,
                model,
                credential_id,
                provenance.0,
                provenance.1,
            ],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn duplicate_provider(
    transaction: &Transaction<'_>,
    source_provider_id: Uuid,
    source_provider_revision: u64,
    name: String,
    base_url: String,
    model: String,
    credential: DuplicateCredential,
) -> Result<(), ProviderMutationError> {
    let name = normalized_name(name)?;
    let base_url = normalized_base_url(base_url)?;
    let source = transaction
        .query_row(
            "SELECT position, provider_revision, protocol, credential_id, provenance_kind,
                    provenance_key, generated_owner_id
             FROM providers WHERE id = ?1 AND target = 'codex'",
            [source_provider_id.to_string()],
            |row| {
                Ok(SourceDeclaration {
                    position: row.get(0)?,
                    provider_revision: row.get(1)?,
                    protocol: row.get(2)?,
                    credential_id: row.get(3)?,
                    provenance_kind: row.get(4)?,
                    provenance_key: row.get(5)?,
                    generated_owner_id: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(|_| ProviderMutationError::Invalid)?
        .ok_or(ProviderMutationError::Invalid)?;
    if source.provider_revision != source_provider_revision {
        return Err(ProviderMutationError::StaleProviderRevision);
    }
    let credential_id = match credential {
        DuplicateCredential::Without => None,
        DuplicateCredential::ReuseSource => source.credential_id.clone(),
        DuplicateCredential::Replace { value } => Some(insert_credential(transaction, value)?),
    };
    let position = source
        .position
        .checked_add(1)
        .ok_or(ProviderMutationError::Invalid)?;
    let (provenance_kind, provenance_key) = if source.generated_owner_id.is_some()
        && source.provenance_kind.as_deref() == Some("universal-provider")
    {
        (None, None)
    } else {
        (source.provenance_kind, source.provenance_key)
    };
    transaction
        .execute(
            "UPDATE providers SET position = position + 1
             WHERE target = 'codex' AND position > ?1",
            [source.position],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    transaction
        .execute(
            "INSERT INTO providers
             (id, target, position, provider_revision, name, base_url, model, protocol, credential_id,
              provenance_kind, provenance_key, generated_owner_id)
             VALUES (?1, 'codex', ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            params![
                Uuid::new_v4().to_string(),
                position,
                name,
                base_url,
                model,
                source.protocol,
                credential_id,
                provenance_kind,
                provenance_key,
            ],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_provider(
    transaction: &Transaction<'_>,
    provider_id: String,
    provider_revision: u64,
    name: String,
    base_url: String,
    model: String,
    credential: CredentialEdit,
) -> Result<(), ProviderMutationError> {
    let provider_id = Uuid::parse_str(&provider_id).map_err(|_| ProviderMutationError::Invalid)?;
    let name = normalized_name(name)?;
    let base_url = normalized_base_url(base_url)?;
    let existing = transaction
        .query_row(
            "SELECT name, base_url, model, credential_id, provider_revision
             FROM providers WHERE id = ?1 AND target = 'codex'",
            [provider_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| ProviderMutationError::Invalid)?
        .ok_or(ProviderMutationError::Invalid)?;
    if existing.4 != provider_revision {
        return Err(ProviderMutationError::StaleProviderRevision);
    }

    let credential_id = match credential {
        CredentialEdit::Keep => existing.3.clone(),
        CredentialEdit::Remove => None,
        CredentialEdit::Replace { value } => Some(insert_credential(transaction, value)?),
    };
    if existing.0 == name
        && existing.1 == base_url
        && existing.2 == model
        && existing.3 == credential_id
    {
        return Err(ProviderMutationError::NoProviderChange);
    }
    transaction
        .execute(
            "UPDATE providers
             SET name = ?1, base_url = ?2, model = ?3, credential_id = ?4,
                 provider_revision = provider_revision + 1
             WHERE id = ?5",
            params![
                name,
                base_url,
                model,
                credential_id,
                provider_id.to_string()
            ],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    if let Some(previous_credential_id) = existing.3.filter(|id| Some(id) != credential_id.as_ref())
    {
        transaction
            .execute(
                "DELETE FROM credentials
                 WHERE id = ?1
                   AND NOT EXISTS (SELECT 1 FROM providers WHERE credential_id = ?1)",
                [previous_credential_id],
            )
            .map_err(|_| ProviderMutationError::Invalid)?;
    }
    Ok(())
}

fn normalized_name(name: String) -> Result<String, ProviderMutationError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(ProviderMutationError::Invalid);
    }
    Ok(name)
}

fn normalized_base_url(base_url: String) -> Result<String, ProviderMutationError> {
    if base_url.is_empty() {
        return Ok(base_url);
    }
    normalize_provider_base_url(&base_url).map_err(|_| ProviderMutationError::Invalid)
}

fn insert_credential(
    transaction: &Transaction<'_>,
    value: String,
) -> Result<String, ProviderMutationError> {
    if value.trim().is_empty() {
        return Err(ProviderMutationError::Invalid);
    }
    let id = Uuid::new_v4().to_string();
    transaction
        .execute(
            "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, 'codex', ?2)",
            params![id, value],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    Ok(id)
}
