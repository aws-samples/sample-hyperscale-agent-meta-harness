//! Fakes shared by the transcript and search route tests: an in-memory bucket that refuses what S3 and the control-api role's IAM policy
//! refuse, and a store with a counting search read. The bucket records every call, so a test can prove which prefixes a route touched.
//!
//! What the bucket enforces, with the source of each rule:
//! - ListObjectsV2: `max-keys` from 1 to 1,000 (the API clamps larger values; a caller that sends one has miscounted), a prefix and a
//!   `start-after` of at most 1,024 bytes of UTF-8, keys in binary order, `start-after` exclusive, `IsTruncated` set when more keys
//!   follow (https://docs.aws.amazon.com/AmazonS3/latest/API/API_ListObjectsV2.html).
//! - GetObject: a key of 1 to 1,024 bytes; a missing key is NoSuchKey; `If-Match` that differs from the ETag is 412 PreconditionFailed.
//! - PutObject with `If-None-Match: *`: a key that exists is 412 (the writer's rule, used here to build fixtures like the writer does).
//! - The control-api role's policy (infra/lib/stacks/control.ts): `s3:ListBucket` only with a prefix matching
//!   [`LIST_PREFIX_PATTERN`], `s3:GetObject` only on [`GET_KEY_PATTERN`]; anything else is AccessDenied.
#![allow(dead_code, clippy::unwrap_used)]

use control_api::handler::{Blobs, ObjectError, ObjectInfo, ObjectPage, Store, StoreError};
use control_api::search::SearchPage;
use control_api::transcript::TranscriptFacts;
use harness_protocol::api::v1::DownloadUrlResponse;
use harness_protocol::envelope::Envelope;
use harness_protocol::store::{ArtifactItem, ControlItem, GoalItem, IdempotencyItem, RunItem};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

/// The `s3:prefix` condition of the control-api role's ListBucket statement.
pub const LIST_PREFIX_PATTERN: &str = "runs/*/agents/direct/turns/1/transcript/*";
/// The resource pattern of its GetObject statement for transcript chunks.
pub const GET_KEY_PATTERN: &str = "runs/*/agents/direct/turns/1/transcript/*";

pub fn star_match(pattern: &str, text: &str) -> bool {
    control_api::handler::principal_matches(pattern, text)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    List {
        prefix: String,
        start_after: Option<String>,
        max_keys: usize,
    },
    Get {
        key: String,
    },
}

#[derive(Debug, Default)]
pub struct Bucket {
    pub objects: Mutex<BTreeMap<String, (Vec<u8>, String)>>,
    pub log: Mutex<Vec<Access>>,
    /// Slept before every list and get.
    pub latency: Duration,
    /// List calls fail.
    pub list_down: bool,
    /// Get calls fail.
    pub get_down: bool,
    /// After the first list call, replace this key's bytes (so its ETag differs from the listing's).
    pub swap_after_list: Mutex<Option<(String, Vec<u8>)>>,
    /// Bytes every get returns for this key, whatever is stored (a lying length).
    pub bytes_returned: Mutex<u64>,
}

fn etag_of(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(body);
    format!(
        "\"{}\"",
        d.iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

impl Bucket {
    pub fn put(&self, key: &str, body: Vec<u8>) {
        let e = etag_of(&body);
        self.objects
            .lock()
            .unwrap()
            .insert(key.to_string(), (body, e));
    }

    /// The writer's PutObject with `If-None-Match: *`: false (HTTP 412) when the key exists.
    pub fn put_if_absent(&self, key: &str, body: Vec<u8>) -> bool {
        let mut g = self.objects.lock().unwrap();
        if g.contains_key(key) {
            return false;
        }
        let e = etag_of(&body);
        g.insert(key.to_string(), (body, e));
        true
    }

    pub fn len(&self) -> usize {
        self.objects.lock().unwrap().len()
    }

    pub fn lists(&self) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|a| matches!(a, Access::List { .. }))
            .count()
    }

    pub fn gets(&self) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|a| matches!(a, Access::Get { .. }))
            .count()
    }

    pub fn calls(&self) -> usize {
        self.log.lock().unwrap().len()
    }

    /// Every prefix listed and every key read.
    pub fn touched(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|a| match a {
                Access::List { prefix, .. } => prefix.clone(),
                Access::Get { key } => key.clone(),
            })
            .collect()
    }
}

fn denied(what: &str) -> StoreError {
    StoreError::Unavailable(format!("AccessDenied: the role may not {what}"))
}

