//! The client side of the chat-client server routes: typed calls on [`Api`] and the download of the work in progress.
//!
//! - [`Api::events_view`]: one page of one shard, forward or from the end (`tail`, `before`), with the user's answers. A single attempt, like
//!   [`Api::events`]: the polling loop is its own retry. An event of a type this client has no name for is left out and the cursors keep
//!   their meaning.
//! - [`Api::run_rows`], [`Api::run_rows_all`]: the run list with its title, model and last activity, filtered on the server.
//! - [`Api::backfill_run_index`]: makes the index items of runs that predate the index, calling until the server is done.
//! - [`Api::transcript_page`]: one page of a run's stored conversation: from the end, before a chunk, or after a cursor. Asked
//!   again like a read; another principal's run is the server's 404.
//! - [`Api::search_runs`]: the search across runs by what was said in them.
//! - [`Api::delivered_head`]: the commit a finished run delivered, with the files it changed and the engine's last message.
//! - [`Api::snapshot_download`] and [`fetch_snapshot`]: the latest snapshot bundle of a run, downloaded in ranges, checked against the
//!   commit and the size the route named, and written under the run's id. Nothing is checked out or merged: the file is a worker snapshot,
//!   not verified by the harness (`ham_core::snapshot::SNAPSHOT_TRUST_NOTE`).
//!
//!   A copy may hold what the run changed and need the repository the run started from. The request says that this client takes
//!   such a copy in two parts (`?parts=1`); the answer then names the base too, and both files are kept: the copy under its usual name
//!   and the base as `<runId>-base.bundle`. The client runs no git and builds nothing: the text names both files and the line that
//!   puts them together. A base that cannot be downloaded leaves the copy on disk and says so. A copy that needs a base which the
//!   stage did not send (a route older than its worker) is kept as well, and the text says what it needs.
//!
//! A presigned URL is a bearer capability: no error here names one, and `SnapshotDownload` hides it from `Debug`.

use crate::api::{Api, ApiError, json_headers};
use crate::http::{ControlPlane, redact_url};
use ham_core::download::{CHUNK_BYTES, plan_ranges};
use ham_core::events_view::{EventsQuery, EventsView, UserAnswer};
use ham_core::head::{DeliveredHead, path as head_path};
use ham_core::run_index::{BackfillReport, RunFilter, RunRow, RunRowsPage, percent_encode};
use ham_core::runsearch::{SearchQuery, SearchResponse};
use ham_core::snapshot::{
    HEADER_BYTES, SnapshotBase, SnapshotDownload, SnapshotError, bundle_head, verify_object,
};
use ham_core::transcript::{TranscriptPage, TranscriptQuery};
use harness_protocol::bundle_header::{HEADER_BYTES_MAX, heads, needs};
use harness_protocol::envelope::{Envelope, EventType};
use serde::Deserialize;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The pages of the run list one `run_rows_all` follows; a longer list is an error that points at `run_rows_walk`.
const MAX_LIST_PAGES: usize = 50;
/// Backfill calls in one `backfill_run_index`: 40 items each, so this reaches 4,000 runs.
const MAX_BACKFILL_CALLS: usize = 100;
/// Times a route that says "a snapshot is being saved" is asked again.
const SNAPSHOT_TRIES: u32 = 4;
/// A snapshot size this client believes. The route is trusted for what it says, not for how much memory it asks for: ranges are planned
/// from the size before a byte arrives.
pub const MAX_SNAPSHOT_BYTES: u64 = 8 << 30;

/// An events page as the server sent it, each event still JSON (see `api::RawEventsPage`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawView {
    events: Vec<serde_json::Value>,
    #[serde(default)]
    next_after: Option<String>,
    #[serde(default)]
    next_before: Option<String>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    answers: Vec<UserAnswer>,
    #[serde(default)]
    answers_truncated: bool,
}

