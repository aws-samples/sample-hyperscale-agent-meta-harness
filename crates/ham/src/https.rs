//! The live transport: hyper 1 with rustls, the ring provider and the webpki root store (S11: no
//! Apple framework, so the same code links for aarch64-apple-darwin). `/v1` calls are signed with
//! [`Signer`]; presigned object URLs are fetched unsigned. Plain HTTP is allowed only to a
//! loopback host (the tests' local API).

use crate::http::{ControlPlane, Response, RetentionAnswer, TransportError, redact_url};
use crate::sigv4::{Signer, error_chain};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use std::collections::BTreeMap;
use std::time::Duration;

/// A `/v1` call answers within this time or is a timeout.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// One 16 MiB range arrives within this time or is a timeout.
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

pub struct HttpsPlane {
    base: String,
    signer: Signer,
    client: Client<HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl std::fmt::Debug for HttpsPlane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsPlane")
            .field("base", &self.base)
            .field("signer", &self.signer)
            .finish_non_exhaustive()
    }
}

/// `https://`, or `http://` to 127.0.0.1, ::1 or localhost.
pub(crate) fn check_url(url: &str) -> Result<http::Uri, TransportError> {
    let refused = |why: &str| TransportError::Refused(format!("{why}: {}", redact_url(url)));
    let uri: http::Uri = url.parse().map_err(|_| refused("not a URL"))?;
    let host = uri.host().ok_or_else(|| refused("no host"))?;
    match uri.scheme_str() {
        Some("https") => Ok(uri),
        Some("http") if matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") => Ok(uri),
        Some("http") => Err(refused("plain HTTP is allowed only to a loopback host")),
        _ => Err(refused("not an http(s) URL")),
    }
}

/// True when `e`, or one of its sources, is the connector's report of a name lookup that failed. hyper-util labels that link `dns error`, and the
/// platform's own text starts `failed to lookup address information`; either counts when it is a whole link, so a longer text that only holds the
/// words does not. A refused connection, a handshake that ends and a certificate that is not trusted carry neither.
pub fn is_dns_failure(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut link: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = link {
        let text = err.to_string();
        if text == "dns error" || text.starts_with("failed to lookup address information") {
            return true;
        }
        link = err.source();
    }
    false
}

