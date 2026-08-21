mod migrations;
pub(crate) mod providers;
mod reconciliation;
mod recovery;
// Task 3 consumes this crate-internal seam from the routed response recorder.
#[allow(dead_code)]
mod request_records;
mod store;
mod subscription_accounts;
mod universal_providers;

pub use crate::control::protocol::CompatibilityClassification;
pub use migrations::SCHEMA_VERSION;
pub(crate) use reconciliation::{
    AdoptReconciliation, ReconciliationCommit, ReconciliationCommitFailpoint,
    ReconciliationCommitInput,
};
pub use recovery::{ManagedWriteStatus, RecoveryIntent, RecoveryPayload, RecoveryState};
pub use store::{
    ActionFailure, ActivationCommit, ActivationPreparation, ActivationRuntime,
    CommittedActivationSnapshot, CommittedRouteRuntime, CommittedTakeover, RoutingSnapshot,
    StateError, StateStore,
};
pub(crate) use store::{ActivatedRoutePlanSnapshot, RouteObservation, RoutePlanMemberSnapshot};
pub(crate) use subscription_accounts::SubscriptionAccountActionFailure;
pub use universal_providers::{
    UniversalProviderActionFailure, UniversalProviderSynchronizationCommit,
    UniversalSynchronizationFailpoint,
};
