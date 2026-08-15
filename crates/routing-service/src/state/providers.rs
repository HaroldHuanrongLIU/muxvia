use std::collections::HashSet;

use secrecy::SecretString;
use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::domain::provider::has_valid_provider_declaration;
use crate::{
    control::protocol::{
        CredentialEdit, DuplicateCredential, ProviderAuthentication, ProviderPresetView,
        ProviderProtocol, Target,
    },
    domain::provider::normalize_provider_base_url,
};

pub const OPENAI_API_RESPONSES_PRESET_KEY: &str = "openai-api-responses";
pub const OPENAI_API_RESPONSES_BASE_URL: &str = "https://api.openai.com/v1";
pub const ANTHROPIC_API_MESSAGES_PRESET_KEY: &str = "anthropic-api-messages";
pub const ANTHROPIC_API_MESSAGES_BASE_URL: &str = "https://api.anthropic.com/v1";

struct SourceDeclaration {
    target: String,
    position: u32,
    provider_revision: u64,
    protocol: String,
    authentication: String,
    routing_requirement: String,
    credential_id: Option<String>,
    provenance_kind: Option<String>,
    provenance_key: Option<String>,
    generated_owner_id: Option<String>,
}

pub(crate) struct ProviderInspectionSnapshot {
    pub base_url: String,
    pub credential: Option<SecretString>,
    pub authentication: ProviderAuthentication,
}

pub(crate) enum ProviderInspectionRead {
    Found(ProviderInspectionSnapshot),
    Missing,
    StaleRevision,
}

