use secrecy::{ExposeSecret, SecretString};
use std::collections::{HashMap, HashSet};
use subtle::ConstantTimeEq;

use tokio_rusqlite::rusqlite::{OptionalExtension, Transaction, params};

use crate::control::protocol::{
    ProviderAuthentication, ProviderImportMatchView, ProviderImportOutcome, ProviderImportProduct,
    ProviderImportRecordResolution, ProviderImportRecordView, ProviderImportSourceTarget,
    ProviderProtocol, ProviderRoutingRequirement, Target, TargetView, UniversalProviderCatalogView,
    UniversalProviderTargetDraft,
};
use crate::domain::view::project_target_view_for;

use super::{StateError, StateStore};

pub(crate) struct ProviderImportCommitInput {
    pub source_product: ProviderImportProduct,
    pub source_target: ProviderImportSourceTarget,
    pub candidates: Vec<ProviderImportCandidateInput>,
    pub failover_drafts: Option<Vec<ProviderImportFailoverDraftInput>>,
    pub historical_usage: Option<MigratedUsageImportInput>,
}

pub(crate) struct MigratedUsageImportInput {
    pub target: Target,
    pub source_export_fingerprint: String,
    pub rollups: Vec<MigratedUsageRollupInput>,
}

pub(crate) struct MigratedUsageRollupInput {
    pub local_date: String,
    pub source_record_count: u64,
    pub successful_request_count: u64,
    pub failed_request_count: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    pub latency_observation_count: u64,
    pub total_latency_ms: u64,
}

pub(crate) struct ProviderImportTargetMatchInput {
    pub target: Target,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub authentication: ProviderAuthentication,
    pub routing_requirement: ProviderRoutingRequirement,
    pub credential: Option<SecretString>,
}

pub(crate) enum ProviderImportCandidateInput {
    Target(ProviderImportTargetInput),
    Universal(ProviderImportUniversalInput),
}

pub(crate) struct ProviderImportTargetInput {
    pub candidate_id: uuid::Uuid,
    pub resolution: ProviderImportResolutionInput,
    pub target: Target,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub protocol: ProviderProtocol,
    pub authentication: ProviderAuthentication,
    pub routing_requirement: ProviderRoutingRequirement,
    pub credential: Option<SecretString>,
    pub source_position: u32,
    pub source_identifier: String,
    pub configuration_fingerprint: String,
    pub exported_source_id: Option<uuid::Uuid>,
}

pub(crate) struct ProviderImportUniversalInput {
    pub candidate_id: uuid::Uuid,
    pub resolution: ProviderImportResolutionInput,
    pub name: String,
    pub base_url: String,
    pub targets: Vec<UniversalProviderTargetDraft>,
    pub credential: Option<SecretString>,
    pub source_position: u32,
    pub source_identifier: String,
    pub configuration_fingerprint: String,
    pub generated_sources: Vec<ProviderImportGeneratedSourceInput>,
}

pub(crate) struct ProviderImportGeneratedSourceInput {
    pub target: Target,
    pub source_id: uuid::Uuid,
    pub source_position: u32,
    pub configuration_fingerprint: String,
}

pub(crate) struct ProviderImportFailoverDraftInput {
    pub target: Target,
    pub provider_source_ids: Vec<uuid::Uuid>,
}

#[derive(Clone, Copy)]
pub(crate) enum ProviderImportResolutionInput {
    Create,
    Existing(uuid::Uuid),
}

pub(crate) struct ProviderImportCommit {
    pub outcome: ProviderImportOutcome,
    pub target_views: Vec<TargetView>,
    pub universal_view: Option<UniversalProviderCatalogView>,
}

pub(crate) struct ProviderConfigurationExportSnapshot {
    pub catalog: UniversalProviderCatalogView,
    pub codex: TargetView,
    pub claude: TargetView,
    pub secrets: Vec<SecretString>,
}

pub(crate) enum ProviderImportCommitError {
    InvalidChoice,
    DuplicateHistoricalUsage,
    State,
}

enum CommitAttempt {
    Applied(ProviderImportCommit),
    InvalidChoice,
    DuplicateHistoricalUsage,
}

