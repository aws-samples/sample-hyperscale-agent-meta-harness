//! The chat-content index of a finished run: when a run is finalized or blocked and its session has been
//! stopped, [`best_effort`] builds one searchable body of at most 32 KiB from the run's objective, the user's answers and the stored
//! transcript, and writes it into the run's chat item under the owner's partition (`PK = OWNER#<sha256 of the run owner's ARN>`,
//! `SK = CHAT#<createdAt>#<runId>`), beside the run index's item, which the run list reads whole and so must stay small.
//! `GET /v1/search` reads that item; nothing scans transcripts.
//!
//! The index never costs the run anything. It runs after the session is stopped and the run has its final status, inside a time budget
//! (60 s of the function's 300), and every failure is a log line, never an error: a run without an index entry is searched on its
//! prompt, repository and model, which is what a run on an image that writes no transcript gets anyway. The transcript prefix is one
//! the engine can write, so the indexer reads it as hostile: it lists with sizes, never fetches an object over 1 MiB, bounds every
//! read, reads at most [`MAX_CHUNK_READS`] chunks (the newest first until the tail of the body is full, then the oldest until the
//! head is full; the chunk the tail stopped inside serves the head too, which stops at the tail's first piece), decodes with
//! the same whole-chunk checks as the read route, and passes the body through the shared redaction function after the fragments of
//! each message are joined. A chunk number missing from the run (a hole, or a first chunk above zero), an unreadable chunk or a
//! record number that skips (inside a chunk or from one chunk to the next) makes the body partial. The owner partition comes from the Run item's owner, which the
//! engine cannot write.
//!
//! Log lines carry the run id, a bounded outcome word and counts, never text from the conversation.

use crate::ops::Deps;
use crate::ports::{Blobs, Res, SearchWrite, Store};
use ham_core::runsearch::{BodyBuilder, Coverage, chat_sk, owner_pk, pieces_of_chunk};
use ham_core::transcript::{
    Edge, FIRST_SEQ, MAX_CHUNK_OBJECT_BYTES, MAX_PREFIX_KEYS, boundary_hole, chunk_edge,
    decode_chunk, record_hole_before, seq_of_key, transcript_prefix,
};
use harness_core::refusal::redact;
use harness_protocol::goal::GoalStatus;
use serde_json::json;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The most chunks one index reads.
pub const MAX_CHUNK_READS: usize = 256;
/// The time reading may take before the body is built from what was read.
pub const READ_BUDGET: Duration = Duration::from_secs(30);
/// The time the whole index step may take.
pub const TOTAL_BUDGET: Duration = Duration::from_secs(60);

/// The limits of one index; tests lower them.
#[derive(Clone, Debug)]
pub struct IndexLimits {
    pub max_reads: usize,
    pub max_keys: usize,
    pub read_budget: Duration,
}

impl Default for IndexLimits {
    fn default() -> Self {
        Self {
            max_reads: MAX_CHUNK_READS,
            max_keys: MAX_PREFIX_KEYS,
            read_budget: READ_BUDGET,
        }
    }
}

/// What an index attempt did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Indexed {
    /// Nothing was written, for this bounded reason.
    Skipped(&'static str),
    /// One search item was written (or rewritten).
    Written {
        coverage: Coverage,
        bytes: usize,
        chunks_read: usize,
    },
}

