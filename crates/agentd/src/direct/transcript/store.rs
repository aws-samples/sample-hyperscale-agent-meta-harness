//! Where chunk objects go: the three calls the writer makes, so the same writer runs over the run's bucket
//! ([`crate::direct::transcript::s3store::S3Store`]) and over an in-memory store in tests.

use crate::BoxFuture;

/// What one write-once put came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    /// The object was written.
    Stored,
    /// The key already holds an object (HTTP 412 for `If-None-Match: *`).
    Exists,
    /// The put failed or its answer was lost: the object may or may not exist.
    Failed(String),
}

/// The three calls of the writer. Keys are whole object keys.
pub trait ChunkStore: Send + Sync {
    /// Every key under `prefix` after `start_after`, in order.
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<String>, String>>;
    /// The first `len` bytes of an object.
    fn head<'a>(&'a self, key: &'a str, len: usize) -> BoxFuture<'a, Result<Vec<u8>, String>>;
    /// A write that fails when the key exists.
    fn put_new<'a>(&'a self, key: &'a str, body: Vec<u8>) -> BoxFuture<'a, PutOutcome>;
}
