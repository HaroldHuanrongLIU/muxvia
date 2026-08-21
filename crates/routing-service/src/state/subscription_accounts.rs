use tokio_rusqlite::rusqlite::{OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use crate::{
    control::protocol::{
        ActionStatus, ControlProblem, SubscriptionAccountAction, SubscriptionAccountCatalogView,
        SubscriptionAccountOutcome, SubscriptionAccountRecoveryState,
        SubscriptionAccountRecoveryView, SubscriptionAccountState, SubscriptionAccountView,
        SubscriptionBindingResolution, SubscriptionBindingResolutionState,
        SubscriptionProviderBinding, SubscriptionProviderBindingView, Target,
    },
    subscription::{AccountAuthorizationState, SubscriptionAccountDocument},
};

use super::{
    StateError, StateStore,
    store::{map_call_error, map_state_call_error},
};

#[derive(Debug, thiserror::Error)]
#[error("{problem:?}")]
pub(crate) struct SubscriptionAccountActionFailure {
    pub(crate) problem: ControlProblem,
    pub(crate) authoritative_view: SubscriptionAccountCatalogView,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingSubscriptionAccountRecovery {
    pub(crate) intent_id: Uuid,
    pub(crate) action_id: Uuid,
    pub(crate) operation: String,
    pub(crate) before_sha256: String,
    pub(crate) desired_sha256: String,
}

impl StateStore {
    pub(crate) async fn subscription_account_catalog(
        &self,
        document: SubscriptionAccountDocument,
    ) -> Result<SubscriptionAccountCatalogView, StateError> {
        self.connection
            .call(move |connection| project_catalog(connection, document))
            .await
            .map_err(map_call_error)
    }

    pub(crate) async fn subscription_account_receipt(
        &self,
        action_id: Uuid,
        action: &SubscriptionAccountAction,
    ) -> Result<Option<SubscriptionAccountOutcome>, StateError> {
        let action_json = serde_json::to_string(action)?;
        self.connection
            .call(
                move |connection| -> Result<Option<SubscriptionAccountOutcome>, StateError> {
                    let receipt = connection
                        .query_row(
                            "SELECT action_json, outcome_json
                         FROM subscription_account_action_receipts
                         WHERE action_id = ?1",
                            params![action_id.to_string()],
                            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                        )
                        .optional()?;
                    let Some((stored_action, outcome_json)) = receipt else {
                        return Ok(None);
                    };
                    if stored_action != action_json {
                        return Err(StateError::InvalidRecoveryState);
                    }
                    let mut outcome: SubscriptionAccountOutcome =
                        serde_json::from_str(&outcome_json)?;
                    outcome.status = ActionStatus::Replayed;
                    Ok(Some(outcome))
                },
            )
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn subscription_account_receipt_by_id(
        &self,
        action_id: Uuid,
    ) -> Result<Option<SubscriptionAccountOutcome>, StateError> {
        self.connection
            .call(
                move |connection| -> Result<Option<SubscriptionAccountOutcome>, StateError> {
                    let outcome_json = connection
                        .query_row(
                            "SELECT outcome_json FROM subscription_account_action_receipts
                             WHERE action_id = ?1",
                            params![action_id.to_string()],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?;
                    let Some(outcome_json) = outcome_json else {
                        return Ok(None);
                    };
                    let mut outcome: SubscriptionAccountOutcome =
                        serde_json::from_str(&outcome_json)?;
                    outcome.status = ActionStatus::Replayed;
                    Ok(Some(outcome))
                },
            )
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn record_subscription_account_change(
        &self,
        document: SubscriptionAccountDocument,
        recovery_intent_id: Uuid,
    ) -> Result<SubscriptionAccountCatalogView, StateError> {
        self.connection
            .call(move |connection| -> Result<_, StateError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = transaction.execute(
                    "UPDATE subscription_account_recovery_intents SET state = 'committed'
                     WHERE id = ?1 AND state = 'pending'",
                    params![recovery_intent_id.to_string()],
                )?;
                if changed != 1 {
                    return Err(StateError::InvalidRecoveryState);
                }
                let changed = transaction.execute(
                    "UPDATE subscription_account_catalog_state
                     SET revision = revision + 1, view_sequence = view_sequence + 1
                     WHERE singleton = 1 AND recovery_state = 'clean'",
                    [],
                )?;
                if changed != 1 {
                    return Err(StateError::InvalidRecoveryState);
                }
                let view = project_catalog(&transaction, document)?;
                transaction.commit()?;
                Ok(view)
            })
            .await
            .map_err(map_state_call_error)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn begin_subscription_account_recovery(
        &self,
        intent_id: Uuid,
        action_id: Uuid,
        operation: &str,
        expected_revision: u64,
        before_sha256: String,
        desired_sha256: String,
    ) -> Result<(), StateError> {
        let operation = operation.to_owned();
        self.connection
            .call(move |connection| -> Result<(), StateError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let (revision, recovery): (u64, String) = transaction.query_row(
                    "SELECT revision, recovery_state FROM subscription_account_catalog_state
                     WHERE singleton = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if revision != expected_revision || recovery != "clean" {
                    return Err(StateError::InvalidRecoveryState);
                }
                let pending: u64 = transaction.query_row(
                    "SELECT COUNT(*) FROM subscription_account_recovery_intents
                     WHERE state = 'pending'",
                    [],
                    |row| row.get(0),
                )?;
                if pending != 0 {
                    return Err(StateError::InvalidRecoveryState);
                }
                transaction.execute(
                    "INSERT INTO subscription_account_recovery_intents
                       (id, action_id, operation, state, before_sha256, desired_sha256, created_revision)
                     VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)",
                    params![
                        intent_id.to_string(),
                        action_id.to_string(),
                        operation,
                        before_sha256,
                        desired_sha256,
                        expected_revision
                    ],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn pending_subscription_account_recovery(
        &self,
    ) -> Result<Option<PendingSubscriptionAccountRecovery>, StateError> {
        self.connection
            .call(move |connection| -> Result<_, StateError> {
                let mut statement = connection.prepare(
                    "SELECT id, action_id, operation, before_sha256, desired_sha256
                     FROM subscription_account_recovery_intents WHERE state = 'pending'",
                )?;
                let mut rows = statement.query([])?;
                let Some(row) = rows.next()? else {
                    return Ok(None);
                };
                let recovery = PendingSubscriptionAccountRecovery {
                    intent_id: Uuid::parse_str(&row.get::<_, String>(0)?)
                        .map_err(|_| StateError::InvalidRecoveryState)?,
                    action_id: Uuid::parse_str(&row.get::<_, String>(1)?)
                        .map_err(|_| StateError::InvalidRecoveryState)?,
                    operation: row.get(2)?,
                    before_sha256: row.get(3)?,
                    desired_sha256: row.get(4)?,
                };
                if rows.next()?.is_some() {
                    return Err(StateError::InvalidRecoveryState);
                }
                Ok(Some(recovery))
            })
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn subscription_account_recovery_state(
        &self,
        intent_id: Uuid,
    ) -> Result<Option<String>, StateError> {
        self.connection
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT state FROM subscription_account_recovery_intents WHERE id = ?1",
                        params![intent_id.to_string()],
                        |row| row.get(0),
                    )
                    .optional()
            })
            .await
            .map_err(map_call_error)
    }

    pub(crate) async fn finish_subscription_account_recovery(
        &self,
        intent_id: Uuid,
        state: &'static str,
    ) -> Result<(), StateError> {
        if !matches!(state, "rolled-back" | "recovery-required") {
            return Err(StateError::InvalidRecoveryState);
        }
        self.connection
            .call(move |connection| -> Result<(), StateError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let changed = transaction.execute(
                    "UPDATE subscription_account_recovery_intents SET state = ?2
                     WHERE id = ?1 AND state = 'pending'",
                    params![intent_id.to_string(), state],
                )?;
                if changed != 1 {
                    return Err(StateError::InvalidRecoveryState);
                }
                if state == "recovery-required" {
                    transaction.execute(
                        "UPDATE subscription_account_catalog_state
                         SET recovery_state = 'recovery-required', view_sequence = view_sequence + 1
                         WHERE singleton = 1",
                        [],
                    )?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn mark_subscription_account_recovery_required(
        &self,
    ) -> Result<(), StateError> {
        self.connection
            .call(move |connection| -> Result<(), StateError> {
                connection.execute(
                    "UPDATE subscription_account_catalog_state
                     SET recovery_state = 'recovery-required',
                         view_sequence = view_sequence + CASE recovery_state WHEN 'clean' THEN 1 ELSE 0 END
                     WHERE singleton = 1",
                    [],
                )?;
                Ok(())
            })
            .await
            .map_err(map_state_call_error)
    }

    pub(crate) async fn commit_subscription_account_action(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        action: SubscriptionAccountAction,
        document: SubscriptionAccountDocument,
        recovery_intent_id: Option<Uuid>,
    ) -> Result<Result<SubscriptionAccountOutcome, SubscriptionAccountActionFailure>, StateError>
    {
        let action_json = serde_json::to_string(&action)?;
        self.connection
            .call(move |connection| -> Result<_, StateError> {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                if let Some((stored_action, outcome_json)) = transaction
                    .query_row(
                        "SELECT action_json, outcome_json
                         FROM subscription_account_action_receipts
                         WHERE action_id = ?1",
                        params![action_id.to_string()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?
                {
                    if stored_action != action_json {
                        return Ok(Err(subscription_failure(
                            &transaction,
                            document,
                            "invalid-action-replay",
                            "Action identifier was already used for a different request",
                        )?));
                    }
                    let mut outcome: SubscriptionAccountOutcome =
                        serde_json::from_str(&outcome_json)?;
                    outcome.status = ActionStatus::Replayed;
                    return Ok(Ok(outcome));
                }
                let (revision, recovery): (u64, String) = transaction.query_row(
                    "SELECT revision, recovery_state
                     FROM subscription_account_catalog_state WHERE singleton = 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if recovery != "clean" {
                    return Ok(Err(subscription_failure(
                        &transaction,
                        document,
                        "subscription-account-recovery-required",
                        "Subscription Account writes are blocked until recovery is resolved",
                    )?));
                }
                if revision != expected_revision {
                    return Ok(Err(subscription_failure(
                        &transaction,
                        document,
                        "stale-subscription-catalog-revision",
                        "Subscription Account state changed; refresh and retry",
                    )?));
                }
                let action_kind = match &action {
                    SubscriptionAccountAction::SetDefaultAccount { account_id, .. } => {
                        if document.default_account_id.as_deref() != Some(account_id.as_str())
                            || !document.accounts.contains_key(account_id)
                        {
                            return Ok(Err(subscription_failure(
                                &transaction,
                                document,
                                "subscription-account-not-found",
                                "Subscription Account does not exist",
                            )?));
                        }
                        "set-default-account"
                    }
                    SubscriptionAccountAction::DeleteAccount { account_id } => {
                        if document.accounts.contains_key(account_id) {
                            return Ok(Err(subscription_failure(
                                &transaction,
                                document,
                                "subscription-account-not-found",
                                "Subscription Account deletion was not applied",
                            )?));
                        }
                        "delete-account"
                    }
                    SubscriptionAccountAction::BindProviderFixed {
                        target,
                        provider_id,
                        provider_revision,
                        account_id,
                    } => {
                        if !document.accounts.contains_key(account_id)
                            || !provider_revision_matches(
                                &transaction,
                                *target,
                                *provider_id,
                                *provider_revision,
                            )?
                        {
                            return Ok(Err(subscription_failure(
                                &transaction,
                                document,
                                "invalid-subscription-binding",
                                "Subscription Provider binding is stale or invalid",
                            )?));
                        }
                        transaction.execute(
                            "INSERT INTO subscription_provider_bindings
                               (target, provider_id, binding_kind, account_id)
                             VALUES (?1, ?2, 'fixed', ?3)
                             ON CONFLICT(target, provider_id) DO UPDATE SET
                               binding_kind = 'fixed', account_id = excluded.account_id",
                            params![target.as_str(), provider_id.to_string(), account_id],
                        )?;
                        "bind-provider-fixed"
                    }
                    SubscriptionAccountAction::BindProviderFollowDefault {
                        target,
                        provider_id,
                        provider_revision,
                    } => {
                        if !provider_revision_matches(
                            &transaction,
                            *target,
                            *provider_id,
                            *provider_revision,
                        )? {
                            return Ok(Err(subscription_failure(
                                &transaction,
                                document,
                                "invalid-subscription-binding",
                                "Subscription Provider binding is stale or invalid",
                            )?));
                        }
                        transaction.execute(
                            "INSERT INTO subscription_provider_bindings
                               (target, provider_id, binding_kind, account_id)
                             VALUES (?1, ?2, 'follow-default', NULL)
                             ON CONFLICT(target, provider_id) DO UPDATE SET
                               binding_kind = 'follow-default', account_id = NULL",
                            params![target.as_str(), provider_id.to_string()],
                        )?;
                        "bind-provider-follow-default"
                    }
                };
                if let Some(intent_id) = recovery_intent_id {
                    let changed = transaction.execute(
                        "UPDATE subscription_account_recovery_intents SET state = 'committed'
                         WHERE id = ?1 AND action_id = ?2 AND state = 'pending'",
                        params![intent_id.to_string(), action_id.to_string()],
                    )?;
                    if changed != 1 {
                        return Err(StateError::InvalidRecoveryState);
                    }
                }
                transaction.execute(
                    "UPDATE subscription_account_catalog_state
                     SET revision = revision + 1, view_sequence = view_sequence + 1
                     WHERE singleton = 1",
                    [],
                )?;
                let view = project_catalog(&transaction, document)?;
                let outcome = SubscriptionAccountOutcome {
                    status: ActionStatus::Applied,
                    view,
                };
                let outcome_json = serde_json::to_string(&outcome)?;
                transaction.execute(
                    "INSERT INTO subscription_account_action_receipts
                       (action_id, action_kind, action_json, committed_revision, outcome_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        action_id.to_string(),
                        action_kind,
                        action_json,
                        outcome.view.revision,
                        outcome_json
                    ],
                )?;
                transaction.commit()?;
                Ok(Ok(outcome))
            })
            .await
            .map_err(map_state_call_error)
    }
}

fn provider_revision_matches(
    connection: &tokio_rusqlite::rusqlite::Connection,
    target: Target,
    provider_id: Uuid,
    provider_revision: u64,
) -> tokio_rusqlite::rusqlite::Result<bool> {
    connection.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM providers
           WHERE target = ?1 AND id = ?2 AND provider_revision = ?3
         )",
        params![target.as_str(), provider_id.to_string(), provider_revision],
        |row| row.get(0),
    )
}

fn subscription_failure(
    connection: &tokio_rusqlite::rusqlite::Connection,
    document: SubscriptionAccountDocument,
    code: &str,
    message: &str,
) -> tokio_rusqlite::rusqlite::Result<SubscriptionAccountActionFailure> {
    Ok(SubscriptionAccountActionFailure {
        problem: ControlProblem {
            code: code.to_owned(),
            message: message.to_owned(),
            source: None,
            selector: None,
        },
        authoritative_view: project_catalog(connection, document)?,
    })
}

pub(super) fn project_catalog(
    connection: &tokio_rusqlite::rusqlite::Connection,
    document: SubscriptionAccountDocument,
) -> tokio_rusqlite::rusqlite::Result<SubscriptionAccountCatalogView> {
    let (revision, view_sequence, recovery): (u64, u64, String) = connection.query_row(
        "SELECT revision, view_sequence, recovery_state
         FROM subscription_account_catalog_state
         WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let mut statement = connection.prepare(
        "SELECT bindings.target,
                bindings.provider_id,
                providers.provider_revision,
                providers.name,
                bindings.binding_kind,
                bindings.account_id
         FROM subscription_provider_bindings AS bindings
         JOIN providers
           ON providers.target = bindings.target
          AND providers.id = bindings.provider_id
         ORDER BY bindings.target, providers.position, bindings.provider_id",
    )?;
    let rows = statement.query_map(params![], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
        ))
    })?;
    let mut bindings = Vec::new();
    for row in rows {
        let (target, provider_id, provider_revision, provider_name, kind, fixed_id) = row?;
        let target = match target.as_str() {
            "codex" => Target::Codex,
            "claude" => Target::Claude,
            _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
        };
        let provider_id = Uuid::parse_str(&provider_id)
            .map_err(|_| tokio_rusqlite::rusqlite::Error::InvalidQuery)?;
        let binding = match kind.as_str() {
            "fixed" => SubscriptionProviderBinding::Fixed {
                account_id: fixed_id
                    .clone()
                    .ok_or(tokio_rusqlite::rusqlite::Error::InvalidQuery)?,
            },
            "follow-default" if fixed_id.is_none() => SubscriptionProviderBinding::FollowDefault,
            _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
        };
        let resolved_id = fixed_id.or_else(|| document.default_account_id.clone());
        let (state, account_id) = match resolved_id {
            Some(account_id) => match document.accounts.get(&account_id) {
                Some(account) if account.state == AccountAuthorizationState::Authorized => (
                    SubscriptionBindingResolutionState::Available,
                    Some(account_id),
                ),
                Some(_) => (
                    SubscriptionBindingResolutionState::NeedsReauthorization,
                    Some(account_id),
                ),
                None => (
                    SubscriptionBindingResolutionState::Missing,
                    Some(account_id),
                ),
            },
            None => (SubscriptionBindingResolutionState::NoDefault, None),
        };
        bindings.push(SubscriptionProviderBindingView {
            target,
            provider_id,
            provider_revision,
            provider_name,
            binding,
            resolution: SubscriptionBindingResolution { state, account_id },
        });
    }
    let default_account_id = document.default_account_id.clone();
    let accounts = document
        .accounts
        .into_values()
        .map(|account| SubscriptionAccountView {
            is_default: default_account_id.as_deref() == Some(account.account_id.as_str()),
            account_id: account.account_id,
            email: account.email,
            authenticated_at: account.authenticated_at,
            state: match account.state {
                AccountAuthorizationState::Authorized => SubscriptionAccountState::Authorized,
                AccountAuthorizationState::NeedsReauthorization => {
                    SubscriptionAccountState::NeedsReauthorization
                }
            },
        })
        .collect();
    let recovery = match recovery.as_str() {
        "clean" => SubscriptionAccountRecoveryState::Clean,
        "recovery-required" => SubscriptionAccountRecoveryState::RecoveryRequired,
        _ => return Err(tokio_rusqlite::rusqlite::Error::InvalidQuery),
    };
    Ok(SubscriptionAccountCatalogView {
        revision,
        view_sequence,
        default_account_id,
        accounts,
        bindings,
        recovery: SubscriptionAccountRecoveryView { state: recovery },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use tempfile::TempDir;
    use tokio_rusqlite::rusqlite::params;
    use uuid::Uuid;

    use super::*;
    use crate::{home::MuxviaHome, subscription::accounts::SubscriptionAccountRecord};

    #[tokio::test]
    async fn fixed_missing_account_does_not_substitute_the_followed_default() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let store = StateStore::open(&home).await.expect("state store");
        let fixed_provider = Uuid::from_u128(0x11781);
        let followed_provider = Uuid::from_u128(0x11782);
        store
            .connection
            .call(move |connection| {
                for (target, provider_id, position, name) in [
                    ("codex", fixed_provider, 0, "Fixed subscription"),
                    ("claude", followed_provider, 0, "Follow subscription"),
                ] {
                    connection.execute(
                        "INSERT INTO providers (
                           id, target, position, provider_revision, name, base_url, model,
                           protocol, authentication, credential_id, routing_requirement
                         ) VALUES (?1, ?2, ?3, 1, ?4, 'https://example.test', 'model',
                                   CASE ?2 WHEN 'codex' THEN 'openai-responses' ELSE 'anthropic-messages' END,
                                   CASE ?2 WHEN 'codex' THEN 'openai-bearer' ELSE 'anthropic-bearer' END,
                                   NULL, 'direct-compatible')",
                        params![provider_id.to_string(), target, position, name],
                    )?;
                }
                connection.execute(
                    "INSERT INTO subscription_provider_bindings
                       (target, provider_id, binding_kind, account_id)
                     VALUES ('codex', ?1, 'fixed', 'account-deleted')",
                    params![fixed_provider.to_string()],
                )?;
                connection.execute(
                    "INSERT INTO subscription_provider_bindings
                       (target, provider_id, binding_kind, account_id)
                     VALUES ('claude', ?1, 'follow-default', NULL)",
                    params![followed_provider.to_string()],
                )?;
                Ok::<(), tokio_rusqlite::rusqlite::Error>(())
            })
            .await
            .expect("binding fixture");
        let document = SubscriptionAccountDocument {
            version: 1,
            accounts: BTreeMap::from([(
                "account-current".to_owned(),
                SubscriptionAccountRecord {
                    account_id: "account-current".to_owned(),
                    email: None,
                    refresh_token: "SUBSCRIPTION_BINDING_SECRET_11783".to_owned(),
                    authenticated_at: 1,
                    state: AccountAuthorizationState::Authorized,
                },
            )]),
            default_account_id: Some("account-current".to_owned()),
        };

        let view = store
            .subscription_account_catalog(document)
            .await
            .expect("subscription catalog");
        assert!(
            view.bindings.len() == 2,
            "subscription binding count changed"
        );
        let fixed = view
            .bindings
            .iter()
            .find(|binding| binding.provider_id == fixed_provider)
            .expect("fixed binding");
        assert!(
            fixed.resolution.state == SubscriptionBindingResolutionState::Missing
                && fixed.resolution.account_id.as_deref() == Some("account-deleted"),
            "fixed binding substituted the current default"
        );
        let followed = view
            .bindings
            .iter()
            .find(|binding| binding.provider_id == followed_provider)
            .expect("follow-default binding");
        assert!(
            followed.resolution.state == SubscriptionBindingResolutionState::Available
                && followed.resolution.account_id.as_deref() == Some("account-current"),
            "follow-default binding did not resolve the current default"
        );
        let diagnostic = format!("{view:?}");
        assert!(
            !diagnostic.contains("SUBSCRIPTION_BINDING_SECRET_11783"),
            "catalog diagnostic exposed a refresh token"
        );
    }
}
