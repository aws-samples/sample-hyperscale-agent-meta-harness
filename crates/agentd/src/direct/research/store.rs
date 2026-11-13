//! Reading what the research route stored. The route writes items under
//! `runs/<run>/agents/direct/research/` of the harness bucket, where the worker's role can read and cannot write (a bucket-policy Deny).
//! agentd reads with the run's own session credentials and trusts nothing it reads until the hashes in the manifest agree.

use crate::BoxFuture;
use crate::creds::CredStore;
use crate::http::{SignedHttp, SignedRequest};
use crate::s3::{object_url, s3_error};
use std::sync::Arc;

/// `runs/<run>/agents/direct/research/<rel>`.
pub fn research_key(run: &str, rel: &str) -> String {
    format!("{}{rel}", harness_protocol::research::s3::prefix(run))
}

/// Why an object could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The key does not exist.
    Missing,
    /// Anything else: the transport, the credentials, a refusal. The text names no secret.
    Unavailable(String),
}

/// The bucket, as the placement code reads it. A seam so tests use a fake.
pub trait ObjectStore: Send + Sync {
    /// The whole object at `key` (a key of the harness bucket).
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Vec<u8>, StoreError>>;
}

/// The real bucket, over SigV4-signed HTTP with the session credentials.
pub struct S3Store {
    http: SignedHttp,
    creds: Arc<CredStore>,
    region: String,
    endpoint: Option<String>,
    bucket: String,
}

impl std::fmt::Debug for S3Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Store")
            .field("bucket", &self.bucket)
            .finish_non_exhaustive()
    }
}

impl S3Store {
    pub fn new(
        creds: Arc<CredStore>,
        region: String,
        endpoint: Option<String>,
        bucket: String,
    ) -> Self {
        Self {
            http: SignedHttp::new(),
            creds,
            region,
            endpoint,
            bucket,
        }
    }
}

impl ObjectStore for S3Store {
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Vec<u8>, StoreError>> {
        Box::pin(async move {
            let uri = format!("s3://{}/{key}", self.bucket);
            let url = object_url(&uri, &self.region, self.endpoint.as_deref())
                .map_err(StoreError::Unavailable)?;
            let creds = self
                .creds
                .current()
                .await
                .map_err(|e| StoreError::Unavailable(e.to_string()))?;
            let resp = self
                .http
                .send(SignedRequest {
                    method: "GET",
                    url: &url,
                    service: "s3",
                    region: &self.region,
                    creds: &creds,
                    content_type: None,
                    body: vec![],
                })
                .await
                .map_err(|e| StoreError::Unavailable(e.to_string()))?;
            match resp.status {
                200 => Ok(resp.body),
                404 => Err(StoreError::Missing),
                s => Err(StoreError::Unavailable(s3_error(
                    "GET", &uri, s, &resp.body,
                ))),
            }
        })
    }
}
