use std::fmt;

use secrecy::ExposeSecret;
use tokio_rusqlite::rusqlite::{OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::{
    control::protocol::{
        ActionOutcome, ActionStatus, CompatibilityClassification, CompatibilityView,
        ReconciliationStrategy, ShadowSource, Target, TargetView,
    },
    domain::{
        activation::ActivatedSnapshot, provider::normalize_provider_base_url,
        view::project_target_view_for,
    },
};

use super::universal_providers::generated_reference_fingerprint;
use super::{ActionFailure, StateError, StateStore};

pub(crate) struct AdoptReconciliation {
    pub(crate) provider_id: Uuid,
    pub(crate) credential_id: Uuid,
    pub(crate) snapshot: ActivatedSnapshot,
    pub(crate) name: String,
    pub(crate) recovery_id: Uuid,
    pub(crate) recovery_payload_json: String,
    pub(crate) file_identity_json: String,
    pub(crate) config_path: String,
    pub(crate) managed_config_version: u32,
    pub(crate) exit_takeover: bool,
}

pub(crate) struct ReconciliationCommitInput {
    pub(crate) target: Target,
    pub(crate) action_id: Uuid,
    pub(crate) expected_revision: u64,
    pub(crate) strategy: ReconciliationStrategy,
    pub(crate) compatibility: CompatibilityView,
    pub(crate) adopt: Option<AdoptReconciliation>,
    pub(crate) refreshed_recovery_payload_json: Option<String>,
    pub(crate) refreshed_file_identity_json: Option<String>,
    pub(crate) failpoint: ReconciliationCommitFailpoint,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ReconciliationCommitFailpoint {
    #[default]
    None,
    CredentialInsert,
    ProviderInsert,
    SnapshotInsert,
    FinalRevision,
    FinalTransaction,
}

pub(crate) enum ReconciliationCommit {
    Applied(ActionOutcome),
    Replayed(ActionOutcome),
    Stale,
}

#[derive(Clone)]
pub(crate) struct PendingReconciliationIntent {
    pub(crate) action_id: Uuid,
    pub(crate) target: Target,
    pub(crate) strategy: ReconciliationStrategy,
    pub(crate) before_json: String,
    pub(crate) desired_json: String,
}

impl fmt::Debug for PendingReconciliationIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingReconciliationIntent")
            .field("action_id", &self.action_id)
            .field("target", &self.target)
            .field("strategy", &self.strategy)
            .field("before_json", &"<redacted>")
            .field("desired_json", &"<redacted>")
            .finish()
    }
}

