//! The transport seam: what `ham` needs from the control plane and the object store. The live
//! implementation is [`crate::https::HttpsPlane`]; tests put a local API behind the same trait.

use std::collections::BTreeMap;
use std::future::Future;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Lowercase header names.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Transport failures. Messages name the host and path only: never a query (a presigned URL's
/// query holds a session token), a request body or a credential.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The connection failed or dropped before a whole answer arrived.
    #[error("network: {0}")]
    Network(String),
    /// The host of the stage address (`api_endpoint`) did not resolve: a DNS failure on connect. The text holds the host; it says the stage may have
    /// been removed or replaced and names the two ways forward, `./setup.sh --status` for the current endpoint and `ham config set api_endpoint <url>`.
    /// Only a call to the stage ends here. A failed lookup of a presigned object URL stays [`TransportError::Network`]: that host is the object
    /// store's, not the stage's. Every other connect failure (refused, a handshake that ends) keeps the `network:` words as well.
    #[error(
        "the stage address does not resolve ({0}). The stage may have been removed or replaced. Run `./setup.sh --status` to see the current endpoint, or `ham config set api_endpoint <url>` to set it."
    )]
    Unresolved(String),
    #[error("timed out: {0}")]
    Timeout(String),
    /// A URL or endpoint `ham` will not call.
    #[error("{0}")]
    Refused(String),
    #[error("AWS credentials: {0}")]
    Credentials(String),
    #[error("SigV4 signing: {0}")]
    Sign(String),
    /// A refusal the client makes itself, before or instead of a call to one of several backends. It never goes to the network and is
    /// never retried.
    #[error("{0}")]
    Local(ham_core::backends::Local),
}

impl TransportError {
    /// Network drops, timeouts and a lookup that failed may succeed on a retry (a resolver can fail once and answer at the next try); the other
    /// kinds cannot.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            TransportError::Network(_) | TransportError::Unresolved(_) | TransportError::Timeout(_)
        )
    }
}

/// The answer of the Bedrock control plane to the read of the account's data retention mode, and the Region it was asked in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionAnswer {
    pub region: String,
    pub response: Response,
}

pub trait ControlPlane: Send + Sync + 'static {
    /// One SigV4-signed `/v1` call: `path` starts with `/v1/` and may carry a query; `headers`
    /// use lowercase names and are signed.
    fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> impl Future<Output = Result<Response, TransportError>> + Send;

    /// An unsigned GET of a presigned object URL: whole, or one inclusive byte range, optionally
    /// only while the object's ETag equals `if_match`.
    fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> impl Future<Output = Result<Response, TransportError>> + Send;

    /// [`ControlPlane::call`] aimed at one backend by its alias. A plane with one backend ignores the alias; the set of backends
    /// (`crate::backends::Backends`) sends the call to that backend and to no other.
    fn call_on(
        &self,
        target: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> impl Future<Output = Result<Response, TransportError>> + Send {
        let _ = target;
        self.call(method, path, headers, body)
    }

    /// The account's Bedrock data retention mode in the stage's Region, read with the caller's own credentials: `GET /data-retention` on the Bedrock
    /// control plane, signed for `bedrock`. `ham doctor` shows it, and the start of a run on a model that needs provider data sharing reads it. `None` for a
    /// plane with no credentials of its own (a stand-in), which adds no line and is never changed. `target` names the backend; the set of backends
    /// refuses a read that names none when it holds several, so no account is picked by its place in the file.
    fn data_retention(
        &self,
        target: Option<&str>,
    ) -> impl Future<Output = Option<Result<RetentionAnswer, TransportError>>> + Send {
        let _ = target;
        async { None }
    }

    /// The switch of the account's Bedrock data retention mode in the stage's Region to `mode`, with the caller's own credentials: `PUT
    /// /data-retention` on the Bedrock control plane with `{"mode": mode}`, signed for `bedrock`. Only `crate::sharing` calls it, for a run
    /// the user agreed to share. `None` for a plane with no credentials of its own (a stand-in). `target` names the backend, as for the read.
    fn put_data_retention(
        &self,
        target: Option<&str>,
        mode: &str,
    ) -> impl Future<Output = Option<Result<RetentionAnswer, TransportError>>> + Send {
        let _ = (target, mode);
        async { None }
    }

    /// The backend a start goes to: the one `target` names, else the one that holds `source` (the run a follow-up, a resume or a retry
    /// continues), else the default one. `Ok(None)` for a plane with one backend, which is never aimed. The set of backends refuses a start that
    /// names another backend than the one that holds `source`, a run no backend holds, and a new run with no backend chosen: the check of
    /// provider data sharing asks this first, so the account it reads and switches is the one the run starts in.
    fn start_backend(
        &self,
        target: Option<&str>,
        source: Option<&str>,
    ) -> impl Future<Output = Result<Option<String>, TransportError>> + Send {
        let _ = (target, source);
        async { Ok(None) }
    }

    /// The backends and how each answered last; `None` for a plane with one backend, so no screen shows a banner, a column or a count for it.
    fn backend_view(&self) -> Option<ham_core::backends::BackendView> {
        None
    }

    /// Each backend of the client with whether its config declares it safe for customer content, for one backend as well as for several;
    /// empty for a plane that knows no declaration.
    fn declared_safe(&self) -> Vec<(String, bool)> {
        Vec::new()
    }

    /// The backend a run is in, once the client has seen the run.
    fn backend_of(&self, run_id: &str) -> Option<String> {
        let _ = run_id;
        None
    }

    /// Makes the backends that were left alone after a failure (a lapsed login, a backend that was down) tried again at the next call.
    fn retry_down(&self) {}

    /// True while a read on a timer leaves this backend alone because its login has lapsed.
    fn paused(&self, alias: &str) -> bool {
        let _ = alias;
        false
    }

    /// Records that a read of this backend ran out of time.
    fn note_slow(&self, alias: &str) {
        let _ = alias;
    }

    /// Records whether the rows shown for this backend are the ones of an earlier read.
    fn set_stale(&self, alias: &str, stale: bool) {
        let _ = (alias, stale);
    }
}

/// `scheme://authority/path` of a URL, without its query or fragment, for messages.
pub fn redact_url(url: &str) -> String {
    let cut = url.find(['?', '#']).unwrap_or(url.len());
    url[..cut].to_string()
}
