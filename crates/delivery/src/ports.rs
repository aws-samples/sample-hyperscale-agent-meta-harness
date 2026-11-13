//! The S3 and DynamoDB surfaces the delivery Lambdas use; `crate::aws` implements them and tests
//! use fakes. CodeCommit is in `crate::gateway`, the only module that calls it.

use crate::error::DeliveryError;
use harness_protocol::store::{ArtifactItem, RepoAllowItem};
use harness_protocol::target::RepoTarget;
use std::future::Future;
use std::path::Path;

/// What a put records besides the body: the content type and the SHA-256, which goes into the
/// object metadata (`x-amz-meta-sha256`) and the S3 checksum. Encryption is the bucket default
/// (SSE-KMS with the data key).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutMeta {
    pub content_type: &'static str,
    pub sha256_hex: String,
}

pub trait Blobs: Send + Sync {
    /// Downloads an object to `path`; `None` when the key does not exist.
    fn get_to_file(
        &self,
        key: &str,
        path: &Path,
    ) -> impl Future<Output = Result<Option<u64>, DeliveryError>> + Send;
    /// Downloads one version of an object to `path`; `None` when the key or the version does not exist. A client that cannot read a
    /// version reports itself unavailable: it never answers with the current object.
    fn get_version_to_file(
        &self,
        key: &str,
        version_id: &str,
        path: &Path,
    ) -> impl Future<Output = Result<Option<u64>, DeliveryError>> + Send {
        let _ = (key, version_id, path);
        async {
            Err(DeliveryError::unavailable(
                "this client cannot read a version of an object",
            ))
        }
    }
    fn put_file(
        &self,
        key: &str,
        path: &Path,
        meta: &PutMeta,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send;
    /// Every key under the prefix.
    fn list(&self, prefix: &str)
    -> impl Future<Output = Result<Vec<String>, DeliveryError>> + Send;
}

pub trait Allowlist: Send + Sync {
    /// The `REPOALLOW#...` / `META` item of the target, if any.
    fn repo_allow(
        &self,
        target: &RepoTarget,
    ) -> impl Future<Output = Result<Option<RepoAllowItem>, DeliveryError>> + Send;
    /// Writes the item only when its key is absent (the harness's entry for a repository it
    /// created); `Some` is the item already there, which the caller compares.
    fn add_repo_allow(
        &self,
        item: &RepoAllowItem,
    ) -> impl Future<Output = Result<Option<RepoAllowItem>, DeliveryError>> + Send;
}

pub trait Artifacts: Send + Sync {
    /// Writes the immutable `ART#<artifactId>` item; the same item again is a no-op, another
    /// item under the same id is an error.
    fn put_artifact(
        &self,
        item: &ArtifactItem,
    ) -> impl Future<Output = Result<(), DeliveryError>> + Send;
}
