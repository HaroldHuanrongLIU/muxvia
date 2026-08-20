use std::{collections::HashMap, sync::Arc};

use tokio::sync::{Mutex, OwnedMutexGuard, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    control::protocol::{
        ControlProblem, SubscriptionAccountAction, SubscriptionAccountCatalogView,
        SubscriptionAccountOutcome, SubscriptionBindingResolutionState, SubscriptionDefaultEffect,
        SubscriptionDefaultPreview, SubscriptionProviderBinding,
    },
    state::{StateStore, SubscriptionAccountActionFailure},
};

use super::accounts::{SubscriptionAccountFileSnapshot, SubscriptionAccountRecord};
use super::{AccountAuthorizationState, SubscriptionAccountStore};

struct DefaultPreviewBinding {
    account_id: String,
    revision: u64,
}

#[derive(Clone)]
pub(crate) struct SubscriptionAuthorizationCancellation {
    token: CancellationToken,
    gate: Arc<Mutex<()>>,
}

impl SubscriptionAuthorizationCancellation {
    pub(crate) fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            gate: Arc::new(Mutex::new(())),
        }
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.token.clone()
    }

    pub(crate) async fn cancel(&self) {
        let _guard = self.gate.lock().await;
        self.token.cancel();
    }

    async fn begin_commit(&self) -> Option<OwnedMutexGuard<()>> {
        let guard = self.gate.clone().lock_owned().await;
        (!self.token.is_cancelled()).then_some(guard)
    }
}

#[derive(Debug)]
pub(crate) enum SubscriptionAuthorizationCommit {
    Cancelled,
    Committed(SubscriptionAccountCatalogView),
}

pub(crate) struct SubscriptionAccountCoordinator {
    state: Arc<StateStore>,
    accounts: Arc<SubscriptionAccountStore>,
    gate: Mutex<()>,
    default_previews: Mutex<HashMap<Uuid, DefaultPreviewBinding>>,
    published_view_sequence: Mutex<Option<u64>>,
    views: broadcast::Sender<SubscriptionAccountCatalogView>,
}