fn lower_headers(h: &http::HeaderMap) -> BTreeMap<String, String> {
    h.iter()
        .map(|(k, v)| {
            (
                k.as_str().to_ascii_lowercase(),
                v.to_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

impl HttpsPlane {
    /// `api_endpoint` is the stage base URL (`https://<api>.execute-api.<region>.amazonaws.com/<stage>`);
    /// routes are appended to it.
    pub fn new(api_endpoint: &str, signer: Signer) -> Result<Self, TransportError> {
        check_url(api_endpoint)?;
        // An explicit provider: in workspace builds another crate enables rustls' aws-lc-rs too, and
        // with two providers compiled in rustls cannot choose a process default.
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(std::sync::Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .map_err(|e| TransportError::Refused(format!("TLS setup: {e}")))?
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            base: api_endpoint.trim_end_matches('/').to_string(),
            signer,
            client: Client::builder(TokioExecutor::new()).build(https),
        })
    }

    /// `stage_host` is the host of the stage address when the request goes to the stage; a name lookup that fails for it is
    /// [`TransportError::Unresolved`]. A request to any other host (a presigned object URL) names no stage, so its failed lookup is a network error.
    async fn send(
        &self,
        req: http::Request<Full<Bytes>>,
        what: &str,
        timeout: Duration,
        stage_host: Option<&str>,
    ) -> Result<Response, TransportError> {
        let exchange = async {
            let resp = self
                .client
                .request(req)
                .await
                .map_err(|e| match stage_host {
                    Some(host) if is_dns_failure(&e) => {
                        TransportError::Unresolved(host.to_string())
                    }
                    _ => TransportError::Network(format!("{what}: {}", error_chain(&e))),
                })?;
            let status = resp.status().as_u16();
            let headers = lower_headers(resp.headers());
            let body = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| TransportError::Network(format!("{what}: {}", error_chain(&e))))?
                .to_bytes()
                .to_vec();
            Ok(Response {
                status,
                headers,
                body,
            })
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| TransportError::Timeout(what.to_string()))?
    }
}

impl HttpsPlane {
    /// `GET /data-retention` on the Bedrock control plane of the signer's Region, signed for `bedrock`.
    async fn read_retention(&self) -> Result<RetentionAnswer, TransportError> {
        let region = self.signer.region().to_string();
        let url = format!("https://bedrock.{region}.amazonaws.com/data-retention");
        let what = format!("GET {}", redact_url(&url));
        let uri = check_url(&url)?;
        let signed = self
            .signer
            .sign_for("bedrock", "GET", &url, &[], &[])
            .await?;
        let mut builder = http::Request::builder().method("GET").uri(uri);
        for (k, v) in &signed {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let req = builder
            .body(Full::new(Bytes::new()))
            .map_err(|e| TransportError::Refused(format!("{what}: {e}")))?;
        let response = self.send(req, &what, CALL_TIMEOUT, None).await?;
        Ok(RetentionAnswer { region, response })
    }

    /// `PUT /data-retention` with `{"mode": mode}` on the Bedrock control plane of the signer's Region, signed for `bedrock`.
    async fn write_retention(&self, mode: &str) -> Result<RetentionAnswer, TransportError> {
        let region = self.signer.region().to_string();
        let url = format!("https://bedrock.{region}.amazonaws.com/data-retention");
        let what = format!("PUT {}", redact_url(&url));
        let uri = check_url(&url)?;
        let body = serde_json::json!({ "mode": mode }).to_string().into_bytes();
        let headers = [("content-type".to_string(), "application/json".to_string())];
        let signed = self
            .signer
            .sign_for("bedrock", "PUT", &url, &headers, &body)
            .await?;
        let mut builder = http::Request::builder().method("PUT").uri(uri);
        for (k, v) in headers.iter().chain(signed.iter()) {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let req = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| TransportError::Refused(format!("{what}: {e}")))?;
        let response = self.send(req, &what, CALL_TIMEOUT, None).await?;
        Ok(RetentionAnswer { region, response })
    }
}

impl ControlPlane for HttpsPlane {
    async fn data_retention(
        &self,
        _target: Option<&str>,
    ) -> Option<Result<RetentionAnswer, TransportError>> {
        // A plain-HTTP stage address is a loopback stand-in of the tests (`check_url` allows no other): it has no Bedrock account behind it, so no
        // call leaves the machine.
        if self.base.starts_with("http://") {
            return None;
        }
        Some(self.read_retention().await)
    }

    async fn put_data_retention(
        &self,
        _target: Option<&str>,
        mode: &str,
    ) -> Option<Result<RetentionAnswer, TransportError>> {
        // The same rule as the read: a plain-HTTP stage address is a loopback stand-in with no Bedrock account behind it.
        if self.base.starts_with("http://") {
            return None;
        }
        Some(self.write_retention(mode).await)
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let url = format!("{}{path}", self.base);
        let what = format!("{method} {}", redact_url(&url));
        let uri = check_url(&url)?;
        let host = uri.host().unwrap_or_default().to_string();
        let signed = self.signer.sign(method, &url, headers, &body).await?;
        let mut builder = http::Request::builder().method(method).uri(uri);
        for (k, v) in headers.iter().chain(signed.iter()) {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let req = builder
            .body(Full::new(Bytes::from(body)))
            .map_err(|e| TransportError::Refused(format!("{what}: {e}")))?;
        let answer = self.send(req, &what, CALL_TIMEOUT, Some(&host)).await?;
        // Every answer of the control plane says which backend sent it (the tag on each screen reads it); a presigned fetch below is S3's.
        crate::version::note_answer(answer.status, &answer.headers);
        Ok(answer)
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        let what = format!("GET {}", redact_url(url));
        let uri = check_url(url)?;
        let mut builder = http::Request::builder().method("GET").uri(uri);
        if let Some((start, end)) = range {
            builder = builder.header("range", format!("bytes={start}-{end}"));
        }
        if let Some(etag) = if_match {
            builder = builder.header("if-match", etag);
        }
        let req = builder
            .body(Full::new(Bytes::new()))
            .map_err(|e| TransportError::Refused(format!("{what}: {e}")))?;
        self.send(req, &what, FETCH_TIMEOUT, None).await
    }
}
