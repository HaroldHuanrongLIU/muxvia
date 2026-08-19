use tokio_rusqlite::rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::{
    control::protocol::{
        ActionStatus, ControlProblem, CredentialEdit, CredentialPresence, DuplicateCredential,
        ProviderAuthentication, ProviderProvenanceView, ProviderReferenceView,
        ProviderRoutingRequirement, Target, UniversalProviderAction, UniversalProviderCatalogView,
        UniversalProviderOutcome, UniversalProviderPresetTargetView, UniversalProviderPresetView,
        UniversalProviderTargetDraft, UniversalProviderTargetView, UniversalProviderView,
        UniversalSynchronizationState,
    },
    domain::provider::normalize_provider_base_url,
};

use super::{
    providers::{
        ANTHROPIC_API_MESSAGES_BASE_URL, ANTHROPIC_API_MESSAGES_PRESET_KEY,
        OPENAI_API_RESPONSES_BASE_URL, OPENAI_API_RESPONSES_PRESET_KEY,
    },
    store::{StateError, StateStore},
};

#[derive(Debug, thiserror::Error)]
#[error("{problem:?}")]
pub struct UniversalProviderActionFailure {
    pub problem: ControlProblem,
    pub authoritative_view: UniversalProviderCatalogView,
}

enum CatalogAttempt {
    Applied(UniversalProviderOutcome),
    Failure(UniversalProviderActionFailure),
}

impl StateStore {
    pub async fn universal_provider_catalog(
        &self,
    ) -> Result<UniversalProviderCatalogView, StateError> {
        self.connection
            .call(|connection| project_universal_provider_catalog(connection))
            .await
            .map_err(map_call_error)
    }

    pub async fn apply_universal_provider_action(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        raw_action: serde_json::Value,
    ) -> Result<UniversalProviderOutcome, UniversalProviderActionFailure> {
        match self.universal_provider_receipt(action_id).await {
            Ok(Some(outcome)) => return Ok(replayed(outcome)),
            Ok(None) => {}
            Err(_) => {
                return Err(self
                    .catalog_failure("state-store-error", "State store operation failed")
                    .await);
            }
        }

        let action = match serde_json::from_value::<UniversalProviderAction>(raw_action) {
            Ok(action) => action,
            Err(_) => {
                return Err(self
                    .catalog_failure(
                        "invalid-universal-provider",
                        "Universal Provider action is malformed",
                    )
                    .await);
            }
        };
        let action_kind = action_kind(&action);
        let action_id = action_id.to_string();
        let attempt = self
            .connection
            .call(move |connection| -> Result<CatalogAttempt, StateError> {
                let transaction = connection.transaction_with_behavior(
                    tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                )?;
                if let Some(outcome) = read_receipt(&transaction, &action_id)? {
                    return Ok(CatalogAttempt::Applied(replayed(outcome)));
                }
                let current_revision: u64 = transaction.query_row(
                    "SELECT revision FROM universal_provider_catalog_state WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )?;
                if current_revision != expected_revision {
                    return Ok(CatalogAttempt::Failure(catalog_failure_for(
                        &transaction,
                        "stale-universal-catalog-revision",
                        "Universal Provider catalog changed; refresh and retry",
                    )?));
                }
                let mutation = match action {
                    UniversalProviderAction::CreateUniversalProvider {
                        name,
                        base_url,
                        credential,
                        preset_key,
                        targets,
                    } => create_universal_provider(
                        &transaction,
                        name,
                        base_url,
                        credential,
                        preset_key,
                        targets,
                    ),
                    UniversalProviderAction::UpdateUniversalProvider {
                        provider_id,
                        provider_revision,
                        name,
                        base_url,
                        credential,
                        targets,
                    } => update_universal_provider(
                        &transaction,
                        provider_id,
                        provider_revision,
                        name,
                        base_url,
                        credential,
                        targets,
                    ),
                    UniversalProviderAction::DuplicateUniversalProvider {
                        source_provider_id,
                        source_provider_revision,
                        name,
                        base_url,
                        credential,
                        targets,
                    } => duplicate_universal_provider(
                        &transaction,
                        source_provider_id,
                        source_provider_revision,
                        name,
                        base_url,
                        credential,
                        targets,
                    ),
                    UniversalProviderAction::DeleteUniversalProvider {
                        provider_id,
                        provider_revision,
                    } => delete_universal_provider(&transaction, provider_id, provider_revision),
                    _ => Err(CatalogMutationError::Unsupported),
                };
                if let Err(error) = mutation {
                    let (code, message) = match error {
                        CatalogMutationError::Invalid => (
                            "invalid-universal-provider",
                            "Universal Provider declaration is invalid",
                        ),
                        CatalogMutationError::Unsupported => (
                            "unsupported-operation",
                            "Universal Provider action is not supported",
                        ),
                        CatalogMutationError::StaleProviderRevision => (
                            "stale-universal-provider-revision",
                            "Universal Provider changed; refresh and retry",
                        ),
                        CatalogMutationError::NoChange => (
                            "no-universal-provider-change",
                            "Universal Provider declaration is unchanged",
                        ),
                        CatalogMutationError::GeneratedProvidersExist => (
                            "provider-synchronization-blocked",
                            "Generated Target Providers must be checked before deletion",
                        ),
                    };
                    return Ok(CatalogAttempt::Failure(catalog_failure_for(
                        &transaction,
                        code,
                        message,
                    )?));
                }

                transaction.execute(
                    "UPDATE universal_provider_catalog_state
                     SET revision = revision + 1, view_sequence = view_sequence + 1
                     WHERE singleton = 1",
                    [],
                )?;
                let view = project_universal_provider_catalog(&transaction)?;
                let outcome = UniversalProviderOutcome {
                    status: ActionStatus::Applied,
                    view,
                };
                transaction.execute(
                    "INSERT INTO universal_action_receipts
                     (action_id, action_kind, committed_revision, outcome_json)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        action_id,
                        action_kind,
                        outcome.view.revision,
                        serde_json::to_string(&outcome)?
                    ],
                )?;
                transaction.commit()?;
                Ok(CatalogAttempt::Applied(outcome))
            })
            .await;

        match attempt {
            Ok(CatalogAttempt::Applied(outcome)) => Ok(outcome),
            Ok(CatalogAttempt::Failure(failure)) => Err(failure),
            Err(_) => Err(self
                .catalog_failure("state-store-error", "State store operation failed")
                .await),
        }
    }

