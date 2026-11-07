//! `GET /v1/runs/{runId}/transcript?after=&before=&limit=`: the stored conversation of a run, read from its chunk objects.
//!
//! The chunks sit in a prefix the engine can write, so the route trusts nothing inside them. It answers for the owner only (another
//! caller's run is a 404 before any parameter is read or any object is touched), lists the transcript prefix, reads whole chunks
//! within hard limits (listing cap, chunks per request, object and inflated size, records, nesting, a response budget, a time budget)
//! and returns every record as engine origin with its strings redacted again. Whatever it cannot show is named in `gaps`; it never
//! leaves a silent hole. State comes from the harness: whether the run's image writes a transcript and whether the run has ended
//! (the Run item), and what the worker declared at turn-complete (the Turn item's summary): `complete` means the worker reported no
//! loss, not that the harness verified it.
//!
//! Paging: `after` takes the `next` of the previous page (or the `open` of a search hit) and reads on from that chunk and record;
//! `before` (`end` or a chunk number) reads the chunks just below it, so a client can open the end of a long run, and `prev` of the
//! answer is the `before` for the chunks older than the page. Records are identified by `(incarnation, n)`, so pages that overlap a
//! chunk can be joined without duplicates. `more` is about reading forward with `next`; `next` is absent when the run has ended and
//! the page reached the end of the prefix (or when the listing stopped where no safe cursor can continue it). A cursor cut inside a
//! chunk carries the chunk's ETag: if the object changed since, the chunk is reported as changed instead of being read at a stale
//! index. No chunk body, cursor or query value is written to a log.
//!
//! Time: the page stops listing and reading at the soft budget ([`TIME_BUDGET`]) and answers with a cursor; no storage call runs
//! past the hard deadline ([`HARD_EXTRA`] later), so the answer always leaves before the function's own timeout.

use crate::cursor::{self, Position};
use crate::handler::{
    Blobs, Deps, Execution, ObjectError, ObjectInfo, Reply, Store, StoreError, V1Request, error,
    json, not_found, owned, unavailable,
};
use ham_core::redact::redact_tokens;
use ham_core::transcript::{
    Before, CompletenessEntry, FIRST_SEQ, GapCode, GapKind, MAX_CHUNK_OBJECT_BYTES,
    MAX_CHUNKS_PER_REQUEST, MAX_PREFIX_KEYS, MAX_RECORD_RESPONSE_BYTES, TranscriptGap,
    TranscriptPage, TranscriptRecord, TranscriptState, TranscriptSummary, decode_chunk,
    record_hole_in, seq_of_key, transcript_prefix,
};
use harness_protocol::goal::GoalStatus;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::time::{Duration, Instant};

/// Chunks one request reads when `limit` is absent.
pub const DEFAULT_CHUNKS: usize = 32;
/// The most a response may carry (the Lambda's synchronous limit is 6 MB).
pub const RESPONSE_BUDGET: usize = 3_000_000;
/// What the record budget leaves for the rest of the answer: up to 256 gaps and 256 completeness entries (about 50 KB) and cursors.
pub const METADATA_RESERVE: usize = 64 * 1024;
/// The time a request may spend listing and reading before it ends the page (the function's own timeout is 15 s).
pub const TIME_BUDGET: Duration = Duration::from_secs(8);
/// How long after the soft budget a storage call already under way may still run; no call starts or runs past it.
pub const HARD_EXTRA: Duration = Duration::from_secs(4);
/// The most gaps one page reports; more ends the page.
pub const MAX_GAPS_PER_PAGE: usize = 256;
/// The most records one page carries.
pub const MAX_RECORDS_PER_PAGE: usize = 10_000;
/// Keys asked of one list call (the S3 maximum).
const LIST_PAGE: usize = 1000;

/// What the harness knows about a run's stored conversation, apart from the chunks.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TranscriptFacts {
    /// The run's Agent item registered `transcript.v1`: its image writes chunks.
    pub supported: bool,
    /// The run's conversation was purged (reserved for the later purge).
    pub purged: bool,
    pub summary: SummaryFact,
}

