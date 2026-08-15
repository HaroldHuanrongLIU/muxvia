mod config;
mod probe;

pub(crate) use config::ClaudeConfigOwnership;
// Consumed when Claude Direct is enabled in the activation transaction.
#[allow(unused_imports)]
pub(crate) use config::ManagedClaudeState;
pub use config::{
    ClaudeConfigCodec, ClaudeConfigSnapshot, ClaudePreflightReport, ClaudeRuntimeShadow,
    DesiredClaudeState, OwnedClaudeState,
};
pub use probe::{ClaudeCapability, ClaudeProbe, ClaudeProblem, CommandClaudeProbe};