impl SubscriptionAccountCoordinator {
    pub(crate) fn new(state: Arc<StateStore>, accounts: Arc<SubscriptionAccountStore>) -> Self {
        let (views, _) = broadcast::channel(64);
        Self {
            state,
            accounts,
            gate: Mutex::new(()),
            default_previews: Mutex::new(HashMap::new()),
            published_view_sequence: Mutex::new(None),
            views,
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SubscriptionAccountCatalogView> {
        self.views.subscribe()
    }

    pub(crate) async fn publish(&self, view: SubscriptionAccountCatalogView) {
        let _guard = self.gate.lock().await;
        let Ok(current) = self.catalog().await else {
            return;
        };
        let mut published = self.published_view_sequence.lock().await;
        if current.view_sequence != view.view_sequence
            || published.is_some_and(|sequence| sequence >= view.view_sequence)
        {
            return;
        }
        *published = Some(view.view_sequence);
        let _ = self.views.send(view);
    }

    #[cfg(test)]
    pub(crate) async fn record_authorization(
        &self,
        flow_id: Uuid,
        account: SubscriptionAccountRecord,
    ) -> Result<SubscriptionAccountCatalogView, SubscriptionAccountActionFailure> {
        match self
            .record_authorization_inner(flow_id, account, None)
            .await?
        {
            SubscriptionAuthorizationCommit::Committed(view) => Ok(view),
            SubscriptionAuthorizationCommit::Cancelled => {
                unreachable!("uncancellable authorization was cancelled")
            }
        }
    }

    pub(crate) async fn record_authorization_cancellable(
        &self,
        flow_id: Uuid,
        account: SubscriptionAccountRecord,
        cancellation: &SubscriptionAuthorizationCancellation,
    ) -> Result<SubscriptionAuthorizationCommit, SubscriptionAccountActionFailure> {
        self.record_authorization_inner(flow_id, account, Some(cancellation))
            .await
    }

    async fn record_authorization_inner(
        &self,
        flow_id: Uuid,
        account: SubscriptionAccountRecord,
        cancellation: Option<&SubscriptionAuthorizationCancellation>,
    ) -> Result<SubscriptionAuthorizationCommit, SubscriptionAccountActionFailure> {
        let _guard = self.gate.lock().await;
        let _commit_guard = match cancellation {
            Some(cancellation) => match cancellation.begin_commit().await {
                Some(guard) => Some(guard),
                None => return Ok(SubscriptionAuthorizationCommit::Cancelled),
            },
            None => None,
        };
        let before = self.accounts.read().map_err(|_| {
            self.empty_failure(
                "subscription-account-file-invalid",
                "Subscription Account state is unavailable",
            )
        })?;
        let mut desired = before.document.clone();
        let account_id = account.account_id.clone();
        desired.accounts.insert(account_id.clone(), account);
        if desired.default_account_id.is_none() {
            desired.default_account_id = Some(account_id);
        }
        self.commit_private_mutation(flow_id, "authorize-account", before, desired)
            .await
            .map(SubscriptionAuthorizationCommit::Committed)
    }

    pub(crate) async fn record_refresh(
        &self,
        action_id: Uuid,
        expected_account: &SubscriptionAccountRecord,
        refresh_token: Option<&str>,
        authenticated_at: Option<i64>,
        state: AccountAuthorizationState,
    ) -> Result<SubscriptionAccountCatalogView, SubscriptionAccountActionFailure> {
        let _guard = self.gate.lock().await;
        let before = self.accounts.read().map_err(|_| {
            self.empty_failure(
                "subscription-account-file-invalid",
                "Subscription Account state is unavailable",
            )
        })?;
        let Some(current_account) = before.document.accounts.get(&expected_account.account_id)
        else {
            return Err(self.empty_failure(
                "subscription-account-not-found",
                "Subscription Account does not exist",
            ));
        };
        if current_account != expected_account {
            return Err(self
                .failure(
                    before.document,
                    "stale-subscription-catalog-revision",
                    "Subscription Account state changed; refresh and retry",
                )
                .await);
        }
        let mut desired = before.document.clone();
        let desired_account = desired
            .accounts
            .get_mut(&expected_account.account_id)
            .expect("validated account disappeared from cloned document");
        if let Some(refresh_token) = refresh_token {
            desired_account.refresh_token = refresh_token.to_owned();
        }
        if let Some(authenticated_at) = authenticated_at {
            desired_account.authenticated_at = authenticated_at;
        }
        desired_account.state = state;
        self.commit_private_mutation(action_id, "refresh-account", before, desired)
            .await
    }

    async fn commit_private_mutation(
        &self,
        action_id: Uuid,
        operation: &'static str,
        before: SubscriptionAccountFileSnapshot,
        desired: super::accounts::SubscriptionAccountDocument,
    ) -> Result<SubscriptionAccountCatalogView, SubscriptionAccountActionFailure> {
        let initial = self
            .state
            .subscription_account_catalog(before.document.clone())
            .await
            .map_err(|_| {
                self.empty_failure(
                    "state-store-error",
                    "Subscription Account state is unavailable",
                )
            })?;
        if initial.recovery.state
            == crate::control::protocol::SubscriptionAccountRecoveryState::RecoveryRequired
        {
            return Err(self.empty_failure(
                "subscription-account-recovery-required",
                "Subscription Account writes are blocked until recovery is resolved",
            ));
        }
        let intent_id = Uuid::new_v4();
        let staged = self
            .accounts
            .stage_mutation(intent_id, action_id, operation, &before.document, &desired)
            .map_err(|_| {
                self.empty_failure(
                    "subscription-account-write-failed",
                    "Subscription Account recovery material could not be written",
                )
            })?;
        if self
            .state
            .begin_subscription_account_recovery(
                intent_id,
                action_id,
                operation,
                initial.revision,
                staged.before_sha256,
                staged.desired_sha256,
            )
            .await
            .is_err()
        {
            let _ = self.accounts.clear_staged_mutation(intent_id);
            return Err(self.empty_failure(
                "state-store-error",
                "Subscription Account recovery intent could not be written",
            ));
        }
        if self.accounts.replace(&before, &desired).is_err() {
            return match self.rollback_intent(intent_id, &before, &desired).await {
                Ok(failure) | Err(failure) => Err(failure),
            };
        }
        match self
            .state
            .record_subscription_account_change(desired.clone(), intent_id)
            .await
        {
            Ok(view) => {
                let _ = self.accounts.clear_staged_mutation(intent_id);
                Ok(view)
            }
            Err(_) => {
                self.rollback_intent(intent_id, &before, &desired).await?;
                Err(self
                    .failure(
                        before.document,
                        "state-store-error",
                        "Subscription Account state could not be updated",
                    )
                    .await)
            }
        }
    }

    pub(crate) async fn recover_pending_intents(
        &self,
    ) -> Result<(), SubscriptionAccountActionFailure> {
        let _guard = self.gate.lock().await;
        let staged = match self.accounts.read_staged_mutation() {
            Ok(staged) => staged,
            Err(_) => {
                self.state
                    .mark_subscription_account_recovery_required()
                    .await
                    .map_err(|_| {
                        self.empty_failure(
                            "subscription-account-recovery-required",
                            "Subscription Account recovery state could not be persisted",
                        )
                    })?;
                return Ok(());
            }
        };
        let pending = self
            .state
            .pending_subscription_account_recovery()
            .await
            .map_err(|_| {
                self.empty_failure(
                    "subscription-account-recovery-required",
                    "Subscription Account recovery state is invalid",
                )
            })?;
        match (pending, staged) {
            (None, None) => Ok(()),
            (None, Some(staged)) => {
                let state = self
                    .state
                    .subscription_account_recovery_state(staged.intent_id)
                    .await
                    .map_err(|_| {
                        self.empty_failure(
                            "subscription-account-recovery-required",
                            "Subscription Account recovery state is invalid",
                        )
                    })?;
                let current = self.accounts.read().map_err(|_| {
                    self.empty_failure(
                        "subscription-account-recovery-required",
                        "Subscription Account recovery could not inspect the private file",
                    )
                })?;
                if matches!(state.as_deref(), Some("committed" | "rolled-back"))
                    || (state.is_none() && current.document == staged.before)
                {
                    self.accounts
                        .clear_staged_mutation(staged.intent_id)
                        .map_err(|_| {
                            self.empty_failure(
                                "subscription-account-recovery-required",
                                "Subscription Account recovery material could not be cleared",
                            )
                        })?;
                    return Ok(());
                }
                self.state
                    .mark_subscription_account_recovery_required()
                    .await
                    .map_err(|_| {
                        self.empty_failure(
                            "subscription-account-recovery-required",
                            "Subscription Account recovery state could not be persisted",
                        )
                    })?;
                Ok(())
            }
            (Some(pending), None) => {
                let _ = self
                    .state
                    .finish_subscription_account_recovery(pending.intent_id, "recovery-required")
                    .await;
                Ok(())
            }
            (Some(pending), Some(staged)) => {
                if pending.intent_id != staged.intent_id
                    || pending.action_id != staged.action_id
                    || pending.operation != staged.operation
                    || pending.before_sha256 != staged.before_sha256
                    || pending.desired_sha256 != staged.desired_sha256
                {
                    let _ = self
                        .state
                        .finish_subscription_account_recovery(
                            pending.intent_id,
                            "recovery-required",
                        )
                        .await;
                    return Ok(());
                }
                let current = match self.accounts.read() {
                    Ok(current) => current,
                    Err(_) => {
                        let _ = self
                            .state
                            .finish_subscription_account_recovery(
                                pending.intent_id,
                                "recovery-required",
                            )
                            .await;
                        return Ok(());
                    }
                };
                if current.document == staged.desired {
                    if self.accounts.replace(&current, &staged.before).is_err() {
                        let _ = self
                            .state
                            .finish_subscription_account_recovery(
                                pending.intent_id,
                                "recovery-required",
                            )
                            .await;
                        return Ok(());
                    }
                } else if current.document != staged.before {
                    let _ = self
                        .state
                        .finish_subscription_account_recovery(
                            pending.intent_id,
                            "recovery-required",
                        )
                        .await;
                    return Ok(());
                }
                self.state
                    .finish_subscription_account_recovery(pending.intent_id, "rolled-back")
                    .await
                    .map_err(|_| {
                        self.empty_failure(
                            "subscription-account-recovery-required",
                            "Subscription Account recovery state could not be finalized",
                        )
                    })?;
                if self
                    .accounts
                    .clear_staged_mutation(pending.intent_id)
                    .is_err()
                {
                    self.state
                        .mark_subscription_account_recovery_required()
                        .await
                        .map_err(|_| {
                            self.empty_failure(
                                "subscription-account-recovery-required",
                                "Subscription Account recovery state could not be persisted",
                            )
                        })?;
                }
                Ok(())
            }
        }
    }

    pub(crate) async fn catalog(
        &self,
    ) -> Result<SubscriptionAccountCatalogView, SubscriptionAccountActionFailure> {
        let snapshot = self.accounts.read().map_err(|_| {
            self.empty_failure(
                "subscription-account-file-invalid",
                "Subscription Account state is unavailable",
            )
        })?;
        self.state
            .subscription_account_catalog(snapshot.document)
            .await
            .map_err(|_| {
                self.empty_failure(
                    "state-store-error",
                    "Subscription Account state is unavailable",
                )
            })
    }

    pub(crate) async fn preview_default(
        &self,
        account_id: &str,
    ) -> Result<SubscriptionDefaultPreview, SubscriptionAccountActionFailure> {
        let _guard = self.gate.lock().await;
        let snapshot = self.accounts.read().map_err(|_| {
            self.empty_failure(
                "subscription-account-file-invalid",
                "Subscription Account state is unavailable",
            )
        })?;
        let Some(next_account) = snapshot.document.accounts.get(account_id) else {
            return Err(self
                .failure(
                    snapshot.document,
                    "subscription-account-not-found",
                    "Subscription Account does not exist",
                )
                .await);
        };
        let view = self
            .state
            .subscription_account_catalog(snapshot.document.clone())
            .await
            .map_err(|_| {
                self.empty_failure(
                    "state-store-error",
                    "Subscription Account state is unavailable",
                )
            })?;
        let next_resolution = match next_account.state {
            AccountAuthorizationState::Authorized => SubscriptionBindingResolutionState::Available,
            AccountAuthorizationState::NeedsReauthorization => {
                SubscriptionBindingResolutionState::NeedsReauthorization
            }
        };
        let effects = view
            .bindings
            .iter()
            .filter(|binding| matches!(binding.binding, SubscriptionProviderBinding::FollowDefault))
            .map(|binding| SubscriptionDefaultEffect {
                target: binding.target,
                provider_id: binding.provider_id,
                provider_revision: binding.provider_revision,
                provider_name: binding.provider_name.clone(),
                current_account_id: binding.resolution.account_id.clone(),
                next_account_id: Some(account_id.to_owned()),
                next_resolution,
            })
            .collect();
        let preview_token = Uuid::new_v4();
        self.default_previews.lock().await.insert(
            preview_token,
            DefaultPreviewBinding {
                account_id: account_id.to_owned(),
                revision: view.revision,
            },
        );
        Ok(SubscriptionDefaultPreview {
            preview_token,
            account_id: account_id.to_owned(),
            effects,
        })
    }

    pub(crate) async fn apply(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        action: SubscriptionAccountAction,
    ) -> Result<SubscriptionAccountOutcome, SubscriptionAccountActionFailure> {
        let _guard = self.gate.lock().await;
        match self
            .state
            .subscription_account_receipt(action_id, &action)
            .await
        {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) => {
                let snapshot = self.accounts.read().ok();
                return Err(match snapshot {
                    Some(snapshot) => {
                        self.failure(
                            snapshot.document,
                            "invalid-action-replay",
                            "Action identifier was already used for a different request",
                        )
                        .await
                    }
                    None => self.empty_failure(
                        "state-store-error",
                        "Subscription Account state is unavailable",
                    ),
                });
            }
        }
        let before = self.accounts.read().map_err(|_| {
            self.empty_failure(
                "subscription-account-file-invalid",
                "Subscription Account state is unavailable",
            )
        })?;
        let initial_view = self
            .state
            .subscription_account_catalog(before.document.clone())
            .await
            .map_err(|_| {
                self.empty_failure(
                    "state-store-error",
                    "Subscription Account state is unavailable",
                )
            })?;
        if initial_view.recovery.state
            == crate::control::protocol::SubscriptionAccountRecoveryState::RecoveryRequired
        {
            return Err(self
                .failure(
                    before.document,
                    "subscription-account-recovery-required",
                    "Subscription Account writes are blocked until recovery is resolved",
                )
                .await);
        }
        if initial_view.revision != expected_revision {
            return Err(self
                .failure(
                    before.document,
                    "stale-subscription-catalog-revision",
                    "Subscription Account state changed; refresh and retry",
                )
                .await);
        }
        let mut desired = before.document.clone();
        let mut file_changed = false;
        match &action {
            SubscriptionAccountAction::SetDefaultAccount {
                account_id,
                preview_token,
            } => {
                let previews = self.default_previews.lock().await;
                let valid = previews.get(preview_token).is_some_and(|preview| {
                    preview.account_id == *account_id && preview.revision == expected_revision
                });
                if !valid || !desired.accounts.contains_key(account_id) {
                    return Err(self
                        .failure(
                            desired,
                            "stale-default-account-preview",
                            "Default Account preview changed; preview and retry",
                        )
                        .await);
                }
                desired.default_account_id = Some(account_id.clone());
                file_changed = desired != before.document;
            }
            SubscriptionAccountAction::DeleteAccount { account_id } => {
                if desired.accounts.remove(account_id).is_none() {
                    return Err(self
                        .failure(
                            desired,
                            "subscription-account-not-found",
                            "Subscription Account does not exist",
                        )
                        .await);
                }
                if desired.default_account_id.as_deref() == Some(account_id.as_str()) {
                    desired.default_account_id = desired
                        .accounts
                        .iter()
                        .min_by(|(left_id, left), (right_id, right)| {
                            right
                                .authenticated_at
                                .cmp(&left.authenticated_at)
                                .then_with(|| left_id.cmp(right_id))
                        })
                        .map(|(identity, _)| identity.clone());
                }
                file_changed = true;
            }
            SubscriptionAccountAction::BindProviderFixed { .. }
            | SubscriptionAccountAction::BindProviderFollowDefault { .. } => {}
        }
        let recovery_intent_id = file_changed.then(Uuid::new_v4);
        if let Some(intent_id) = recovery_intent_id {
            let operation = subscription_action_operation(&action);
            let staged = self
                .accounts
                .stage_mutation(intent_id, action_id, operation, &before.document, &desired)
                .map_err(|_| {
                    self.empty_failure(
                        "subscription-account-write-failed",
                        "Subscription Account recovery material could not be written",
                    )
                })?;
            if self
                .state
                .begin_subscription_account_recovery(
                    intent_id,
                    action_id,
                    operation,
                    expected_revision,
                    staged.before_sha256,
                    staged.desired_sha256,
                )
                .await
                .is_err()
            {
                let _ = self.accounts.clear_staged_mutation(intent_id);
                return Err(self
                    .failure(
                        before.document,
                        "state-store-error",
                        "Subscription Account recovery intent could not be written",
                    )
                    .await);
            }
            if self.accounts.replace(&before, &desired).is_err() {
                let failure = match self.rollback_intent(intent_id, &before, &desired).await {
                    Ok(failure) | Err(failure) => failure,
                };
                return Err(failure);
            }
        }
        let result = self
            .state
            .commit_subscription_account_action(
                action_id,
                expected_revision,
                action.clone(),
                desired.clone(),
                recovery_intent_id,
            )
            .await;
        match result {
            Ok(Ok(outcome)) => {
                if let SubscriptionAccountAction::SetDefaultAccount { preview_token, .. } = action {
                    self.default_previews.lock().await.remove(&preview_token);
                }
                if let Some(intent_id) = recovery_intent_id {
                    let _ = self.accounts.clear_staged_mutation(intent_id);
                }
                Ok(outcome)
            }
            Ok(Err(failure)) => {
                if let Some(intent_id) = recovery_intent_id {
                    self.rollback_intent(intent_id, &before, &desired).await?;
                }
                Err(failure)
            }
            Err(_) => {
                if let Some(intent_id) = recovery_intent_id {
                    self.rollback_intent(intent_id, &before, &desired).await?;
                }
                Err(self
                    .failure(
                        before.document,
                        "state-store-error",
                        "Subscription Account state could not be updated",
                    )
                    .await)
            }
        }
    }

    pub(crate) async fn apply_raw(
        &self,
        action_id: Uuid,
        expected_revision: u64,
        action: serde_json::Value,
    ) -> Result<SubscriptionAccountOutcome, SubscriptionAccountActionFailure> {
        match self
            .state
            .subscription_account_receipt_by_id(action_id)
            .await
        {
            Ok(Some(outcome)) => return Ok(outcome),
            Ok(None) => {}
            Err(_) => {
                return Err(self.empty_failure(
                    "state-store-error",
                    "Subscription Account receipt could not be read",
                ));
            }
        }
        let action = serde_json::from_value(action).map_err(|_| {
            self.empty_failure(
                "invalid-request",
                "Subscription Account action is malformed",
            )
        })?;
        self.apply(action_id, expected_revision, action).await
    }

    async fn rollback_intent(
        &self,
        intent_id: Uuid,
        before: &super::accounts::SubscriptionAccountFileSnapshot,
        desired: &super::accounts::SubscriptionAccountDocument,
    ) -> Result<SubscriptionAccountActionFailure, SubscriptionAccountActionFailure> {
        let current = self.accounts.read().map_err(|_| {
            self.empty_failure(
                "subscription-account-write-failed",
                "Subscription Account rollback failed",
            )
        })?;
        if current.document != *desired && current.document != before.document {
            let _ = self
                .state
                .finish_subscription_account_recovery(intent_id, "recovery-required")
                .await;
            return Err(self.empty_failure(
                "subscription-account-recovery-required",
                "Subscription Account rollback found an external file change",
            ));
        }
        if current.document == before.document {
            self.state
                .finish_subscription_account_recovery(intent_id, "rolled-back")
                .await
                .map_err(|_| {
                    self.empty_failure(
                        "subscription-account-recovery-required",
                        "Subscription Account rollback state could not be finalized",
                    )
                })?;
            self.accounts
                .clear_staged_mutation(intent_id)
                .map_err(|_| {
                    self.empty_failure(
                        "subscription-account-recovery-required",
                        "Subscription Account rollback material could not be cleared",
                    )
                })?;
            return Ok(self.empty_failure(
                "subscription-account-write-failed",
                "Subscription Account file could not be updated",
            ));
        }
        self.accounts
            .replace(&current, &before.document)
            .map_err(|_| {
                self.empty_failure(
                    "subscription-account-write-failed",
                    "Subscription Account rollback failed",
                )
            })?;
        self.state
            .finish_subscription_account_recovery(intent_id, "rolled-back")
            .await
            .map_err(|_| {
                self.empty_failure(
                    "subscription-account-recovery-required",
                    "Subscription Account rollback state could not be finalized",
                )
            })?;
        self.accounts
            .clear_staged_mutation(intent_id)
            .map_err(|_| {
                self.empty_failure(
                    "subscription-account-recovery-required",
                    "Subscription Account rollback material could not be cleared",
                )
            })?;
        Ok(self.empty_failure(
            "state-store-error",
            "Subscription Account state could not be updated",
        ))
    }

    async fn failure(
        &self,
        document: super::accounts::SubscriptionAccountDocument,
        code: &str,
        message: &str,
    ) -> SubscriptionAccountActionFailure {
        let authoritative_view = self
            .state
            .subscription_account_catalog(document)
            .await
            .unwrap_or_else(|_| self.empty_failure(code, message).authoritative_view);
        SubscriptionAccountActionFailure {
            problem: ControlProblem {
                code: code.to_owned(),
                message: message.to_owned(),
                source: None,
                selector: None,
            },
            authoritative_view,
        }
    }

    fn empty_failure(&self, code: &str, message: &str) -> SubscriptionAccountActionFailure {
        SubscriptionAccountActionFailure {
            problem: ControlProblem {
                code: code.to_owned(),
                message: message.to_owned(),
                source: None,
                selector: None,
            },
            authoritative_view: SubscriptionAccountCatalogView {
                revision: 0,
                view_sequence: 0,
                default_account_id: None,
                accounts: Vec::new(),
                bindings: Vec::new(),
                recovery: crate::control::protocol::SubscriptionAccountRecoveryView {
                    state:
                        crate::control::protocol::SubscriptionAccountRecoveryState::RecoveryRequired,
                },
            },
        }
    }
}

fn subscription_action_operation(action: &SubscriptionAccountAction) -> &'static str {
    match action {
        SubscriptionAccountAction::SetDefaultAccount { .. } => "set-default-account",
        SubscriptionAccountAction::BindProviderFixed { .. } => "bind-provider-fixed",
        SubscriptionAccountAction::BindProviderFollowDefault { .. } => {
            "bind-provider-follow-default"
        }
        SubscriptionAccountAction::DeleteAccount { .. } => "delete-account",
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{
        SubscriptionAccountCoordinator, SubscriptionAuthorizationCancellation,
        SubscriptionAuthorizationCommit,
    };
    use crate::{
        control::protocol::{
            ActionStatus, SubscriptionAccountAction, SubscriptionProviderBinding, Target,
        },
        home::MuxviaHome,
        state::StateStore,
        subscription::{
            AccountAuthorizationState, SubscriptionAccountDocument, SubscriptionAccountStore,
            accounts::SubscriptionAccountRecord,
        },
    };

    #[tokio::test]
    async fn default_preview_is_revision_bound_and_replay_is_receipt_first() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_accounts(&accounts);
        let provider_outcome = state
            .apply_provider_action_for(
                Target::Codex,
                Uuid::new_v4(),
                0,
                serde_json::json!({
                    "kind": "create-provider",
                    "name": "Subscription metadata",
                    "baseUrl": "https://example.test/v1",
                    "model": "subscription-model",
                    "credential": {"kind": "replace", "value": "PROVIDER_SECRET_11791"},
                    "authentication": "openai-bearer",
                    "presetKey": null
                }),
            )
            .await
            .expect("provider fixture");
        let provider = provider_outcome
            .view
            .providers
            .first()
            .expect("provider fixture");
        let coordinator = SubscriptionAccountCoordinator::new(state, accounts);
        let missing = coordinator
            .preview_default("account-missing")
            .await
            .expect_err("missing account produced a default preview");
        assert!(
            missing.problem.code == "subscription-account-not-found",
            "missing account did not use the stable Subscription Account problem code"
        );
        let follow = SubscriptionAccountAction::BindProviderFollowDefault {
            target: Target::Codex,
            provider_id: provider.id,
            provider_revision: provider.provider_revision,
        };
        let bound = coordinator
            .apply(Uuid::new_v4(), 0, follow)
            .await
            .expect("bind follow default");
        assert!(bound.view.revision == 1, "binding revision did not advance");

        let preview = coordinator
            .preview_default("account-secondary")
            .await
            .expect("default preview");
        assert!(
            preview.effects.len() == 1
                && preview.effects[0].provider_id == provider.id
                && preview.effects[0].current_account_id.as_deref() == Some("account-primary")
                && preview.effects[0].next_account_id.as_deref() == Some("account-secondary"),
            "default preview did not disclose the old and new resolved account identities"
        );
        let action = SubscriptionAccountAction::SetDefaultAccount {
            account_id: "account-secondary".to_owned(),
            preview_token: preview.preview_token,
        };
        let stale_preview = coordinator
            .apply(
                Uuid::new_v4(),
                1,
                SubscriptionAccountAction::SetDefaultAccount {
                    account_id: "account-secondary".to_owned(),
                    preview_token: Uuid::new_v4(),
                },
            )
            .await
            .expect_err("unbound default preview token was accepted");
        assert!(
            stale_preview.problem.code == "stale-default-account-preview",
            "stale default preview did not use the stable problem code"
        );
        let action_id = Uuid::new_v4();
        let applied = coordinator
            .apply(action_id, 1, action.clone())
            .await
            .expect("set default");
        assert!(
            applied.status == ActionStatus::Applied
                && applied.view.revision == 2
                && applied.view.default_account_id.as_deref() == Some("account-secondary"),
            "default action did not commit its preview"
        );
        let replay = coordinator
            .apply(action_id, 999, action)
            .await
            .expect("receipt replay");
        assert!(
            replay.status == ActionStatus::Replayed && replay.view == applied.view,
            "receipt replay revalidated revision or changed the outcome"
        );
        assert!(
            replay.view.bindings[0].binding == SubscriptionProviderBinding::FollowDefault,
            "default change rewrote binding metadata"
        );
    }