/// The worker's turn-complete summary as stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum SummaryFact {
    /// None was recorded.
    #[default]
    Absent,
    /// One was recorded and is not well formed: it counts as none.
    Invalid,
    Valid(TranscriptSummary),
}

/// The limits of one request; tests lower them to make small fixtures reach them.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_chunks: usize,
    pub max_keys: usize,
    pub page_bytes: usize,
    pub time: Duration,
    pub record_bytes: usize,
    pub max_records: usize,
    pub max_gaps: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_chunks: MAX_CHUNKS_PER_REQUEST,
            max_keys: MAX_PREFIX_KEYS,
            page_bytes: RESPONSE_BUDGET - METADATA_RESERVE,
            time: TIME_BUDGET,
            record_bytes: MAX_RECORD_RESPONSE_BYTES,
            max_records: MAX_RECORDS_PER_PAGE,
            max_gaps: MAX_GAPS_PER_PAGE,
        }
    }
}

struct Params {
    after: Option<Position>,
    before: Option<Before>,
    limit: usize,
}

fn parse_params(
    q: &BTreeMap<String, String>,
    run_id: &str,
    caller: &str,
    limits: &Limits,
) -> Result<Params, String> {
    let after = match q.get("after") {
        Some(t) => Some(cursor::decode(run_id, caller, t).map_err(|e| e.to_string())?),
        None => None,
    };
    let before = match q.get("before").map(String::as_str) {
        None => None,
        Some("end") => Some(Before::End),
        Some(n) if (1..=12).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()) => {
            Some(Before::Seq(
                n.parse()
                    .map_err(|_| "before is not a chunk number".to_string())?,
            ))
        }
        Some(_) => return Err("before must be `end` or a chunk number".into()),
    };
    if after.is_some() && before.is_some() {
        return Err("after and before exclude each other".into());
    }
    let max = limits.max_chunks;
    let limit = match q.get("limit").map(String::as_str) {
        None => DEFAULT_CHUNKS.min(max),
        Some(n) if (1..=3).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()) => {
            let v: usize = n.parse().unwrap_or(0);
            if v == 0 || v > max {
                return Err(format!("limit must be from 1 to {max}"));
            }
            v
        }
        Some(_) => return Err(format!("limit must be from 1 to {max}")),
    };
    Ok(Params {
        after,
        before,
        limit,
    })
}

/// Runs a storage call and gives up at the hard deadline; a call that would start after it fails at once.
async fn within<T>(hard: Instant, fut: impl Future<Output = T>) -> Result<T, StoreError> {
    let left = hard.saturating_duration_since(Instant::now());
    tokio::time::timeout(left, fut)
        .await
        .map_err(|_| StoreError::Unavailable("a storage call passed its deadline".into()))
}

/// A chunk object the listing showed.
#[derive(Clone, Debug)]
struct Listed {
    seq: u64,
    info: ObjectInfo,
}

#[derive(Default)]
struct Listing {
    /// Chunk keys at or above the start, in key order.
    valid: Vec<Listed>,
    skipped: u32,
    /// The last page showed the end of the prefix.
    exhausted: bool,
    /// The cap on examined keys stopped the listing.
    cap_hit: bool,
    /// The soft time budget stopped the listing.
    timed_out: bool,
    last_key: Option<String>,
}

/// What a read produced.
#[derive(Default)]
struct Read {
    records: Vec<TranscriptRecord>,
    gaps: Vec<TranscriptGap>,
    published_through: Option<u64>,
    /// The first chunk examined, and whether its records were read from the start.
    first: Option<(u64, bool)>,
    /// The highest chunk number examined (read, or found unreadable).
    examined_through: Option<u64>,
    /// Where the unread part starts, when the page ended inside the listing.
    stop: Option<Position>,
    diagnostics: Vec<String>,
}

fn serialized_len(r: &TranscriptRecord) -> usize {
    serde_json::to_vec(r).map_or(usize::MAX / 2, |v| v.len())
}

