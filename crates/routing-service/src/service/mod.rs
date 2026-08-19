pub mod activate;
pub mod process;
pub mod provider_inspector;
// Task 5 wires the completed coordinator into the independent catalog session.
#[allow(dead_code)]
pub(crate) mod provider_synchronization;
pub(crate) mod reconcile;
// Task 4 consumes the prepared target-native values exposed by this seam.
#[allow(dead_code)]
pub(crate) mod reconciliation_adapter;