enum TargetInsert<'a> {
    Ordinary(&'a ProviderImportTargetInput, uuid::Uuid),
    Generated {
        source: &'a ProviderImportUniversalInput,
        generated: &'a ProviderImportGeneratedSourceInput,
        overlay: &'a UniversalProviderTargetDraft,
        owner_id: uuid::Uuid,
        provider_id: uuid::Uuid,
    },
}

impl TargetInsert<'_> {
    fn target(&self) -> Target {
        match self {
            Self::Ordinary(input, _) => input.target,
            Self::Generated { generated, .. } => generated.target,
        }
    }

    fn source_position(&self) -> u32 {
        match self {
            Self::Ordinary(input, _) => input.source_position,
            Self::Generated { generated, .. } => generated.source_position,
        }
    }
}

impl StateStore {
    pub(crate) async fn provider_configuration_export_views(
        &self,
    ) -> Result<ProviderConfigurationExportSnapshot, StateError> {
        let service_epoch = self.service_epoch().to_string();
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let catalog =
                    super::universal_providers::project_universal_provider_catalog(&transaction)?;
                let codex = project_target_view_for(&transaction, &service_epoch, Target::Codex)?;
                let claude = project_target_view_for(&transaction, &service_epoch, Target::Claude)?;
                let secrets = transaction
                    .prepare(
                        "SELECT bearer_token FROM credentials
                         UNION ALL SELECT bearer_token FROM universal_credentials
                         UNION ALL SELECT routing_credential FROM target_route_state
                           WHERE routing_credential IS NOT NULL",
                    )?
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .map(SecretString::from)
                    .collect();
                transaction.commit()?;
                Ok(ProviderConfigurationExportSnapshot {
                    catalog,
                    codex,
                    claude,
                    secrets,
                })
            })
            .await
            .map_err(super::store::map_call_error)
    }

    pub(crate) async fn exact_target_provider_import_matches(
        &self,
        input: ProviderImportTargetMatchInput,
    ) -> Result<Vec<ProviderImportMatchView>, StateError> {
        self.connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT p.id, p.name, c.bearer_token
                     FROM providers p
                     LEFT JOIN credentials c ON c.id = p.credential_id
                     WHERE p.target = ?1
                       AND p.base_url = ?2
                       AND p.model = ?3
                       AND p.protocol = ?4
                       AND p.authentication = ?5
                       AND p.routing_requirement = ?6
                       AND p.generated_owner_id IS NULL
                     ORDER BY p.position, p.id",
                )?;
                let rows = statement.query_map(
                    params![
                        input.target.as_str(),
                        input.base_url,
                        input.model,
                        input.protocol.to_string(),
                        input.authentication.to_string(),
                        input.routing_requirement.to_string()
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )?;
                let mut matches = Vec::new();
                for row in rows {
                    let (provider_id, name, existing_credential) = row?;
                    if credentials_equal(input.credential.as_ref(), existing_credential.as_deref())
                    {
                        let provider_id = uuid::Uuid::parse_str(&provider_id)
                            .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                        matches.push(ProviderImportMatchView { provider_id, name });
                    }
                }
                Ok(matches)
            })
            .await
            .map_err(super::store::map_call_error)
    }

    pub(crate) async fn exact_universal_provider_import_matches(
        &self,
        base_url: String,
        targets: Vec<UniversalProviderTargetDraft>,
        credential: Option<SecretString>,
    ) -> Result<Vec<ProviderImportMatchView>, StateError> {
        self.connection
            .call(move |connection| {
                let expected = targets
                    .iter()
                    .map(|overlay| {
                        (
                            overlay.target.as_str().to_owned(),
                            (
                                overlay.enabled,
                                overlay.model.clone(),
                                overlay.authentication.to_string(),
                                overlay.routing_requirement.to_string(),
                            ),
                        )
                    })
                    .collect::<HashMap<_, _>>();
                let mut statement = connection.prepare(
                    "SELECT p.id, p.name, c.bearer_token
                     FROM universal_providers p
                     LEFT JOIN universal_credentials c ON c.id = p.credential_id
                     WHERE p.base_url = ?1
                     ORDER BY p.position, p.id",
                )?;
                let rows = statement.query_map([base_url], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?;
                let mut matches = Vec::new();
                for row in rows {
                    let (provider_id, name, existing_credential) = row?;
                    if !credentials_equal(credential.as_ref(), existing_credential.as_deref()) {
                        continue;
                    }
                    let mut overlay_statement = connection.prepare(
                        "SELECT target, enabled, model, authentication, routing_requirement
                         FROM universal_provider_targets
                         WHERE universal_provider_id = ?1",
                    )?;
                    let overlays = overlay_statement.query_map([&provider_id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            (
                                row.get::<_, bool>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ),
                        ))
                    })?;
                    let existing = overlays.collect::<Result<HashMap<_, _>, _>>()?;
                    if existing == expected {
                        let provider_id = uuid::Uuid::parse_str(&provider_id)
                            .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                        matches.push(ProviderImportMatchView { provider_id, name });
                    }
                }
                Ok(matches)
            })
            .await
            .map_err(super::store::map_call_error)
    }

    pub(crate) async fn commit_provider_import(
        &self,
        input: ProviderImportCommitInput,
    ) -> Result<ProviderImportCommit, ProviderImportCommitError> {
        let service_epoch = self.service_epoch().to_string();
        let attempt = self
            .connection
            .call(move |connection| -> Result<CommitAttempt, StateError> {
                let transaction = connection.transaction_with_behavior(
                    tokio_rusqlite::rusqlite::TransactionBehavior::Immediate,
                )?;
                let attempt =
                    commit_provider_import_transaction(&transaction, &service_epoch, &input)?;
                if matches!(attempt, CommitAttempt::Applied(_)) {
                    transaction.commit()?;
                }
                Ok(attempt)
            })
            .await
            .map_err(|_| ProviderImportCommitError::State)?;
        match attempt {
            CommitAttempt::Applied(commit) => Ok(commit),
            CommitAttempt::InvalidChoice => Err(ProviderImportCommitError::InvalidChoice),
            CommitAttempt::DuplicateHistoricalUsage => {
                Err(ProviderImportCommitError::DuplicateHistoricalUsage)
            }
        }
    }
}

