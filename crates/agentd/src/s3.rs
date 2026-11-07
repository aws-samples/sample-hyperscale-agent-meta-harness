//! S3 GET and PUT with the session credentials (bundles under `runs/<run>/...`). In AWS the URL is
//! virtual-hosted (`https://<bucket>.s3.<region>.amazonaws.com/<key>`, routed through the S3
//! gateway endpoint); a configured endpoint uses path style (tests).

use crate::http::{HttpError, SignedHttp, SignedRequest};
use harness_protocol::api::internal::ProcessCredentials;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

/// Standard base64 with padding (RFC 4648 section 4). It exists for one header, `x-amz-checksum-sha256`, so the crate takes no
/// dependency for it.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        // A chunk of one byte makes two characters, a chunk of two makes three, and `=` pads the rest.
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The value of an `x-amz-checksum-sha256` header for `body`: base64 of its 32-byte SHA-256 digest.
pub fn sha256_base64(body: &[u8]) -> String {
    base64_encode(&Sha256::digest(body))
}

pub fn object_url(uri: &str, region: &str, endpoint: Option<&str>) -> Result<String, String> {
    let rest = uri
        .strip_prefix("s3://")
        .ok_or_else(|| format!("{uri} is not an s3:// URI"))?;
    let (bucket, key) = rest
        .split_once('/')
        .ok_or_else(|| format!("{uri} has no key"))?;
    if bucket.is_empty() || key.is_empty() {
        return Err(format!("{uri} needs a bucket and a key"));
    }
    Ok(match endpoint {
        Some(e) => format!("{}/{bucket}/{key}", e.trim_end_matches('/')),
        None => format!("https://{bucket}.s3.{region}.amazonaws.com/{key}"),
    })
}

/// The text between `<tag>` and `</tag>`, if the body has it.
fn xml_field<'a>(body: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = body.find(&open)? + open.len();
    let len = body[start..].find(&format!("</{tag}>"))?;
    Some(&body[start..start + len])
}

/// A failed request as `s3 <op> <uri> returned <status>[ <Code>: <Message>]`. Only S3's `Code`
/// and `Message` (at most 300 characters) are kept; signature details in the error document
/// (`StringToSign`, `CanonicalRequest`, `AWSAccessKeyId`) never reach the report.
pub fn s3_error(op: &str, uri: &str, status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let base = format!("s3 {op} {uri} returned {status}");
    match xml_field(&text, "Code") {
        Some(code) => {
            let message: String = xml_field(&text, "Message")
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            format!("{base} {code}: {message}")
        }
        None => base,
    }
}

#[derive(Debug)]
pub struct S3Objects {
    http: SignedHttp,
    region: String,
    endpoint: Option<String>,
}

/// What a HEAD of an object told (`x-amz-checksum-mode: ENABLED`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHead {
    /// The object's `x-amz-checksum-sha256`, if it was stored with one. A multipart upload's composite value ends in `-<parts>` and is
    /// never equal to the digest of a whole body; an object stored without a SHA-256 checksum has none.
    pub sha256: Option<String>,
}

/// How one create-only put attempt ended.
enum Attempt {
    Stored,
    Exists,
    /// `sent`: the request left agentd, so S3 may have committed the object whatever the answer was. A failure before it (an unreadable
    /// file, a bad URL, a signing error) cannot have stored anything.
    Failed {
        error: String,
        sent: bool,
    },
}

impl S3Objects {
    pub fn new(region: String, endpoint: Option<String>) -> Self {
        Self {
            http: SignedHttp::new(),
            region,
            endpoint,
        }
    }