/// Whether `v` names an event type this client does not know.
fn has_unknown_type(v: &serde_json::Value) -> bool {
    v.get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|t| serde_json::from_value::<EventType>(serde_json::json!(t)).is_err())
}

impl RawView {
    fn into_view(self) -> Result<EventsView, ApiError> {
        let mut events = Vec::with_capacity(self.events.len());
        for v in self.events {
            match serde_json::from_value::<Envelope>(v.clone()) {
                Ok(e) => events.push(e),
                Err(_) if has_unknown_type(&v) => {}
                Err(e) => {
                    return Err(ApiError::Decode {
                        what: "EventsView".into(),
                        msg: e.to_string(),
                    });
                }
            }
        }
        Ok(EventsView {
            events,
            next_after: self.next_after,
            next_before: self.next_before,
            has_more: self.has_more,
            answers: self.answers,
            answers_truncated: self.answers_truncated,
        })
    }
}

impl<P: ControlPlane> Api<P> {
    /// One page of one shard of a run's events: forward from a cursor, or the newest N / the N before an event.
    pub async fn events_view(
        &self,
        run_id: &str,
        query: &EventsQuery,
    ) -> Result<EventsView, ApiError> {
        let path = format!("/v1/runs/{run_id}/events?{}", query.to_path_query());
        let raw: RawView = self.once("EventsView", "GET", &path, &[], &[]).await?;
        raw.into_view()
    }

    /// One page of the run list under `filter`; `token` is the `nextToken` of the page before.
    pub async fn run_rows(
        &self,
        filter: &RunFilter,
        token: Option<&str>,
    ) -> Result<RunRowsPage, ApiError> {
        let mut path = "/v1/runs".to_string();
        let mut sep = '?';
        for part in [
            Some(filter.to_query()).filter(|q| !q.is_empty()),
            token.map(|t| format!("nextToken={}", percent_encode(t))),
        ]
        .into_iter()
        .flatten()
        {
            path.push(sep);
            path.push_str(&part);
            sep = '&';
        }
        self.retried("RunRowsPage", "GET", &path, &[], &[]).await
    }

    /// Up to `max_pages` pages of the run list under `filter`: the rows read and the token to go on from, `None` when the list ended. A
    /// page with no rows and a token is normal for a filtered list. A token the server repeats is an error (a loop in the walk).
    pub async fn run_rows_walk(
        &self,
        filter: &RunFilter,
        max_pages: usize,
    ) -> Result<RunRowsWalk, ApiError> {
        let mut rows = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..max_pages {
            let page = self.run_rows(filter, token.as_deref()).await?;
            rows.extend(page.runs);
            match page.next_token {
                None => {
                    return Ok(RunRowsWalk {
                        rows,
                        next_token: None,
                    });
                }
                Some(t) if token.as_deref() == Some(t.as_str()) => {
                    return Err(ApiError::Decode {
                        what: "RunRowsPage".into(),
                        msg: "the server repeated a page token".into(),
                    });
                }
                Some(t) => token = Some(t),
            }
        }
        Ok(RunRowsWalk {
            rows,
            next_token: token,
        })
    }

    /// Every row of the run list under `filter`. A list of more than [`MAX_LIST_PAGES`] pages is an error, not a short list: use
    /// [`Api::run_rows_walk`] to read it in parts, or narrow the filter.
    pub async fn run_rows_all(&self, filter: &RunFilter) -> Result<Vec<RunRow>, ApiError> {
        let walk = self.run_rows_walk(filter, MAX_LIST_PAGES).await?;
        if walk.next_token.is_some() {
            return Err(ApiError::Decode {
                what: "RunRowsPage".into(),
                msg: format!(
                    "the list has more than {MAX_LIST_PAGES} pages; read it in parts or narrow the filter"
                ),
            });
        }
        Ok(walk.rows)
    }