fn commit_provider_import_transaction(
    transaction: &Transaction<'_>,
    service_epoch: &str,
    input: &ProviderImportCommitInput,
) -> Result<CommitAttempt, StateError> {
    let source_product = import_product_name(input.source_product);
    let source_target = import_target_name(input.source_target);
    let mut destination_ids = HashMap::new();
    let mut exported_provider_ids = HashMap::new();
    let mut generated_ids = HashMap::new();
    let mut records = Vec::with_capacity(input.candidates.len());

    for candidate in &input.candidates {
        match candidate {
            ProviderImportCandidateInput::Target(candidate) => {
                let (provider_id, resolution) = match candidate.resolution {
                    ProviderImportResolutionInput::Create => (
                        uuid::Uuid::new_v4(),
                        ProviderImportRecordResolution::Created,
                    ),
                    ProviderImportResolutionInput::Existing(provider_id) => {
                        if !target_provider_is_exact(transaction, candidate, provider_id)? {
                            return Ok(CommitAttempt::InvalidChoice);
                        }
                        (provider_id, ProviderImportRecordResolution::Existing)
                    }
                };
                destination_ids.insert(candidate.candidate_id, provider_id);
                if let Some(source_id) = candidate.exported_source_id {
                    exported_provider_ids.insert(source_id, provider_id);
                }
                records.push(ProviderImportRecordView::TargetProvider {
                    candidate_id: candidate.candidate_id,
                    resolution,
                    target: candidate.target,
                    provider_id,
                });
            }
            ProviderImportCandidateInput::Universal(candidate) => {
                let (provider_id, resolution) = match candidate.resolution {
                    ProviderImportResolutionInput::Create => (
                        uuid::Uuid::new_v4(),
                        ProviderImportRecordResolution::Created,
                    ),
                    ProviderImportResolutionInput::Existing(provider_id) => {
                        if !universal_provider_is_exact(transaction, candidate, provider_id)? {
                            return Ok(CommitAttempt::InvalidChoice);
                        }
                        (provider_id, ProviderImportRecordResolution::Existing)
                    }
                };
                destination_ids.insert(candidate.candidate_id, provider_id);
                if matches!(
                    candidate.resolution,
                    ProviderImportResolutionInput::Existing(_)
                ) {
                    for source in &candidate.generated_sources {
                        let generated = transaction
                            .query_row(
                                "SELECT id FROM providers
                                 WHERE generated_owner_id = ?1 AND target = ?2",
                                params![provider_id.to_string(), source.target.as_str()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()?
                            .map(|id| uuid::Uuid::parse_str(&id))
                            .transpose()
                            .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                        if let Some(generated) = generated {
                            exported_provider_ids.insert(source.source_id, generated);
                        }
                    }
                }
                records.push(ProviderImportRecordView::UniversalProvider {
                    candidate_id: candidate.candidate_id,
                    resolution,
                    provider_id,
                });
            }
        }
    }

    let mut created_universal = input
        .candidates
        .iter()
        .filter_map(|candidate| match candidate {
            ProviderImportCandidateInput::Universal(candidate)
                if matches!(candidate.resolution, ProviderImportResolutionInput::Create) =>
            {
                Some(candidate)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    created_universal.sort_by_key(|candidate| candidate.source_position);
    let mut universal_position: u32 = transaction.query_row(
        "SELECT COALESCE(MAX(position) + 1, 0) FROM universal_providers",
        [],
        |row| row.get(0),
    )?;
    for candidate in created_universal {
        let provider_id = destination_ids[&candidate.candidate_id];
        let credential_id =
            insert_universal_credential(transaction, candidate.credential.as_ref())?;
        transaction.execute(
            "INSERT INTO universal_providers
             (id, position, provider_revision, name, base_url, credential_id,
              provenance_kind, provenance_key, import_source_product, import_source_target,
              import_source_identifier, import_configuration_fingerprint)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, NULL, NULL, ?6, ?7, ?8, ?9)",
            params![
                provider_id.to_string(),
                universal_position,
                candidate.name,
                candidate.base_url,
                credential_id,
                source_product,
                source_target,
                candidate.source_identifier,
                candidate.configuration_fingerprint,
            ],
        )?;
        universal_position = universal_position
            .checked_add(1)
            .ok_or_else(|| StateError::Sqlite(tokio_rusqlite::rusqlite::Error::InvalidQuery))?;
        for overlay in &candidate.targets {
            transaction.execute(
                "INSERT INTO universal_provider_targets
                 (universal_provider_id, target, enabled, model, authentication,
                  routing_requirement, overlay_revision, synchronized_source_revision,
                  synchronized_overlay_revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1,
                         CASE WHEN ?3 THEN 1 ELSE NULL END,
                         CASE WHEN ?3 THEN 1 ELSE NULL END)",
                params![
                    provider_id.to_string(),
                    overlay.target.as_str(),
                    overlay.enabled,
                    overlay.model,
                    overlay.authentication.to_string(),
                    overlay.routing_requirement.to_string(),
                ],
            )?;
        }
        for generated in &candidate.generated_sources {
            let provider_id = uuid::Uuid::new_v4();
            generated_ids.insert(generated.source_id, provider_id);
            exported_provider_ids.insert(generated.source_id, provider_id);
        }
    }

    let mut target_inserts = Vec::new();
    for candidate in &input.candidates {
        match candidate {
            ProviderImportCandidateInput::Target(candidate)
                if matches!(candidate.resolution, ProviderImportResolutionInput::Create) =>
            {
                target_inserts.push(TargetInsert::Ordinary(
                    candidate,
                    destination_ids[&candidate.candidate_id],
                ))
            }
            ProviderImportCandidateInput::Universal(candidate)
                if matches!(candidate.resolution, ProviderImportResolutionInput::Create) =>
            {
                let owner_id = destination_ids[&candidate.candidate_id];
                for generated in &candidate.generated_sources {
                    let Some(overlay) = candidate
                        .targets
                        .iter()
                        .find(|overlay| overlay.target == generated.target && overlay.enabled)
                    else {
                        return Ok(CommitAttempt::InvalidChoice);
                    };
                    target_inserts.push(TargetInsert::Generated {
                        source: candidate,
                        generated,
                        overlay,
                        owner_id,
                        provider_id: generated_ids[&generated.source_id],
                    });
                }
            }
            _ => {}
        }
    }
    target_inserts.sort_by_key(|insert| (target_order(insert.target()), insert.source_position()));

    let mut changed_targets =
        insert_target_providers(transaction, target_inserts, source_product, source_target)?;
    if let Some(drafts) = &input.failover_drafts {
        replace_failover_drafts(
            transaction,
            drafts,
            &exported_provider_ids,
            &mut changed_targets,
        )?;
    }

    let historical_usage_imported_records = if let Some(usage) = &input.historical_usage {
        if !usage.rollups.is_empty() {
            let duplicate = transaction.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM migrated_usage_rollups
                   WHERE target = ?1 AND source_export_fingerprint = ?2
                 )",
                params![usage.target.as_str(), usage.source_export_fingerprint],
                |row| row.get::<_, bool>(0),
            )?;
            if duplicate {
                return Ok(CommitAttempt::DuplicateHistoricalUsage);
            }
        }
        let mut imported = 0_u64;
        for rollup in &usage.rollups {
            transaction.execute(
                "INSERT INTO migrated_usage_rollups
                 (id, target, source_product, source_export_fingerprint, local_date,
                  source_record_count, successful_request_count, failed_request_count,
                  input_tokens, cached_input_tokens, cache_creation_input_tokens,
                  output_tokens, latency_observation_count, total_latency_ms)
                 VALUES (?1, ?2, 'cc-switch', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    uuid::Uuid::new_v4().to_string(),
                    usage.target.as_str(),
                    usage.source_export_fingerprint,
                    rollup.local_date,
                    rollup.source_record_count,
                    rollup.successful_request_count,
                    rollup.failed_request_count,
                    rollup.input_tokens,
                    rollup.cached_input_tokens,
                    rollup.cache_creation_input_tokens,
                    rollup.output_tokens,
                    rollup.latency_observation_count,
                    rollup.total_latency_ms,
                ],
            )?;
            imported = imported
                .checked_add(rollup.source_record_count)
                .ok_or_else(|| StateError::Sqlite(tokio_rusqlite::rusqlite::Error::InvalidQuery))?;
        }
        Some(imported)
    } else {
        None
    };

    let universal_changed = input.candidates.iter().any(|candidate| {
        matches!(
            candidate,
            ProviderImportCandidateInput::Universal(ProviderImportUniversalInput {
                resolution: ProviderImportResolutionInput::Create,
                ..
            })
        )
    });
    let universal_view = if universal_changed {
        transaction.execute(
            "UPDATE universal_provider_catalog_state
             SET revision = revision + 1, view_sequence = view_sequence + 1
             WHERE singleton = 1",
            [],
        )?;
        Some(super::universal_providers::project_universal_provider_catalog(transaction)?)
    } else {
        None
    };

    let mut target_views = Vec::new();
    for target in [Target::Codex, Target::Claude] {
        if !changed_targets.contains(&target) {
            continue;
        }
        transaction.execute(
            "UPDATE target_route_state
             SET management_revision = management_revision + 1,
                 view_sequence = view_sequence + 1
             WHERE target = ?1",
            [target.as_str()],
        )?;
        target_views.push(project_target_view_for(transaction, service_epoch, target)?);
    }

    Ok(CommitAttempt::Applied(ProviderImportCommit {
        outcome: ProviderImportOutcome {
            records,
            historical_usage_imported_records,
        },
        target_views,
        universal_view,
    }))
}

fn insert_target_providers(
    transaction: &Transaction<'_>,
    inserts: Vec<TargetInsert<'_>>,
    source_product: &str,
    source_target: &str,
) -> Result<HashSet<Target>, StateError> {
    let mut next_positions: HashMap<Target, u32> = HashMap::new();
    let mut changed_targets = HashSet::new();
    for insert in inserts {
        let target = insert.target();
        let position = if let Some(position) = next_positions.get_mut(&target) {
            let current = *position;
            *position = position
                .checked_add(1)
                .ok_or_else(|| StateError::Sqlite(tokio_rusqlite::rusqlite::Error::InvalidQuery))?;
            current
        } else {
            let current: u32 = transaction.query_row(
                "SELECT COALESCE(MAX(position) + 1, 0) FROM providers WHERE target = ?1",
                [target.as_str()],
                |row| row.get(0),
            )?;
            next_positions.insert(
                target,
                current.checked_add(1).ok_or_else(|| {
                    StateError::Sqlite(tokio_rusqlite::rusqlite::Error::InvalidQuery)
                })?,
            );
            current
        };
        match insert {
            TargetInsert::Ordinary(input, provider_id) => {
                let credential_id =
                    insert_target_credential(transaction, input.target, input.credential.as_ref())?;
                transaction.execute(
                    "INSERT INTO providers
                     (id, target, position, provider_revision, name, base_url, model, protocol,
                      authentication, credential_id, provenance_kind, provenance_key,
                      generated_owner_id, routing_requirement, generated_source_revision,
                      generated_overlay_revision, import_source_product, import_source_target,
                      import_source_identifier, import_configuration_fingerprint)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL,
                             NULL, ?10, NULL, NULL, ?11, ?12, ?13, ?14)",
                    params![
                        provider_id.to_string(),
                        input.target.as_str(),
                        position,
                        input.name,
                        input.base_url,
                        input.model,
                        input.protocol.to_string(),
                        input.authentication.to_string(),
                        credential_id,
                        input.routing_requirement.to_string(),
                        source_product,
                        source_target,
                        input.source_identifier,
                        input.configuration_fingerprint,
                    ],
                )?;
            }
            TargetInsert::Generated {
                source,
                generated,
                overlay,
                owner_id,
                provider_id,
            } => {
                let credential_id = insert_target_credential(
                    transaction,
                    generated.target,
                    source.credential.as_ref(),
                )?;
                transaction.execute(
                    "INSERT INTO providers
                     (id, target, position, provider_revision, name, base_url, model, protocol,
                      authentication, credential_id, provenance_kind, provenance_key,
                      generated_owner_id, routing_requirement, generated_source_revision,
                      generated_overlay_revision, import_source_product, import_source_target,
                      import_source_identifier, import_configuration_fingerprint)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9,
                             'universal-provider', ?10, ?10, ?11, 1, 1, ?12, ?13, ?14, ?15)",
                    params![
                        provider_id.to_string(),
                        generated.target.as_str(),
                        position,
                        source.name,
                        source.base_url,
                        overlay.model,
                        protocol_name(generated.target),
                        overlay.authentication.to_string(),
                        credential_id,
                        owner_id.to_string(),
                        overlay.routing_requirement.to_string(),
                        source_product,
                        source_target,
                        generated.source_id.to_string(),
                        generated.configuration_fingerprint,
                    ],
                )?;
            }
        }
        changed_targets.insert(target);
    }
    Ok(changed_targets)
}