    /// The same client with another limit for one request. The product keeps the default; a test uses this to end a request whose
    /// answer never comes within a moment.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.http = self.http.with_timeout(timeout);
        self
    }

    pub async fn get_to_file(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        dest: &Path,
    ) -> Result<(), String> {
        let url = object_url(uri, &self.region, self.endpoint.as_deref())?;
        let resp = self
            .http
            .send(SignedRequest {
                method: "GET",
                url: &url,
                service: "s3",
                region: &self.region,
                creds,
                content_type: None,
                body: vec![],
            })
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 {
            return Err(s3_error("GET", uri, resp.status, &resp.body));
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(dest, resp.body).map_err(|e| format!("{}: {e}", dest.display()))
    }

    pub async fn put_file(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
    ) -> Result<(), String> {
        let url = object_url(uri, &self.region, self.endpoint.as_deref())?;
        let body = std::fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
        let resp = self
            .http
            .send(SignedRequest {
                method: "PUT",
                url: &url,
                service: "s3",
                region: &self.region,
                creds,
                content_type: Some("application/octet-stream"),
                body,
            })
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 {
            return Err(s3_error("PUT", uri, resp.status, &resp.body));
        }
        Ok(())
    }

    /// PUTs `src` to `uri`, trying again `retries` times with a pause that doubles. For the objects that agentd writes more than once or
    /// writes over (the snapshot, the stop digests, a foreign object it replaces); the end-of-run objects use
    /// [`put_file_new_retrying`](Self::put_file_new_retrying) first.
    pub async fn put_file_retrying(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
        retries: u32,
        pause: Duration,
    ) -> Result<(), String> {
        let mut wait = pause;
        let mut last = String::new();
        for attempt in 0..=retries {
            if attempt > 0 {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            match self.put_file(uri, creds, src).await {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// HEADs `uri` with `x-amz-checksum-mode: ENABLED` (signed) and returns the checksum the object was stored with. Any status other
    /// than 200 is an error ([`s3_error`]): a missing key, a refusal (S3 answers 403 for a missing key when the role may not list the
    /// bucket) and a 5xx all say nothing about who wrote the object.
    pub async fn head_checksum(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
    ) -> Result<ObjectHead, String> {
        let url = object_url(uri, &self.region, self.endpoint.as_deref())?;
        let resp = self
            .http
            .send_with_headers(
                SignedRequest {
                    method: "HEAD",
                    url: &url,
                    service: "s3",
                    region: &self.region,
                    creds,
                    content_type: None,
                    body: vec![],
                },
                &[("x-amz-checksum-mode", "ENABLED")],
            )
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 {
            return Err(s3_error("HEAD", uri, resp.status, &resp.body));
        }
        Ok(ObjectHead {
            sha256: resp.header("x-amz-checksum-sha256").map(str::to_string),
        })
    }

    /// One create-only attempt: PUTs `body` to `uri` only when the key holds no object. The request carries `If-None-Match: *` and
    /// `x-amz-checksum-sha256` for the body, both signed (S3 needs SigV4 for a conditional write and refuses an `x-amz-*` header that the
    /// signature does not cover). S3 checks the checksum
    /// against the bytes it received, so an object stored with this value holds exactly these bytes; it answers 412 `PreconditionFailed`
    /// when the key is taken.
    async fn put_new_once(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        body: Vec<u8>,
        checksum: &str,
    ) -> Attempt {
        let local = |error: String| Attempt::Failed { error, sent: false };
        let url = match object_url(uri, &self.region, self.endpoint.as_deref()) {
            Ok(u) => u,
            Err(e) => return local(e),
        };
        let resp = self
            .http
            .send_with_headers(
                SignedRequest {
                    method: "PUT",
                    url: &url,
                    service: "s3",
                    region: &self.region,
                    creds,
                    content_type: Some("application/octet-stream"),
                    body,
                },
                &[("if-none-match", "*"), ("x-amz-checksum-sha256", checksum)],
            )
            .await;
        match resp {
            Err(e @ HttpError::Transport(_)) => Attempt::Failed {
                error: e.to_string(),
                sent: true,
            },
            Err(e) => local(e.to_string()),
            Ok(r) => match r.status {
                200 => Attempt::Stored,
                412 => Attempt::Exists,
                status => Attempt::Failed {
                    error: s3_error("PUT", uri, status, &r.body),
                    sent: true,
                },
            },
        }
    }

    /// PUTs `src` to `uri` once, only when the key holds no object (see [`put_new_once`](Self::put_new_once) for the request). The end
    /// sequence writes the bundle, `summary.json` and `test-report.json` this way, through
    /// [`put_file_new_retrying`](Self::put_file_new_retrying). Any status but 200 and 412, and a transport failure, is an error
    /// ([`s3_error`]: the status and S3's code and message, never a credential).
    pub async fn put_file_new(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
    ) -> Result<PutNew, String> {
        let body = std::fs::read(src).map_err(|e| format!("{}: {e}", src.display()))?;
        let checksum = sha256_base64(&body);
        match self.put_new_once(uri, creds, body, &checksum).await {
            Attempt::Stored => Ok(PutNew::Stored),
            Attempt::Exists => Ok(PutNew::Exists),
            Attempt::Failed { error, .. } => Err(error),
        }
    }

    /// The create-only put of an end-of-run object with its retries: at most `retries` more tries after the first, with a pause that
    /// doubles. A 412 on the first try, with nothing failed before it, is [`PutNew::Exists`]: the key was taken before agentd wrote.
    ///
    /// A 412 that follows a try that failed after the request left agentd (a timeout, a broken connection, a 5xx or 409 answer) may be
    /// agentd's own object, committed by the try whose answer was lost. The object is then read back with a HEAD: when it carries the
    /// checksum of the bytes agentd sent, it is those bytes and the result is [`PutNew::Stored`]; when it carries another value (none, a
    /// multipart composite, a different digest) it is another writer's and the result is [`PutNew::Exists`]. A HEAD that fails leaves the
    /// 412 unexplained: the loop tries again, and when the tries run out the result is an error, never `Exists` and never `Stored`.
    pub async fn put_file_new_retrying(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
        retries: u32,
        pause: Duration,
    ) -> Result<PutNew, String> {
        let mut wait = pause;
        let mut last = String::new();
        // The checksums of the tries that left agentd: the object a lost answer committed carries one of them.
        let mut sent: Vec<String> = Vec::new();
        for attempt in 0..=retries {
            if attempt > 0 {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            let body = match std::fs::read(src) {
                Ok(b) => b,
                Err(e) => {
                    last = format!("{}: {e}", src.display());
                    continue;
                }
            };
            let checksum = sha256_base64(&body);
            match self.put_new_once(uri, creds, body, &checksum).await {
                Attempt::Stored => return Ok(PutNew::Stored),
                Attempt::Exists if sent.is_empty() => return Ok(PutNew::Exists),
                Attempt::Exists => match self.head_checksum(uri, creds).await {
                    Ok(head) if head.sha256.as_ref().is_some_and(|s| sent.contains(s)) => {
                        return Ok(PutNew::Stored);
                    }
                    Ok(_) => return Ok(PutNew::Exists),
                    Err(e) => {
                        last = format!(
                            "{uri} exists and could not be told from agentd's own object: {e}"
                        );
                    }
                },
                Attempt::Failed { error, sent: left } => {
                    last = error;
                    if left {
                        sent.push(checksum);
                    }
                }
            }
        }
        Err(last)
    }
}

/// What a create-only put ([`S3Objects::put_file_new`]) did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutNew {
    /// The key was free and holds the file now.
    Stored,
    /// The key already held an object that is not agentd's own (S3's 412; after a try whose answer was lost, a HEAD showed another
    /// checksum): nothing was written. The end sequence records the conflict and then writes its own object over it.
    Exists,
}
