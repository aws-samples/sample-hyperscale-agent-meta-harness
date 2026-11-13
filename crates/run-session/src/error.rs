//! The three errors the machine's Retry and Catch rules match by name.

/// What an op raises. The Lambda answers with `errorType` = [`RunSessionError::error_type`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RunSessionError {
    /// A conditional write lost to another writer; the machine retries.
    #[error("{0}")]
    Conflict(String),
    /// AWS or the store failed; the machine retries.
    #[error("{0}")]
    Unavailable(String),
    /// The input can never work (a run that is not active, a model not in `models.json`): no retry.
    #[error("{0}")]
    Invalid(String),
}

impl RunSessionError {
    /// The name `statemachines/direct.asl.json` matches in `ErrorEquals`.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::Conflict(_) => "RunSessionConflict",
            Self::Unavailable(_) => "RunSessionUnavailable",
            Self::Invalid(_) => "RunSessionInvalid",
        }
    }
}

/// The names the machine's `Retry` rules for run-session list.
pub const RETRYABLE: [&str; 2] = ["RunSessionConflict", "RunSessionUnavailable"];