fn replace_failover_drafts(
    transaction: &Transaction<'_>,
    drafts: &[ProviderImportFailoverDraftInput],
    exported_provider_ids: &HashMap<uuid::Uuid, uuid::Uuid>,
    changed_targets: &mut HashSet<Target>,
) -> Result<(), StateError> {
    for draft in drafts {
        transaction.execute(
            "DELETE FROM failover_draft_members WHERE target = ?1",
            [draft.target.as_str()],
        )?;
        let mut position = 0_u32;
        for source_id in &draft.provider_source_ids {
            let Some(provider_id) = exported_provider_ids.get(source_id) else {
                continue;
            };
            let provider_revision = transaction
                .query_row(
                    "SELECT provider_revision FROM providers WHERE id = ?1 AND target = ?2",
                    params![provider_id.to_string(), draft.target.as_str()],
                    |row| row.get::<_, u64>(0),
                )
                .optional()?;
            let Some(provider_revision) = provider_revision else {
                return Err(StateError::Sqlite(
                    tokio_rusqlite::rusqlite::Error::InvalidQuery,
                ));
            };
            transaction.execute(
                "INSERT INTO failover_draft_members
                 (target, position, provider_id, provider_revision)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    draft.target.as_str(),
                    position,
                    provider_id.to_string(),
                    provider_revision,
                ],
            )?;
            position = position
                .checked_add(1)
                .ok_or_else(|| StateError::Sqlite(tokio_rusqlite::rusqlite::Error::InvalidQuery))?;
        }
        transaction.execute(
            "UPDATE failover_drafts SET draft_revision = draft_revision + 1 WHERE target = ?1",
            [draft.target.as_str()],
        )?;
        changed_targets.insert(draft.target);
    }
    Ok(())
}

