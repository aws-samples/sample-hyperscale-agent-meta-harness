//! The stored conversation as the read side sees it.
//!
//! The worker writes the conversation as numbered, write-once gzip chunk objects under the run's agent prefix in S3; the engine holds
//! credentials that can write the same prefix, so a reader trusts nothing inside a chunk. This module defines, once and outside the
//! worker image key, what the writer will write ([`ChunkHeader`], [`ChunkRecord`], [`Chunk`], the key names), how a reader checks it
//! ([`decode_chunk`]: bounded gzip, header and record rules, nothing from the chunk is exposed unless the whole chunk is valid) and what
//! the read route answers ([`TranscriptPage`] and its parts) with the client's call helpers ([`TranscriptQuery`], [`parse_page`]).
//!
//! The stored form of a record is `{"n","t","dir","kind","data"}`: `n` counts records from 0 in each writer incarnation, `dir` is `in`
//! for what the engine sent agentd and `out` for what agentd sent it, `kind` names the update (an ACP `sessionUpdate` word, or
//! `prompt`, `prompt_result`, `cancel`, `permission_request`, `permission_response`, `session`, `gap`, `opaque`) and `data` is the
//! payload. The payload is nested because ACP's own `tool_call` carries a member named `kind` that a flat record would collide with.
//! Members this module does not know, an advisory `author` among them, are dropped when a chunk is read: no reader may use them.
//!
//! Every record that leaves the route is [`Origin::Engine`], whatever the chunk says (R6): user and harness text come from sources the
//! engine cannot write. The route also passes every string of a record through a redaction function, as a second line behind the
//! writer's, so a token or a presigned URL that slipped through is not sent.

use crate::chat::GapCause;
use crate::gzip::{GzipError, gunzip_bounded};
use harness_protocol::common::{is_ulid, is_utc_timestamp};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The number of the first chunk of a run's transcript: the writer starts at `000000000000`, so a first chunk listed with a higher
/// number means the chunks before it were never written.
pub const FIRST_SEQ: u64 = 0;
/// The only chunk format this reader accepts.
pub const CHUNK_VERSION: u32 = 1;
/// The writer's cap on the text of one chunk.
pub const MAX_CHUNK_BYTES: usize = 1 << 20;
/// The largest chunk object a reader fetches or accepts.
pub const MAX_CHUNK_OBJECT_BYTES: usize = 1 << 20;
/// A chunk may expand to this many times [`MAX_CHUNK_BYTES`] before it is refused.
pub const READER_EXPANSION: usize = 4;
/// The most bytes a chunk may inflate to.
pub const MAX_CHUNK_DECODED_BYTES: usize = MAX_CHUNK_BYTES * READER_EXPANSION;
/// The most records a chunk may hold.
pub const MAX_RECORDS_PER_CHUNK: usize = 10_000;
/// The deepest nesting a record's payload may have.
pub const MAX_RECORD_DEPTH: usize = 32;
/// The most keys one request examines in the transcript prefix.
pub const MAX_PREFIX_KEYS: usize = 20_000;
/// The most chunks one request fetches.
pub const MAX_CHUNKS_PER_REQUEST: usize = 64;
/// A record whose serialized form passes this many bytes is replaced by an omission record in a response: the page budget of the transcript
/// route (3,000,000 bytes, a little under the 6 MB a Lambda function may answer, less 64 KiB for the rest of the answer). The writer cuts a
/// record long before this (`AGENTD_TRANSCRIPT_RECORD_CAP_BYTES`, 440 KiB at most), so a record that was written whole is shown whole.
pub const MAX_RECORD_RESPONSE_BYTES: usize = 3_000_000 - 64 * 1024;
/// The longest `kind` word kept.
pub const MAX_KIND_CHARS: usize = 64;
/// The longest record time kept.
pub const MAX_TIME_CHARS: usize = 40;
/// The longest label (engine, protocol) a header may carry.
pub const MAX_LABEL_CHARS: usize = 32;
// The feature word, the cap on a completeness summary, the object keys and the file-name rule are the writer's words: they have one
// definition, in `harness_protocol`, so the reader and agentd's writer cannot drift apart.
pub use harness_protocol::direct::s3::{transcript_chunk_key as chunk_key, transcript_prefix};
pub use harness_protocol::transcript::{MAX_COMPLETENESS_ENTRIES, TRANSCRIPT_FEATURE, seq_of_name};