impl Blobs for &Bucket {
    async fn put(&self, key: &str, body: &[u8]) -> Result<String, StoreError> {
        Bucket::put(self, key, body.to_vec());
        Ok(format!("s3://fake/{key}"))
    }

    fn bucket(&self) -> &str {
        "fake-harness-bucket"
    }

    async fn presign_get(
        &self,
        _key: &str,
        _ttl: Duration,
    ) -> Result<DownloadUrlResponse, StoreError> {
        Err(StoreError::Unavailable(
            "a transcript route never presigns".into(),
        ))
    }

    async fn list_objects(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<ObjectPage, StoreError> {
        self.log.lock().unwrap().push(Access::List {
            prefix: prefix.to_string(),
            start_after: start_after.map(str::to_string),
            max_keys,
        });
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        if self.list_down {
            return Err(StoreError::Unavailable(
                "SECRET-STORE-DETAIL: list failed".into(),
            ));
        }
        if !(1..=1000).contains(&max_keys) {
            return Err(StoreError::Unavailable(format!(
                "InvalidArgument: max-keys {max_keys}"
            )));
        }
        if prefix.len() > 1024 || start_after.is_some_and(|a| a.len() > 1024) {
            return Err(StoreError::Unavailable("KeyTooLongError".into()));
        }
        if !star_match(LIST_PREFIX_PATTERN, &format!("{prefix}x")) || prefix.is_empty() {
            return Err(denied(&format!("list the prefix {prefix:?}")));
        }
        let g = self.objects.lock().unwrap();
        let mut objects = Vec::new();
        let mut truncated = false;
        for (key, (body, etag)) in g.iter() {
            if !key.starts_with(prefix) || start_after.is_some_and(|a| key.as_str() <= a) {
                continue;
            }
            if objects.len() == max_keys {
                truncated = true;
                break;
            }
            objects.push(ObjectInfo {
                key: key.clone(),
                size: body.len() as u64,
                etag: Some(etag.clone()),
            });
        }
        drop(g);
        if let Some((key, body)) = self.swap_after_list.lock().unwrap().take() {
            Bucket::put(self, &key, body);
        }
        Ok(ObjectPage { objects, truncated })
    }

    async fn get_object(
        &self,
        key: &str,
        max_bytes: usize,
        if_match: Option<&str>,
    ) -> Result<Option<Vec<u8>>, ObjectError> {
        self.log.lock().unwrap().push(Access::Get {
            key: key.to_string(),
        });
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        if self.get_down {
            return Err(ObjectError::Store(StoreError::Unavailable(
                "SECRET-STORE-DETAIL: get failed".into(),
            )));
        }
        if key.is_empty() || key.len() > 1024 {
            return Err(ObjectError::Store(StoreError::Unavailable(
                "KeyTooLongError".into(),
            )));
        }
        if !star_match(GET_KEY_PATTERN, key) {
            return Err(ObjectError::Store(denied(&format!("read {key:?}"))));
        }
        let g = self.objects.lock().unwrap();
        let Some((body, etag)) = g.get(key) else {
            return Ok(None);
        };
        if if_match.is_some_and(|m| m != etag) {
            return Err(ObjectError::Changed);
        }
        if body.len() > max_bytes {
            return Err(ObjectError::TooLarge);
        }
        *self.bytes_returned.lock().unwrap() += body.len() as u64;
        Ok(Some(body.clone()))
    }
}

/// One search read: the partition, the sort-key floor, the key to continue after and the limit.
pub type SearchCall = (String, String, Option<String>, usize);

/// A store with run items, per-run transcript facts and a counting search read.
#[derive(Debug, Default)]
pub struct Tables {
    pub runs: Mutex<BTreeMap<String, RunItem>>,
    pub facts: Mutex<BTreeMap<String, TranscriptFacts>>,
    /// Search items by partition key, in any order.
    pub search: Mutex<BTreeMap<String, Vec<ham_core::runsearch::SearchItem>>>,
    /// Search reads: (partition, floor, after, limit) of each call.
    pub search_calls: Mutex<Vec<SearchCall>>,
    /// Items and bytes returned by search reads.
    pub search_items_read: Mutex<usize>,
    pub search_bytes_read: Mutex<usize>,
    /// DynamoDB stops a Query page at this many bytes of items (1 MB in the service).
    pub page_bytes: usize,
    pub latency: Duration,
    pub down: bool,
    /// Every this-many-th item a read examines cannot be decoded (it counts as examined and is left out of the page).
    pub bad_every: usize,
    /// The first this-many reads return only undecodable items.
    pub bad_pages: usize,
}

impl Tables {
    pub fn with_page_bytes(page_bytes: usize) -> Self {
        Self {
            page_bytes,
            ..Self::default()
        }
    }
}

impl Store for &Tables {
    async fn claim_idempotency(&self, _c: &IdempotencyItem) -> Result<IdempotencyItem, StoreError> {
        Err(StoreError::Unavailable("not used".into()))
    }
    async fn create_run(
        &self,
        _r: &RunItem,
        _g: &GoalItem,
        _e: &Envelope,
    ) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("not used".into()))
    }
    async fn get_run(&self, run_id: &str) -> Result<Option<RunItem>, StoreError> {
        if self.down {
            return Err(StoreError::Unavailable(
                "SECRET-STORE-DETAIL: table down".into(),
            ));
        }
        Ok(self.runs.lock().unwrap().get(run_id).cloned())
    }
    async fn get_root_goal(&self, _run_id: &str) -> Result<Option<GoalItem>, StoreError> {
        Ok(None)
    }
    async fn list_runs(
        &self,
        _o: &str,
        _n: Option<&str>,
    ) -> Result<(Vec<RunItem>, Option<String>), StoreError> {
        Ok((vec![], None))
    }
    async fn events(
        &self,
        _r: &str,
        _s: u8,
        _a: Option<&str>,
        _l: usize,
    ) -> Result<Vec<Envelope>, StoreError> {
        Ok(vec![])
    }
    async fn list_artifacts(&self, _r: &str) -> Result<Vec<ArtifactItem>, StoreError> {
        Ok(vec![])
    }
    async fn get_artifact(&self, _r: &str, _a: &str) -> Result<Option<ArtifactItem>, StoreError> {
        Ok(None)
    }
    async fn put_control(&self, _c: &ControlItem) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("not used".into()))
    }

    async fn transcript_facts(&self, run_id: &str) -> Result<TranscriptFacts, StoreError> {
        Ok(self
            .facts
            .lock()
            .unwrap()
            .get(run_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn search_items(
        &self,
        owner_pk: &str,
        sk_floor: &str,
        sk_after: Option<&str>,
        limit: usize,
    ) -> Result<SearchPage, StoreError> {
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        self.search_calls.lock().unwrap().push((
            owner_pk.to_string(),
            sk_floor.to_string(),
            sk_after.map(str::to_string),
            limit,
        ));
        if self.down {
            return Err(StoreError::Unavailable(
                "SECRET-STORE-DETAIL: query failed".into(),
            ));
        }
        // The key condition: this partition, sort keys from the floor to the ceiling, newest first, after the start key.
        let call_no = self.search_calls.lock().unwrap().len();
        let g = self.search.lock().unwrap();
        let mut items: Vec<_> = g
            .get(owner_pk)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| {
                i.sk.as_str() >= sk_floor && i.sk.as_str() <= ham_core::runsearch::SK_CEILING
            })
            .collect();
        items.sort_by(|a, b| b.sk.cmp(&a.sk));
        if let Some(after) = sk_after {
            items.retain(|i| i.sk.as_str() < after);
        }
        let mut page = Vec::new();
        let mut examined = 0u32;
        let mut last_sk = None;
        let mut bytes = 0usize;
        let mut more = false;
        for item in items {
            let size = item.body.as_ref().map_or(0, String::len)
                + item.segs.as_ref().map_or(0, String::len)
                + item.prompt_head.as_ref().map_or(0, String::len)
                + 200;
            if examined as usize == limit
                || (self.page_bytes > 0 && examined > 0 && bytes + size > self.page_bytes)
            {
                more = true;
                break;
            }
            bytes += size;
            examined += 1;
            last_sk = Some(item.sk.clone());
            let number: u64 = item
                .run_id
                .get(6..)
                .and_then(|d| d.parse().ok())
                .unwrap_or(0);
            let bad = call_no <= self.bad_pages
                || (self.bad_every > 0 && number.is_multiple_of(self.bad_every as u64));
            if !bad {
                page.push(item);
            }
        }
        *self.search_items_read.lock().unwrap() += examined as usize;
        *self.search_bytes_read.lock().unwrap() += bytes;
        let next_sk = more.then(|| last_sk.clone()).flatten();
        Ok(SearchPage {
            items: page,
            examined,
            last_sk,
            next_sk,
        })
    }
}

#[derive(Debug)]
pub struct NoExecution;

impl control_api::handler::Execution for NoExecution {
    async fn start(&self, _name: &str, _input: &serde_json::Value) -> Result<(), StoreError> {
        Err(StoreError::Unavailable("not used".into()))
    }
}
