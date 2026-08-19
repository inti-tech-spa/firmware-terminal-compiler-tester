use serde::{Serialize, Serializer, ser::SerializeStruct};
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

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct SamdebugError {
    code: String,
    message: String,
    category: ErrorCategory,
    details: Option<serde_json::Value>,
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
            category,
            details: None,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        self.category
    }

    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        self.category.exit_code()
    }
}

impl Serialize for SamdebugError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let field_count = if self.details.is_some() { 4 } else { 3 };
        let mut state = serializer.serialize_struct("SamdebugError", field_count)?;
        state.serialize_field("code", &self.code)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("exit_code", &self.exit_code())?;
        if let Some(details) = &self.details {
            state.serialize_field("details", details)?;
        }
        state.end()
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