pub(crate) fn read_provider_for_inspection(
    connection: &tokio_rusqlite::rusqlite::Connection,
    target: Target,
    provider_id: Uuid,
    provider_revision: u64,
) -> Result<ProviderInspectionRead, tokio_rusqlite::rusqlite::Error> {
    let provider = connection
        .query_row(
            "SELECT p.base_url, p.provider_revision, c.bearer_token, p.authentication
             FROM providers p LEFT JOIN credentials c ON c.id = p.credential_id
             WHERE p.id = ?1 AND p.target = ?2",
            params![provider_id.to_string(), target.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((base_url, actual_revision, credential, authentication)) = provider else {
        return Ok(ProviderInspectionRead::Missing);
    };
    if actual_revision != provider_revision {
        return Ok(ProviderInspectionRead::StaleRevision);
    }
    Ok(ProviderInspectionRead::Found(ProviderInspectionSnapshot {
        base_url,
        credential: credential.map(SecretString::from),
        authentication: parse_authentication(&authentication),
    }))
}

pub(crate) fn provider_presets(target: Target) -> Vec<ProviderPresetView> {
    let openai = ProviderPresetView {
        key: OPENAI_API_RESPONSES_PRESET_KEY.to_owned(),
        base_url: OPENAI_API_RESPONSES_BASE_URL.to_owned(),
        model: String::new(),
        protocol: ProviderProtocol::OpenaiResponses,
        authentication: ProviderAuthentication::OpenaiBearer,
    };
    let anthropic = ProviderPresetView {
        key: ANTHROPIC_API_MESSAGES_PRESET_KEY.to_owned(),
        base_url: ANTHROPIC_API_MESSAGES_BASE_URL.to_owned(),
        model: String::new(),
        protocol: ProviderProtocol::AnthropicMessages,
        authentication: ProviderAuthentication::AnthropicApiKey,
    };
    match target {
        Target::Codex => vec![openai],
        Target::Claude => vec![anthropic],
    }
}

pub(super) enum ProviderAction {
    Create {
        name: String,
        base_url: String,
        model: String,
        credential: CredentialEdit,
        authentication: Option<ProviderAuthentication>,
        preset_key: Option<String>,
    },
    Update {
        provider_id: String,
        provider_revision: u64,
        name: String,
        base_url: String,
        model: String,
        credential: CredentialEdit,
        authentication: Option<ProviderAuthentication>,
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
    target: Target,
    action: ProviderAction,
) -> Result<(), ProviderMutationError> {
    match action {
        ProviderAction::Create {
            name,
            base_url,
            model,
            credential,
            authentication,
            preset_key,
        } => create_provider(
            transaction,
            target,
            name,
            base_url,
            model,
            credential,
            authentication,
            preset_key,
        ),
        ProviderAction::Update {
            provider_id,
            provider_revision,
            name,
            base_url,
            model,
            credential,
            authentication,
        } => update_provider(
            transaction,
            target,
            provider_id,
            provider_revision,
            name,
            base_url,
            model,
            credential,
            authentication,
        ),
        ProviderAction::Reorder { provider_ids } => {
            reorder_providers_for(transaction, target, &provider_ids)
        }
        ProviderAction::Delete {
            provider_id,
            provider_revision,
        } => delete_provider_for(transaction, target, provider_id, provider_revision),
        ProviderAction::Duplicate {
            source_provider_id,
            source_provider_revision,
            name,
            base_url,
            model,
            credential,
        } => duplicate_provider(
            transaction,
            target,
            source_provider_id,
            source_provider_revision,
            name,
            base_url,
            model,
            credential,
        ),
    }
}

pub(super) fn reorder_providers_for(
    transaction: &Transaction<'_>,
    target: Target,
    provider_ids: &[Uuid],
) -> Result<(), ProviderMutationError> {
    let existing = transaction
        .prepare("SELECT id FROM providers WHERE target = ?1 ORDER BY position")
        .map_err(|_| ProviderMutationError::Invalid)?
        .query_map([target.as_str()], |row| row.get::<_, String>(0))
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
            "SELECT COALESCE(MAX(position) + 1, 0) FROM providers WHERE target = ?1",
            [target.as_str()],
            |row| row.get(0),
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    for (position, provider_id) in provider_ids.iter().enumerate() {
        let temporary_position = temporary_start
            .checked_add(position as u64)
            .ok_or(ProviderMutationError::Invalid)?;
        transaction
            .execute(
                "UPDATE providers SET position = ?1 WHERE id = ?2 AND target = ?3",
                params![temporary_position, provider_id.to_string(), target.as_str()],
            )
            .map_err(|_| ProviderMutationError::Invalid)?;
    }
    for (position, provider_id) in provider_ids.iter().enumerate() {
        transaction
            .execute(
                "UPDATE providers SET position = ?1 WHERE id = ?2 AND target = ?3",
                params![position as u32, provider_id.to_string(), target.as_str()],
            )
            .map_err(|_| ProviderMutationError::Invalid)?;
    }
    Ok(())
}

pub(super) fn delete_provider_for(
    transaction: &Transaction<'_>,
    target: Target,
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
             FROM providers WHERE id = ?1 AND target = ?2",
            params![provider_id.to_string(), target.as_str()],
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
                 WHERE target = ?2 AND current_provider_id = ?1
                UNION ALL
                SELECT 1 FROM target_route_state r
                 JOIN activated_snapshots s ON s.id = r.activated_snapshot_id
                 WHERE r.target = ?2 AND s.provider_id = ?1
             )",
            params![provider_id.to_string(), target.as_str()],
            |row| row.get(0),
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    if referenced {
        return Err(ProviderMutationError::ProviderReferenced);
    }
    transaction
        .execute(
            "DELETE FROM providers WHERE id = ?1 AND target = ?2",
            params![provider_id.to_string(), target.as_str()],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    transaction
        .execute(
            "UPDATE providers SET position = position - 1
             WHERE target = ?2 AND position > ?1",
            params![position, target.as_str()],
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

#[allow(clippy::too_many_arguments)]
fn create_provider(
    transaction: &Transaction<'_>,
    target: Target,
    name: String,
    base_url: String,
    model: String,
    credential: CredentialEdit,
    authentication: Option<ProviderAuthentication>,
    preset_key: Option<String>,
) -> Result<(), ProviderMutationError> {
    let name = normalized_name(name)?;
    let base_url = normalized_base_url(base_url)?;
    let preset_authentication = match preset_key.as_deref() {
        Some(key) if key == preset_key_for(target) => Some(preset_authentication_for(target)),
        Some(_) => return Err(ProviderMutationError::Invalid),
        None => None,
    };
    let provenance = match preset_authentication {
        Some(_) => (Some("preset"), Some(preset_key_for(target))),
        None => (None, None),
    };
    let credential_id = match credential {
        CredentialEdit::Keep => return Err(ProviderMutationError::Invalid),
        CredentialEdit::Remove => None,
        CredentialEdit::Replace { value } => Some(insert_credential(transaction, target, value)?),
    };
    let authentication = authentication
        .or(preset_authentication)
        .or_else(|| (target == Target::Codex).then_some(ProviderAuthentication::OpenaiBearer))
        .ok_or(ProviderMutationError::Invalid)?;
    let protocol = protocol_for(target);
    if !has_valid_provider_declaration(target, protocol, authentication) {
        return Err(ProviderMutationError::Invalid);
    }
    let position = transaction
        .query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM providers WHERE target = ?1",
            [target.as_str()],
            |row| row.get::<_, u32>(0),
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    transaction
        .execute(
            "INSERT INTO providers
             (id, target, position, provider_revision, name, base_url, model, protocol,
              authentication, routing_requirement, credential_id, provenance_kind, provenance_key,
              generated_owner_id)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8,
                     'direct-compatible', ?9, ?10, ?11, NULL)",
            params![
                Uuid::new_v4().to_string(),
                target.as_str(),
                position,
                name,
                base_url,
                model,
                protocol.to_string(),
                authentication.to_string(),
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
    target: Target,
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
            "SELECT target, position, provider_revision, protocol, authentication, routing_requirement, credential_id,
                    provenance_kind, provenance_key, generated_owner_id
             FROM providers WHERE id = ?1 AND target = ?2",
            params![source_provider_id.to_string(), target.as_str()],
            |row| {
                Ok(SourceDeclaration {
                    target: row.get(0)?,
                    position: row.get(1)?,
                    provider_revision: row.get(2)?,
                    protocol: row.get(3)?,
                    authentication: row.get(4)?,
                    routing_requirement: row.get(5)?,
                    credential_id: row.get(6)?,
                    provenance_kind: row.get(7)?,
                    provenance_key: row.get(8)?,
                    generated_owner_id: row.get(9)?,
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
        DuplicateCredential::Replace { value } => {
            Some(insert_credential(transaction, target, value)?)
        }
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
             WHERE target = ?2 AND position > ?1",
            params![source.position, target.as_str()],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    transaction
        .execute(
            "INSERT INTO providers
             (id, target, position, provider_revision, name, base_url, model, protocol,
              authentication, routing_requirement, credential_id, provenance_kind, provenance_key,
              generated_owner_id)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL)",
            params![
                Uuid::new_v4().to_string(),
                source.target,
                position,
                name,
                base_url,
                model,
                source.protocol,
                source.authentication,
                source.routing_requirement,
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
    target: Target,
    provider_id: String,
    provider_revision: u64,
    name: String,
    base_url: String,
    model: String,
    credential: CredentialEdit,
    authentication: Option<ProviderAuthentication>,
) -> Result<(), ProviderMutationError> {
    let provider_id = Uuid::parse_str(&provider_id).map_err(|_| ProviderMutationError::Invalid)?;
    let name = normalized_name(name)?;
    let base_url = normalized_base_url(base_url)?;
    let existing = transaction
        .query_row(
            "SELECT name, base_url, model, credential_id, provider_revision, authentication, protocol
             FROM providers WHERE id = ?1 AND target = ?2",
            params![provider_id.to_string(), target.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, u64>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?,
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
        CredentialEdit::Replace { value } => Some(insert_credential(transaction, target, value)?),
    };
    let authentication = authentication.unwrap_or_else(|| parse_authentication(&existing.5));
    if existing.6 != protocol_for(target).to_string()
        || !has_valid_provider_declaration(target, protocol_for(target), authentication)
    {
        return Err(ProviderMutationError::Invalid);
    }
    if existing.0 == name
        && existing.1 == base_url
        && existing.2 == model
        && existing.3 == credential_id
        && existing.5 == authentication.to_string()
    {
        return Err(ProviderMutationError::NoProviderChange);
    }
    transaction
        .execute(
            "UPDATE providers
             SET name = ?1, base_url = ?2, model = ?3, credential_id = ?4, authentication = ?5,
                 provider_revision = provider_revision + 1
             WHERE id = ?6 AND target = ?7",
            params![
                name,
                base_url,
                model,
                credential_id,
                authentication.to_string(),
                provider_id.to_string(),
                target.as_str()
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
    target: Target,
    value: String,
) -> Result<String, ProviderMutationError> {
    if value.trim().is_empty() {
        return Err(ProviderMutationError::Invalid);
    }
    let id = Uuid::new_v4().to_string();
    transaction
        .execute(
            "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, ?3)",
            params![id, target.as_str(), value],
        )
        .map_err(|_| ProviderMutationError::Invalid)?;
    Ok(id)
}

fn protocol_for(target: Target) -> ProviderProtocol {
    match target {
        Target::Codex => ProviderProtocol::OpenaiResponses,
        Target::Claude => ProviderProtocol::AnthropicMessages,
    }
}
fn preset_key_for(target: Target) -> &'static str {
    match target {
        Target::Codex => OPENAI_API_RESPONSES_PRESET_KEY,
        Target::Claude => ANTHROPIC_API_MESSAGES_PRESET_KEY,
    }
}
fn preset_authentication_for(target: Target) -> ProviderAuthentication {
    match target {
        Target::Codex => ProviderAuthentication::OpenaiBearer,
        Target::Claude => ProviderAuthentication::AnthropicApiKey,
    }
}
fn parse_authentication(value: &str) -> ProviderAuthentication {
    match value {
        "anthropic-api-key" => ProviderAuthentication::AnthropicApiKey,
        "anthropic-bearer" => ProviderAuthentication::AnthropicBearer,
        _ => ProviderAuthentication::OpenaiBearer,
    }
}
