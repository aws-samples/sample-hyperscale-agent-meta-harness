//! S3 GET and PUT with the session credentials (bundles under `runs/<run>/...`). In AWS the URL is
//! virtual-hosted (`https://<bucket>.s3.<region>.amazonaws.com/<key>`, routed through the S3
//! gateway endpoint); a configured endpoint uses path style (tests).

use crate::http::{FilePut, HttpError, HttpResponse, Limits, SignedHttp, SignedRequest};
use harness_protocol::api::internal::ProcessCredentials;
use harness_protocol::output_pins::OutputPin;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// A request of an object is given up when nothing of it moved for this long (`http::Limits::no_progress`): no byte of an
/// upload left for the connection, the answer did not come after the body had ended, or no byte of the body of a download arrived
/// once the head of its answer had come. The whole-time limit of a request stays beside it ([`S3Objects::with_request_timeout`]): a
/// transfer that moves a little every 29 s still ends; the body of a download has `http::body_time` when that is longer.
pub const NO_PROGRESS: Duration = Duration::from_secs(30);

/// A file is hashed in pieces of this size ([`digest_file`]).
const DIGEST_PIECE_BYTES: usize = 1024 * 1024;

/// The SHA-256 of a file in the two forms a request needs, and its size, from one pass over the file ([`digest_file`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDigest {
    /// Lower-case hexadecimal: the payload hash of the signature, and the `sha256` of the record of the write.
    pub sha256_hex: String,
    /// Base64: the value of `x-amz-checksum-sha256`.
    pub sha256_base64: String,
    /// The size of the file in bytes.
    pub bytes: u64,
}

/// How many files [`digest_file`] has hashed in this process.
static DIGESTS_MADE: AtomicU64 = AtomicU64::new(0);

/// How many files this process has hashed with [`digest_file`] (tests: a store hashes its file once).
pub fn digests_made() -> u64 {
    DIGESTS_MADE.load(Ordering::Relaxed)
}

/// Hashes the file at `path` once, on a thread of the blocking pool, [`DIGEST_PIECE_BYTES`] at a time: no runtime thread reads
/// or hashes it, and it is never whole in memory. Before, an upload read the whole file on the thread that called it and hashed it
/// twice there (once for the record, once for the signature), which held agentd's other work for as long as that took.
pub async fn digest_file(path: &Path) -> Result<FileDigest, String> {
    let file = path.to_path_buf();
    let made = tokio::task::spawn_blocking(move || -> std::io::Result<FileDigest> {
        use std::io::Read;
        let mut src = std::fs::File::open(&file)?;
        let mut hasher = Sha256::new();
        let mut piece = vec![0_u8; DIGEST_PIECE_BYTES];
        let mut bytes: u64 = 0;
        loop {
            match src.read(&mut piece) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&piece[..n]);
                    bytes += n as u64;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let digest = hasher.finalize();
        Ok(FileDigest {
            sha256_hex: digest.iter().map(|b| format!("{b:02x}")).collect(),
            sha256_base64: base64_encode(&digest),
            bytes,
        })
    })
    .await
    .map_err(|e| format!("{}: {e}", path.display()))?
    .map_err(|e| format!("{}: {e}", path.display()))?;
    DIGESTS_MADE.fetch_add(1, Ordering::Relaxed);
    Ok(made)
}

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
    /// [`NO_PROGRESS`], or what a test set ([`Self::with_no_progress`]).
    no_progress: Duration,
}

/// What a HEAD of an object told (`x-amz-checksum-mode: ENABLED`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectHead {
    /// The object's `x-amz-checksum-sha256`, if it was stored with one. A multipart upload's composite value ends in `-<parts>` and is
    /// never equal to the digest of a whole body; an object stored without a SHA-256 checksum has none.
    pub sha256: Option<String>,
    /// The `x-amz-version-id` of the object the HEAD found (the current version), when the answer carried one.
    pub version_id: Option<String>,
}