struct Ctx<'a, B> {
    blobs: &'a B,
    run_id: &'a str,
    prefix: String,
    limits: &'a Limits,
    /// The soft deadline: listing and reading stop here and answer with a cursor.
    soft: Instant,
    /// No storage call runs past this.
    hard: Instant,
}

impl<B: Blobs> Ctx<'_, B> {
    fn key_of(&self, suffix: &str) -> String {
        format!("{}{suffix}", self.prefix)
    }

    /// Lists the prefix from `start_after`, collecting chunk keys numbered `from_seq` or more. `want` stops the listing once that many
    /// were found; `below` stops it at the first chunk numbered at or above it.
    async fn list(
        &self,
        start_after: Option<String>,
        from_seq: u64,
        want: Option<usize>,
        below: Option<u64>,
    ) -> Result<Listing, StoreError> {
        let mut out = Listing::default();
        let mut examined = 0usize;
        let mut after = start_after;
        'pages: loop {
            let room = self.limits.max_keys.saturating_sub(examined).min(LIST_PAGE);
            if room == 0 {
                out.cap_hit = true;
                break;
            }
            // A listing that has examined something goes on only while the soft budget lasts; the cursor takes up the rest.
            if examined > 0 && Instant::now() >= self.soft {
                out.timed_out = true;
                break;
            }
            let page = within(
                self.hard,
                self.blobs
                    .list_objects(&self.prefix, after.as_deref(), room),
            )
            .await??;
            let truncated = page.truncated;
            for obj in page.objects {
                examined += 1;
                out.last_key = Some(obj.key.clone());
                after = Some(obj.key.clone());
                match seq_of_key(self.run_id, &obj.key) {
                    Some(seq) if below.is_some_and(|b| seq >= b) => {
                        break 'pages;
                    }
                    Some(seq) if seq >= from_seq => out.valid.push(Listed { seq, info: obj }),
                    Some(_) => {}
                    None => out.skipped = out.skipped.saturating_add(1),
                }
            }
            if !truncated {
                out.exhausted = true;
                break;
            }
            if want.is_some_and(|w| out.valid.len() >= w) {
                break;
            }
        }
        Ok(out)
    }

    /// Reads `chunks` in order. `start` positions the first chunk (a record index inside it, and the ETag of the object the position
    /// was cut from); `expected` is the next number a chunk should have, when known, so a hole before the first chunk is a gap.
    async fn read(
        &self,
        chunks: &[Listed],
        start: Option<&Position>,
        mut expected: Option<u64>,
    ) -> Result<Read, StoreError> {
        let mut out = Read::default();
        let mut page_bytes = 0usize;
        // The writers (incarnations) met in this request, in order, with the last record number each wrote and the ids already shown.
        let mut writers: Vec<String> = Vec::new();
        let mut last_writer: Option<usize> = None;
        let mut last_n: HashMap<usize, (u64, bool)> = HashMap::new();
        let mut shown: HashSet<(usize, u64)> = HashSet::new();
        // A chunk was read since the start or since the last gap (record numbers continue across read chunks only).
        let mut continuous = false;
        for (n, listed) in chunks.iter().enumerate() {
            let seq = listed.seq;
            if let Some(e) = expected
                && seq > e
            {
                out.gaps
                    .push(TranscriptGap::new(GapCode::MissingChunk, e, seq - 1));
                last_n.clear();
                continuous = false;
            }
            expected = Some(seq + 1);
            let resuming = start.filter(|p| n == 0 && p.seq == seq);
            let from_idx = resuming.map_or(0, |p| p.idx as usize);
            let idx32 = u32::try_from(from_idx).unwrap_or(u32::MAX);
            // A position inside a chunk is cut from one object; a stop there names that object again.
            let stop_here = |idx: u32| Position {
                seq,
                idx,
                after_key: None,
                etag: if idx > 0 {
                    listed.info.etag.clone()
                } else {
                    None
                },
            };
            // A page that has examined nothing yet goes on whatever the clock says, so every request makes progress.
            let spent = Instant::now() >= self.soft
                || out.gaps.len() >= self.limits.max_gaps
                || out.records.len() >= self.limits.max_records;
            if spent && out.examined_through.is_some() {
                out.stop = Some(stop_here(idx32));
                break;
            }
            if out.first.is_none() {
                out.first = Some((seq, from_idx == 0));
            }
            // The cursor was cut from one object; if the chunk was replaced since, it is not read at a stale index.
            if let Some(p) = resuming
                && from_idx > 0
                && p.etag.is_some()
                && p.etag != listed.info.etag
            {
                out.gaps
                    .push(TranscriptGap::new(GapCode::ChangedWhileReading, seq, seq));
                out.examined_through = Some(seq);
                last_n.clear();
                continuous = false;
                continue;
            }
            if listed.info.size > MAX_CHUNK_OBJECT_BYTES as u64 {
                out.gaps
                    .push(TranscriptGap::new(GapCode::TooLarge, seq, seq));
                out.examined_through = Some(seq);
                last_n.clear();
                continuous = false;
                continue;
            }
            let fetched = within(
                self.hard,
                self.blobs.get_object(
                    &listed.info.key,
                    MAX_CHUNK_OBJECT_BYTES,
                    listed.info.etag.as_deref(),
                ),
            )
            .await;
            let bytes = match fetched {
                Ok(Ok(Some(b))) => b,
                Ok(Ok(None)) => {
                    out.gaps
                        .push(TranscriptGap::new(GapCode::MissingChunk, seq, seq));
                    out.examined_through = Some(seq);
                    last_n.clear();
                    continuous = false;
                    continue;
                }
                Ok(Err(ObjectError::TooLarge)) => {
                    out.gaps
                        .push(TranscriptGap::new(GapCode::TooLarge, seq, seq));
                    out.examined_through = Some(seq);
                    last_n.clear();
                    continuous = false;
                    continue;
                }
                Ok(Err(ObjectError::Changed)) => {
                    out.gaps
                        .push(TranscriptGap::new(GapCode::ChangedWhileReading, seq, seq));
                    out.examined_through = Some(seq);
                    last_n.clear();
                    continuous = false;
                    continue;
                }
                Ok(Err(ObjectError::Store(e))) | Err(e) => {
                    if out.examined_through.is_none() {
                        return Err(e);
                    }
                    // Something was read: end the page there and let the client come back for the rest.
                    out.diagnostics.push(format!(
                        "transcript {}: reading chunk {seq} failed; the page ends before it",
                        self.run_id
                    ));
                    out.stop = Some(stop_here(idx32));
                    break;
                }
            };
            let chunk = match decode_chunk(&bytes, seq) {
                Ok(c) => c,
                Err(e) => {
                    out.diagnostics.push(format!(
                        "transcript {}: chunk {seq} unreadable: {}",
                        self.run_id,
                        serde_json::to_string(&e.code()).unwrap_or_default()
                    ));
                    out.gaps.push(TranscriptGap::new(e.code(), seq, seq));
                    out.examined_through = Some(seq);
                    last_n.clear();
                    continuous = false;
                    continue;
                }
            };
            let inc = chunk.header.incarnation.clone();
            let (w, known) = match writers.iter().position(|x| *x == inc) {
                Some(i) => (i, true),
                None => {
                    writers.push(inc);
                    (writers.len() - 1, false)
                }
            };
            if last_writer != Some(w) {
                // A writer that comes back after another wrote in between: two writers overlapped. Nothing is removed.
                if known {
                    out.gaps
                        .push(TranscriptGap::new(GapCode::Interleaved, seq, seq));
                }
                last_writer = Some(w);
            }
            let header = chunk.header;
            let records = chunk.records;
            // Record numbers that skip, with no `gap` record to explain it, are a hole in the conversation. A gap record just before
            // the first record read (the end of the previous chunk, or the record before a resume point) explains the numbers it shed.
            let (expected_first, prev_gap) = if from_idx > 0 {
                let prev = records.get(from_idx - 1);
                (
                    prev.and_then(|r| r.n.checked_add(1)),
                    prev.is_some_and(|r| r.kind == "gap"),
                )
            } else if let Some((n, gap)) = last_n.get(&w) {
                (n.checked_add(1), *gap)
            } else if continuous {
                (Some(0), false)
            } else {
                (None, false)
            };
            let hole_checked = from_idx == 0 || from_idx < records.len();
            if hole_checked
                && record_hole_in(
                    &header,
                    &records[from_idx.min(records.len())..],
                    expected_first,
                    prev_gap,
                )
                .is_some()
            {
                out.gaps
                    .push(TranscriptGap::new(GapCode::RecordHole, seq, seq));
            }
            if let Some(last) = records.last() {
                last_n.insert(w, (last.n, last.kind == "gap"));
            }
            continuous = true;
            let mut emitted_all = true;
            let mut emitted_here = 0usize;
            let mut repeated = false;
            for (i, rec) in records.into_iter().enumerate().skip(from_idx) {
                // A record number a writer already used in this response is a conflict; the later copy is not shown.
                if !shown.insert((w, rec.n)) {
                    repeated = true;
                    continue;
                }
                let mut projected = TranscriptRecord::project(&header, rec, &redact_tokens);
                let mut size = serialized_len(&projected);
                if size > self.limits.record_bytes {
                    projected = projected.omitted(size);
                    size = serialized_len(&projected);
                }
                let full = page_bytes + size > self.limits.page_bytes
                    || out.records.len() >= self.limits.max_records;
                if full && !out.records.is_empty() {
                    out.stop = Some(stop_here(u32::try_from(i).unwrap_or(u32::MAX)));
                    emitted_all = false;
                    break;
                }
                page_bytes += size + 1;
                out.records.push(projected);
                emitted_here += 1;
            }
            if repeated {
                out.gaps
                    .push(TranscriptGap::new(GapCode::DuplicateRecord, seq, seq));
            }
            out.examined_through = Some(seq);
            // A chunk the page ended before showing anything of is not published.
            if emitted_all || emitted_here > 0 {
                out.published_through = Some(seq);
            }
            if !emitted_all {
                break;
            }
        }
        Ok(out)
    }
}

