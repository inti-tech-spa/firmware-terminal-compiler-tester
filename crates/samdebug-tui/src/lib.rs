//! Human TUI boundary. Ratatui implementation is milestone M7.

use samdebug_core::{ErrorCategory, SamdebugError, SamdebugResult};

pub fn run() -> SamdebugResult<()> {
    Err(SamdebugError::new(
        ErrorCategory::Debugger,
        "TUI_NOT_IMPLEMENTED",
        "the debug TUI is introduced in milestone M7",
    ))
}