impl StateStore {
    pub(crate) async fn project_managed_write_problem(
        &self,
        target: Target,
        code: &'static str,
        source: Option<ShadowSource>,
        compatibility: CompatibilityView,
    ) -> Result<Option<TargetView>, StateError> {
        let service_epoch = self.service_epoch().to_string();
        let selector = source.as_ref().and_then(shadow_selector_code);
        let source = source.as_ref().map(shadow_source_code);
        let message = match code {
            "shadowing-configuration" => "A higher-priority configuration source is active",
            "compatibility-acknowledgement-required" => {
                "Acknowledge this exact untested Target CLI version"
            }
            "incompatible-target-cli" => "Target CLI is incompatible",
            _ => return Err(StateError::InvalidCompatibilityState),
        };
        self.connection
            .call(
                move |connection| -> Result<Option<TargetView>, StateError> {
                    let transaction =
                        connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let prior_compatibility = match read_compatibility(&transaction, target) {
                        Ok(compatibility) => Some(compatibility),
                        Err(StateError::MissingCompatibility) => None,
                        Err(error) => return Err(error),
                    };
                    let compatibility_changed =
                        prior_compatibility.as_ref() != Some(&compatibility);
                    if compatibility_changed {
                        let classification = compatibility.classification.as_str();
                        let acknowledged_version = (!compatibility.acknowledgement_required
                            && compatibility.classification
                                == CompatibilityClassification::UnknownCompatible)
                            .then(|| compatibility.version.clone());
                        transaction.execute(
                            "INSERT INTO target_compatibility
                           (target, observed_version, classification, acknowledged_version)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(target) DO UPDATE SET
                           observed_version = excluded.observed_version,
                           classification = excluded.classification,
                           acknowledged_version = excluded.acknowledged_version",
                            params![
                                target.as_str(),
                                compatibility.version,
                                classification,
                                acknowledged_version
                            ],
                        )?;
                    }
                    let removed = transaction.execute(
                        "DELETE FROM target_problems
                     WHERE target = ?1
                       AND code IN ('shadowing-configuration',
                                    'compatibility-acknowledgement-required',
                                    'incompatible-target-cli')
                       AND code != ?2",
                        params![target.as_str(), code],
                    )?;
                    let problem_changed = transaction.execute(
                        "INSERT INTO target_problems (target, code, message, source, selector)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(target, code) DO UPDATE SET
                       message = excluded.message, source = excluded.source,
                       selector = excluded.selector
                     WHERE target_problems.message != excluded.message
                        OR target_problems.source IS NOT excluded.source
                        OR target_problems.selector IS NOT excluded.selector",
                        params![target.as_str(), code, message, source, selector],
                    )?;
                    if compatibility_changed || removed > 0 || problem_changed > 0 {
                        transaction.execute(
                            "UPDATE target_route_state SET view_sequence = view_sequence + 1
                         WHERE target = ?1",
                            [target.as_str()],
                        )?;
                        let view = project_target_view_for(&transaction, &service_epoch, target)?;
                        transaction.commit()?;
                        Ok(Some(view))
                    } else {
                        transaction.commit()?;
                        Ok(None)
                    }
                },
            )
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub(crate) async fn pending_reconciliation_intents(
        &self,
    ) -> Result<Vec<PendingReconciliationIntent>, StateError> {
        self.connection
            .call(|connection| -> Result<Vec<_>, StateError> {
                let mut statement = connection.prepare(
                    "SELECT action_id, target, strategy, before_json, desired_json
                     FROM reconciliation_intents WHERE state = 'pending'
                     ORDER BY target, action_id",
                )?;
                let rows = statement.query_map([], |row| {
                    let action_id = Uuid::parse_str(&row.get::<_, String>(0)?)
                        .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
                    let target = match row.get::<_, String>(1)?.as_str() {
                        "codex" => Target::Codex,
                        "claude" => Target::Claude,
                        _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
                    };
                    let strategy = match row.get::<_, String>(2)?.as_str() {
                        "adopt" => ReconciliationStrategy::Adopt,
                        "reapply" => ReconciliationStrategy::Reapply,
                        "restore" => ReconciliationStrategy::Restore,
                        _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
                    };
                    Ok(PendingReconciliationIntent {
                        action_id,
                        target,
                        strategy,
                        before_json: row.get(3)?,
                        desired_json: row.get(4)?,
                    })
                })?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StateError::from)
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub(crate) async fn insert_reconciliation_intent(
        &self,
        target: Target,
        action_id: Uuid,
        strategy: ReconciliationStrategy,
        created_revision: u64,
        before_json: String,
        desired_json: String,
    ) -> Result<(), StateError> {
        self.connection
            .call(move |connection| {
                let changed = connection.execute(
                    "INSERT INTO reconciliation_intents
                     (action_id, target, strategy, state, created_revision, before_json, desired_json)
                     VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)
                     ON CONFLICT(target, action_id) DO UPDATE SET
                       strategy = excluded.strategy, state = 'pending',
                       created_revision = excluded.created_revision,
                       before_json = excluded.before_json, desired_json = excluded.desired_json
                     WHERE reconciliation_intents.state = 'rolled-back'",
                    params![
                        action_id.to_string(),
                        target.as_str(),
                        strategy.as_str(),
                        created_revision,
                        before_json,
                        desired_json
                    ],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                Ok(())
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub(crate) async fn set_reconciliation_intent_state(
        &self,
        target: Target,
        action_id: Uuid,
        state: &'static str,
    ) -> Result<(), StateError> {
        self.connection
            .call(move |connection| {
                let changed = connection.execute(
                    "UPDATE reconciliation_intents SET state = ?1
                     WHERE target = ?2 AND action_id = ?3 AND state = 'pending'",
                    params![state, target.as_str(), action_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                Ok(())
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub(crate) async fn commit_reconciliation(
        &self,
        input: ReconciliationCommitInput,
    ) -> Result<ReconciliationCommit, StateError> {
        let service_epoch = self.service_epoch().to_string();
        self.connection
            .call(move |connection| -> Result<ReconciliationCommit, StateError> {
                let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let recorded = transaction.query_row(
                    "SELECT outcome_json FROM action_receipts WHERE target = ?1 AND action_id = ?2",
                    params![input.target.as_str(), input.action_id.to_string()],
                    |row| row.get::<_, String>(0),
                );
                match recorded {
                    Ok(json) => {
                        let mut outcome: ActionOutcome = serde_json::from_str(&json)?;
                        outcome.status = ActionStatus::Replayed;
                        return Ok(ReconciliationCommit::Replayed(outcome));
                    }
                    Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {}
                    Err(error) => return Err(StateError::Sqlite(error)),
                }
                let revision: u64 = transaction.query_row(
                    "SELECT management_revision FROM target_route_state WHERE target = ?1",
                    [input.target.as_str()],
                    |row| row.get(0),
                )?;
                if revision != input.expected_revision {
                    return Ok(ReconciliationCommit::Stale);
                }
                let pending: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM reconciliation_intents
                     WHERE target = ?1 AND action_id = ?2 AND strategy = ?3 AND state = 'pending')",
                    params![input.target.as_str(), input.action_id.to_string(), input.strategy.as_str()],
                    |row| row.get(0),
                )?;
                if !pending {
                    return Err(StateError::MissingRecoveryIntent);
                }
                let generated_references_before = generated_reference_fingerprint(&transaction)?;

                match input.strategy {
                    ReconciliationStrategy::Adopt => {
                        let adopt = input.adopt.ok_or(StateError::InvalidActivatedSnapshot)?;
                        let normalized = normalize_provider_base_url(&adopt.snapshot.base_url)
                            .map_err(|_| StateError::InvalidActivatedSnapshot)?;
                        let prior: Option<(Option<String>, Option<String>)> = transaction
                            .query_row(
                                "SELECT p.credential_id, c.bearer_token
                                 FROM target_route_state r
                                 LEFT JOIN providers p ON p.id = r.current_provider_id AND p.target = r.target
                                 LEFT JOIN credentials c ON c.id = p.credential_id AND c.target = p.target
                                 WHERE r.target = ?1",
                                [input.target.as_str()],
                                |row| Ok((row.get(0)?, row.get(1)?)),
                            )
                            .optional()?;
                        let observed = adopt.snapshot.provider_credential.expose_secret();
                        let credential_id = match prior {
                            Some((Some(id), Some(secret))) if secret == observed => id,
                            _ => {
                                transaction.execute(
                                    "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, ?3)",
                                    params![adopt.credential_id.to_string(), input.target.as_str(), observed],
                                )?;
                                fail_reconciliation_commit(
                                    input.failpoint,
                                    ReconciliationCommitFailpoint::CredentialInsert,
                                )?;
                                adopt.credential_id.to_string()
                            }
                        };
                        let position: u64 = transaction.query_row(
                            "SELECT COALESCE(MAX(position) + 1, 0) FROM providers WHERE target = ?1",
                            [input.target.as_str()],
                            |row| row.get(0),
                        )?;
                        transaction.execute(
                            "INSERT INTO providers
                             (id, target, position, provider_revision, name, base_url, model,
                              protocol, authentication, credential_id, provenance_kind,
                              provenance_key, generated_owner_id, routing_requirement)
                             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9,
                                     NULL, NULL, NULL, 'direct-compatible')",
                            params![adopt.provider_id.to_string(), input.target.as_str(), position,
                                adopt.name, normalized, adopt.snapshot.model,
                                adopt.snapshot.protocol.to_string(), adopt.snapshot.authentication.to_string(),
                                credential_id],
                        )?;
                        if adopt.exit_takeover {
                            transaction.execute(
                                "UPDATE target_route_state SET serving_provider_id = NULL,
                                   takeover_state = 'inactive', route_port = NULL,
                                   routing_credential = NULL WHERE target = ?1",
                                [input.target.as_str()],
                            )?;
                        }
                        fail_reconciliation_commit(
                            input.failpoint,
                            ReconciliationCommitFailpoint::ProviderInsert,
                        )?;
                        transaction.execute(
                            "INSERT INTO activated_snapshots
                             (id, target, provider_id, base_url, model, protocol, authentication,
                              provider_bearer_token, epoch)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            params![adopt.snapshot.id.to_string(), input.target.as_str(),
                                adopt.provider_id.to_string(), normalized, adopt.snapshot.model,
                                adopt.snapshot.protocol.to_string(), adopt.snapshot.authentication.to_string(),
                                observed, adopt.snapshot.epoch.to_string()],
                        )?;
                        transaction.execute(
                            "INSERT INTO activated_route_plans
                             (id, target, epoch, created_revision)
                             SELECT ?1, ?2, ?3, management_revision + 1
                             FROM target_route_state WHERE target = ?2",
                            params![
                                adopt.snapshot.id.to_string(),
                                input.target.as_str(),
                                adopt.snapshot.epoch.to_string()
                            ],
                        )?;
                        transaction.execute(
                            "INSERT INTO activated_route_plan_members
                             (plan_id, position, provider_id, provider_revision, name, base_url,
                              model, protocol, authentication, credential_id, routing_requirement)
                             VALUES (?1, 0, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8,
                                     'direct-compatible')",
                            params![
                                adopt.snapshot.id.to_string(),
                                adopt.provider_id.to_string(),
                                adopt.name,
                                normalized,
                                adopt.snapshot.model,
                                adopt.snapshot.protocol.to_string(),
                                adopt.snapshot.authentication.to_string(),
                                credential_id,
                            ],
                        )?;
                        transaction.execute(
                            "DELETE FROM failover_draft_members WHERE target = ?1",
                            [input.target.as_str()],
                        )?;
                        transaction.execute(
                            "UPDATE failover_drafts SET draft_revision = draft_revision + 1
                             WHERE target = ?1",
                            [input.target.as_str()],
                        )?;
                        transaction.execute(
                            "INSERT INTO failover_draft_members
                             (target, position, provider_id, provider_revision)
                             VALUES (?1, 0, ?2, 1)",
                            params![input.target.as_str(), adopt.provider_id.to_string()],
                        )?;
                        fail_reconciliation_commit(
                            input.failpoint,
                            ReconciliationCommitFailpoint::SnapshotInsert,
                        )?;
                        transaction.execute(
                            "INSERT INTO activation_recovery
                             (id, target, action_id, config_path, file_identity_json, payload_json,
                              state, created_revision)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'committed', ?7)",
                            params![adopt.recovery_id.to_string(), input.target.as_str(),
                                input.action_id.to_string(), adopt.config_path, adopt.file_identity_json,
                                adopt.recovery_payload_json, input.expected_revision],
                        )?;
                        transaction.execute(
                            "UPDATE target_route_state SET current_provider_id = ?1,
                               activated_snapshot_id = ?2, recovery_intent_id = ?3,
                               managed_config_version = ?4, active_route_plan_id = ?2,
                               recovery_state = 'clean'
                             WHERE target = ?5",
                            params![adopt.provider_id.to_string(), adopt.snapshot.id.to_string(),
                                adopt.recovery_id.to_string(), adopt.managed_config_version,
                                input.target.as_str()],
                        )?;
                    }
                    ReconciliationStrategy::Reapply => {
                        let payload = input
                            .refreshed_recovery_payload_json
                            .ok_or(StateError::InvalidRecoveryPayload)?;
                        let identity = input
                            .refreshed_file_identity_json
                            .ok_or(StateError::InvalidRecoveryPayload)?;
                        let changed = transaction.execute(
                            "UPDATE activation_recovery SET payload_json = ?1,
                               file_identity_json = ?2
                             WHERE id = (SELECT recovery_intent_id FROM target_route_state
                                         WHERE target = ?3)
                               AND target = ?3 AND state = 'committed'",
                            params![payload, identity, input.target.as_str()],
                        )?;
                        if changed != 1 {
                            return Err(StateError::MissingRecoveryIntent);
                        }
                    }
                    ReconciliationStrategy::Restore => {
                        transaction.execute(
                            "DELETE FROM failover_draft_members WHERE target = ?1",
                            [input.target.as_str()],
                        )?;
                        transaction.execute(
                            "UPDATE failover_drafts SET draft_revision = draft_revision + 1
                             WHERE target = ?1",
                            [input.target.as_str()],
                        )?;
                        transaction.execute(
                            "UPDATE target_route_state SET current_provider_id = NULL,
                               serving_provider_id = NULL, takeover_state = 'inactive',
                               route_port = NULL, routing_credential = NULL,
                               activated_snapshot_id = NULL, managed_config_path = NULL,
                               managed_config_version = 1, recovery_intent_id = NULL,
                               active_route_plan_id = NULL, recovery_state = 'clean'
                             WHERE target = ?1",
                            [input.target.as_str()],
                        )?;
                    }
                }
                let (classification, acknowledged) = compatibility_database_values(&input.compatibility)?;
                transaction.execute(
                    "INSERT INTO target_compatibility
                     (target, observed_version, classification, acknowledged_version)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(target) DO UPDATE SET observed_version = excluded.observed_version,
                       classification = excluded.classification,
                       acknowledged_version = excluded.acknowledged_version",
                    params![input.target.as_str(), input.compatibility.version,
                        classification, acknowledged],
                )?;
                transaction.execute(
                    "DELETE FROM target_problems WHERE target = ?1 AND code IN
                     ('configuration-drift', 'shadowing-configuration', 'untested-target-cli',
                      'compatibility-acknowledgement-required', 'incompatible-target-cli')",
                    [input.target.as_str()],
                )?;
                transaction.execute(
                    "UPDATE target_route_state SET management_revision = management_revision + 1,
                       view_sequence = view_sequence + 1 WHERE target = ?1",
                    [input.target.as_str()],
                )?;
                if generated_references_before != generated_reference_fingerprint(&transaction)? {
                    transaction.execute(
                        "UPDATE universal_provider_catalog_state
                         SET view_sequence = view_sequence + 1 WHERE singleton = 1",
                        [],
                    )?;
                }
                fail_reconciliation_commit(
                    input.failpoint,
                    ReconciliationCommitFailpoint::FinalRevision,
                )?;
                transaction.execute(
                    "UPDATE reconciliation_intents SET state = 'committed'
                     WHERE target = ?1 AND action_id = ?2 AND state = 'pending'",
                    params![input.target.as_str(), input.action_id.to_string()],
                )?;
                let view = project_target_view_for(&transaction, &service_epoch, input.target)?;
                let outcome = ActionOutcome { status: ActionStatus::Applied, view };
                transaction.execute(
                    "INSERT INTO action_receipts
                     (target, action_id, action_kind, committed_revision, outcome_json)
                     VALUES (?1, ?2, 'reconcile', ?3, ?4)",
                    params![input.target.as_str(), input.action_id.to_string(),
                        outcome.view.management_revision, serde_json::to_string(&outcome)?],
                )?;
                fail_reconciliation_commit(
                    input.failpoint,
                    ReconciliationCommitFailpoint::FinalTransaction,
                )?;
                transaction.commit()?;
                Ok(ReconciliationCommit::Applied(outcome))
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub(crate) async fn mark_reconciliation_recovery_required(
        &self,
        target: Target,
        action_id: Uuid,
    ) -> Result<ActionOutcome, StateError> {
        let service_epoch = self.service_epoch().to_string();
        self.connection
            .call(move |connection| -> Result<ActionOutcome, StateError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = transaction.execute(
                    "UPDATE reconciliation_intents SET state = 'recovery-required'
                     WHERE target = ?1 AND action_id = ?2 AND state = 'pending'",
                    params![target.as_str(), action_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                transaction.execute(
                    "UPDATE target_route_state SET recovery_state = 'recovery-required',
                       view_sequence = view_sequence + 1 WHERE target = ?1",
                    [target.as_str()],
                )?;
                let view = project_target_view_for(&transaction, &service_epoch, target)?;
                let outcome = ActionOutcome {
                    status: ActionStatus::Applied,
                    view,
                };
                transaction.execute(
                    "INSERT INTO action_receipts
                     (target, action_id, action_kind, committed_revision, outcome_json)
                     VALUES (?1, ?2, 'reconcile', ?3, ?4)",
                    params![
                        target.as_str(),
                        action_id.to_string(),
                        outcome.view.management_revision,
                        serde_json::to_string(&outcome)?
                    ],
                )?;
                transaction.commit()?;
                Ok(outcome)
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

    pub(crate) async fn mark_committed_reconciliation_recovery_required(
        &self,
        target: Target,
        action_id: Uuid,
    ) -> Result<ActionOutcome, StateError> {
        let service_epoch = self.service_epoch().to_string();
        self.connection
            .call(move |connection| -> Result<ActionOutcome, StateError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = transaction.execute(
                    "UPDATE reconciliation_intents SET state = 'recovery-required'
                     WHERE target = ?1 AND action_id = ?2 AND state = 'committed'",
                    params![target.as_str(), action_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                transaction.execute(
                    "UPDATE target_route_state SET recovery_state = 'recovery-required',
                       view_sequence = view_sequence + 1 WHERE target = ?1",
                    [target.as_str()],
                )?;
                let view = project_target_view_for(&transaction, &service_epoch, target)?;
                let outcome = ActionOutcome {
                    status: ActionStatus::Applied,
                    view,
                };
                let changed = transaction.execute(
                    "UPDATE action_receipts SET outcome_json = ?1
                     WHERE target = ?2 AND action_id = ?3 AND action_kind = 'reconcile'",
                    params![
                        serde_json::to_string(&outcome)?,
                        target.as_str(),
                        action_id.to_string()
                    ],
                )?;
                if changed != 1 {
                    return Err(StateError::MissingRecoveryIntent);
                }
                transaction.commit()?;
                Ok(outcome)
            })
            .await
            .map_err(super::store::map_state_call_error)
    }

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

    pub(crate) async fn apply_compatibility_resolution(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        version: String,
        compatibility: CompatibilityView,
    ) -> Result<Result<ActionOutcome, ActionFailure>, StateError> {
        let service_epoch = self.service_epoch().to_string();
        self.connection
            .call(
                move |connection| -> Result<Result<ActionOutcome, ActionFailure>, StateError> {
                    let transaction =
                        connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                    let recorded = transaction.query_row(
                        "SELECT outcome_json FROM action_receipts
                         WHERE target = ?1 AND action_id = ?2",
                        params![target.as_str(), action_id.to_string()],
                        |row| row.get::<_, String>(0),
                    );
                    match recorded {
                        Ok(json) => {
                            let mut outcome: ActionOutcome = serde_json::from_str(&json)?;
                            outcome.status = ActionStatus::Replayed;
                            transaction.commit()?;
                            return Ok(Ok(outcome));
                        }
                        Err(tokio_rusqlite::rusqlite::Error::QueryReturnedNoRows) => {}
                        Err(error) => return Err(StateError::Sqlite(error)),
                    }
                    let current_revision: u64 = transaction.query_row(
                        "SELECT management_revision FROM target_route_state WHERE target = ?1",
                        [target.as_str()],
                        |row| row.get(0),
                    )?;
                    if current_revision != expected_revision {
                        let authoritative_view =
                            project_target_view_for(&transaction, &service_epoch, target)?;
                        transaction.commit()?;
                        return Ok(Err(ActionFailure {
                            problem: crate::control::protocol::ControlProblem {
                                code: "stale-revision".into(),
                                message: "Target state changed; refresh and retry".into(),
                                source: None,
                                selector: None,
                            },
                            authoritative_view,
                        }));
                    }
                    if compatibility.classification == CompatibilityClassification::Incompatible
                        || compatibility.version != version
                    {
                        let code = if compatibility.classification
                            == CompatibilityClassification::Incompatible
                        {
                            "incompatible-target-cli"
                        } else {
                            "stale-compatibility-probe"
                        };
                        let authoritative_view =
                            project_target_view_for(&transaction, &service_epoch, target)?;
                        transaction.commit()?;
                        return Ok(Err(ActionFailure {
                            problem: crate::control::protocol::ControlProblem {
                                code: code.into(),
                                message: "Compatibility resolution is not valid".into(),
                                source: None,
                                selector: None,
                            },
                            authoritative_view,
                        }));
                    }
                    let classification = compatibility.classification.as_str();
                    let acknowledged_version = (compatibility.classification
                        == CompatibilityClassification::UnknownCompatible)
                        .then(|| version.clone());
                    transaction.execute(
                        "INSERT INTO target_compatibility
                           (target, observed_version, classification, acknowledged_version)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(target) DO UPDATE SET
                           observed_version = excluded.observed_version,
                           classification = excluded.classification,
                           acknowledged_version = excluded.acknowledged_version",
                        params![
                            target.as_str(),
                            version,
                            classification,
                            acknowledged_version
                        ],
                    )?;
                    transaction.execute(
                        "DELETE FROM target_problems
                         WHERE target = ?1 AND code IN
                           ('compatibility-acknowledgement-required', 'incompatible-target-cli')",
                        [target.as_str()],
                    )?;
                    transaction.execute(
                        "UPDATE target_route_state SET view_sequence = view_sequence + 1
                         WHERE target = ?1",
                        [target.as_str()],
                    )?;
                    let view = project_target_view_for(&transaction, &service_epoch, target)?;
                    let outcome = ActionOutcome {
                        status: ActionStatus::Applied,
                        view,
                    };
                    transaction.execute(
                        "INSERT INTO action_receipts
                         (target, action_id, action_kind, committed_revision, outcome_json)
                         VALUES (?1, ?2, 'resolve-compatibility', ?3, ?4)",
                        params![
                            target.as_str(),
                            action_id.to_string(),
                            outcome.view.management_revision,
                            serde_json::to_string(&outcome)?
                        ],
                    )?;
                    transaction.commit()?;
                    Ok(Ok(outcome))
                },
            )
            .await
            .map_err(super::store::map_state_call_error)
    }
}

fn fail_reconciliation_commit(
    actual: ReconciliationCommitFailpoint,
    expected: ReconciliationCommitFailpoint,
) -> Result<(), StateError> {
    if actual == expected {
        Err(StateError::Unavailable)
    } else {
        Ok(())
    }
}

fn compatibility_database_values(
    view: &CompatibilityView,
) -> Result<(&'static str, Option<String>), StateError> {
    match view.classification {
        CompatibilityClassification::Tested => Ok(("tested", None)),
        CompatibilityClassification::UnknownCompatible if !view.acknowledgement_required => {
            Ok(("unknown-compatible", Some(view.version.clone())))
        }
        CompatibilityClassification::UnknownCompatible
        | CompatibilityClassification::Incompatible => Err(StateError::InvalidCompatibilityState),
    }
}

impl ReconciliationStrategy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Adopt => "adopt",
            Self::Reapply => "reapply",
            Self::Restore => "restore",
        }
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

fn shadow_source_code(source: &ShadowSource) -> &'static str {
    match source {
        ShadowSource::CodexProfile => "codex-profile",
        ShadowSource::ClaudeManaged => "claude-managed",
        ShadowSource::ClaudeShared => "claude-shared",
        ShadowSource::ClaudeProject => "claude-project",
        ShadowSource::ClaudeLocal => "claude-local",
        ShadowSource::ClaudeSelector(_) => "claude-selector",
        ShadowSource::ClaudeHostManaged => "claude-host-managed",
    }
}

fn shadow_selector_code(source: &ShadowSource) -> Option<&'static str> {
    match source {
        ShadowSource::ClaudeSelector(selector) => Some(selector.as_str()),
        ShadowSource::ClaudeHostManaged => {
            Some(crate::control::protocol::ClaudeBlockingSelector::HostManaged.as_str())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use tempfile::TempDir;
    use tokio_rusqlite::rusqlite::params;
    use uuid::Uuid;

    use super::{
        AdoptReconciliation, ReconciliationCommit, ReconciliationCommitFailpoint,
        ReconciliationCommitInput,
    };
    use crate::{
        control::protocol::{
            ActionStatus, CompatibilityClassification, CompatibilityView, ProviderAuthentication,
            ProviderProtocol, ReconciliationStrategy, Target,
        },
        domain::activation::ActivatedSnapshot,
        home::MuxviaHome,
        state::{ManagedWriteStatus, StateStore},
    };

    async fn seed_managed_route(store: &StateStore, target: Target) -> (Uuid, Uuid, Uuid) {
        let provider_id = Uuid::new_v4();
        let credential_id = Uuid::new_v4();
        let snapshot_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        store
            .connection
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO credentials (id, target, bearer_token) VALUES (?1, ?2, 'OLD_SECRET')",
                    params![credential_id.to_string(), target.as_str()],
                )?;
                connection.execute(
                    "INSERT INTO providers
                     (id, target, position, provider_revision, name, base_url, model, protocol,
                      authentication, credential_id, provenance_kind, provenance_key,
                      generated_owner_id, routing_requirement)
                     VALUES (?1, ?2, 0, 1, 'Old', 'https://old.test/v1', 'old-model',
                             'openai-responses', 'openai-bearer', ?3, NULL, NULL, NULL,
                             'direct-compatible')",
                    params![
                        provider_id.to_string(),
                        target.as_str(),
                        credential_id.to_string()
                    ],
                )?;
                connection.execute(
                    "INSERT INTO activated_snapshots
                     (id, target, provider_id, base_url, model, protocol, authentication,
                      provider_bearer_token, epoch)
                     VALUES (?1, ?2, ?3, 'https://old.test/v1', 'old-model',
                             'openai-responses', 'openai-bearer', 'OLD_SECRET', ?4)",
                    params![
                        snapshot_id.to_string(),
                        target.as_str(),
                        provider_id.to_string(),
                        epoch.to_string()
                    ],
                )?;
                connection.execute(
                    "INSERT INTO activated_route_plans (id, target, epoch, created_revision)
                     VALUES (?1, ?2, ?3, 0)",
                    params![snapshot_id.to_string(), target.as_str(), epoch.to_string()],
                )?;
                connection.execute(
                    "INSERT INTO activated_route_plan_members
                     (plan_id, position, provider_id, provider_revision, name, base_url, model,
                      protocol, authentication, credential_id, routing_requirement)
                     VALUES (?1, 0, ?2, 1, 'Old', 'https://old.test/v1', 'old-model',
                             'openai-responses', 'openai-bearer', ?3, 'direct-compatible')",
                    params![
                        snapshot_id.to_string(),
                        provider_id.to_string(),
                        credential_id.to_string()
                    ],
                )?;
                connection.execute(
                    "INSERT INTO failover_draft_members
                     (target, position, provider_id, provider_revision) VALUES (?1, 0, ?2, 1)",
                    params![target.as_str(), provider_id.to_string()],
                )?;
                connection.execute(
                    "UPDATE target_route_state SET current_provider_id = ?1,
                       activated_snapshot_id = ?2, active_route_plan_id = ?2
                     WHERE target = ?3",
                    params![
                        provider_id.to_string(),
                        snapshot_id.to_string(),
                        target.as_str()
                    ],
                )?;
                Ok::<_, tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .unwrap();
        (provider_id, credential_id, snapshot_id)
    }

    #[tokio::test]
    async fn adopt_replaces_the_active_plan_and_draft_without_mutating_history() {
        let temp = TempDir::new().unwrap();
        let home = MuxviaHome::from_user_home(temp.path());
        let store = StateStore::open(&home).await.unwrap();
        let (old_provider_id, _old_credential_id, old_plan_id) =
            seed_managed_route(&store, Target::Codex).await;
        let action_id = Uuid::new_v4();
        store
            .insert_reconciliation_intent(
                Target::Codex,
                action_id,
                ReconciliationStrategy::Adopt,
                0,
                "before".to_owned(),
                "desired".to_owned(),
            )
            .await
            .unwrap();
        let provider_id = Uuid::new_v4();
        let credential_id = Uuid::new_v4();
        let snapshot_id = Uuid::new_v4();
        let epoch = Uuid::new_v4();

        let committed = store
            .commit_reconciliation(ReconciliationCommitInput {
                target: Target::Codex,
                action_id,
                expected_revision: 0,
                strategy: ReconciliationStrategy::Adopt,
                compatibility: CompatibilityView {
                    version: "tested".to_owned(),
                    classification: CompatibilityClassification::Tested,
                    acknowledgement_required: false,
                },
                adopt: Some(AdoptReconciliation {
                    provider_id,
                    credential_id,
                    snapshot: ActivatedSnapshot {
                        id: snapshot_id,
                        target: Target::Codex,
                        provider_id,
                        base_url: "https://adopted.test/v1".to_owned(),
                        model: "adopted-model".to_owned(),
                        protocol: ProviderProtocol::OpenaiResponses,
                        authentication: ProviderAuthentication::OpenaiBearer,
                        provider_credential: SecretString::from("ADOPTED_SECRET"),
                        epoch,
                    },
                    name: "Adopted".to_owned(),
                    recovery_id: Uuid::new_v4(),
                    recovery_payload_json: "{}".to_owned(),
                    file_identity_json: "{}".to_owned(),
                    config_path: "/tmp/adopted".to_owned(),
                    managed_config_version: 1,
                    exit_takeover: false,
                }),
                refreshed_recovery_payload_json: None,
                refreshed_file_identity_json: None,
                failpoint: ReconciliationCommitFailpoint::None,
            })
            .await
            .unwrap();
        let ReconciliationCommit::Applied(outcome) = committed else {
            panic!("Adopt did not commit");
        };

        let active = outcome.view.failover.active_plan.unwrap();
        assert!(
            active.id == snapshot_id
                && active.epoch == epoch
                && active.members.len() == 1
                && active.members[0].provider_id == provider_id
                && outcome.view.failover.draft_revision == 2
                && outcome.view.failover.draft_members.len() == 1
                && outcome.view.failover.draft_members[0].provider_id == provider_id,
            "Adopt did not replace the active plan and draft with its new Current Provider"
        );
        let immutable_history = store
            .connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM activated_route_plans plan
                       JOIN activated_route_plan_members member ON member.plan_id = plan.id
                       WHERE plan.id = ?1 AND plan.target = 'codex' AND member.provider_id = ?2
                     )",
                    params![old_plan_id.to_string(), old_provider_id.to_string()],
                    |row| row.get::<_, bool>(0),
                )
            })
            .await
            .unwrap();
        assert!(
            immutable_history,
            "Adopt mutated historical route-plan material"
        );
    }

    #[tokio::test]
    async fn restore_clears_the_active_plan_and_draft_without_mutating_history() {
        let temp = TempDir::new().unwrap();
        let home = MuxviaHome::from_user_home(temp.path());
        let store = StateStore::open(&home).await.unwrap();
        let (old_provider_id, _old_credential_id, old_plan_id) =
            seed_managed_route(&store, Target::Codex).await;
        let action_id = Uuid::new_v4();
        store
            .insert_reconciliation_intent(
                Target::Codex,
                action_id,
                ReconciliationStrategy::Restore,
                0,
                "before".to_owned(),
                "desired".to_owned(),
            )
            .await
            .unwrap();

        let committed = store
            .commit_reconciliation(ReconciliationCommitInput {
                target: Target::Codex,
                action_id,
                expected_revision: 0,
                strategy: ReconciliationStrategy::Restore,
                compatibility: CompatibilityView {
                    version: "tested".to_owned(),
                    classification: CompatibilityClassification::Tested,
                    acknowledgement_required: false,
                },
                adopt: None,
                refreshed_recovery_payload_json: None,
                refreshed_file_identity_json: None,
                failpoint: ReconciliationCommitFailpoint::None,
            })
            .await
            .unwrap();
        let ReconciliationCommit::Applied(outcome) = committed else {
            panic!("Restore did not commit");
        };

        assert!(
            outcome.view.current_provider_id.is_none()
                && outcome.view.failover.active_plan.is_none()
                && outcome.view.failover.draft_revision == 2
                && outcome.view.failover.draft_members.is_empty(),
            "Restore left an active plan or draft after clearing the Current Provider"
        );
        let immutable_history = store
            .connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM activated_route_plans plan
                       JOIN activated_route_plan_members member ON member.plan_id = plan.id
                       WHERE plan.id = ?1 AND plan.target = 'codex' AND member.provider_id = ?2
                     )",
                    params![old_plan_id.to_string(), old_provider_id.to_string()],
                    |row| row.get::<_, bool>(0),
                )
            })
            .await
            .unwrap();
        assert!(
            immutable_history,
            "Restore mutated historical route-plan material"
        );
    }

    #[tokio::test]
    async fn ambiguous_startup_intent_marks_only_its_target_and_leaves_a_replay_receipt() {
        let temp = TempDir::new().unwrap();
        let home = MuxviaHome::from_user_home(temp.path());
        let store = StateStore::open(&home).await.unwrap();
        let action_id = Uuid::new_v4();
        store
            .insert_reconciliation_intent(
                Target::Codex,
                action_id,
                ReconciliationStrategy::Reapply,
                0,
                "CORRUPT_BEFORE_SENTINEL_97109".to_owned(),
                "CORRUPT_DESIRED_SENTINEL_97110".to_owned(),
            )
            .await
            .unwrap();
        let pending_debug = format!(
            "{:?}",
            store.pending_reconciliation_intents().await.unwrap()[0]
        );
        for sentinel in [
            "CORRUPT_BEFORE_SENTINEL_97109",
            "CORRUPT_DESIRED_SENTINEL_97110",
        ] {
            assert!(!pending_debug.contains(sentinel));
            assert!(!pending_debug.contains(&format!("{:?}", sentinel.as_bytes())));
        }

        let outcome = store
            .mark_reconciliation_recovery_required(Target::Codex, action_id)
            .await
            .unwrap();

        assert_eq!(outcome.view.recovery.state, "recovery-required");
        assert_eq!(
            store.managed_write_status_for(Target::Codex).await.unwrap(),
            ManagedWriteStatus::RecoveryRequired
        );
        assert_eq!(
            store
                .managed_write_status_for(Target::Claude)
                .await
                .unwrap(),
            ManagedWriteStatus::Allowed
        );
        let replay = store
            .receipt_for(Target::Codex, action_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.status, ActionStatus::Replayed);
        assert_eq!(replay.view, outcome.view);
        let diagnostic = format!("{outcome:?}\n{replay:?}");
        for sentinel in [
            "CORRUPT_BEFORE_SENTINEL_97109",
            "CORRUPT_DESIRED_SENTINEL_97110",
        ] {
            assert!(!diagnostic.contains(sentinel));
            assert!(!diagnostic.contains(&format!("{:?}", sentinel.as_bytes())));
        }
    }
}