    /// `POST /v1/runs/index`: asks again while the server says there is more, and returns the sum. Safe to repeat.
    pub async fn backfill_run_index(&self) -> Result<BackfillReport, ApiError> {
        let mut total = BackfillReport::default();
        let mut token: Option<String> = None;
        for _ in 0..MAX_BACKFILL_CALLS {
            let path = match &token {
                Some(t) => format!("/v1/runs/index?nextToken={}", percent_encode(t)),
                None => "/v1/runs/index".to_string(),
            };
            let key = crate::start::new_nonce();
            let report: BackfillReport = self
                .retried(
                    "BackfillReport",
                    "POST",
                    &path,
                    &json_headers(Some(&key)),
                    b"{}",
                )
                .await?;
            total.absorb(&report);
            if !report.more {
                total.more = false;
                total.next_token = None;
                break;
            }
            token = report.next_token;
        }
        Ok(total)
    }

    /// `GET /v1/runs/{runId}/transcript`: one page of the stored conversation. Reads are asked again after a network error, a 429 or a 5xx.
    pub async fn transcript_page(
        &self,
        run_id: &str,
        query: &TranscriptQuery,
    ) -> Result<TranscriptPage, ApiError> {
        self.retried("TranscriptPage", "GET", &query.path(run_id), &[], &[])
            .await
    }

    /// `GET /v1/search`: the runs whose conversation, prompt, repository or model hold every word of the query, newest first.
    pub async fn search_runs(&self, query: &SearchQuery) -> Result<SearchResponse, ApiError> {
        self.retried("SearchResponse", "GET", &query.path(), &[], &[])
            .await
    }

    /// `GET /v1/runs/{runId}/head`: the commit a finished run delivered, with the files it changed and the engine's last message. Asked
    /// again like a read; another principal's run is the server's 404, a run that delivered nothing its 409. The answer is checked
    /// (`DeliveredHead::check`): one about another run, or with a head that is not a full commit id, is an error and not a guess.
    pub async fn delivered_head(&self, run_id: &str) -> Result<DeliveredHead, ApiError> {
        let head: DeliveredHead = self
            .retried("DeliveredHead", "GET", &head_path(run_id), &[], &[])
            .await?;
        head.check(run_id).map_err(|e| ApiError::Decode {
            what: "DeliveredHead".into(),
            msg: e.to_string(),
        })?;
        Ok(head)
    }

    /// `POST /v1/runs/{runId}/snapshot/download-url`: the latest snapshot bundle of the run, where to get it and what it must hold. The
    /// call writes nothing; a pair that is being saved answers 409, which [`fetch_snapshot`] asks again.
    pub async fn snapshot_download(&self, run_id: &str) -> Result<SnapshotDownload, ApiError> {
        self.copy_download(run_id, None).await
    }

    /// The same route with `?readBack=<session>`: the files the service read back from the storage of that session after it
    /// ended, whether the run went on from them or not.
    pub async fn read_back_download(
        &self,
        run_id: &str,
        session: u32,
    ) -> Result<SnapshotDownload, ApiError> {
        self.copy_download(run_id, Some(session)).await
    }

    async fn copy_download(
        &self,
        run_id: &str,
        read_back: Option<u32>,
    ) -> Result<SnapshotDownload, ApiError> {
        let key = crate::start::new_nonce();
        let path = match read_back {
            Some(session) => {
                format!("/v1/runs/{run_id}/snapshot/download-url?readBack={session}")
            }
            // This client takes a copy that needs the repository the run started from in two parts. A stage that predates the
            // member ignores it. The files read back are whole and are asked for as before.
            None => format!("/v1/runs/{run_id}/snapshot/download-url?parts=1"),
        };
        self.once(
            "SnapshotDownload",
            "POST",
            &path,
            &json_headers(Some(&key)),
            b"{}",
        )
        .await
    }
}