// ---- object keys -----------------------------------------------------------------------------------------------------

/// The sequence number of a chunk key of this run; `None` for any other key under or outside the prefix.
pub fn seq_of_key(run_id: &str, key: &str) -> Option<u64> {
    seq_of_name(key.strip_prefix(&transcript_prefix(run_id))?)
}

// ---- the stored chunk ------------------------------------------------------------------------------------------------

/// The first line of a chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkHeader {
    /// The format version ([`CHUNK_VERSION`]).
    pub v: u32,
    /// The sequence number; it must equal the number in the object's key.
    pub seq: u64,
    /// The writer's process identifier (a ULID), made once per agentd process.
    pub incarnation: String,
    /// The first and last record numbers the chunk covers.
    pub first_rec: u64,
    pub last_rec: u64,
    /// When the chunk was made (RFC 3339 UTC); display metadata.
    pub created_at: String,
    /// The engine's label and the harness protocol version of the writer.
    pub engine: String,
    pub protocol: String,
}

/// Which way a record went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// From the engine to agentd: updates, results, permission requests.
    In,
    /// From agentd to the engine: prompts, cancels, answers.
    Out,
    /// Any other word.
    #[serde(other)]
    Unknown,
}

/// One line of a chunk after its header.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkRecord {
    /// The record number within the writer's incarnation.
    pub n: u64,
    /// The writer's clock; a claim, shown as display metadata only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    pub dir: Dir,
    pub kind: String,
    #[serde(default)]
    pub data: Value,
}

/// A decoded chunk.
#[derive(Clone, Debug, PartialEq)]
pub struct Chunk {
    pub header: ChunkHeader,
    pub records: Vec<ChunkRecord>,
}

/// Why a chunk is unreadable. No variant carries text from the chunk.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("the gzip stream is invalid: {0}")]
    Gzip(GzipError),
    #[error("the chunk is over a size limit")]
    TooLarge,
    #[error("the chunk is not UTF-8")]
    NotUtf8,
    #[error("the chunk has no header line")]
    NoHeader,
    #[error("the header is invalid")]
    BadHeader,
    #[error("line {line} is not a record")]
    BadJson { line: usize },
    #[error("chunk format version {0} is not supported")]
    Version(u32),
    #[error("the header says seq {header} but the key says {key}")]
    SeqMismatch { key: u64, header: u64 },
    #[error("the chunk holds more than {} records", MAX_RECORDS_PER_CHUNK)]
    TooManyRecords,
    #[error("record numbers on line {line} are out of order or outside the header's range")]
    RecordOrder { line: usize },
    #[error("line {line} nests deeper than {}", MAX_RECORD_DEPTH)]
    Depth { line: usize },
}

impl From<GzipError> for ChunkError {
    fn from(e: GzipError) -> Self {
        match e {
            GzipError::OutputLimit => Self::TooLarge,
            other => Self::Gzip(other),
        }
    }
}

impl ChunkError {
    /// The bounded word a gap carries for this fault.
    pub fn code(&self) -> GapCode {
        match self {
            Self::Gzip(_) => GapCode::BadGzip,
            Self::TooLarge => GapCode::TooLarge,
            Self::NotUtf8 | Self::NoHeader | Self::BadJson { .. } => GapCode::BadJson,
            Self::BadHeader => GapCode::BadHeader,
            Self::Version(_) => GapCode::Version,
            Self::SeqMismatch { .. } => GapCode::SeqMismatch,
            Self::TooManyRecords => GapCode::TooManyRecords,
            Self::RecordOrder { .. } => GapCode::RecordOrder,
            Self::Depth { .. } => GapCode::Depth,
        }
    }
}

/// The deepest nesting of `v` (a scalar is 0), counted without recursion.
pub fn json_depth(v: &Value) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(v, 0usize)];
    while let Some((value, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        match value {
            Value::Array(items) => stack.extend(items.iter().map(|i| (i, depth + 1))),
            Value::Object(map) => stack.extend(map.values().map(|i| (i, depth + 1))),
            _ => {}
        }
    }
    deepest
}

fn short_label(s: &str) -> bool {
    !s.is_empty() && s.chars().count() <= MAX_LABEL_CHARS
}