/// Builds and writes the run's search body. Errors are the store's or the bucket's; [`best_effort`] swallows them.
pub async fn index_run<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    run_id: &str,
    limits: &IndexLimits,
) -> Res<Indexed>
where
    S: Store,
    B: Blobs,
{
    let Some(run) = deps.store.get_run(run_id).await? else {
        return Ok(Indexed::Skipped("no-run"));
    };
    if run.status == GoalStatus::Cleared {
        return Ok(Indexed::Skipped("cleared"));
    }
    if !deps.store.transcript_supported(run_id).await? {
        return Ok(Indexed::Skipped("unsupported"));
    }
    let mut builder = BodyBuilder::new();
    if let Some(goal) = deps.store.get_root_goal(run_id).await? {
        builder.add_objective(&redact(&goal.objective));
    }
    // The answers are a convenience: a failed read leaves them out and the body is still built.
    for answer in deps.store.user_answers(run_id).await.unwrap_or_default() {
        builder.add_answer(&redact(&answer));
    }

    // The chunk keys, in order, with the sizes the listing shows.
    let prefix = transcript_prefix(run_id);
    let mut chunks: Vec<(u64, String, u64)> = Vec::new();
    let mut after: Option<String> = None;
    let mut examined = 0usize;
    let mut all_read = true;
    loop {
        let room = limits.max_keys.saturating_sub(examined).min(1000);
        if room == 0 {
            all_read = false;
            break;
        }
        let page = deps.blobs.list(&prefix, after.as_deref(), room).await?;
        for obj in &page.objects {
            examined += 1;
            after = Some(obj.key.clone());
            if let Some(seq) = seq_of_key(run_id, &obj.key) {
                chunks.push((seq, obj.key.clone(), obj.size));
            }
        }
        if !page.truncated || page.objects.is_empty() {
            break;
        }
    }

    // The chunk numbers run from the first chunk without a hole; a run that does not has lost chunks the index cannot see.
    let mut next_seq = FIRST_SEQ;
    for (seq, _, _) in &chunks {
        if *seq != next_seq {
            all_read = false;
        }
        next_seq = seq.saturating_add(1);
    }

    // The newest chunks fill the tail of the body, then the oldest fill the head; the middle is left out when both are full.
    let started = Instant::now();
    let mut hi = chunks.len();
    let mut reads = 0usize;
    // The chunk the tail ends in is kept: when the tail stops inside it the head takes the rest from the same bytes, so the two halves
    // cannot come from two versions of an object the engine can replace.
    let mut last_tail: Option<(u64, Vec<ham_core::runsearch::Piece>)> = None;
    // The first and last record of every chunk read, to check that record numbers run on from one chunk to the next.
    let mut edges: BTreeMap<u64, Edge> = BTreeMap::new();
    while hi > 0
        && builder.tail_open()
        && reads < limits.max_reads
        && started.elapsed() < limits.read_budget
    {
        hi -= 1;
        let (seq, key, size) = (chunks[hi].0, chunks[hi].1.clone(), chunks[hi].2);
        reads += 1;
        match read_pieces(deps, seq, &key, size).await {
            Some(read) => {
                all_read &= read.whole;
                edges.extend(read.edge.map(|e| (seq, e)));
                last_tail = Some((seq, read.pieces.clone()));
                builder.tail_chunk(seq, read.pieces);
            }
            None => {
                all_read = false;
                last_tail = None;
            }
        }
    }
    // A chunk the tail stopped inside is offered to the head: it takes what lies before the tail's first piece.
    let head_end = if hi < chunks.len() && builder.tail_cut_seq() == Some(chunks[hi].0) {
        hi + 1
    } else {
        hi
    };
    let mut lo = 0usize;
    while lo < head_end && builder.head_open() {
        let (seq, key, size) = (chunks[lo].0, chunks[lo].1.clone(), chunks[lo].2);
        if let Some((tail_seq, pieces)) = &last_tail
            && *tail_seq == seq
        {
            lo += 1;
            builder.head_chunk(seq, pieces.clone());
            continue;
        }
        if reads >= limits.max_reads || started.elapsed() >= limits.read_budget {
            break;
        }
        lo += 1;
        reads += 1;
        match read_pieces(deps, seq, &key, size).await {
            Some(read) => {
                all_read &= read.whole;
                edges.extend(read.edge.map(|e| (seq, e)));
                builder.head_chunk(seq, read.pieces);
            }
            None => all_read = false,
        }
    }
    if lo < hi {
        // Chunks lie between the head and the tail that were never read.
        all_read = false;
    }
    // Record numbers that skip from one chunk to the next, with nothing to say why, are a loss the chunk numbers do not show.
    let seen: Vec<&Edge> = edges.values().collect();
    if seen
        .windows(2)
        .any(|w| w[1].seq == w[0].seq + 1 && boundary_hole(w[0], w[1]))
    {
        all_read = false;
    }
    let body = builder.finish_with(all_read, &redact);
    let pk = owner_pk(&run.owner_principal_arn);
    let sk = chat_sk(&run.created_at, run_id);
    let indexed_at = dispatcher::items::rfc3339(deps.cfg.now_ms() / 1000);
    deps.store
        .put_search_body(&SearchWrite {
            pk: &pk,
            sk: &sk,
            run_id,
            created_at: &run.created_at,
            body: &body,
            indexed_at: &indexed_at,
        })
        .await?;
    Ok(Indexed::Written {
        coverage: body.coverage,
        bytes: body.body.len(),
        chunks_read: reads,
    })
}

