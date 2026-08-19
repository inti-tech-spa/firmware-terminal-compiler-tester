use serde::Serialize;

use crate::{SCHEMA_VERSION, SamdebugError};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// Finite command envelope. Its private representation prevents contradictory
/// `ok`, schema-version, data, and error combinations.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct FiniteResult<T: Serialize>(FiniteResultKind<T>);

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
enum FiniteResultKind<T: Serialize> {
    Success {
        schema_version: u32,
        ok: bool,
        command: String,
        data: T,
        warnings: Vec<Diagnostic>,
    },
    Failure {
        schema_version: u32,
        ok: bool,
        command: String,
        error: SamdebugError,
        warnings: Vec<Diagnostic>,
    },
}

impl<T: Serialize> FiniteResult<T> {
    #[must_use]
    pub fn success(command: impl Into<String>, data: T) -> Self {
        Self(FiniteResultKind::Success {
            schema_version: SCHEMA_VERSION,
            ok: true,
            command: command.into(),
            data,
            warnings: Vec::new(),
        })
    }

    #[must_use]
    pub fn failure(command: impl Into<String>, error: SamdebugError) -> Self {
        Self(FiniteResultKind::Failure {
            schema_version: SCHEMA_VERSION,
            ok: false,
            command: command.into(),
            error,
            warnings: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::FiniteResult;
    use crate::{ErrorCategory, SamdebugError};

    #[test]
    fn success_has_data_and_no_error() {
        let value = serde_json::to_value(FiniteResult::success("version", json!({"v": 1})))
            .expect("serialize success");
        assert_eq!(value["ok"], true);
        assert!(value.get("data").is_some());
        assert!(value.get("error").is_none());
    }

    #[test]
    fn failure_has_error_and_no_data() {
        let error = SamdebugError::new(ErrorCategory::Command, "BAD", "bad");
        let value = serde_json::to_value(FiniteResult::<serde_json::Value>::failure("x", error))
            .expect("serialize failure");
        assert_eq!(value["ok"], false);
        assert!(value.get("error").is_some());
        assert!(value.get("data").is_none());
    }
}