/// Rows of the run list and where to go on.
#[derive(Clone, Debug, PartialEq)]
pub struct RunRowsWalk {
    pub rows: Vec<RunRow>,
    /// The token of the page after the last one read; `None` when the list ended.
    pub next_token: Option<String>,
}

/// Why a snapshot could not be fetched. No variant holds the URL.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotFetchError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("download: {0}")]
    Fetch(String),
    #[error("io: {0}")]
    Io(String),
    #[error("the downloaded snapshot is not what the route described: {0}")]
    Verify(#[from] SnapshotError),
    /// The copy is on disk, and the repository the run started from, which it needs, is not: its download or its check failed.
    /// Nothing is removed; the same command downloads both again.
    #[error(
        "the copy is at {copy}; the repository the run started from could not be downloaded ({reason}); run the command again"
    )]
    Base { copy: String, reason: String },
}

/// A snapshot written to disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFetched {
    pub path: PathBuf,
    pub head_sha: String,
    pub base_sha: Option<String>,
    pub taken_at: String,
    pub count: Option<u32>,
    pub size_bytes: u64,
    pub run_status: harness_protocol::goal::GoalStatus,
    pub record_stale: bool,
    /// The copy was saved when the engine had stopped: the work as the engine left it.
    pub final_copy: bool,
    /// The engine of the run, as the service names it; `None` from a service older than the member.
    pub engine: Option<String>,
    /// The session whose storage the copy was read back from, when the copy is files the service read back after that
    /// session ended; `None` for a copy a session saved while it worked.
    pub read_back_from: Option<u32>,
    /// The command asked for the files read back from one session (`--read-back <n>`), which the run may not have gone on
    /// from; `false` when it asked for the run's newest copy.
    pub asked_read_back: bool,
    /// The copy holds what the run changed and needs the repository the run started from, which was downloaded beside it:
    /// where that file is and its size in bytes. `None` for a copy that clones alone.
    pub base: Option<(PathBuf, u64)>,
    /// The copy needs the repository the run started from and the stage did not send it (a route older than its worker). The
    /// copy is on disk; it does not clone alone.
    pub needs_base: bool,
}

fn io(e: std::io::Error) -> SnapshotFetchError {
    SnapshotFetchError::Io(e.to_string())
}