/// How one create-only put attempt ended.
enum Attempt {
    /// The key holds the body now; the answer's `x-amz-version-id`, when it carried one.
    Stored(Option<String>),
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
            no_progress: NO_PROGRESS,
        }
    }

    /// The same client with another limit for one request. The product keeps the default; a test uses this to end a request whose
    /// answer never comes within a moment.
    #[must_use]
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.http = self.http.with_timeout(timeout);
        self
    }

    /// The same client with another no-progress limit for a streamed request ([`NO_PROGRESS`]); tests.
    #[must_use]
    pub fn with_no_progress(mut self, no_progress: Duration) -> Self {
        self.no_progress = no_progress;
        self
    }

    /// The limits of one streamed request of this client: its whole-time limit and its no-progress limit.
    fn limits(&self) -> Limits {
        Limits {
            total: self.http.timeout(),
            no_progress: self.no_progress,
        }
    }

    /// GETs `uri` into the file `dest`. The body is written as it arrives (`SignedHttp::get_to_file`: to `<dest>.part`, renamed
    /// after the last byte), so an object of any size costs agentd no memory and a download that fails leaves no file. An answer
    /// other than 200 is an error ([`s3_error`]).
    pub async fn get_to_file(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        dest: &Path,
    ) -> Result<(), String> {
        let url = object_url(uri, &self.region, self.endpoint.as_deref())?;
        let resp = self
            .http
            .get_to_file(
                SignedRequest {
                    method: "GET",
                    url: &url,
                    service: "s3",
                    region: &self.region,
                    creds,
                    content_type: None,
                    body: vec![],
                },
                dest,
                self.limits(),
            )
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 {
            return Err(s3_error("GET", uri, resp.status, &resp.body));
        }
        Ok(())
    }

    /// The first bytes of the object at `uri`, at most `max_bytes` of them: one GET with `range: bytes=0-<max_bytes - 1>`
    /// (signed). S3 answers 206 with the range, or with the whole object when it is shorter; a store that ignores the header answers
    /// 200 with the whole object, of which the first `max_bytes` are kept. Any other status is an error ([`s3_error`]). The answer
    /// is read into memory: it is what a caller asks for the header of a bundle, a few kilobytes.
    pub async fn get_prefix(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        let url = object_url(uri, &self.region, self.endpoint.as_deref())?;
        let range = format!("bytes=0-{}", max_bytes.max(1) - 1);
        let resp = self
            .http
            .send_with_headers(
                SignedRequest {
                    method: "GET",
                    url: &url,
                    service: "s3",
                    region: &self.region,
                    creds,
                    content_type: None,
                    body: vec![],
                },
                &[("range", &range)],
            )
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 && resp.status != 206 {
            return Err(s3_error("GET", uri, resp.status, &resp.body));
        }
        let mut body = resp.body;
        body.truncate(usize::try_from(max_bytes).unwrap_or(usize::MAX));
        Ok(body)
    }

    pub async fn put_file(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
    ) -> Result<(), String> {
        self.put_file_receipt(uri, creds, src).await.map(|_| ())
    }

    /// PUTs `src` to `uri` (an overwrite is allowed) and returns what agentd knows of the object it stored: the SHA-256 of the bytes it sent
    /// and the version id S3 answered with. The version id is `None` when the answer carried none. The file is hashed once
    /// off the runtime's threads ([`digest_file`]) and sent as a stream.
    pub async fn put_file_receipt(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
    ) -> Result<OutputPin, String> {
        let digest = digest_file(src).await?;
        self.put_digested(uri, creds, src, &digest).await
    }

    /// One streamed PUT of `src`, whose hash and size are `digest`, with the `extra` headers signed and sent.
    async fn put_stream(
        &self,
        url: &str,
        creds: &ProcessCredentials,
        src: &Path,
        digest: &FileDigest,
        extra: &[(&str, &str)],
    ) -> Result<HttpResponse, HttpError> {
        self.http
            .put_file(
                FilePut {
                    url,
                    service: "s3",
                    region: &self.region,
                    creds,
                    content_type: Some("application/octet-stream"),
                    path: src,
                    digest,
                    extra,
                },
                self.limits(),
            )
            .await
    }

    /// One PUT of `src` to `uri` (an overwrite is allowed) for a caller that has hashed the file: the record of the write.
    async fn put_digested(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
        digest: &FileDigest,
    ) -> Result<OutputPin, String> {
        let url = object_url(uri, &self.region, self.endpoint.as_deref())?;
        let resp = self
            .put_stream(&url, creds, src, digest, &[])
            .await
            .map_err(|e| e.to_string())?;
        if resp.status != 200 {
            return Err(s3_error("PUT", uri, resp.status, &resp.body));
        }
        Ok(OutputPin {
            sha256: digest.sha256_hex.clone(),
            version_id: resp.header("x-amz-version-id").map(str::to_string),
        })
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
        self.put_file_retrying_receipt(uri, creds, src, retries, pause)
            .await
            .map(|_| ())
    }

    /// [`put_file_retrying`](Self::put_file_retrying) for a caller that has hashed the file already: `digest` is the hash and
    /// the size of `src`. The look of the copy task hashes its bundle as the last part of its own work, where the spacing of the looks
    /// counts that time, and hands the hash over, so the bundle is hashed once: nothing here reads the file but the stream of each try.
    pub async fn put_file_retrying_with(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
        digest: &FileDigest,
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
            match self.put_digested(uri, creds, src, digest).await {
                Ok(_) => return Ok(()),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// [`put_file_retrying`](Self::put_file_retrying) that returns the record of the write that stored the object: its digest and
    /// the version id of that write's answer, never of an earlier try. The file is hashed once for all the tries (the callers do
    /// not change it while it is stored); a try that could not read it is tried again, as before.
    pub async fn put_file_retrying_receipt(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
        retries: u32,
        pause: Duration,
    ) -> Result<OutputPin, String> {
        let mut wait = pause;
        let mut last = String::new();
        let mut digest: Option<FileDigest> = None;
        for attempt in 0..=retries {
            if attempt > 0 {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            let hashed = match &digest {
                Some(hashed) => hashed,
                None => match digest_file(src).await {
                    Ok(hashed) => digest.insert(hashed),
                    Err(e) => {
                        last = e;
                        continue;
                    }
                },
            };
            match self.put_digested(uri, creds, src, hashed).await {
                Ok(pin) => return Ok(pin),
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
            version_id: resp.header("x-amz-version-id").map(str::to_string),
        })
    }

    /// One create-only attempt: PUTs the file `src` to `uri` only when the key holds no object. The request carries `If-None-Match: *`
    /// and `x-amz-checksum-sha256` for the body, both signed (S3 needs SigV4 for a conditional write and refuses an `x-amz-*` header that
    /// the signature does not cover). S3 checks the checksum
    /// against the bytes it received, so an object stored with this value holds exactly these bytes; it answers 412 `PreconditionFailed`
    /// when the key is taken. The body is streamed from the file, whose hash and size are `digest`.
    async fn put_new_once(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
        digest: &FileDigest,
    ) -> Attempt {
        let local = |error: String| Attempt::Failed { error, sent: false };
        let url = match object_url(uri, &self.region, self.endpoint.as_deref()) {
            Ok(u) => u,
            Err(e) => return local(e),
        };
        let resp = self
            .put_stream(
                &url,
                creds,
                src,
                digest,
                &[
                    ("if-none-match", "*"),
                    ("x-amz-checksum-sha256", &digest.sha256_base64),
                ],
            )
            .await;
        match resp {
            Err(e @ HttpError::Transport(_)) => Attempt::Failed {
                error: e.to_string(),
                sent: true,
            },
            Err(e) => local(e.to_string()),
            Ok(r) => match r.status {
                200 => Attempt::Stored(r.header("x-amz-version-id").map(str::to_string)),
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
        let digest = digest_file(src).await?;
        match self.put_new_once(uri, creds, src, &digest).await {
            Attempt::Stored(_) => Ok(PutNew::Stored),
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
        self.put_file_new_retrying_receipt(uri, creds, src, retries, pause)
            .await
            .map(|(put, _)| put)
    }

    /// [`put_file_new_retrying`](Self::put_file_new_retrying) that also returns the record of the object agentd stored: `Some` exactly
    /// when the result is [`PutNew::Stored`]. The record holds the SHA-256 of the bytes of the try that stored them and the version id S3
    /// answered with. When a lost answer is settled by the HEAD, the version id is the HEAD's: that object carries the digest of agentd's own
    /// bytes, so the digest in the record holds whichever write made the version.
    pub async fn put_file_new_retrying_receipt(
        &self,
        uri: &str,
        creds: &ProcessCredentials,
        src: &Path,
        retries: u32,
        pause: Duration,
    ) -> Result<(PutNew, Option<OutputPin>), String> {
        let mut wait = pause;
        let mut last = String::new();
        // The file is hashed once for all the tries (it does not change while it is stored); a try that could not read it is
        // tried again, as before.
        let mut digest: Option<FileDigest> = None;
        // A try left agentd: the object a lost answer committed carries the checksum of the file.
        let mut sent = false;
        for attempt in 0..=retries {
            if attempt > 0 {
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
            let hashed = match &digest {
                Some(hashed) => hashed,
                None => match digest_file(src).await {
                    Ok(hashed) => digest.insert(hashed),
                    Err(e) => {
                        last = e;
                        continue;
                    }
                },
            };
            match self.put_new_once(uri, creds, src, hashed).await {
                Attempt::Stored(version_id) => {
                    return Ok((
                        PutNew::Stored,
                        Some(OutputPin {
                            sha256: hashed.sha256_hex.clone(),
                            version_id,
                        }),
                    ));
                }
                Attempt::Exists if !sent => return Ok((PutNew::Exists, None)),
                Attempt::Exists => match self.head_checksum(uri, creds).await {
                    Ok(head) => {
                        let own = head.sha256.as_deref() == Some(hashed.sha256_base64.as_str());
                        return Ok(if own {
                            (
                                PutNew::Stored,
                                Some(OutputPin {
                                    sha256: hashed.sha256_hex.clone(),
                                    version_id: head.version_id,
                                }),
                            )
                        } else {
                            (PutNew::Exists, None)
                        });
                    }
                    Err(e) => {
                        last = format!(
                            "{uri} exists and could not be told from agentd's own object: {e}"
                        );
                    }
                },
                Attempt::Failed { error, sent: left } => {
                    last = error;
                    sent |= left;
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