    #[doc(hidden)]
    pub async fn seed_universal_provider_from_preset(
        &self,
        preset_key: &str,
    ) -> Result<UniversalProviderCatalogView, StateError> {
        let preset_key = preset_key.to_owned();
        self.connection
            .call(
                move |connection| -> Result<UniversalProviderCatalogView, StateError> {
                    let transaction = connection.transaction_with_behavior(
                        tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                    )?;
                    let already_seeded: bool = transaction.query_row(
                        "SELECT EXISTS(
                       SELECT 1 FROM universal_provider_seeds WHERE preset_key = ?1
                     )",
                        [&preset_key],
                        |row| row.get(0),
                    )?;
                    if already_seeded {
                        return Ok(project_universal_provider_catalog(&transaction)?);
                    }
                    let preset = universal_provider_presets()
                        .into_iter()
                        .find(|preset| preset.key == preset_key)
                        .ok_or_else(|| {
                            StateError::Sqlite(tokio_rusqlite::rusqlite::Error::InvalidQuery)
                        })?;
                    let targets = preset
                        .targets
                        .into_iter()
                        .map(|target| UniversalProviderTargetDraft {
                            target: target.target,
                            enabled: target.enabled,
                            model: target.model,
                            authentication: target.authentication,
                            routing_requirement: target.routing_requirement,
                        })
                        .collect();
                    create_universal_provider(
                        &transaction,
                        preset.name,
                        preset.base_url,
                        CredentialEdit::Remove,
                        Some(preset_key.clone()),
                        targets,
                    )
                    .map_err(|_| {
                        StateError::Sqlite(tokio_rusqlite::rusqlite::Error::InvalidQuery)
                    })?;
                    let seeded_provider_id: String = transaction.query_row(
                        "SELECT id FROM universal_providers ORDER BY position DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )?;
                    transaction.execute(
                        "INSERT INTO universal_provider_seeds (preset_key, seeded_provider_id)
                     VALUES (?1, ?2)",
                        params![preset_key, seeded_provider_id],
                    )?;
                    transaction.execute(
                        "UPDATE universal_provider_catalog_state
                     SET revision = revision + 1, view_sequence = view_sequence + 1
                     WHERE singleton = 1",
                        [],
                    )?;
                    let view = project_universal_provider_catalog(&transaction)?;
                    transaction.commit()?;
                    Ok(view)
                },
            )
            .await
            .map_err(map_state_call_error)
    }

