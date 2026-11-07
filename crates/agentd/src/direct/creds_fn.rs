//! The session credentials as a function, and the path-segment encoder, that the route probe and the driver use to sign Bedrock
//! calls.
use harness_protocol::api::internal::ProcessCredentials;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type CredsFuture = Pin<Box<dyn Future<Output = Result<ProcessCredentials, String>> + Send>>;
/// The session credentials, fresh for each upstream call.
pub type CredsFn = Arc<dyn Fn() -> CredsFuture + Send + Sync>;

/// Percent-encodes one path segment the way `encodeURIComponent` does for model ids.
pub(crate) fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
