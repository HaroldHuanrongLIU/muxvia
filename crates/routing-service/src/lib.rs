#![allow(
    clippy::result_large_err,
    reason = "action failures intentionally carry the authoritative projection required for recovery"
)]

pub mod claude;
pub mod codex;
mod config;
pub mod control;
pub mod domain;
pub mod home;
pub mod model;
mod native_usage;
mod probe_process;
pub mod release_bundle;
mod request_history;
pub mod service;
pub mod state;
mod subscription;
mod subscription_bridge;