/// The route's answer, asked again while it says a pair is being saved (409 `snapshot-updating`).
async fn download_when_settled<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    read_back: Option<u32>,
) -> Result<SnapshotDownload, SnapshotFetchError> {
    let retry = api.retry();
    let mut wait = retry.base;
    let mut tries = 0;
    loop {
        tries += 1;
        match api.copy_download(run_id, read_back).await {
            Ok(d) => return Ok(d),
            Err(ApiError::Http { status: 409, error })
                if error.code == "snapshot-updating" && tries < SNAPSHOT_TRIES =>
            {
                tokio::time::sleep(wait).await;
                wait = wait.saturating_mul(2);
            }
            Err(e) if e.is_retryable() && tries < retry.attempts.max(1) => {
                tokio::time::sleep(wait).await;
                wait = wait.saturating_mul(2);
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// One ranged GET of the bundle, only while it is the object the route checked (`If-Match`).
enum Got {
    Bytes(Vec<u8>),
    /// 403: the URL expired or its credentials lapsed.
    Expired,
}

/// One object a download reads in ranges: where it is, the ETag it must still have, and its size.
struct Part<'a> {
    url: &'a str,
    etag: &'a str,
    size_bytes: u64,
}

async fn get_range<P: ControlPlane>(
    api: &Api<P>,
    d: &Part<'_>,
    (start, end): (u64, u64),
) -> Result<Got, SnapshotFetchError> {
    let retry = api.retry();
    let mut wait = retry.base;
    let mut attempt = 1;
    let what = format!("GET {} bytes={start}-{end}", redact_url(d.url));
    loop {
        let outcome = api
            .plane()
            .fetch(d.url, Some((start, end)), Some(d.etag))
            .await;
        let transient = match &outcome {
            Ok(r) => r.status >= 500,
            Err(e) => e.is_retryable(),
        };
        if transient && attempt < retry.attempts.max(1) {
            tokio::time::sleep(wait).await;
            wait = wait.saturating_mul(2);
            attempt += 1;
            continue;
        }
        let r = outcome.map_err(|e| SnapshotFetchError::Fetch(e.to_string()))?;
        return match r.status {
            206 => {
                let want = format!("bytes {start}-{end}/{}", d.size_bytes);
                let got = r.headers.get("content-range").cloned().unwrap_or_default();
                // The store's own total is the object's size: a bundle of another size than the route named is not the one it checked.
                if let Some(total) = got.rsplit('/').next().and_then(|n| n.parse::<u64>().ok())
                    && total != d.size_bytes
                {
                    return Err(SnapshotFetchError::Verify(SnapshotError::Size {
                        expected: d.size_bytes,
                        actual: total,
                    }));
                }
                // The last range may be shorter than asked: the store clips it to the object.
                let clipped_end = end.min(d.size_bytes.saturating_sub(1));
                let want_clipped = format!("bytes {start}-{clipped_end}/{}", d.size_bytes);
                if got != want && got != want_clipped {
                    Err(SnapshotFetchError::Fetch(format!(
                        "{what}: Content-Range {got:?}, expected {want_clipped:?}"
                    )))
                } else if r.body.len() as u64 != clipped_end - start + 1 {
                    Err(SnapshotFetchError::Fetch(format!(
                        "{what}: {} bytes, expected {}",
                        r.body.len(),
                        clipped_end - start + 1
                    )))
                } else {
                    Ok(Got::Bytes(r.body))
                }
            }
            403 => Ok(Got::Expired),
            412 => Err(SnapshotFetchError::Fetch(format!(
                "{what}: the snapshot changed during the download; run the command again"
            ))),
            s => Err(SnapshotFetchError::Fetch(format!("{what}: HTTP {s}"))),
        };
    }
}

/// The parts of the route's answer this client uses to name a file, plan ranges and ask the store, checked before any is used.
fn sane(d: &SnapshotDownload) -> Result<(), SnapshotFetchError> {
    let bad = |m: &str| SnapshotFetchError::Verify(SnapshotError::Record(m.to_string()));
    if !ham_core::snapshot::is_object_id(&d.head_sha) {
        return Err(bad("headSha is not a full object id"));
    }
    sane_object(d.size_bytes, &d.etag, &d.url)
}

/// [`sane`] for one object of the answer: its size, its ETag and where it is.
fn sane_object(size_bytes: u64, etag: &str, url: &str) -> Result<(), SnapshotFetchError> {
    let bad = |m: &str| SnapshotFetchError::Verify(SnapshotError::Record(m.to_string()));
    if size_bytes > MAX_SNAPSHOT_BYTES {
        return Err(bad("sizeBytes is larger than this client downloads"));
    }
    if etag.is_empty() || etag.chars().any(char::is_control) {
        return Err(bad("etag is not a plain tag"));
    }
    // The bytes come from S3 through the URL the route signed: any other host is not what the route is documented to answer.
    let host_ok = url.parse::<http::Uri>().is_ok_and(|u| {
        let host = u.host().unwrap_or_default();
        match u.scheme_str() {
            Some("https") => {
                host.ends_with(".amazonaws.com") || host.ends_with(".amazonaws.com.cn")
            }
            Some("http") => matches!(host, "127.0.0.1" | "localhost"),
            _ => false,
        }
    });
    if !host_ok {
        return Err(SnapshotFetchError::Fetch(
            "the download url is not an S3 address".into(),
        ));
    }
    Ok(())
}

/// Downloads the run's latest snapshot bundle into `dir` as `<runId>-snapshot-<count>.bundle` and checks it against the route's answer
/// (size and commit). The count is the number of the copy within the run; when a file of that name already holds another commit,
/// the name carries the commit too: `<runId>-snapshot-<count>-<first 12 of the commit>.bundle`. See [`fetch_snapshot_with`].
pub async fn fetch_snapshot<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    dir: &Path,
) -> Result<SnapshotFetched, SnapshotFetchError> {
    fetch_snapshot_with(api, run_id, dir, CHUNK_BYTES).await
}

/// Downloads the files the service read back from the storage of `session` into `dir` as `<runId>-read-back-<session>.bundle`,
/// whether the run went on from them or not, with the checks of [`fetch_snapshot`].
pub async fn fetch_read_back<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    session: u32,
    dir: &Path,
) -> Result<SnapshotFetched, SnapshotFetchError> {
    fetch_copy_with(api, run_id, Some(session), dir, CHUNK_BYTES).await
}

/// [`fetch_snapshot`] with `chunk`-byte ranges (the header the check reads is gathered from the first bytes, whatever the chunk). The bytes go to a `.part` file
/// that is renamed only after the checks pass; a failed check leaves nothing behind. An expired URL is replaced once.
pub async fn fetch_snapshot_with<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    dir: &Path,
    chunk: u64,
) -> Result<SnapshotFetched, SnapshotFetchError> {
    fetch_copy_with(api, run_id, None, dir, chunk).await
}

