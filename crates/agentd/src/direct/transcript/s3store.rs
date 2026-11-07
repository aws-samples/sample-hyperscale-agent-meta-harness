//! The S3 side of the transcript writer: a write-once put (`If-None-Match: *`), a listing after a key and a
//! ranged read of an object's first bytes, each signed with the run's session credentials (SigV4, service `s3`).
//!
//! agentd has no AWS SDK, and the shared signed client (`crate::http`) signs no header besides the SigV4 ones, so this store signs its
//! own requests with the same crate (`aws-sigv4`) and puts the headers it needs into the signature: the condition, the range and the
//! content type. The TLS stack is the same as the shared client's (rustls with the webpki roots, ring). A failure never carries a
//! credential: the text is the status and S3's own error code and message.

use crate::BoxFuture;
use crate::creds::CredStore;
use crate::direct::transcript::store::{ChunkStore, PutOutcome};
use crate::s3::{object_url, s3_error};
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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// The most keys one listing returns, and the most pages it reads.
pub const MAX_LIST_KEYS: usize = 20_000;
const MAX_LIST_PAGES: usize = 40;
const PAGE_KEYS: usize = 1000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Transcript objects in the run's bucket.
pub struct S3Store {
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
    creds: Arc<CredStore>,
    region: String,
    endpoint: Option<String>,
    bucket: String,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

struct Reply {
    status: u16,
    body: Vec<u8>,
}

/// `s` percent-encoded for a query value: only the unreserved characters stay.
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// The text between `<tag>` and `</tag>`, from `from` on, and the position after it.
fn next_field<'a>(body: &'a str, tag: &str, from: usize) -> Option<(&'a str, usize)> {
    let open = format!("<{tag}>");
    let start = body[from..].find(&open)? + from + open.len();
    let len = body[start..].find(&format!("</{tag}>"))?;
    Some((&body[start..start + len], start + len))
}

impl S3Store {
    pub fn new(
        creds: Arc<CredStore>,
        region: String,
        endpoint: Option<String>,
        bucket: String,
    ) -> Self {
        let mut http = HttpConnector::new();
        http.enforce_http(false);
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(Arc::new(rustls::crypto::ring::default_provider()))
            .unwrap_or_else(|e| panic!("ring TLS provider rejected its own defaults: {e}"))
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Self {
            client: Client::builder(TokioExecutor::new()).build(https),
            creds,
            region,
            endpoint,
            bucket,
        }
    }

    fn object_url(&self, key: &str) -> Result<String, String> {
        object_url(
            &format!("s3://{}/{key}", self.bucket),
            &self.region,
            self.endpoint.as_deref(),
        )
    }

    /// The listing URL: path style for a configured endpoint (tests), virtual-hosted otherwise.
    fn list_url(&self, prefix: &str, start_after: Option<&str>, token: Option<&str>) -> String {
        let base = match &self.endpoint {
            Some(e) => format!("{}/{}", e.trim_end_matches('/'), self.bucket),
            None => format!("https://{}.s3.{}.amazonaws.com/", self.bucket, self.region),
        };
        let mut q = format!(
            "list-type=2&max-keys={PAGE_KEYS}&prefix={}",
            encode_query(prefix)
        );
        match (token, start_after) {
            (Some(t), _) => q.push_str(&format!("&continuation-token={}", encode_query(t))),
            (None, Some(a)) => q.push_str(&format!("&start-after={}", encode_query(a))),
            _ => {}
        }
        format!("{base}?{q}")
    }

    fn signed(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: &[u8],
        creds: &ProcessCredentials,
    ) -> Result<Vec<(String, String)>, String> {
        let identity = Credentials::new(
            creds.access_key_id.clone(),
            creds.secret_access_key.clone(),
            Some(creds.session_token.clone()),
            None,
            "agentd",
        )
        .into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|e| format!("SigV4 signing failed: {e}"))?
            .into();
        let request = SignableRequest::new(
            method,
            url,
            headers.iter().copied(),
            SignableBody::Bytes(body),
        )
        .map_err(|e| format!("SigV4 signing failed: {e}"))?;
        let (instructions, _) = sign(request, &params)
            .map_err(|e| format!("SigV4 signing failed: {e}"))?
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

    async fn send(
        &self,
        method: &'static str,
        url: &str,
        extra: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<Reply, String> {
        let creds = self.creds.current().await.map_err(|e| e.to_string())?;
        let signed = self.signed(method, url, extra, &body, &creds)?;
        let mut builder = http::Request::builder().method(method).uri(url);
        for (k, v) in signed.iter().chain(
            extra
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<Vec<_>>()
                .iter(),
        ) {
            builder = builder.header(k, v);
        }
        let req = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|_| format!("bad URL {url}"))?;
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(req))
            .await
            .map_err(|_| format!("{method} {url} timed out"))?
            .map_err(|e| format!("transport: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("transport: {e}"))?
            .to_bytes()
            .to_vec();
        Ok(Reply { status, body })
    }
}

impl ChunkStore for S3Store {
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            let mut keys: Vec<String> = Vec::new();
            let mut token: Option<String> = None;
            for _ in 0..MAX_LIST_PAGES {
                let url = self.list_url(prefix, start_after, token.as_deref());
                let r = self.send("GET", &url, &[], Vec::new()).await?;
                if r.status != 200 {
                    return Err(s3_error(
                        "LIST",
                        &format!("s3://{}/{prefix}", self.bucket),
                        r.status,
                        &r.body,
                    ));
                }
                let body = String::from_utf8_lossy(&r.body).into_owned();
                let mut at = 0;
                while let Some((k, end)) = next_field(&body, "Key", at) {
                    keys.push(unescape(k));
                    at = end;
                    if keys.len() >= MAX_LIST_KEYS {
                        return Ok(keys);
                    }
                }
                let truncated =
                    next_field(&body, "IsTruncated", 0).is_some_and(|(v, _)| v == "true");
                token = next_field(&body, "NextContinuationToken", 0).map(|(t, _)| unescape(t));
                if !truncated || token.is_none() {
                    return Ok(keys);
                }
            }
            Ok(keys)
        })
    }

    fn head<'a>(&'a self, key: &'a str, len: usize) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let url = self.object_url(key)?;
            let range = format!("bytes=0-{}", len.saturating_sub(1));
            let r = self
                .send("GET", &url, &[("range", &range)], Vec::new())
                .await?;
            match r.status {
                200 | 206 => Ok(r.body.into_iter().take(len).collect()),
                404 => Err("no such key".to_string()),
                other => Err(s3_error(
                    "GET",
                    &format!("s3://{}/{key}", self.bucket),
                    other,
                    &r.body,
                )),
            }
        })
    }

    fn put_new<'a>(&'a self, key: &'a str, body: Vec<u8>) -> BoxFuture<'a, PutOutcome> {
        Box::pin(async move {
            let url = match self.object_url(key) {
                Ok(u) => u,
                Err(e) => return PutOutcome::Failed(e),
            };
            match self
                .send(
                    "PUT",
                    &url,
                    &[("if-none-match", "*"), ("content-type", "application/gzip")],
                    body,
                )
                .await
            {
                Ok(r) if r.status == 200 => PutOutcome::Stored,
                Ok(r) if r.status == 412 => PutOutcome::Exists,
                Ok(r) => PutOutcome::Failed(s3_error(
                    "PUT",
                    &format!("s3://{}/{key}", self.bucket),
                    r.status,
                    &r.body,
                )),
                Err(e) => PutOutcome::Failed(e),
            }
        })
    }
}