fn check_header(h: &ChunkHeader, key_seq: u64) -> Result<(), ChunkError> {
    if h.v != CHUNK_VERSION {
        return Err(ChunkError::Version(h.v));
    }
    if h.seq != key_seq {
        return Err(ChunkError::SeqMismatch {
            key: key_seq,
            header: h.seq,
        });
    }
    if !is_ulid(&h.incarnation)
        || !is_utc_timestamp(&h.created_at)
        || !short_label(&h.engine)
        || !short_label(&h.protocol)
        || h.first_rec > h.last_rec
    {
        return Err(ChunkError::BadHeader);
    }
    Ok(())
}

/// Reads one chunk object (`gz` is the whole object, `key_seq` the number in its key). The whole chunk is checked before any record
/// is returned: the gzip stream (bounded, CRC and length verified), the header (version, seq equal to the key's, a ULID incarnation,
/// a UTC time, short labels), every line (a record, no deeper than [`MAX_RECORD_DEPTH`]) and the record numbers (strictly increasing
/// and inside the header's range).
pub fn decode_chunk(gz: &[u8], key_seq: u64) -> Result<Chunk, ChunkError> {
    if gz.len() > MAX_CHUNK_OBJECT_BYTES {
        return Err(ChunkError::TooLarge);
    }
    let raw = gunzip_bounded(gz, MAX_CHUNK_DECODED_BYTES)?;
    let text = std::str::from_utf8(&raw).map_err(|_| ChunkError::NotUtf8)?;
    let mut lines = text.split('\n');
    let head = lines
        .next()
        .filter(|l| !l.is_empty())
        .ok_or(ChunkError::NoHeader)?;
    let header: ChunkHeader = serde_json::from_str(head).map_err(|_| ChunkError::BadHeader)?;
    check_header(&header, key_seq)?;
    let mut records: Vec<ChunkRecord> = Vec::new();
    let mut last_n: Option<u64> = None;
    let body: Vec<&str> = lines.collect();
    // A final newline leaves one empty piece; any other empty line is not a record.
    let end = if body.last() == Some(&"") {
        body.len() - 1
    } else {
        body.len()
    };
    for (i, line) in body[..end].iter().enumerate() {
        let number = i + 2;
        if records.len() >= MAX_RECORDS_PER_CHUNK {
            return Err(ChunkError::TooManyRecords);
        }
        let rec: ChunkRecord =
            serde_json::from_str(line).map_err(|_| ChunkError::BadJson { line: number })?;
        if json_depth(&rec.data) > MAX_RECORD_DEPTH {
            return Err(ChunkError::Depth { line: number });
        }
        let in_range = rec.n >= header.first_rec && rec.n <= header.last_rec;
        if !in_range || last_n.is_some_and(|prev| rec.n <= prev) {
            return Err(ChunkError::RecordOrder { line: number });
        }
        last_n = Some(rec.n);
        records.push(rec);
    }
    Ok(Chunk { header, records })
}

/// Whether record numbers are missing before the first record of `chunk` or between its records, with no `gap` record to say why.
/// `prev` is the writer (incarnation) and last record number of the chunk read just before this one, when that chunk was read in the
/// same request. Returns the record number the hole is found before (or the first number a header claims and the chunk lacks).
///
/// A writer sheds records under load and records what it shed in a `gap` record, so a skip that a `gap` record sits at or after is
/// explained. A new incarnation restarts at 0. A number that goes backwards or repeats is not a hole (see [`GapCode::DuplicateRecord`]).
pub fn record_hole_before(chunk: &Chunk, prev: Option<(String, u64)>) -> Option<u64> {
    let expected = match &prev {
        Some((inc, last)) if *inc == chunk.header.incarnation => last.checked_add(1),
        Some(_) => Some(0),
        None => None,
    };
    record_hole_in(&chunk.header, &chunk.records, expected, false)
}

