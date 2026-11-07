//! The one error type of the delivery Lambdas. Its kind becomes the Lambda `errorType`, so the
//! state machine can catch `Refused`, `Invalid` and `AccessDenied` (the run is blocked with the
//! message as the reason) and retry `Unavailable`.

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    /// A target or push the harness never serves; the message is the exact reason `ham` shows.
    #[error("{0}")]
    Refused(String),
    /// A malformed request from the state machine.
    #[error("invalid input: {0}")]
    Invalid(String),
    /// A transient AWS, git or file failure.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// S3 refused the role the key or the action. The grant is the same on every retry, so the machine
    /// catches this by name without retrying it. Without `s3:ListBucket` S3 also answers it for a key that
    /// does not exist, so the message names the operation and the key.
    #[error("access denied: {0}")]
    AccessDenied(String),
}

impl DeliveryError {
    pub fn error_type(&self) -> &'static str {
        match self {
            DeliveryError::Refused(_) => "Refused",
            DeliveryError::Invalid(_) => "Invalid",
            DeliveryError::Unavailable(_) => "Unavailable",
            DeliveryError::AccessDenied(_) => "AccessDenied",
        }
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        DeliveryError::Invalid(msg.into())
    }

    pub fn unavailable(msg: impl Into<String>) -> Self {
        DeliveryError::Unavailable(msg.into())
    }
}

impl From<std::io::Error> for DeliveryError {
    fn from(e: std::io::Error) -> Self {
        DeliveryError::Unavailable(format!("file I/O: {e}"))
    }
}
