//! Shared by the transcript tests: an in-memory chunk store that can fail, stall and answer 412 the way S3 does, and the reader's rules
//! (`ham_core::transcript::decode_chunk`) applied to what the writer stored.
#![allow(dead_code, clippy::unwrap_used)]

use agentd::BoxFuture;
use agentd::direct::transcript::store::{ChunkStore, PutOutcome};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const RUN: &str = "01M4XTESTRUNDIRECT00000001";
pub const ULID_A: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
pub const ULID_B: &str = "01M4ZHFBV5SDX7J92E6YRH4QTX";

pub fn prefix() -> String {
    harness_protocol::direct::s3::transcript_prefix(RUN)
}

pub fn key(seq: u64) -> String {
    harness_protocol::direct::s3::transcript_chunk_key(RUN, seq)
}

pub fn seq_of(key: &str) -> Option<u64> {
    harness_protocol::transcript::seq_of_name(key.rsplit('/').next()?)
}

/// What a test makes S3 do.
#[derive(Default)]
pub struct MemStore {
    pub objects: Mutex<BTreeMap<String, Vec<u8>>>,
    /// Every put that arrived: the key and the size.
    pub attempts: Mutex<Vec<(String, usize)>>,
    /// The next this many puts answer a failure (`u32::MAX`: every one).
    pub fail: Mutex<u32>,
    /// The next this many puts store the object and then answer a failure (the answer was lost).
    pub ambiguous: Mutex<u32>,
    /// How long a put takes.
    pub delay: Mutex<Duration>,
    pub list_fails: AtomicBool,
    pub lists: Mutex<u32>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_raw(&self, key: &str, bytes: Vec<u8>) {
        self.objects.lock().unwrap().insert(key.to_string(), bytes);
    }

    pub fn keys(&self) -> Vec<String> {
        self.objects.lock().unwrap().keys().cloned().collect()
    }

    pub fn seqs(&self) -> Vec<u64> {
        self.keys().iter().filter_map(|k| seq_of(k)).collect()
    }

    pub fn fail_puts(&self, n: u32) {
        *self.fail.lock().unwrap() = n;
    }

    pub fn attempts_on(&self, key: &str) -> usize {
        self.attempts
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == key)
            .count()
    }
}

impl ChunkStore for MemStore {
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        start_after: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async move {
            *self.lists.lock().unwrap() += 1;
            if self.list_fails.load(Ordering::SeqCst) {
                return Err("listing failed".into());
            }
            Ok(self
                .objects
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix) && start_after.is_none_or(|s| k.as_str() > s))
                .cloned()
                .collect())
        })
    }

    fn head<'a>(&'a self, key: &'a str, len: usize) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let o = self.objects.lock().unwrap();
            let b = o.get(key).ok_or_else(|| "no such key".to_string())?;
            Ok(b[..len.min(b.len())].to_vec())
        })
    }

    fn put_new<'a>(&'a self, key: &'a str, body: Vec<u8>) -> BoxFuture<'a, PutOutcome> {
        Box::pin(async move {
            self.attempts
                .lock()
                .unwrap()
                .push((key.to_string(), body.len()));
            let delay = *self.delay.lock().unwrap();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            {
                let mut f = self.fail.lock().unwrap();
                if *f > 0 {
                    if *f != u32::MAX {
                        *f -= 1;
                    }
                    return PutOutcome::Failed("503 SlowDown".into());
                }
            }
            let mut o = self.objects.lock().unwrap();
            if o.contains_key(key) {
                return PutOutcome::Exists;
            }
            o.insert(key.to_string(), body);
            drop(o);
            let mut a = self.ambiguous.lock().unwrap();
            if *a > 0 {
                *a -= 1;
                return PutOutcome::Failed("timeout".into());
            }
            PutOutcome::Stored
        })
    }
}

// ---- the reader's rules -------------------------------------------------------------------------------------------------------

pub fn gunzip(gz: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    GzDecoder::new(gz)
        .take(limit as u64)
        .read_to_end(&mut out)
        .map_err(|e| format!("not gzip: {e}"))?;
    Ok(out)
}