/// The download behind [`fetch_snapshot_with`] and [`fetch_read_back`]: the run's newest copy, or with `read_back` the files read
/// back from that session. A copy that is files read back is named `<runId>-read-back-<session>.bundle`, whichever way it was asked
/// for; when a file of that name holds another commit, the name carries the commit too.
async fn fetch_copy_with<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    read_back: Option<u32>,
    dir: &Path,
    chunk: u64,
) -> Result<SnapshotFetched, SnapshotFetchError> {
    if !harness_protocol::common::is_ulid(run_id) {
        return Err(SnapshotFetchError::Fetch(format!(
            "{run_id:?} is not a run id"
        )));
    }
    std::fs::create_dir_all(dir).map_err(io)?;
    let mut d = download_when_settled(api, run_id, read_back).await?;
    sane(&d)?;
    // The files read back from one session were asked for: the answer must be about that session and nothing else.
    if let Some(session) = read_back
        && !(d.recovered && d.session == Some(session))
    {
        return Err(SnapshotFetchError::Fetch(format!(
            "the stage did not answer with the files read back from session {session}; it may predate ham pull --read-back"
        )));
    }
    let read_back_from = d.session.filter(|_| d.recovered);
    let name = match (read_back_from, d.count) {
        (Some(session), _) => {
            let plain = format!("{run_id}-read-back-{session}.bundle");
            if holds_another_copy(&dir.join(&plain), &d.head_sha) {
                format!("{run_id}-read-back-{session}-{}.bundle", &d.head_sha[..12])
            } else {
                plain
            }
        }
        (None, Some(n)) => {
            let plain = format!("{run_id}-snapshot-{n}.bundle");
            // A number names one copy of a run. A worker older than that rule numbers the copies of every session from 1, so a
            // file of this name can hold another copy of the run: it is kept, and this copy is named with its commit as well.
            if holds_another_copy(&dir.join(&plain), &d.head_sha) {
                format!("{run_id}-snapshot-{n}-{}.bundle", &d.head_sha[..12])
            } else {
                plain
            }
        }
        (None, None) => format!("{run_id}-snapshot-{}.bundle", &d.head_sha[..12]),
    };
    let dest = dir.join(&name);
    // A name nobody can guess, created exclusively: a link planted at the path is not followed and two downloads do not share a file. The
    // guard removes the file on every way out but the rename; it is declared before the file so the file is closed first.
    let mut part = PartFile::new(dir.join(format!("{name}.{}.part", crate::start::new_nonce())));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part.path)
        .map_err(io)?;
    let mut refreshed = false;
    let mut ranges = plan_ranges(d.size_bytes, chunk, 0);
    ranges.reverse();
    let mut written = 0u64;
    let mut header: Vec<u8> = Vec::new();
    let header_room = usize::try_from(HEADER_BYTES).unwrap_or(usize::MAX);
    while let Some(range) = ranges.pop() {
        let part_of_copy = Part {
            url: &d.url,
            etag: &d.etag,
            size_bytes: d.size_bytes,
        };
        match get_range(api, &part_of_copy, range).await? {
            Got::Bytes(b) => {
                if header.len() < header_room {
                    let room = header_room - header.len();
                    header.extend_from_slice(&b[..b.len().min(room)]);
                }
                file.write_all(&b).map_err(io)?;
                written += b.len() as u64;
            }
            Got::Expired if !refreshed => {
                refreshed = true;
                ranges.push(range);
                // A renewed URL is the same object's: the bytes already written, the header and the ranges still to come belong to the
                // object the first answer described, so a renewal that describes another one ends the download.
                let renewed = download_when_settled(api, run_id, read_back).await?;
                sane(&renewed)?;
                if renewed.etag != d.etag
                    || renewed.size_bytes != d.size_bytes
                    || renewed.head_sha != d.head_sha
                {
                    return Err(SnapshotFetchError::Fetch(
                        "the snapshot changed while it was downloading; run the command again"
                            .into(),
                    ));
                }
                d.url = renewed.url;
                d.expires_at = renewed.expires_at;
            }
            Got::Expired => {
                return Err(SnapshotFetchError::Fetch(format!(
                    "GET {}: HTTP 403 with a fresh URL",
                    redact_url(&d.url)
                )));
            }
        }
    }
    file.flush().map_err(io)?;
    drop(file);
    verify_object(&header, written, &d)?;
    std::fs::rename(&part.path, &dest).map_err(io)?;
    part.keep = true;
    // The copy is on disk and checked. A copy that holds what the run changed needs the repository the run started from:
    // the stage sent where it is, and it is downloaded beside the copy. When that fails the copy stays where it is and the error
    // says so. A stage that sent none for such a copy (a route older than its worker) leaves the copy alone, and the text says what
    // it needs.
    let (base, needs_base) = match d.base.clone() {
        Some(base) => match fetch_base(api, run_id, read_back, dir, chunk, base).await {
            Ok(got) => (Some(got), false),
            Err(e) => {
                return Err(SnapshotFetchError::Base {
                    copy: dest.display().to_string(),
                    reason: e.to_string(),
                });
            }
        },
        None => (None, needs(&header).is_ok_and(|n| !n.is_empty())),
    };
    Ok(SnapshotFetched {
        path: dest,
        head_sha: d.head_sha.clone(),
        base_sha: d.base_sha.clone(),
        taken_at: d.taken_at.clone(),
        count: d.count,
        size_bytes: d.size_bytes,
        run_status: d.run_status,
        record_stale: d.record_stale,
        final_copy: d.final_copy,
        engine: d.engine.clone(),
        read_back_from,
        asked_read_back: read_back.is_some(),
        base,
        needs_base,
    })
}

