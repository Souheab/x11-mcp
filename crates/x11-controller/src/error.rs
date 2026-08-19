use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, ControllerError>;

#[derive(Debug, Clone, Error, Serialize, Deserialize, JsonSchema)]
#[error("{message}")]
pub struct ControllerError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    StaleWindow,
    StaleFrame,
    StaleElement,
    PreconditionFailed,
    UnsupportedCapability,
    Accessibility,
    Timeout,
    RateLimited,
    EmergencyStop,
    AccessDenied,
    X11,
    Internal,
}

impl ControllerError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: false,
            details: None,
        }
    }

    #[must_use]
    pub const fn retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    #[must_use]
    pub fn x11(context: &str, error: impl std::fmt::Display) -> Self {
        Self::new(ErrorCode::X11, format!("{context}: {error}"))
    }
}