/// [`record_hole_before`] for `records` of the chunk with `header`, given the record number the first of them should have
/// (`None`: the header's `first_rec`) and whether the record just before them was a `gap` record (which explains numbers shed after
/// it). A reader that follows several writers in one request knows each writer's last number and passes the next one; a page that
/// resumes inside a chunk passes the slice after the position, the number that follows it and what the record before it was.
pub fn record_hole_in(
    header: &ChunkHeader,
    records: &[ChunkRecord],
    expected_first: Option<u64>,
    prev_is_gap: bool,
) -> Option<u64> {
    let mut expected: Option<u64> = Some(expected_first.unwrap_or(header.first_rec));
    let mut before_gap_record = prev_is_gap;
    for rec in records {
        if let Some(e) = expected
            && rec.n > e
            && rec.kind != "gap"
            && !before_gap_record
        {
            return Some(rec.n);
        }
        expected = rec.n.checked_add(1);
        before_gap_record = rec.kind == "gap";
    }
    match records.last() {
        None => Some(header.first_rec),
        Some(last) if last.kind != "gap" && last.n < header.last_rec => last.n.checked_add(1),
        Some(_) => None,
    }
}

/// What a reader keeps of a chunk to check that record numbers run on from one chunk to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub seq: u64,
    pub incarnation: String,
    pub first_n: u64,
    pub last_n: u64,
    pub first_is_gap: bool,
    pub last_is_gap: bool,
}

/// The first and last record of a chunk; `None` for a chunk with no records.
pub fn chunk_edge(chunk: &Chunk) -> Option<Edge> {
    let (first, last) = (chunk.records.first()?, chunk.records.last()?);
    Some(Edge {
        seq: chunk.header.seq,
        incarnation: chunk.header.incarnation.clone(),
        first_n: first.n,
        last_n: last.n,
        first_is_gap: first.kind == "gap",
        last_is_gap: last.kind == "gap",
    })
}

/// Whether record numbers skip between `prev` and `next` (neighbouring chunks) with no `gap` record to say why. A writer goes on from
/// its last number; a new writer starts at 0.
pub fn boundary_hole(prev: &Edge, next: &Edge) -> bool {
    if next.first_is_gap || prev.last_is_gap {
        return false;
    }
    if prev.incarnation == next.incarnation {
        prev.last_n.checked_add(1).is_some_and(|e| next.first_n > e)
    } else {
        next.first_n > 0
    }
}

/// The uncompressed bytes of a chunk (a header line, then one line per record, each ending in a newline). The writer compresses
/// these; fixtures and tools use [`chunk_gzip_stored`].
pub fn chunk_ndjson(chunk: &Chunk) -> Vec<u8> {
    let mut out = serde_json::to_vec(&chunk.header).unwrap_or_default();
    out.push(b'\n');
    for rec in &chunk.records {
        out.extend_from_slice(&serde_json::to_vec(rec).unwrap_or_default());
        out.push(b'\n');
    }
    out
}

/// A valid chunk object that stores the text without compression, for fixtures and tools.
pub fn chunk_gzip_stored(chunk: &Chunk) -> Vec<u8> {
    crate::gzip::gzip_stored(&chunk_ndjson(chunk))
}

// ---- what the route answers ------------------------------------------------------------------------------------------

/// Who a record speaks for. The transcript route answers [`Origin::Engine`] only; the other two come from the routes whose sources
/// the engine cannot write, and a client merges them for display.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    User,
    Harness,
    Engine,
}

impl From<Origin> for crate::chat::Origin {
    fn from(o: Origin) -> Self {
        match o {
            Origin::User => Self::User,
            Origin::Harness => Self::Harness,
            Origin::Engine => Self::Engine,
        }
    }
}

/// One record as the route returns it. Its identity is `(incarnation, n)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptRecord {
    /// The sequence number of the chunk it was read from.
    pub seq: u64,
    pub n: u64,
    pub incarnation: String,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
    pub dir: Dir,
    pub kind: String,
    #[serde(default)]
    pub data: Value,
}

/// `kind` kept when it is a short token, else `unknown`.
pub fn clean_kind(kind: &str) -> String {
    let ok = !kind.is_empty()
        && kind.chars().count() <= MAX_KIND_CHARS
        && kind
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
    if ok {
        kind.to_string()
    } else {
        "unknown".to_string()
    }
}

/// `v` with every string and property name passed through `redact`.
pub fn redact_value(v: Value, redact: &dyn Fn(&str) -> String) -> Value {
    match v {
        Value::String(s) => Value::String(redact(&s)),
        Value::Array(items) => {
            Value::Array(items.into_iter().map(|i| redact_value(i, redact)).collect())
        }
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, i)| (redact(&k), redact_value(i, redact)))
                .collect(),
        ),
        other => other,
    }
}