    #[tokio::test]
    async fn cancellation_while_authorization_waits_for_the_account_gate_commits_nothing() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        let coordinator = Arc::new(SubscriptionAccountCoordinator::new(state, accounts.clone()));
        let cancellation = SubscriptionAuthorizationCancellation::new();
        let held_gate = coordinator.gate.lock().await;
        let commit = tokio::spawn({
            let coordinator = coordinator.clone();
            let cancellation = cancellation.clone();
            async move {
                coordinator
                    .record_authorization_cancellable(
                        Uuid::new_v4(),
                        SubscriptionAccountRecord {
                            account_id: "account-cancelled".to_owned(),
                            email: None,
                            refresh_token: "CANCELLED_REFRESH_SECRET_11841".to_owned(),
                            authenticated_at: 1,
                            state: AccountAuthorizationState::Authorized,
                        },
                        &cancellation,
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        cancellation.cancel().await;
        drop(held_gate);

        let result = commit.await.expect("authorization commit task");
        assert!(
            matches!(result, Ok(SubscriptionAuthorizationCommit::Cancelled)),
            "cancelled authorization waiting on the account gate was committed"
        );
        let catalog = coordinator.catalog().await.expect("cancelled catalog");
        assert!(
            catalog.revision == 0
                && catalog.view_sequence == 0
                && catalog.accounts.is_empty()
                && !home.subscription_accounts_path().exists(),
            "cancelled authorization mutated the account catalog or private file"
        );
    }

    #[tokio::test]
    async fn startup_recovers_a_pending_private_file_write_without_exposing_tokens_to_sqlite() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_accounts(&accounts);
        let before = accounts.read().expect("before account file");
        let mut desired = before.document.clone();
        desired.accounts.remove("account-secondary");
        let action_id = Uuid::new_v4();
        let intent_id = Uuid::new_v4();
        let staged = accounts
            .stage_mutation(
                intent_id,
                action_id,
                "delete-account",
                &before.document,
                &desired,
            )
            .expect("stage private account recovery");
        state
            .begin_subscription_account_recovery(
                intent_id,
                action_id,
                "delete-account",
                0,
                staged.before_sha256,
                staged.desired_sha256,
            )
            .await
            .expect("begin SQLite recovery intent");
        accounts
            .replace(&before, &desired)
            .expect("simulate crash after private file write");

        let coordinator =
            SubscriptionAccountCoordinator::new(Arc::clone(&state), Arc::clone(&accounts));
        coordinator
            .recover_pending_intents()
            .await
            .expect("recover pending account mutation");
        let restored = accounts.read().expect("restored account file");
        assert!(
            restored.document == before.document,
            "startup recovery did not restore the exact prior account document"
        );
        assert!(
            accounts
                .read_staged_mutation()
                .expect("read staged mutation")
                .is_none(),
            "startup recovery left private mutation material behind"
        );
        let sqlite = std::fs::read(home.database_path()).expect("read SQLite file");
        for token in [
            "REFRESH_SECRET_account-primary",
            "REFRESH_SECRET_account-secondary",
        ] {
            assert!(
                !String::from_utf8_lossy(&sqlite).contains(token),
                "SQLite contained private Subscription Account material"
            );
        }
    }

    #[tokio::test]
    async fn deleting_the_default_selects_the_newest_remaining_account_deterministically() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_accounts(&accounts);
        let coordinator = SubscriptionAccountCoordinator::new(state, accounts);
        let deleted = coordinator
            .apply(
                Uuid::new_v4(),
                0,
                SubscriptionAccountAction::DeleteAccount {
                    account_id: "account-primary".to_owned(),
                },
            )
            .await
            .expect("delete default account");
        assert!(
            deleted.view.default_account_id.as_deref() == Some("account-secondary"),
            "default deletion did not select the deterministic remaining account"
        );
    }

    #[tokio::test]
    async fn missing_private_recovery_material_marks_only_the_account_catalog_recovery_required() {
        let temp = TempDir::new().expect("temporary home");
        let home = MuxviaHome::from_user_home(temp.path());
        let state = Arc::new(StateStore::open(&home).await.expect("state store"));
        let accounts = Arc::new(SubscriptionAccountStore::open(&home).expect("account store"));
        install_accounts(&accounts);
        state
            .begin_subscription_account_recovery(
                Uuid::new_v4(),
                Uuid::new_v4(),
                "delete-account",
                0,
                "missing-before-hash".to_owned(),
                "missing-desired-hash".to_owned(),
            )
            .await
            .expect("begin incomplete recovery intent");
        let coordinator =
            SubscriptionAccountCoordinator::new(Arc::clone(&state), Arc::clone(&accounts));
        coordinator
            .recover_pending_intents()
            .await
            .expect("fail closed without blocking unrelated service startup");
        let view = coordinator
            .catalog()
            .await
            .expect("recovery-required catalog");
        assert!(
            view.recovery.state
                == crate::control::protocol::SubscriptionAccountRecoveryState::RecoveryRequired,
            "missing recovery material did not persist account-local Recovery Required"
        );
        let blocked = coordinator
            .apply(
                Uuid::new_v4(),
                view.revision,
                SubscriptionAccountAction::DeleteAccount {
                    account_id: "account-secondary".to_owned(),
                },
            )
            .await
            .expect_err("Recovery Required allowed a Subscription Account mutation");
        assert!(
            blocked.problem.code == "subscription-account-recovery-required",
            "Recovery Required did not use the stable Subscription Account problem code"
        );
    }

    fn install_accounts(store: &SubscriptionAccountStore) {
        let snapshot = store.read().expect("empty account file");
        let accounts = ["account-primary", "account-secondary"]
            .into_iter()
            .map(|account_id| {
                (
                    account_id.to_owned(),
                    SubscriptionAccountRecord {
                        account_id: account_id.to_owned(),
                        email: None,
                        refresh_token: format!("REFRESH_SECRET_{account_id}"),
                        authenticated_at: 1,
                        state: AccountAuthorizationState::Authorized,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        store
            .replace(
                &snapshot,
                &SubscriptionAccountDocument {
                    version: 1,
                    accounts,
                    default_account_id: Some("account-primary".to_owned()),
                },
            )
            .expect("account fixture");
    }
}