    async fn universal_provider_receipt(
        &self,
        action_id: Uuid,
    ) -> Result<Option<UniversalProviderOutcome>, StateError> {
        let action_id = action_id.to_string();
        self.connection
            .call(move |connection| read_receipt(connection, &action_id))
            .await
            .map_err(map_state_call_error)
    }

    async fn catalog_failure(&self, code: &str, message: &str) -> UniversalProviderActionFailure {
        UniversalProviderActionFailure {
            problem: ControlProblem {
                code: code.to_owned(),
                message: message.to_owned(),
                source: None,
                selector: None,
            },
            authoritative_view: self.universal_provider_catalog().await.unwrap_or_else(|_| {
                UniversalProviderCatalogView {
                    revision: 0,
                    view_sequence: 0,
                    providers: Vec::new(),
                    presets: universal_provider_presets(),
                }
            }),
        }
    }
}

fn create_universal_provider(
    transaction: &Transaction<'_>,
    name: String,
    base_url: String,
    credential: CredentialEdit,
    preset_key: Option<String>,
    targets: Vec<UniversalProviderTargetDraft>,
) -> Result<(), CatalogMutationError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(CatalogMutationError::Invalid);
    }
    let base_url = if base_url.is_empty() {
        base_url
    } else {
        normalize_provider_base_url(&base_url).map_err(|_| CatalogMutationError::Invalid)?
    };
    let provenance_key = match preset_key.as_deref() {
        Some(OPENAI_API_RESPONSES_PRESET_KEY) => Some(OPENAI_API_RESPONSES_PRESET_KEY),
        Some(ANTHROPIC_API_MESSAGES_PRESET_KEY) => Some(ANTHROPIC_API_MESSAGES_PRESET_KEY),
        Some(_) => return Err(CatalogMutationError::Invalid),
        None => None,
    };
    let targets = validated_targets(targets)?;
    let credential_id = match credential {
        CredentialEdit::Keep => return Err(CatalogMutationError::Invalid),
        CredentialEdit::Remove => None,
        CredentialEdit::Replace { value } => {
            if value.trim().is_empty() {
                return Err(CatalogMutationError::Invalid);
            }
            let credential_id = Uuid::new_v4().to_string();
            transaction
                .execute(
                    "INSERT INTO universal_credentials (id, bearer_token) VALUES (?1, ?2)",
                    params![credential_id, value],
                )
                .map_err(|_| CatalogMutationError::Invalid)?;
            Some(credential_id)
        }
    };
    let position: u32 = transaction
        .query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM universal_providers",
            [],
            |row| row.get(0),
        )
        .map_err(|_| CatalogMutationError::Invalid)?;
    let provider_id = Uuid::new_v4().to_string();
    transaction
        .execute(
            "INSERT INTO universal_providers
             (id, position, provider_revision, name, base_url, credential_id,
              provenance_kind, provenance_key)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7)",
            params![
                provider_id,
                position,
                name,
                base_url,
                credential_id,
                provenance_key.map(|_| "preset"),
                provenance_key,
            ],
        )
        .map_err(|_| CatalogMutationError::Invalid)?;
    for target in targets {
        transaction
            .execute(
                "INSERT INTO universal_provider_targets
                 (universal_provider_id, target, enabled, model, authentication,
                  routing_requirement, overlay_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                params![
                    provider_id,
                    target.target.as_str(),
                    target.enabled,
                    target.model,
                    target.authentication.to_string(),
                    target.routing_requirement.to_string(),
                ],
            )
            .map_err(|_| CatalogMutationError::Invalid)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_universal_provider(
    transaction: &Transaction<'_>,
    provider_id: Uuid,
    provider_revision: u64,
    name: String,
    base_url: String,
    credential: CredentialEdit,
    targets: Vec<UniversalProviderTargetDraft>,
) -> Result<(), CatalogMutationError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(CatalogMutationError::Invalid);
    }
    let base_url = if base_url.is_empty() {
        base_url
    } else {
        normalize_provider_base_url(&base_url).map_err(|_| CatalogMutationError::Invalid)?
    };
    let targets = validated_targets(targets)?;
    let provider_id = provider_id.to_string();
    let existing = transaction
        .query_row(
            "SELECT name, base_url, credential_id, provider_revision
             FROM universal_providers WHERE id = ?1",
            [&provider_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|_| CatalogMutationError::Invalid)?
        .ok_or(CatalogMutationError::StaleProviderRevision)?;
    if existing.3 != provider_revision {
        return Err(CatalogMutationError::StaleProviderRevision);
    }

    let credential_id = match credential {
        CredentialEdit::Keep => existing.2.clone(),
        CredentialEdit::Remove => None,
        CredentialEdit::Replace { value } => {
            if value.trim().is_empty() {
                return Err(CatalogMutationError::Invalid);
            }
            let credential_id = Uuid::new_v4().to_string();
            transaction
                .execute(
                    "INSERT INTO universal_credentials (id, bearer_token) VALUES (?1, ?2)",
                    params![credential_id, value],
                )
                .map_err(|_| CatalogMutationError::Invalid)?;
            Some(credential_id)
        }
    };
    let source_changed =
        existing.0 != name || existing.1 != base_url || existing.2 != credential_id;

    let mut target_changed = false;
    for target in targets {
        let current = transaction
            .query_row(
                "SELECT enabled, model, authentication, routing_requirement
                 FROM universal_provider_targets
                 WHERE universal_provider_id = ?1 AND target = ?2",
                params![provider_id, target.target.as_str()],
                |row| {
                    Ok((
                        row.get::<_, bool>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(|_| CatalogMutationError::Invalid)?;
        let changed = current.0 != target.enabled
            || current.1 != target.model
            || current.2 != target.authentication.to_string()
            || current.3 != target.routing_requirement.to_string();
        if changed {
            transaction
                .execute(
                    "UPDATE universal_provider_targets
                     SET enabled = ?1, model = ?2, authentication = ?3,
                         routing_requirement = ?4, overlay_revision = overlay_revision + 1
                     WHERE universal_provider_id = ?5 AND target = ?6",
                    params![
                        target.enabled,
                        target.model,
                        target.authentication.to_string(),
                        target.routing_requirement.to_string(),
                        provider_id,
                        target.target.as_str(),
                    ],
                )
                .map_err(|_| CatalogMutationError::Invalid)?;
            target_changed = true;
        }
    }
    if !source_changed && !target_changed {
        return Err(CatalogMutationError::NoChange);
    }
    if source_changed {
        transaction
            .execute(
                "UPDATE universal_providers
                 SET name = ?1, base_url = ?2, credential_id = ?3,
                     provider_revision = provider_revision + 1
                 WHERE id = ?4",
                params![name, base_url, credential_id, provider_id],
            )
            .map_err(|_| CatalogMutationError::Invalid)?;
        if let Some(previous_credential_id) =
            existing.2.filter(|id| Some(id) != credential_id.as_ref())
        {
            transaction
                .execute(
                    "DELETE FROM universal_credentials
                     WHERE id = ?1
                       AND NOT EXISTS (
                         SELECT 1 FROM universal_providers WHERE credential_id = ?1
                       )",
                    [previous_credential_id],
                )
                .map_err(|_| CatalogMutationError::Invalid)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn duplicate_universal_provider(
    transaction: &Transaction<'_>,
    source_provider_id: Uuid,
    source_provider_revision: u64,
    name: String,
    base_url: String,
    credential: DuplicateCredential,
    targets: Vec<UniversalProviderTargetDraft>,
) -> Result<(), CatalogMutationError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(CatalogMutationError::Invalid);
    }
    let base_url = if base_url.is_empty() {
        base_url
    } else {
        normalize_provider_base_url(&base_url).map_err(|_| CatalogMutationError::Invalid)?
    };
    let targets = validated_targets(targets)?;
    let source = transaction
        .query_row(
            "SELECT provider_revision, credential_id
             FROM universal_providers WHERE id = ?1",
            [source_provider_id.to_string()],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|_| CatalogMutationError::Invalid)?
        .ok_or(CatalogMutationError::StaleProviderRevision)?;
    if source.0 != source_provider_revision {
        return Err(CatalogMutationError::StaleProviderRevision);
    }
    let credential_id = match credential {
        DuplicateCredential::Without => None,
        DuplicateCredential::ReuseSource => source.1,
        DuplicateCredential::Replace { value } => {
            if value.trim().is_empty() {
                return Err(CatalogMutationError::Invalid);
            }
            let credential_id = Uuid::new_v4().to_string();
            transaction
                .execute(
                    "INSERT INTO universal_credentials (id, bearer_token) VALUES (?1, ?2)",
                    params![credential_id, value],
                )
                .map_err(|_| CatalogMutationError::Invalid)?;
            Some(credential_id)
        }
    };
    let position: u32 = transaction
        .query_row(
            "SELECT COALESCE(MAX(position) + 1, 0) FROM universal_providers",
            [],
            |row| row.get(0),
        )
        .map_err(|_| CatalogMutationError::Invalid)?;
    let provider_id = Uuid::new_v4().to_string();
    transaction
        .execute(
            "INSERT INTO universal_providers
             (id, position, provider_revision, name, base_url, credential_id,
              provenance_kind, provenance_key)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, NULL, NULL)",
            params![provider_id, position, name, base_url, credential_id],
        )
        .map_err(|_| CatalogMutationError::Invalid)?;
    for target in targets {
        transaction
            .execute(
                "INSERT INTO universal_provider_targets
                 (universal_provider_id, target, enabled, model, authentication,
                  routing_requirement, overlay_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                params![
                    provider_id,
                    target.target.as_str(),
                    target.enabled,
                    target.model,
                    target.authentication.to_string(),
                    target.routing_requirement.to_string(),
                ],
            )
            .map_err(|_| CatalogMutationError::Invalid)?;
    }
    Ok(())
}

fn delete_universal_provider(
    transaction: &Transaction<'_>,
    provider_id: Uuid,
    provider_revision: u64,
) -> Result<(), CatalogMutationError> {
    let provider_id = provider_id.to_string();
    let provider = transaction
        .query_row(
            "SELECT position, provider_revision, credential_id
             FROM universal_providers WHERE id = ?1",
            [&provider_id],
            |row| {
                Ok((
                    row.get::<_, u32>(0)?,
                    row.get::<_, u64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|_| CatalogMutationError::Invalid)?
        .ok_or(CatalogMutationError::StaleProviderRevision)?;
    if provider.1 != provider_revision {
        return Err(CatalogMutationError::StaleProviderRevision);
    }
    let has_generated: bool = transaction
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM providers WHERE generated_owner_id = ?1
             )",
            [&provider_id],
            |row| row.get(0),
        )
        .map_err(|_| CatalogMutationError::Invalid)?;
    if has_generated {
        return Err(CatalogMutationError::GeneratedProvidersExist);
    }
    transaction
        .execute(
            "DELETE FROM universal_providers WHERE id = ?1",
            [&provider_id],
        )
        .map_err(|_| CatalogMutationError::Invalid)?;
    transaction
        .execute(
            "UPDATE universal_providers SET position = position - 1 WHERE position > ?1",
            [provider.0],
        )
        .map_err(|_| CatalogMutationError::Invalid)?;
    if let Some(credential_id) = provider.2 {
        transaction
            .execute(
                "DELETE FROM universal_credentials
                 WHERE id = ?1
                   AND NOT EXISTS (
                     SELECT 1 FROM universal_providers WHERE credential_id = ?1
                   )",
                [credential_id],
            )
            .map_err(|_| CatalogMutationError::Invalid)?;
    }
    Ok(())
}

fn validated_targets(
    targets: Vec<UniversalProviderTargetDraft>,
) -> Result<Vec<UniversalProviderTargetDraft>, CatalogMutationError> {
    if targets.len() != 2 {
        return Err(CatalogMutationError::Invalid);
    }
    let mut codex = false;
    let mut claude = false;
    for target in &targets {
        match target.target {
            Target::Codex
                if !codex && target.authentication == ProviderAuthentication::OpenaiBearer =>
            {
                codex = true;
            }
            Target::Claude
                if !claude
                    && matches!(
                        target.authentication,
                        ProviderAuthentication::AnthropicApiKey
                            | ProviderAuthentication::AnthropicBearer
                    ) =>
            {
                claude = true;
            }
            _ => return Err(CatalogMutationError::Invalid),
        }
    }
    if !codex || !claude {
        return Err(CatalogMutationError::Invalid);
    }
    Ok(targets)
}

pub(super) fn project_universal_provider_catalog(
    connection: &Connection,
) -> Result<UniversalProviderCatalogView, tokio_rusqlite::rusqlite::Error> {
    let (revision, view_sequence) = connection.query_row(
        "SELECT revision, view_sequence
         FROM universal_provider_catalog_state
         WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut statement = connection.prepare(
        "SELECT id, position, provider_revision, name, base_url, credential_id,
                provenance_kind, provenance_key
         FROM universal_providers ORDER BY position, id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u32>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<String>>(7)?,
        ))
    })?;
    let mut providers = Vec::new();
    for row in rows {
        let (id, position, provider_revision, name, base_url, credential_id, kind, key) = row?;
        let id = Uuid::parse_str(&id).map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
        let provenance = match (kind, key) {
            (None, None) => None,
            (Some(kind), Some(key)) => Some(ProviderProvenanceView { kind, key }),
            _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
        };
        providers.push(UniversalProviderView {
            id,
            position,
            provider_revision,
            name,
            base_url,
            credential: if credential_id.is_some() {
                CredentialPresence::Present
            } else {
                CredentialPresence::Missing
            },
            provenance,
            targets: project_targets(connection, id, provider_revision)?,
        });
    }
    Ok(UniversalProviderCatalogView {
        revision,
        view_sequence,
        providers,
        presets: universal_provider_presets(),
    })
}

fn project_targets(
    connection: &Connection,
    provider_id: Uuid,
    provider_revision: u64,
) -> Result<Vec<UniversalProviderTargetView>, tokio_rusqlite::rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT t.target, t.enabled, t.model, t.authentication, t.routing_requirement,
                t.overlay_revision, t.synchronized_source_revision,
                t.synchronized_overlay_revision, p.id
         FROM universal_provider_targets t
         LEFT JOIN providers p
           ON p.generated_owner_id = t.universal_provider_id AND p.target = t.target
         WHERE t.universal_provider_id = ?1
         ORDER BY CASE t.target WHEN 'codex' THEN 0 ELSE 1 END",
    )?;
    let rows = statement.query_map([provider_id.to_string()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, bool>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, u64>(5)?,
            row.get::<_, Option<u64>>(6)?,
            row.get::<_, Option<u64>>(7)?,
            row.get::<_, Option<String>>(8)?,
        ))
    })?;
    let mut targets = Vec::new();
    for row in rows {
        let (
            target,
            enabled,
            model,
            authentication,
            routing_requirement,
            overlay_revision,
            synchronized_source_revision,
            synchronized_overlay_revision,
            generated_provider_id,
        ) = row?;
        let target = parse_target(&target)?;
        let authentication = parse_authentication(&authentication)?;
        let routing_requirement = parse_routing_requirement(&routing_requirement)?;
        let generated_provider_id = generated_provider_id
            .map(|value| {
                Uuid::parse_str(&value).map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)
            })
            .transpose()?;
        let current = if enabled {
            generated_provider_id.is_some()
                && synchronized_source_revision == Some(provider_revision)
                && synchronized_overlay_revision == Some(overlay_revision)
        } else {
            generated_provider_id.is_none()
        };
        let active_references = generated_provider_id
            .map(|id| project_active_references(connection, target, id))
            .transpose()?
            .unwrap_or_default();
        targets.push(UniversalProviderTargetView {
            target,
            enabled,
            model,
            authentication,
            routing_requirement,
            overlay_revision,
            generated_provider_id,
            synchronization: if current {
                UniversalSynchronizationState::Current
            } else {
                UniversalSynchronizationState::Pending
            },
            active_references,
        });
    }
    if targets.len() != 2 {
        return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery);
    }
    Ok(targets)
}