impl TranscriptRecord {
    /// The response form of a stored record: engine origin always, the kind a short token that redaction leaves alone, the time kept
    /// only when it is a UTC timestamp, the payload redacted.
    pub fn project(
        header: &ChunkHeader,
        rec: ChunkRecord,
        redact: &dyn Fn(&str) -> String,
    ) -> Self {
        Self {
            seq: header.seq,
            n: rec.n,
            incarnation: header.incarnation.clone(),
            origin: Origin::Engine,
            t: rec
                .t
                .filter(|t| t.len() <= MAX_TIME_CHARS && is_utc_timestamp(t)),
            dir: rec.dir,
            kind: {
                let kind = clean_kind(&rec.kind);
                // A kind the redactor would change is not a word of ours.
                if redact(&kind) == kind {
                    kind
                } else {
                    "unknown".to_string()
                }
            },
            data: redact_value(rec.data, redact),
        }
    }

    /// `(incarnation, n)`: unique across writers that overlapped.
    pub fn id(&self) -> (&str, u64) {
        (&self.incarnation, self.n)
    }

    /// The record that stands in for one too large to send: its place, its kind and its size.
    pub fn omitted(&self, bytes: usize) -> Self {
        Self {
            kind: "omitted".into(),
            data: serde_json::json!({ "reason": "oversized", "kind": self.kind, "bytes": bytes }),
            ..self.clone()
        }
    }
}

/// Why a range is not fully shown: the names R7, and the two a reader adds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapKind {
    /// Thoughts or binary output: left out on purpose.
    Excluded,
    /// A size cap cut it.
    Truncated,
    /// The writer shed it under load.
    Shed,
    /// Redaction withheld it.
    Suppressed,
    /// Storage failed.
    Failed,
    /// Two writers overlapped.
    Conflict,
    /// An older image stores no transcript.
    Unsupported,
    /// The run ended abnormally.
    TailUnknown,
    /// A chunk could not be read (the reader's word).
    Unreadable,
    /// A chunk that should exist is absent (the reader's word).
    Missing,
}

impl GapKind {
    /// The client's word for it.
    pub fn cause(self) -> GapCause {
        match self {
            Self::Excluded => GapCause::Excluded,
            Self::Truncated => GapCause::Truncated,
            Self::Shed => GapCause::Shed,
            Self::Suppressed => GapCause::Suppressed,
            Self::Failed | Self::Missing => GapCause::Failed,
            Self::Conflict => GapCause::Conflict,
            Self::Unsupported => GapCause::Unsupported,
            Self::TailUnknown => GapCause::TailUnknown,
            Self::Unreadable => GapCause::Unreadable,
        }
    }

    /// The words a writer may declare in a summary (the reader's own two are not among them).
    pub fn declarable(self) -> bool {
        !matches!(self, Self::Unreadable | Self::Missing)
    }
}

/// The bounded reason a reader gives for a gap; never text from the chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GapCode {
    MissingChunk,
    BadGzip,
    TooLarge,
    BadJson,
    BadHeader,
    Version,
    SeqMismatch,
    TooManyRecords,
    RecordOrder,
    Depth,
    ListLimit,
    Interleaved,
    ChangedWhileReading,
    TailMissing,
    /// Record numbers skip with no `gap` record to say why.
    RecordHole,
    /// A writer's record number appears twice in what was read; the later copy is dropped.
    DuplicateRecord,
}

impl GapCode {
    /// The gap kind a reader files this under.
    pub fn kind(self) -> GapKind {
        match self {
            Self::MissingChunk | Self::TailMissing | Self::RecordHole => GapKind::Missing,
            Self::ListLimit => GapKind::Truncated,
            Self::Interleaved | Self::ChangedWhileReading | Self::DuplicateRecord => {
                GapKind::Conflict
            }
            _ => GapKind::Unreadable,
        }
    }
}

/// A range of chunks the page could not show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptGap {
    pub kind: GapKind,
    pub code: GapCode,
    pub from_seq: u64,
    pub to_seq: u64,
}

impl TranscriptGap {
    pub fn new(code: GapCode, from_seq: u64, to_seq: u64) -> Self {
        Self {
            kind: code.kind(),
            code,
            from_seq,
            to_seq,
        }
    }
}

