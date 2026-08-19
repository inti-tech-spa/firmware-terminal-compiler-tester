use serde::Serialize;
use thiserror::Error;

pub type SamdebugResult<T> = Result<T, SamdebugError>;

/// Stable exit-code categories promised by the CLI contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Command,
    Tool,
    Project,
    Connection,
    Programming,
    Debugger,
    Authorization,
    Interrupted,
}

impl ErrorCategory {
    #[must_use]
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Command => 2,
            Self::Tool => 3,
            Self::Project => 4,
            Self::Connection => 5,
            Self::Programming => 6,
            Self::Debugger => 7,
            Self::Authorization => 8,
            Self::Interrupted => 130,
        }
    }
}

#[derive(Debug, Clone, Error, Serialize)]
#[error("{message}")]
pub struct SamdebugError {
    pub code: String,
    pub message: String,
    pub exit_code: i32,
    #[serde(skip)]
    pub category: ErrorCategory,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl SamdebugError {
    #[must_use]
    pub fn new(
        category: ErrorCategory,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            exit_code: category.exit_code(),
            category,
            details: None,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorCategory, SamdebugError};

    #[test]
    fn category_exit_codes_match_contract() {
        assert_eq!(ErrorCategory::Command.exit_code(), 2);
        assert_eq!(ErrorCategory::Tool.exit_code(), 3);
        assert_eq!(ErrorCategory::Project.exit_code(), 4);
        assert_eq!(ErrorCategory::Connection.exit_code(), 5);
        assert_eq!(ErrorCategory::Programming.exit_code(), 6);
        assert_eq!(ErrorCategory::Debugger.exit_code(), 7);
        assert_eq!(ErrorCategory::Authorization.exit_code(), 8);
        assert_eq!(ErrorCategory::Interrupted.exit_code(), 130);
    }

    #[test]
    fn error_serialization_uses_stable_shape() {
        let error = SamdebugError::new(ErrorCategory::Tool, "TOOL_MISSING", "missing");
        let value = serde_json::to_value(error).expect("serialize error");
        assert_eq!(value["exit_code"], 3);
        assert_eq!(value["code"], "TOOL_MISSING");
        assert!(value.get("category").is_none());
    }
}