fn project_active_references(
    connection: &Connection,
    target: Target,
    provider_id: Uuid,
) -> Result<Vec<ProviderReferenceView>, tokio_rusqlite::rusqlite::Error> {
    let (is_current, is_snapshot): (bool, bool) = connection.query_row(
        "SELECT COALESCE(current_provider_id = ?2, 0),
                EXISTS(
                  SELECT 1 FROM activated_snapshots s
                  WHERE s.id = r.activated_snapshot_id AND s.provider_id = ?2
                )
         FROM target_route_state r WHERE r.target = ?1",
        params![target.as_str(), provider_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut references = Vec::new();
    if is_current {
        references.push(ProviderReferenceView::Current);
    }
    if is_snapshot {
        references.push(ProviderReferenceView::ActivatedSnapshot);
    }
    Ok(references)
}

fn read_receipt(
    connection: &Connection,
    action_id: &str,
) -> Result<Option<UniversalProviderOutcome>, StateError> {
    let outcome = connection
        .query_row(
            "SELECT outcome_json FROM universal_action_receipts WHERE action_id = ?1",
            [action_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    outcome
        .map(|json| serde_json::from_str(&json).map_err(StateError::from))
        .transpose()
}

fn replayed(mut outcome: UniversalProviderOutcome) -> UniversalProviderOutcome {
    outcome.status = ActionStatus::Replayed;
    outcome
}

fn catalog_failure_for(
    connection: &Connection,
    code: &str,
    message: &str,
) -> Result<UniversalProviderActionFailure, StateError> {
    Ok(UniversalProviderActionFailure {
        problem: ControlProblem {
            code: code.to_owned(),
            message: message.to_owned(),
            source: None,
            selector: None,
        },
        authoritative_view: project_universal_provider_catalog(connection)?,
    })
}

fn action_kind(action: &UniversalProviderAction) -> &'static str {
    match action {
        UniversalProviderAction::CreateUniversalProvider { .. } => "create-universal-provider",
        UniversalProviderAction::UpdateUniversalProvider { .. } => "update-universal-provider",
        UniversalProviderAction::DuplicateUniversalProvider { .. } => {
            "duplicate-universal-provider"
        }
        UniversalProviderAction::DeleteUniversalProvider { .. } => "delete-universal-provider",
        UniversalProviderAction::SynchronizeUniversalProvider { .. } => {
            "synchronize-universal-provider"
        }
    }
}

fn parse_target(value: &str) -> Result<Target, tokio_rusqlite::rusqlite::Error> {
    match value {
        "codex" => Ok(Target::Codex),
        "claude" => Ok(Target::Claude),
        _ => Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
    }
}

fn parse_authentication(
    value: &str,
) -> Result<ProviderAuthentication, tokio_rusqlite::rusqlite::Error> {
    match value {
        "openai-bearer" => Ok(ProviderAuthentication::OpenaiBearer),
        "anthropic-api-key" => Ok(ProviderAuthentication::AnthropicApiKey),
        "anthropic-bearer" => Ok(ProviderAuthentication::AnthropicBearer),
        _ => Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
    }
}

fn parse_routing_requirement(
    value: &str,
) -> Result<ProviderRoutingRequirement, tokio_rusqlite::rusqlite::Error> {
    match value {
        "direct-compatible" => Ok(ProviderRoutingRequirement::DirectCompatible),
        "takeover-required" => Ok(ProviderRoutingRequirement::TakeoverRequired),
        _ => Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
    }
}

fn universal_provider_presets() -> Vec<UniversalProviderPresetView> {
    vec![
        UniversalProviderPresetView {
            key: OPENAI_API_RESPONSES_PRESET_KEY.to_owned(),
            name: "OpenAI API".to_owned(),
            base_url: OPENAI_API_RESPONSES_BASE_URL.to_owned(),
            targets: vec![
                preset_target(Target::Codex, true, ProviderAuthentication::OpenaiBearer),
                preset_target(
                    Target::Claude,
                    false,
                    ProviderAuthentication::AnthropicBearer,
                ),
            ],
        },
        UniversalProviderPresetView {
            key: ANTHROPIC_API_MESSAGES_PRESET_KEY.to_owned(),
            name: "Anthropic API".to_owned(),
            base_url: ANTHROPIC_API_MESSAGES_BASE_URL.to_owned(),
            targets: vec![
                preset_target(Target::Codex, false, ProviderAuthentication::OpenaiBearer),
                preset_target(
                    Target::Claude,
                    true,
                    ProviderAuthentication::AnthropicApiKey,
                ),
            ],
        },
    ]
}

fn preset_target(
    target: Target,
    enabled: bool,
    authentication: ProviderAuthentication,
) -> UniversalProviderPresetTargetView {
    UniversalProviderPresetTargetView {
        target,
        enabled,
        model: String::new(),
        authentication,
        routing_requirement: ProviderRoutingRequirement::DirectCompatible,
    }
}

enum CatalogMutationError {
    Invalid,
    Unsupported,
    StaleProviderRevision,
    NoChange,
    GeneratedProvidersExist,
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