/// The name of the file the repository a run started from is kept under, beside the copies of the run.
pub fn base_file_name(run_id: &str) -> String {
    format!("{run_id}-base.bundle")
}

/// Downloads the repository the run started from, as the route's answer names it, into `dir` as `<runId>-base.bundle` and
/// returns where it is and its size. It is read in ranges into a part file, only while it is the object the route read (`If-Match`),
/// checked (its size, and that it is a bundle whose header names no commit it needs), and then renamed; a file of that name is
/// replaced, because one run has one base. A URL that expired is replaced once, by asking the route again; an answer that then names
/// another object, or no base at all, ends the download with the reason that is true. Nothing is run and nothing is unpacked.
async fn fetch_base<P: ControlPlane>(
    api: &Api<P>,
    run_id: &str,
    read_back: Option<u32>,
    dir: &Path,
    chunk: u64,
    mut base: SnapshotBase,
) -> Result<(PathBuf, u64), SnapshotFetchError> {
    sane_object(base.size_bytes, &base.etag, &base.url)?;
    let name = base_file_name(run_id);
    let dest = dir.join(&name);
    let mut part = PartFile::new(dir.join(format!("{name}.{}.part", crate::start::new_nonce())));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part.path)
        .map_err(io)?;
    let mut refreshed = false;
    let mut ranges = plan_ranges(base.size_bytes, chunk, 0);
    ranges.reverse();
    let mut written = 0u64;
    let mut header: Vec<u8> = Vec::new();
    let header_room = usize::try_from(HEADER_BYTES_MAX).unwrap_or(usize::MAX);
    while let Some(range) = ranges.pop() {
        let part_of_base = Part {
            url: &base.url,
            etag: &base.etag,
            size_bytes: base.size_bytes,
        };
        match get_range(api, &part_of_base, range).await? {
            Got::Bytes(b) => {
                if header.len() < header_room {
                    let room = header_room - header.len();
                    header.extend_from_slice(&b[..b.len().min(room)]);
                }
                file.write_all(&b).map_err(io)?;
                written += b.len() as u64;
            }
            Got::Expired if !refreshed => {
                refreshed = true;
                ranges.push(range);
                // The route is asked again, and its answer is about the run's newest copy now. That copy can be a whole one by then
                // (the engine removed `.git`, or the files read back were chosen), and such an answer names no base: nothing changed
                // about the base, the stage only sends none any more. An answer that names another object is a base that changed.
                let renewed = match download_when_settled(api, run_id, read_back).await?.base {
                    Some(r) if r.etag == base.etag && r.size_bytes == base.size_bytes => r,
                    Some(_) => {
                        return Err(SnapshotFetchError::Fetch(
                            "the repository the run started from changed while it was downloading"
                                .into(),
                        ));
                    }
                    None => {
                        return Err(SnapshotFetchError::Fetch(
                            "the stage no longer sends the repository the run started from with the newest copy"
                                .into(),
                        ));
                    }
                };
                sane_object(renewed.size_bytes, &renewed.etag, &renewed.url)?;
                base.url = renewed.url;
            }
            Got::Expired => {
                return Err(SnapshotFetchError::Fetch(format!(
                    "GET {}: HTTP 403 with a fresh URL",
                    redact_url(&base.url)
                )));
            }
        }
    }
    file.flush().map_err(io)?;
    drop(file);
    if written != base.size_bytes {
        return Err(SnapshotFetchError::Fetch(format!(
            "the file is {written} bytes; the route said {}",
            base.size_bytes
        )));
    }
    // A bundle that clones alone: its header ends inside the bytes read and names no commit it needs.
    heads(&header).map_err(|e| {
        SnapshotFetchError::Fetch(format!("the file is not a bundle that clones alone: {e}"))
    })?;
    std::fs::rename(&part.path, &dest).map_err(io)?;
    part.keep = true;
    Ok((dest, written))
}

/// Whether `path` is a file that is not the bundle of the commit `head`: a bundle of another commit, or a file that is no
/// bundle. A path that holds nothing, and a bundle of `head` itself (the same copy, downloaded before), is not.
fn holds_another_copy(path: &Path, head: &str) -> bool {
    use std::io::Read as _;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut prefix = Vec::new();
    if file.take(HEADER_BYTES).read_to_end(&mut prefix).is_err() {
        return true;
    }
    bundle_head(&prefix).map_or(true, |held| held != head)
}

/// A download in progress on disk: removed when dropped, unless it was renamed into place.
struct PartFile {
    path: PathBuf,
    keep: bool,
}

impl PartFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
