pub mod claude;
pub mod codex;
mod config;
pub mod control;
pub mod domain;
pub mod home;
pub mod model;
mod probe_process;
pub mod service;
pub mod state;
mod subscription;
// Task 4 wires the completed Task 2 codec into the Claude listener.
#[allow(dead_code)]
mod subscription_bridge;
