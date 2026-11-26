pub mod commands;
pub mod config;
#[cfg(test)]
mod contract_tests;
pub mod host;
pub mod manifest;
pub mod registry;

// Engine-facing modules are implemented alongside the scheduler integration.
pub mod client;
pub mod routing;

pub use manifest::{PluginAction, PluginManifest};
pub use registry::{PluginInstance, PluginSummary};
