//! SigV4-signed HTTP over hyper with rustls and the webpki root store (the same TLS stack as `ham`,
//! S11). Used for `/internal/*` (service `execute-api`) and S3 objects (service `s3`).

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings, sign,
};
use aws_sigv4::sign::v4;
use bytes::Bytes;
use harness_protocol::api::internal::ProcessCredentials;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use std::time::{Duration, SystemTime};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    #[error("bad URL {0}")]
    BadUrl(String),
    #[error("SigV4 signing failed: {0}")]
    Sign(String),
    #[error("transport: {0}")]
    Transport(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    /// `x-amzn-errortype` of an AWS error answer.
    pub error_type: Option<String>,
    /// Every header of the answer, names in lower case (the answer to a HEAD carries its facts here).
    pub headers: Vec<(String, String)>,
}

impl HttpResponse {
    /// The value of the response header `name` (lower case), if the answer has it.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct SignedRequest<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub service: &'a str,
    pub region: &'a str,
    pub creds: &'a ProcessCredentials,
    pub content_type: Option<&'a str>,
    pub body: Vec<u8>,
}

/// The headers SigV4 adds (`authorization`, `x-amz-date`, `x-amz-security-token`, and for S3
/// `x-amz-content-sha256`). The host header is signed from the URL authority.
pub fn signed_headers(
    method: &str,
    url: &str,
    service: &str,
    region: &str,
    creds: &ProcessCredentials,
    body: &[u8],
    time: SystemTime,
) -> Result<Vec<(String, String)>, HttpError> {
    signed_headers_with(method, url, service, region, creds, &[], body, time)
}

/// [`signed_headers`] with `extra` request headers put into the signature (S3 needs SigV4 for a conditional write such as
/// `if-none-match`, and refuses an `x-amz-*` header that the signature does not cover). The result is still only the headers SigV4
/// adds: the caller puts `extra` on the request itself, once.
#[allow(clippy::too_many_arguments)]
pub fn signed_headers_with(
    method: &str,
    url: &str,
    service: &str,
    region: &str,
    creds: &ProcessCredentials,
    extra: &[(&str, &str)],
    body: &[u8],
    time: SystemTime,
) -> Result<Vec<(String, String)>, HttpError> {
    let uri: http::Uri = url
        .parse()
        .map_err(|_| HttpError::BadUrl(url.to_string()))?;
    if uri.scheme().is_none() || uri.authority().is_none() {
        return Err(HttpError::BadUrl(url.to_string()));
    }
    let identity = Credentials::new(
        creds.access_key_id.clone(),
        creds.secret_access_key.clone(),
        Some(creds.session_token.clone()),
        None,
        "agentd",
    )
    .into();
    let mut settings = SigningSettings::default();
    if service == "s3" {
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    }
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(service)
        .time(time)
        .settings(settings)
        .build()
        .map_err(|e| HttpError::Sign(e.to_string()))?
        .into();
    let request = SignableRequest::new(
        method,
        url,
        extra.iter().copied(),
        SignableBody::Bytes(body),
    )
    .map_err(|e| HttpError::Sign(e.to_string()))?;
    let (instructions, _signature) = sign(request, &params)
        .map_err(|e| HttpError::Sign(e.to_string()))?
        .into_parts();
    let mut carrier = http::Request::new(());
    instructions.apply_to_request_http1x(&mut carrier);
    Ok(carrier
        .headers()
        .iter()
        .map(|(k, v)| {
            (
                k.as_str().to_string(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect())
}

pub struct SignedHttp {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    /// How long [`send`](Self::send) waits for a whole answer; [`REQUEST_TIMEOUT`] unless a test shortens it.
    timeout: Duration,
}

impl std::fmt::Debug for SignedHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignedHttp")
    }
}

impl Default for SignedHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl SignedHttp {
    pub fn new() -> Self {
        // The connector `HttpsConnectorBuilder::build` makes.
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        // An explicit provider: in workspace builds another crate enables rustls' aws-lc-rs too, and
        // with two providers compiled in rustls cannot choose a process default.
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .unwrap_or_else(|e| panic!("ring TLS provider rejected its own defaults: {e}"))
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Self {
            client: Client::builder(TokioExecutor::new()).build(https),
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// The same client with another limit for [`send`](Self::send) and [`send_with_headers`](Self::send_with_headers). The product keeps
    /// [`REQUEST_TIMEOUT`]; a test uses this to end a request whose answer never comes within a moment.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Signs and sends `r` (SigV4, the session credentials) and reads the whole answer, within [`REQUEST_TIMEOUT`].
    pub async fn send(&self, r: SignedRequest<'_>) -> Result<HttpResponse, HttpError> {
        self.send_timeout(r, self.timeout).await
    }

    /// [`send`](Self::send) with `extra` headers, which are signed and sent (`if-none-match: *` for a create-only put, the checksum
    /// headers of an object write and of a HEAD).
    pub async fn send_with_headers(
        &self,
        r: SignedRequest<'_>,
        extra: &[(&str, &str)],
    ) -> Result<HttpResponse, HttpError> {
        self.send_timeout_with(r, extra, self.timeout).await
    }

    pub async fn send_timeout(
        &self,
        r: SignedRequest<'_>,
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        self.send_timeout_with(r, &[], timeout).await
    }

    /// Signs `r` with `extra` in the signature, sends it with `extra` on the request and reads the whole answer within `timeout`.
    pub async fn send_timeout_with(
        &self,
        r: SignedRequest<'_>,
        extra: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<HttpResponse, HttpError> {
        let headers = signed_headers_with(
            r.method,
            r.url,
            r.service,
            r.region,
            r.creds,
            extra,
            &r.body,
            SystemTime::now(),
        )?;
        let mut builder = http::Request::builder().method(r.method).uri(r.url);
        for (k, v) in headers {
            builder = builder.header(k, v);
        }
        for (k, v) in extra {
            builder = builder.header(*k, *v);
        }
        if let Some(ct) = r.content_type {
            builder = builder.header("content-type", ct);
        }
        let req = builder
            .body(Full::new(Bytes::from(r.body)))
            .map_err(|_| HttpError::BadUrl(r.url.to_string()))?;
        let resp = tokio::time::timeout(timeout, self.client.request(req))
            .await
            .map_err(|_| HttpError::Transport(format!("{} timed out", r.url)))?
            .map_err(|e| HttpError::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let error_type = resp
            .headers()
            .get("x-amzn-errortype")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| HttpError::Transport(e.to_string()))?
            .to_bytes()
            .to_vec();
        Ok(HttpResponse {
            status,
            body,
            error_type,
            headers,
        })
    }
}