fn depth(v: &Value) -> usize {
    match v {
        Value::Array(a) => 1 + a.iter().map(depth).max().unwrap_or(0),
        Value::Object(m) => 1 + m.values().map(depth).max().unwrap_or(0),
        _ => 0,
    }
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.as_bytes()[0] <= b'7'
        && s.bytes()
            .all(|c| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&c))
}

/// One chunk read with the rules of the route's reader: its header and its records, or why it is refused.
pub fn read_chunk(gz: &[u8], key_seq: u64) -> Result<(Value, Vec<Value>), String> {
    if gz.len() > 1 << 20 {
        return Err("object over 1 MiB".into());
    }
    let raw = gunzip(gz, 4 << 20)?;
    if raw.len() >= 4 << 20 {
        return Err("inflates past 4 MiB".into());
    }
    let text = String::from_utf8(raw).map_err(|_| "not UTF-8".to_string())?;
    let mut lines = text.split('\n');
    let head: Value =
        serde_json::from_str(lines.next().filter(|l| !l.is_empty()).ok_or("no header")?)
            .map_err(|e| e.to_string())?;
    if head["v"] != 1 || head["seq"].as_u64() != Some(key_seq) {
        return Err("version or seq".into());
    }
    let (first, last) = (
        head["firstRec"].as_u64().ok_or("firstRec")?,
        head["lastRec"].as_u64().ok_or("lastRec")?,
    );
    let time = head["createdAt"].as_str().unwrap_or_default();
    let label = |k: &str| {
        head[k]
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.chars().count() <= 32)
    };
    if !head["incarnation"].as_str().is_some_and(is_ulid)
        || !time.ends_with('Z')
        || time.len() < 20
        || !label("engine")
        || !label("protocol")
        || first > last
    {
        return Err("bad header".into());
    }
    let mut body: Vec<&str> = lines.collect();
    if body.last() == Some(&"") {
        body.pop();
    }
    if body.len() > 10_000 {
        return Err("too many records".into());
    }
    let mut recs = Vec::new();
    let mut prev: Option<u64> = None;
    for (i, l) in body.iter().enumerate() {
        let rec: Value =
            serde_json::from_str(l).map_err(|_| format!("line {} is not a record", i + 2))?;
        let n = rec["n"].as_u64().ok_or("n")?;
        if depth(&rec["data"]) > 32 {
            return Err("too deep".into());
        }
        if n < first || n > last || prev.is_some_and(|p| n <= p) {
            return Err(format!("record numbers out of order at line {}", i + 2));
        }
        if !matches!(rec["dir"].as_str(), Some("in" | "out"))
            || !rec["kind"]
                .as_str()
                .is_some_and(|k| !k.is_empty() && k.chars().count() <= 64)
        {
            return Err("dir or kind".into());
        }
        prev = Some(n);
        recs.push(rec);
    }
    Ok((head, recs))
}

/// Every object of the store that has a chunk name, read with the reader's rules, in key order: `(seq, header, records)`.
///
/// With `HAMH_TRANSCRIPT_DUMP_DIR` set, every object is also written there as `<test name>-<key file name>`, so the reader of another crate
/// (`ham_core::transcript::decode_chunk`) can be run over what the writer made.
pub fn read_all(store: &MemStore) -> Vec<(u64, Value, Vec<Value>)> {
    let o = store.objects.lock().unwrap();
    if let Some(dir) = std::env::var_os("HAMH_TRANSCRIPT_DUMP_DIR") {
        let test = std::thread::current()
            .name()
            .unwrap_or("test")
            .replace("::", "-");
        for (k, v) in o.iter().filter(|(k, _)| seq_of(k).is_some()) {
            let file = k.rsplit('/').next().unwrap_or(k);
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{test}-{file}")), v);
        }
    }
    o.iter()
        .filter_map(|(k, v)| {
            let seq = seq_of(k)?;
            let (h, r) =
                read_chunk(v, seq).unwrap_or_else(|e| panic!("{k} is refused by the reader: {e}"));
            Some((seq, h, r))
        })
        .collect()
}

/// All records of the store, in key order then record order, with the incarnation they came from.
pub fn all_records(store: &MemStore) -> Vec<(String, Value)> {
    read_all(store)
        .into_iter()
        .flat_map(|(_, h, recs)| {
            let inc = h["incarnation"].as_str().unwrap().to_string();
            recs.into_iter().map(move |r| (inc.clone(), r))
        })
        .collect()
}
