use serde::Serialize;
use thiserror::Error;
use crate::security::SecurityError;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Security violation: {0}")]
    Security(#[from] SecurityError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Gateway: Token expired or invalid — {0}")]
    TokenInvalid(String),
    #[error("Gateway: Device fingerprint mismatch — possible token theft attempt")]
    DeviceMismatch,
    #[error("Gateway: Request scope violation — {0}")]
    ScopeViolation(String),
    #[error("Gateway: Rate limit exceeded — {0}")]
    RateLimitExceeded(String),
    #[error("Gateway: Daily quota exceeded — {0}")]
    QuotaExceeded(String),
    /// Non-2xx response from an upstream LLM API. `status` lets callers decide
    /// whether a retry (401/429/5xx) or a forced credential refresh (401) is
    /// worth attempting before surfacing the error to the user.
    #[error("API error ({status}): {message}")]
    Api { status: u16, message: String },
    #[error("General error: {0}")]
    General(String),
}

// Convert AppError to serializable string for Tauri IPC return payloads
impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type Result<T> = std::result::Result<T, AppError>;
