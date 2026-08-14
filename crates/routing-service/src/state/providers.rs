use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{
    control::protocol::{CredentialEdit, ProviderPresetView, ProviderProtocol},
    domain::provider::normalize_provider_base_url,
};

pub const OPENAI_API_RESPONSES_PRESET_KEY: &str = "openai-api-responses";
pub const OPENAI_API_RESPONSES_BASE_URL: &str = "https://api.openai.com/v1";

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
}

pub(super) enum ProviderMutationError {
    Invalid,
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
    }
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
