use std::sync::Arc;

use uuid::Uuid;

use crate::{
    control::protocol::{ActionOutcome, ActionStatus, ControlProblem, FailoverDraftMember, Target},
    state::{ActionFailure, StateStore},
};

use super::{
    reconcile::{DeferredPublication, ReconciliationService},
    reconciliation_adapter::ReconciliationContext,
};

/// Owns the complete Target mutation ordering for Failover Chain draft and Apply actions.
pub(crate) struct RoutePlanCoordinator {
    store: Arc<StateStore>,
    reconciliation: Arc<ReconciliationService>,
}

impl RoutePlanCoordinator {
    pub(crate) fn new(store: Arc<StateStore>, reconciliation: Arc<ReconciliationService>) -> Self {
        Self {
            store,
            reconciliation,
        }
    }

    pub(crate) async fn save_draft(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        members: Vec<FailoverDraftMember>,
    ) -> DeferredPublication<Result<ActionOutcome, ActionFailure>> {
        if let Some(replayed) = self.receipt(target, action_id).await {
            return replayed;
        }
        let _gate = self.reconciliation.lock_target_mutation(target).await;
        let result = self
            .store
            .save_failover_draft_for(target, action_id, expected_revision, members)
            .await;
        deferred(result)
    }

    pub(crate) async fn apply(
        &self,
        target: Target,
        action_id: Uuid,
        expected_revision: u64,
        expected_draft_revision: u64,
        context: Option<ReconciliationContext>,
    ) -> DeferredPublication<Result<ActionOutcome, ActionFailure>> {
        if let Some(replayed) = self.receipt(target, action_id).await {
            return replayed;
        }
        let _gate = self.reconciliation.lock_target_mutation(target).await;
        if let Some(replayed) = self.receipt(target, action_id).await {
            return replayed;
        }
        let authoritative = match self.store.target_view_for(target).await {
            Ok(view) => view,
            Err(_) => {
                return deferred(Err(self
                    .store
                    .failure_for(target, "state-store-error", "State store unavailable")
                    .await));
            }
        };
        if authoritative.management_revision != expected_revision {
            return deferred(Err(ActionFailure {
                problem: ControlProblem {
                    code: "stale-revision".to_owned(),
                    message: "Target revision is stale".to_owned(),
                    source: None,
                    selector: None,
                },
                authoritative_view: authoritative,
            }));
        }
        if authoritative.failover.draft_revision != expected_draft_revision {
            return deferred(Err(ActionFailure {
                problem: ControlProblem {
                    code: "stale-failover-draft-revision".to_owned(),
                    message: "Failover Chain draft revision is stale".to_owned(),
                    source: None,
                    selector: None,
                },
                authoritative_view: authoritative,
            }));
        }
        let allowed = self
            .reconciliation
            .ensure_ordinary_write_allowed(target, context, false)
            .await;
        if let Err(failure) = allowed.result {
            return DeferredPublication {
                result: Err(failure),
                publication: allowed.publication,
            };
        }
        let result = self
            .store
            .apply_failover_chain_for(
                target,
                action_id,
                expected_revision,
                expected_draft_revision,
            )
            .await;
        deferred(result)
    }

    async fn receipt(
        &self,
        target: Target,
        action_id: Uuid,
    ) -> Option<DeferredPublication<Result<ActionOutcome, ActionFailure>>> {
        match self.store.receipt_for(target, action_id).await {
            Ok(Some(outcome)) => Some(DeferredPublication {
                result: Ok(outcome),
                publication: None,
            }),
            Ok(None) => None,
            Err(_) => Some(DeferredPublication {
                result: Err(self
                    .store
                    .failure_for(target, "state-store-error", "State store unavailable")
                    .await),
                publication: None,
            }),
        }
    }
}

fn deferred(
    result: Result<ActionOutcome, ActionFailure>,
) -> DeferredPublication<Result<ActionOutcome, ActionFailure>> {
    let publication = result.as_ref().ok().and_then(|outcome| {
        (outcome.status == ActionStatus::Applied).then(|| outcome.view.clone())
    });
    DeferredPublication {
        result,
        publication,
    }
}
