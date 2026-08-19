//! Shared domain contracts for all samdebug frontends and adapters.

mod cancellation;
mod config;
mod error;
mod output;
pub mod ports;

pub use cancellation::CancellationToken;
pub use config::{
    Configuration, LoadedConfig, ProbeConfig, ProjectConfig, SamdebugConfig, ToolConfig,
};
pub use error::{ErrorCategory, SamdebugError, SamdebugResult};
pub use output::{Diagnostic, FiniteResult};

/// Stable machine-protocol schema version.
pub const SCHEMA_VERSION: u32 = 1;
