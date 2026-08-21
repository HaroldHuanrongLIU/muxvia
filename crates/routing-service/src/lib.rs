pub mod claude;
pub mod codex;
mod config;
pub mod control;
pub mod domain;
pub mod home;
pub mod model;
mod probe_process;
// Task 3 consumes this crate-internal seam from the routed response recorder.
#[allow(dead_code)]
mod request_history;
pub mod service;
pub mod state;
mod subscription;
mod subscription_bridge;