/// The state of a run's stored conversation (R9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptState {
    /// The run's image registered no `transcript.v1`: the progress feed is all there is.
    Unsupported,
    /// Supported, and nothing is committed yet.
    Empty,
    /// The run is active and chunks exist.
    Live,
    /// The run ended and the harness cannot say nothing was lost.
    Incomplete,
    /// The run ended and the worker's summary says nothing was lost.
    Complete,
    /// Reserved for the later per-run purge.
    Purged,
}

/// One declared range of loss (or exclusion) in a worker's completeness summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletenessEntry {
    pub kind: GapKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

/// What the worker declares at the end of the turn (turn-complete): the last chunk it wrote and what it left out or lost. It is the
/// worker's report, written by a process the engine can reach; the state `complete` means "the worker reports no loss".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transcript_seq: Option<u64>,
    #[serde(default)]
    pub entries: Vec<CompletenessEntry>,
}

impl TranscriptSummary {
    /// Whether the summary is well formed: a bounded number of entries, ranges in order, only the words a writer may declare.
    pub fn validate(&self) -> bool {
        self.entries.len() <= MAX_COMPLETENESS_ENTRIES
            && self.entries.iter().all(|e| {
                e.kind.declarable()
                    && match (e.from_seq, e.to_seq) {
                        (Some(a), Some(b)) => a <= b,
                        (None, None) => true,
                        _ => false,
                    }
            })
    }

    /// Valid, and declaring nothing but what was left out on purpose.
    pub fn is_lossless(&self) -> bool {
        self.validate() && self.entries.iter().all(|e| e.kind == GapKind::Excluded)
    }
}

/// `GET /v1/runs/{id}/transcript`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptPage {
    pub run_id: String,
    pub state: TranscriptState,
    /// The records read, engine origin, in chunk then record order.
    #[serde(default)]
    pub records: Vec<TranscriptRecord>,
    /// What this page could not show, in sequence order.
    #[serde(default)]
    pub gaps: Vec<TranscriptGap>,
    /// What the worker declared (valid entries only).
    #[serde(default)]
    pub completeness: Vec<CompletenessEntry>,
    /// The highest chunk number any record of this page came from, or whose records are all consumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_through: Option<u64>,
    /// The highest chunk number the listing showed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed_through: Option<u64>,
    /// Pass as `after` to read on from here. On a run that is still going it is the place to poll from; absent when the run has ended
    /// and everything was read, and when a listing stopped at its cap in a way that cannot be resumed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Pass as `before` to read the chunks older than this page; absent when none is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<u64>,
    /// More of what comes after this page is known or may exist: call again with `next`. Older chunks are `prev`'s business, not this.
    #[serde(default)]
    pub more: bool,
    /// Keys under the prefix that were not chunks and were skipped.
    #[serde(default)]
    pub skipped_keys: u32,
}

/// Which end a backward read starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Before {
    /// The newest chunks.
    End,
    /// The chunks numbered below this one.
    Seq(u64),
}

/// The query of `GET /v1/runs/{id}/transcript`. `after` and `before` exclude each other; with neither the read starts at the first
/// chunk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TranscriptQuery {
    /// The `next` of the previous page, or the `open` of a search hit.
    pub after: Option<String>,
    pub before: Option<Before>,
    /// Chunks to read, 1 to [`MAX_CHUNKS_PER_REQUEST`].
    pub limit: Option<u32>,
}

/// A query value with everything but unreserved characters percent-encoded (RFC 3986).
pub fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl TranscriptQuery {
    /// The request path with its query string.
    pub fn path(&self, run_id: &str) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(a) = &self.after {
            parts.push(format!("after={}", encode_component(a)));
        }
        match self.before {
            Some(Before::End) => parts.push("before=end".into()),
            Some(Before::Seq(n)) => parts.push(format!("before={n}")),
            None => {}
        }
        if let Some(l) = self.limit {
            parts.push(format!("limit={l}"));
        }
        let base = format!("/v1/runs/{run_id}/transcript");
        if parts.is_empty() {
            base
        } else {
            format!("{base}?{}", parts.join("&"))
        }
    }
}

/// Reads a response body.
pub fn parse_page(body: &[u8]) -> Result<TranscriptPage, String> {
    serde_json::from_slice(body).map_err(|e| e.to_string())
}