/// What one chunk gives the index.
struct ChunkRead {
    /// The searchable pieces, not yet redacted: the body redacts the joined text.
    pieces: Vec<ham_core::runsearch::Piece>,
    /// The record numbers run without a skip inside the chunk.
    whole: bool,
    edge: Option<Edge>,
}

/// The chunk's searchable pieces; `None` when it could not be read (too large, gone, not a valid chunk).
async fn read_pieces<S, T, M, R, B, P>(
    deps: &Deps<S, T, M, R, B, P>,
    seq: u64,
    key: &str,
    size: u64,
) -> Option<ChunkRead>
where
    B: Blobs,
{
    if size > MAX_CHUNK_OBJECT_BYTES as u64 {
        return None;
    }
    let bytes = deps
        .blobs
        .get_bounded(key, MAX_CHUNK_OBJECT_BYTES)
        .await
        .ok()??;
    let chunk = decode_chunk(&bytes, seq).ok()?;
    Some(ChunkRead {
        whole: record_hole_before(&chunk, None).is_none(),
        edge: chunk_edge(&chunk),
        pieces: pieces_of_chunk(&chunk),
    })
}

/// Runs [`index_run`] inside [`TOTAL_BUDGET`], writes one log line and returns what happened. It never fails: a run's end does not
/// depend on its search entry.
pub async fn best_effort<S, T, M, R, B, P>(deps: &Deps<S, T, M, R, B, P>, run_id: &str) -> Report
where
    S: Store,
    B: Blobs,
{
    let result = tokio::time::timeout(
        TOTAL_BUDGET,
        index_run(deps, run_id, &IndexLimits::default()),
    )
    .await;
    let report = match result {
        Ok(Ok(Indexed::Written {
            coverage,
            bytes,
            chunks_read,
        })) => Report {
            outcome: "written",
            reason: None,
            coverage: Some(coverage),
            bytes,
            chunks_read,
        },
        Ok(Ok(Indexed::Skipped(why))) => Report {
            outcome: "skipped",
            reason: Some(why.to_string()),
            coverage: None,
            bytes: 0,
            chunks_read: 0,
        },
        Ok(Err(e)) => Report {
            outcome: "failed",
            reason: Some(e.error_type().to_string()),
            coverage: None,
            bytes: 0,
            chunks_read: 0,
        },
        Err(_) => Report {
            outcome: "failed",
            reason: Some("timeout".to_string()),
            coverage: None,
            bytes: 0,
            chunks_read: 0,
        },
    };
    println!("{}", report.line(run_id));
    report
}

/// The outcome of [`best_effort`], for the log and for tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub outcome: &'static str,
    pub reason: Option<String>,
    pub coverage: Option<Coverage>,
    pub bytes: usize,
    pub chunks_read: usize,
}

impl Report {
    /// One JSON log line: the run, a bounded outcome and counts; no text from the conversation.
    pub fn line(&self, run_id: &str) -> String {
        json!({
            "function": "run-session", "op": "index", "runId": run_id, "outcome": self.outcome, "reason": self.reason,
            "coverage": self.coverage, "bytes": self.bytes, "chunksRead": self.chunks_read,
        })
        .to_string()
    }
}