/// The route with its default limits.
pub(crate) async fn get_transcript<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
) -> Reply {
    transcript_with(req, run_id, caller, deps, &Limits::default()).await
}

fn sealed(status: GoalStatus) -> bool {
    matches!(
        status,
        GoalStatus::Complete
            | GoalStatus::Cleared
            | GoalStatus::Blocked
            | GoalStatus::Paused
            | GoalStatus::Stalled
    )
}

/// The route with explicit limits (tests make small ones).
pub async fn transcript_with<S: Store, B: Blobs, E: Execution>(
    req: &V1Request,
    run_id: &str,
    caller: &str,
    deps: &Deps<S, B, E>,
    limits: &Limits,
) -> Reply {
    let soft = Instant::now() + limits.time;
    let hard = soft + HARD_EXTRA;
    // The owner check comes first: a run that is not the caller's is a 404 whatever else the request holds.
    let run = match within(hard, owned(&deps.store, run_id, caller)).await {
        Ok(Ok(Some(run))) => run,
        Ok(Ok(None)) => return not_found(&format!("no run {run_id} for this caller")),
        Ok(Err(e)) | Err(e) => return unavailable(&e, "retry"),
    };
    let params = match parse_params(&req.query, run_id, caller, limits) {
        Ok(p) => p,
        Err(msg) => return error(400, "bad-request", &msg),
    };
    let facts = match within(hard, deps.store.transcript_facts(run_id)).await {
        Ok(Ok(f)) => f,
        Ok(Err(e)) | Err(e) => return unavailable(&e, "retry"),
    };
    let mut page = TranscriptPage {
        run_id: run_id.to_string(),
        state: TranscriptState::Unsupported,
        records: Vec::new(),
        gaps: Vec::new(),
        completeness: Vec::new(),
        published_through: None,
        listed_through: None,
        next: None,
        prev: None,
        more: false,
        skipped_keys: 0,
    };
    if facts.purged {
        page.state = TranscriptState::Purged;
        return json(200, &page);
    }
    if !facts.supported {
        return json(200, &page);
    }
    let ended = sealed(run.status);
    let ctx = Ctx {
        blobs: &deps.blobs,
        run_id,
        prefix: transcript_prefix(run_id),
        limits,
        soft,
        hard,
    };
    let mut diagnostics: Vec<String> = Vec::new();
    let backward = params.before.is_some();

    // List, then pick the chunks to read.
    let listing_result = match params.before {
        None => {
            let (from_seq, after) = match &params.after {
                Some(p) => (
                    p.seq,
                    p.after_key.as_ref().map(|k| ctx.key_of(k)).or_else(|| {
                        p.seq
                            .checked_sub(1)
                            .map(|s| ham_core::transcript::chunk_key(run_id, s))
                    }),
                ),
                None => (FIRST_SEQ, None),
            };
            ctx.list(after, from_seq, Some(params.limit + 1), None)
                .await
        }
        Some(Before::End) => ctx.list(None, 0, None, None).await,
        Some(Before::Seq(b)) => ctx.list(None, 0, None, Some(b)).await,
    };
    let listing = match listing_result {
        Ok(l) => l,
        Err(e) => return unavailable(&e, "retry"),
    };
    page.skipped_keys = listing.skipped;
    let highest_listed = listing.valid.last().map(|l| l.seq);
    page.listed_through = highest_listed;
    // A listing that stopped before the end of the prefix (its cap or the soft budget) has not located the end of the run.
    let listing_cut = listing.cap_hit || listing.timed_out;
    let base_seq = params.after.as_ref().map_or(FIRST_SEQ, |p| p.seq);

    let (chunks, start, expected, earlier): (Vec<Listed>, Option<Position>, Option<u64>, bool) =
        if backward && listing_cut {
            // The chunks found are the front of the run: they are not the ones just below the bound, so none is shown as if it were.
            (Vec::new(), None, None, false)
        } else if backward {
            let n = listing.valid.len();
            let from = n.saturating_sub(params.limit);
            let window: Vec<Listed> = listing.valid[from..].to_vec();
            let start = window.first().map(|l| Position::at(l.seq, 0));
            (window, start, None, from > 0)
        } else {
            let take = listing.valid.len().min(params.limit);
            (
                listing.valid[..take].to_vec(),
                params.after.clone(),
                // The first chunk of a run is number zero: a first chunk listed with a higher number is a hole, unless a cursor says
                // where the read began.
                Some(base_seq),
                false,
            )
        };
    let more_listed = !backward && listing.valid.len() > params.limit;

    let read = match ctx.read(&chunks, start.as_ref(), expected).await {
        Ok(r) => r,
        Err(e) => return unavailable(&e, "retry"),
    };
    diagnostics.extend(read.diagnostics);
    page.records = read.records;
    page.gaps = read.gaps;
    page.published_through = read.published_through;
    let stopped_early = read.stop.is_some();

    // A listing that stopped early is a continuation when the last key examined is plain enough to carry in a cursor; the cap is a
    // gap in any case, the soft budget only when no cursor can go on.
    let mut after_key_next: Option<String> = None;
    let mut unresumable = false;
    if listing_cut {
        let key = listing
            .last_key
            .as_ref()
            .and_then(|k| k.strip_prefix(&ctx.prefix))
            .filter(|k| cursor::safe_key(k))
            .map(str::to_string);
        if listing.cap_hit || key.is_none() {
            let from = highest_listed.map_or(base_seq, |h| h + 1);
            page.gaps
                .push(TranscriptGap::new(GapCode::ListLimit, from, from));
        }
        // Unread chunks lie before the last key examined when the page ended inside the listing: they come first.
        if !(stopped_early || more_listed) {
            match key {
                Some(k) => after_key_next = Some(k),
                // The cursor restarts at the first chunk found, so the key the listing stopped on is not needed.
                None if backward && !listing.valid.is_empty() => {}
                None => unresumable = true,
            }
        }
    }
    let exhausted_all = listing.exhausted && !stopped_early && !more_listed;
    // A cursor cut inside a chunk names a chunk that is no longer there: say so and go on past it. (A position at the start of a
    // chunk that does not exist yet is the poll position of a live run, not a loss.)
    let resume_gone = !backward
        && listing.valid.is_empty()
        && listing.exhausted
        && params.after.as_ref().is_some_and(|p| p.idx > 0);
    if resume_gone {
        page.gaps.push(TranscriptGap::new(
            GapCode::MissingChunk,
            base_seq,
            base_seq,
        ));
    }

    // The tail the worker declared but the prefix lacks, once the whole prefix has been seen.
    let sealed_summary = match (&facts.summary, ended) {
        (SummaryFact::Valid(s), true) => Some(s),
        _ => None,
    };
    if let Some(s) = sealed_summary
        && exhausted_all
        && !backward
        && let Some(last) = s.last_transcript_seq
    {
        let from = highest_listed.map_or(base_seq, |h| h + 1);
        if from <= last {
            page.gaps
                .push(TranscriptGap::new(GapCode::TailMissing, from, last));
        }
    }

    // Completeness and state.
    page.completeness = match (&facts.summary, ended) {
        (SummaryFact::Valid(s), _) => s.entries.clone(),
        (_, true) => vec![CompletenessEntry {
            kind: GapKind::TailUnknown,
            from_seq: None,
            to_seq: None,
            count: None,
        }],
        _ => Vec::new(),
    };
    page.state = if ended {
        match &facts.summary {
            SummaryFact::Valid(s) if s.is_lossless() => TranscriptState::Complete,
            _ => TranscriptState::Incomplete,
        }
    } else if params.after.is_some() || !listing.valid.is_empty() {
        TranscriptState::Live
    } else {
        TranscriptState::Empty
    };

    // Where to read on, and how to read what precedes.
    let next_pos = match (&read.stop, read.examined_through) {
        (Some(p), _) => Position {
            after_key: None,
            ..p.clone()
        },
        (None, Some(t)) => Position {
            seq: t + 1,
            idx: 0,
            after_key: after_key_next.clone(),
            etag: None,
        },
        (None, None) if backward && listing_cut => match listing.valid.first() {
            // Chunks were located but none was read: read on from the first of them.
            Some(first) => Position::at(first.seq, 0),
            None => Position {
                seq: base_seq,
                idx: 0,
                after_key: after_key_next.clone(),
                etag: None,
            },
        },
        (None, None) if resume_gone => Position::at(base_seq + 1, 0),
        (None, None) => {
            let base = params
                .after
                .clone()
                .unwrap_or_else(|| Position::at(FIRST_SEQ, 0));
            Position {
                after_key: after_key_next.clone().or(base.after_key.clone()),
                ..base
            }
        }
    };
    // Nothing is left to read on from once the run has ended and the page reached the end of the prefix, and a listing that stopped
    // where no safe cursor can continue it offers none.
    page.next = if unresumable || (ended && exhausted_all) {
        None
    } else {
        Some(cursor::encode(run_id, caller, &next_pos))
    };
    page.prev = read.first.and_then(|(seq, from_start)| {
        let boundary = if from_start { seq } else { seq + 1 };
        let older_possible = if backward { earlier } else { boundary > 0 };
        older_possible.then_some(boundary)
    });
    page.more = if unresumable {
        false
    } else if backward {
        // Chunks above the bound are found by reading on with next: the listing stopped at the bound, short of the end of the prefix.
        stopped_early || listing_cut || !listing.exhausted
    } else {
        stopped_early || more_listed || (listing_cut && after_key_next.is_some())
    };
    json_with(page, diagnostics)
}

fn json_with(page: TranscriptPage, diagnostics: Vec<String>) -> Reply {
    let (resp, _) = json(200, &page);
    (resp, diagnostics)
}