fn target_provider_is_exact(
    transaction: &Transaction<'_>,
    candidate: &ProviderImportTargetInput,
    provider_id: uuid::Uuid,
) -> Result<bool, tokio_rusqlite::rusqlite::Error> {
    let existing = transaction
        .query_row(
            "SELECT c.bearer_token
             FROM providers p LEFT JOIN credentials c ON c.id = p.credential_id
             WHERE p.id = ?1 AND p.target = ?2 AND p.base_url = ?3 AND p.model = ?4
               AND p.protocol = ?5 AND p.authentication = ?6
               AND p.routing_requirement = ?7 AND p.generated_owner_id IS NULL",
            params![
                provider_id.to_string(),
                candidate.target.as_str(),
                candidate.base_url,
                candidate.model,
                candidate.protocol.to_string(),
                candidate.authentication.to_string(),
                candidate.routing_requirement.to_string(),
            ],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(existing.is_some_and(|credential| {
        credentials_equal(candidate.credential.as_ref(), credential.as_deref())
    }))
}

fn universal_provider_is_exact(
    transaction: &Transaction<'_>,
    candidate: &ProviderImportUniversalInput,
    provider_id: uuid::Uuid,
) -> Result<bool, tokio_rusqlite::rusqlite::Error> {
    let credential = transaction
        .query_row(
            "SELECT c.bearer_token
             FROM universal_providers p
             LEFT JOIN universal_credentials c ON c.id = p.credential_id
             WHERE p.id = ?1 AND p.base_url = ?2",
            params![provider_id.to_string(), candidate.base_url],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    if !credential.is_some_and(|credential| {
        credentials_equal(candidate.credential.as_ref(), credential.as_deref())
    }) {
        return Ok(false);
    }
    let expected = candidate
        .targets
        .iter()
        .map(|overlay| {
            (
                overlay.target.as_str().to_owned(),
                (
                    overlay.enabled,
                    overlay.model.clone(),
                    overlay.authentication.to_string(),
                    overlay.routing_requirement.to_string(),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    let existing = transaction
        .prepare(
            "SELECT target, enabled, model, authentication, routing_requirement
             FROM universal_provider_targets WHERE universal_provider_id = ?1",
        )?
        .query_map([provider_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (
                    row.get::<_, bool>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ),
            ))
        })?
        .collect::<Result<HashMap<_, _>, _>>()?;
    Ok(existing == expected)
}

fn insert_target_credential(
    transaction: &Transaction<'_>,
    target: Target,
    credential: Option<&SecretString>,
) -> Result<Option<String>, tokio_rusqlite::rusqlite::Error> {
    let Some(credential) = credential else {
        return Ok(None);
    };
    let id = uuid::Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, ?3)",
        params![id, target.as_str(), credential.expose_secret()],
    )?;
    Ok(Some(id))
}

fn insert_universal_credential(
    transaction: &Transaction<'_>,
    credential: Option<&SecretString>,
) -> Result<Option<String>, tokio_rusqlite::rusqlite::Error> {
    let Some(credential) = credential else {
        return Ok(None);
    };
    let id = uuid::Uuid::new_v4().to_string();
    transaction.execute(
        "INSERT INTO universal_credentials (id, bearer_token) VALUES (?1, ?2)",
        params![id, credential.expose_secret()],
    )?;
    Ok(Some(id))
}

fn import_product_name(product: ProviderImportProduct) -> &'static str {
    match product {
        ProviderImportProduct::TargetCli => "target-cli",
        ProviderImportProduct::CcSwitch => "cc-switch",
        ProviderImportProduct::Muxvia => "muxvia",
    }
}

fn import_target_name(target: ProviderImportSourceTarget) -> &'static str {
    match target {
        ProviderImportSourceTarget::Codex => "codex",
        ProviderImportSourceTarget::Claude => "claude",
        ProviderImportSourceTarget::Universal => "universal",
    }
}

fn protocol_name(target: Target) -> &'static str {
    match target {
        Target::Codex => "openai-responses",
        Target::Claude => "anthropic-messages",
    }
}

fn target_order(target: Target) -> u8 {
    match target {
        Target::Codex => 0,
        Target::Claude => 1,
    }
}

fn credentials_equal(candidate: Option<&SecretString>, existing: Option<&str>) -> bool {
    match (candidate, existing) {
        (None, None) => true,
        (Some(candidate), Some(existing)) => candidate
            .expose_secret()
            .as_bytes()
            .ct_eq(existing.as_bytes())
            .into(),
        _ => false,
    }
}
