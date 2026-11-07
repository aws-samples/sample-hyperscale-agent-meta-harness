//! Typed `/v1` calls over a [`ControlPlane`]. Calls that are safe to
//! repeat are retried on network errors, 429 and 5xx with exponential backoff; `POST /v1/direct/runs` is
//! one of them because every retry carries the same `Idempotency-Key`. Event reads are
//! single attempts: the polling loop is itself the retry.

use crate::http::{ControlPlane, TransportError};
use ham_core::poll::PollPlan;
use ham_core::runfilter::{IndexEntry, IndexedRead, Pages, Refresh, RunsRead};
use ham_core::runtag::wire::{TagEditRequest, TagEditResponse};
use ham_core::state::RunState;
use harness_protocol::api::ApiError as WireError;
use harness_protocol::api::v1::{
    ArtifactView, DownloadUrlResponse, EventsPage, InputAccepted, InputAnswer,
    ListArtifactsResponse, ListReposResponse, ListRunsResponse, ModelsResponse, RepoAdminRequest,
    RepoAdminResponse, RepoAllowView, RunControlResponse, RunDetail, RunSummary,
    StartDirectRunRequest, StartRunResponse, WhoAmIResponse,
};
use harness_protocol::envelope::{Envelope, EventType};
use harness_protocol::store::EVENT_SHARDS;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// `text` for a query value: every byte but an unreserved one (letters, digits, `-` `.` `_` `~`) as `%XX`.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// How far back the name read looks for the run's row (ten minutes): a run the screen follows is seconds old.
const NAME_WINDOW_SECS: i64 = 600;

/// Unix seconds as an RFC 3339 UTC time (`2030-01-01T00:00:00Z`), by the civil-from-days algorithm; a time before 1970 reads as 1970.
pub(crate) fn utc_rfc3339(unix: i64) -> String {
    let secs = unix.max(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60
    )
}

/// An events page as the server sent it, each event still JSON. `EventType` has no catch-all, so an
/// older client decoding `EventsPage` whole failed on one event of a newer type: `ApiError::Decode`
/// is not retried, the shard's cursor never moved, and the shard's `run.completed` stayed hidden
/// behind it. The server skips the items it cannot parse so
/// they cannot stall a cursor; this does the same for a type the client has no name for. Binaries
/// built before this change still stall, so deploy the new `ham` before relying on a new event type.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEventsPage {
    events: Vec<serde_json::Value>,
    #[serde(default)]
    next_after: Option<String>,
}

impl RawEventsPage {
    /// The events this client knows, in the server's order, and the cursor past everything sent: the
    /// server's `nextAfter`, else the largest `eventId` read (skipped events included). An event of
    /// a known type that does not decode is corrupt data and stays an error.
    fn into_page(self) -> Result<EventsPage, ApiError> {
        let mut events = Vec::with_capacity(self.events.len());
        let mut last: Option<String> = None;
        for v in self.events {
            if let Some(id) = v.get("eventId").and_then(serde_json::Value::as_str)
                && last.as_deref().is_none_or(|l| id > l)
            {
                last = Some(id.to_string());
            }
            match serde_json::from_value::<Envelope>(v.clone()) {
                Ok(e) => events.push(e),
                Err(_) if has_unknown_type(&v) => {}
                Err(e) => {
                    return Err(ApiError::Decode {
                        what: "EventsPage".into(),
                        msg: e.to_string(),
                    });
                }
            }
        }
        Ok(EventsPage {
            events,
            next_after: self.next_after.or(last),
        })
    }
}

/// Whether `v` names an event type this client does not know.
fn has_unknown_type(v: &serde_json::Value) -> bool {
    v.get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|t| serde_json::from_value::<EventType>(serde_json::json!(t)).is_err())
}

/// The API's page size for event reads (`control_api::handler::EVENTS_PAGE`): a page this full
/// may have more behind it, so the shard is read again at once.
pub const FULL_PAGE: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    /// Tries in total, the first included.
    pub attempts: u32,
    /// Wait before the second try; doubled for each later one.
    pub base: Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 5,
            base: Duration::from_millis(500),
        }
    }
}

/// True for the answer of a stage that does not know the `engineConfig` member: control-api's `400 bad-request` wrapping serde's "unknown field
/// `engineConfig`" of its strict request type. Any other 400 (an empty prompt, a refused setting) is not this.
fn names_engine_config(error: &WireError) -> bool {
    error.code == "bad-request" && error.message.contains("unknown field `engineConfig`")
}

/// What `ham` says when the stage predates engine settings: the stage is older than this `ham`, the keys the user set are named and so is the
/// command that removes each one.
pub fn predates_words(cfg: &harness_protocol::direct::EngineConfig) -> String {
    let keys: Vec<&str> = harness_protocol::direct::keys_for(&cfg.engine)
        .unwrap_or(&[])
        .iter()
        .filter(|row| cfg.get(row.path).is_some())
        .map(|row| row.path)
        .collect();
    format!(
        "this stage predates engine settings; run `ham engine {} unset <key>` or update the stage (keys set: {})",
        cfg.engine,
        if keys.is_empty() {
            "none".to_string()
        } else {
            keys.join(", ")
        }
    )
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ApiError {
    #[error("HTTP {status}: {} ({})", error.message, error.code)]
    Http { status: u16, error: WireError },
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("decode {what}: {msg}")]
    Decode { what: String, msg: String },
}

impl ApiError {
    pub fn is_retryable(&self) -> bool {
        match self {
            ApiError::Http { status, .. } => *status == 429 || *status >= 500,
            ApiError::Transport(t) => t.is_retryable(),
            ApiError::Decode { .. } => false,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self {
            ApiError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// What the screen says when the service has no tag edit route (an older backend answers 404): the edits cannot be made, nothing is broken.
pub const TAGS_UNSUPPORTED: &str = "this service cannot edit tags yet";

/// The words for a failed tag edit: [`TAGS_UNSUPPORTED`] for a 404, else the error as the client words every API failure.
pub fn tags_failure(e: &ApiError) -> String {
    if e.status() == Some(404) {
        TAGS_UNSUPPORTED.to_string()
    } else {
        e.to_string()
    }
}

pub struct Api<P> {
    pub(crate) plane: Arc<P>,
    retry: Retry,
    /// The backend every call of this Api is aimed at; `None` leaves the choice to the plane.
    pub(crate) target: Option<Arc<str>>,
    /// The deadline of one page of a list read on the timer and of one the person asked for, when the plane holds several backends.
    pub(crate) list_deadlines: (Duration, Duration),
    /// The rows of each backend's last good list read, in memory only: what stays on screen when a backend fails.
    pub(crate) seen: Arc<Mutex<BTreeMap<String, Vec<ham_core::run_index::RunRow>>>>,
}

/// The lock of [`Api::seen`]; a poisoned lock still holds rows that are fine to read.
pub(crate) fn lock_seen(
    m: &Mutex<BTreeMap<String, Vec<ham_core::run_index::RunRow>>>,
) -> MutexGuard<'_, BTreeMap<String, Vec<ham_core::run_index::RunRow>>> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<P> Clone for Api<P> {
    fn clone(&self) -> Self {
        Self {
            plane: self.plane.clone(),
            retry: self.retry,
            target: self.target.clone(),
            list_deadlines: self.list_deadlines,
            seen: Arc::clone(&self.seen),
        }
    }
}

impl<P> std::fmt::Debug for Api<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api")
            .field("retry", &self.retry)
            .finish_non_exhaustive()
    }
}

pub(crate) fn json_headers(key: Option<&str>) -> Vec<(String, String)> {
    let mut h = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(k) = key {
        h.push(("idempotency-key".to_string(), k.to_string()));
    }
    h
}

impl<P: ControlPlane> Api<P> {
    pub fn new(plane: Arc<P>) -> Self {
        Self {
            plane,
            retry: Retry::default(),
            target: None,
            list_deadlines: (
                crate::api_multi::TIMER_DEADLINE,
                crate::api_multi::MANUAL_DEADLINE,
            ),
            seen: Arc::default(),
        }
    }

    pub fn with_retry(mut self, retry: Retry) -> Self {
        self.retry = retry;
        self
    }

    pub fn retry(&self) -> Retry {
        self.retry
    }

    pub fn plane(&self) -> &Arc<P> {
        &self.plane
    }

    pub(crate) async fn once<T: DeserializeOwned>(
        &self,
        what: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<T, ApiError> {
        let r = self
            .plane
            .call_on(self.target.as_deref(), method, path, headers, body.to_vec())
            .await?;
        if !(200..300).contains(&r.status) {
            let error = wire_error(r.status, &r.body);
            return Err(ApiError::Http {
                status: r.status,
                error,
            });
        }
        serde_json::from_slice(&r.body).map_err(|e| ApiError::Decode {
            what: what.into(),
            msg: e.to_string(),
        })
    }

    pub(crate) async fn retried<T: DeserializeOwned>(
        &self,
        what: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<T, ApiError> {
        let mut wait = self.retry.base;
        let mut attempt = 1;
        loop {
            match self.once(what, method, path, headers, body).await {
                Err(e) if e.is_retryable() && attempt < self.retry.attempts.max(1) => {
                    tokio::time::sleep(wait).await;
                    wait = wait.saturating_mul(2);
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// `POST /v1/direct/runs`: safe to repeat because every retry sends the same `Idempotency-Key`, so the
    /// API returns one run.
    ///
    /// A stage that was deployed before the `engineConfig` member existed refuses the request with `400 bad-request` ("unknown field"); that one
    /// answer, for a request that carries the member, becomes the code `stage-predates-engine-settings` and the words of [`predates_words`]. The
    /// request is not sent again without the member: a setting the run did not get must not look as if it did.
    pub async fn start_direct_run(
        &self,
        req: &StartDirectRunRequest,
        idempotency_key: &str,
    ) -> Result<StartRunResponse, ApiError> {
        let body = serde_json::to_vec(req).map_err(|e| ApiError::Decode {
            what: "StartDirectRunRequest".into(),
            msg: e.to_string(),
        })?;
        // The one limit of a request: what the platform takes in one call. A request over it is refused here, before a byte is sent, with
        // the words the service would use; the platform's own answer to it names nothing.
        if let Err(e) = harness_protocol::direct::check_request_body(&body, req) {
            return Err(ApiError::Http {
                status: 413,
                error: WireError {
                    code: "payload-too-large".into(),
                    message: e.to_string(),
                    request_id: None,
                },
            });
        }
        let result = self
            .retried(
                "StartRunResponse",
                "POST",
                "/v1/direct/runs",
                &json_headers(Some(idempotency_key)),
                &body,
            )
            .await;
        match (result, &req.engine_config) {
            (Err(ApiError::Http { status: 400, error }), Some(cfg))
                if names_engine_config(&error) =>
            {
                Err(ApiError::Http {
                    status: 400,
                    error: WireError {
                        code: "stage-predates-engine-settings".into(),
                        message: predates_words(cfg),
                        request_id: error.request_id,
                    },
                })
            }
            (other, _) => other,
        }
    }

    /// One page of the caller's runs, newest first: the first when `next` is `None`. The page token is percent-encoded in the query.
    async fn runs_page(&self, next: Option<&str>) -> Result<ListRunsResponse, ApiError> {
        let path = match next {
            Some(t) => format!("/v1/runs?nextToken={}", percent_encode(t)),
            None => "/v1/runs".to_string(),
        };
        self.retried("ListRunsResponse", "GET", &path, &[], &[])
            .await
    }

    /// Every page of the caller's runs, newest first.
    pub async fn list_runs(&self) -> Result<Vec<RunSummary>, ApiError> {
        if let Some(aliases) = self.multi_aliases() {
            let (rows, _) = self.multi_rows(aliases, true, false).await?;
            return Ok(rows.into_iter().map(|r| r.summary).collect());
        }
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let page = self.runs_page(next.as_deref()).await?;
            out.extend(page.runs);
            match page.next_token {
                Some(t) if next.as_deref() != Some(t.as_str()) => next = Some(t),
                _ => return Ok(out),
            }
        }
    }

    /// Every page of the caller's runs as rows, newest first: the summaries with what the run index adds.
    /// A stage without the index sends bare summaries, and the rows then carry nothing more.
    pub async fn list_run_rows(&self) -> Result<Vec<ham_core::run_index::RunRow>, ApiError> {
        if let Some(aliases) = self.multi_aliases() {
            return self
                .multi_rows(aliases, true, false)
                .await
                .map(|(rows, _)| rows);
        }
        let filter = ham_core::run_index::RunFilter::default();
        let mut out = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let page = self.run_rows(&filter, next.as_deref()).await?;
            out.extend(page.runs);
            match page.next_token {
                Some(t) if next.as_deref() != Some(t.as_str()) => next = Some(t),
                _ => return Ok(out),
            }
        }
    }

    /// The name and its source (`ai`, `seed`) in the row of `run_id`, read the way the run screen follows a new run: one page of
    /// `GET /v1/runs?q=<run id>&since=<ten minutes ago>`, whose scan the server stops after that page, and one attempt, because the screen
    /// asks again every few seconds. `None` when the page holds no row of the run (an older run is outside the window) or its row has no name.
    pub async fn run_name_of(
        &self,
        run_id: &str,
    ) -> Result<Option<(String, Option<String>)>, ApiError> {
        let api = self.owner_of(run_id);
        let this = &api;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
        let filter = ham_core::run_index::RunFilter {
            q: Some(run_id.to_string()),
            since: Some(utc_rfc3339(now - NAME_WINDOW_SECS)),
            ..ham_core::run_index::RunFilter::default()
        };
        let path = format!("/v1/runs?{}", filter.to_query());
        let page: ham_core::run_index::RunRowsPage =
            this.once("RunRowsPage", "GET", &path, &[], &[]).await?;
        Ok(page
            .runs
            .into_iter()
            .find(|r| r.summary.run_id == run_id)
            .and_then(|r| r.name.map(|n| (n, r.name_source))))
    }

    /// One read of the Runs list for the screen: the timer ([`Refresh::Timer`]) reads the newest page only, so a long history does
    /// not load the server every second; `r` ([`Refresh::Manual`]) reads every page. The result is applied with [`RunsRead::apply`]
    /// against the list held at that moment, so two reads in flight cannot overwrite each other with a stale copy. On an error the
    /// caller keeps its list.
    pub async fn read_runs(&self, how: Refresh) -> Result<RunsRead, ApiError> {
        if let Some(aliases) = self.multi_aliases() {
            let all = how.pages() == Pages::All;
            let (rows, has_more) = self.multi_rows(aliases, all, how == Refresh::Timer).await?;
            let runs: Vec<RunSummary> = rows.into_iter().map(|r| r.summary).collect();
            return Ok(if all {
                RunsRead::All(runs)
            } else {
                RunsRead::First { runs, has_more }
            });
        }
        match how.pages() {
            Pages::All => self.list_runs().await.map(RunsRead::All),
            Pages::First => {
                let page = self.runs_page(None).await?;
                let has_more = page.next_token.is_some();
                Ok(RunsRead::First {
                    runs: page.runs,
                    has_more,
                })
            }
        }
    }

    /// [`Api::read_runs`] with what the run index holds about each row: the same route, read as rows that carry a title, a model and,
    /// the run's name with its source. A stage without the index sends bare summaries and gives no entries.
    pub async fn read_runs_indexed(&self, how: Refresh) -> Result<IndexedRead, ApiError> {
        if let Some(aliases) = self.multi_aliases() {
            let all = how.pages() == Pages::All;
            let (rows, has_more) = self.multi_rows(aliases, all, how == Refresh::Timer).await?;
            let mut runs: Vec<RunSummary> = Vec::with_capacity(rows.len());
            let mut index: Vec<IndexEntry> = Vec::new();
            for r in rows {
                if r.title.is_some() || r.model.is_some() || r.name.is_some() || r.tags.is_some() {
                    index.push(IndexEntry {
                        run_id: r.summary.run_id.clone(),
                        title: r.title.clone(),
                        model: r.model.clone(),
                        name: r.name.clone(),
                        name_source: r.name_source.clone(),
                        tags: r.tags.clone(),
                    });
                }
                runs.push(r.summary);
            }
            return Ok(IndexedRead {
                read: if all {
                    RunsRead::All(runs)
                } else {
                    RunsRead::First { runs, has_more }
                },
                index,
            });
        }
        let filter = ham_core::run_index::RunFilter::default();
        let mut runs: Vec<RunSummary> = Vec::new();
        let mut index: Vec<IndexEntry> = Vec::new();
        let mut take = |rows: Vec<ham_core::run_index::RunRow>| {
            for r in rows {
                if r.title.is_some() || r.model.is_some() || r.name.is_some() || r.tags.is_some() {
                    index.push(IndexEntry {
                        run_id: r.summary.run_id.clone(),
                        title: r.title.clone(),
                        model: r.model.clone(),
                        name: r.name.clone(),
                        name_source: r.name_source.clone(),
                        tags: r.tags.clone(),
                    });
                }
                runs.push(r.summary);
            }
        };
        match how.pages() {
            Pages::All => {
                let mut next: Option<String> = None;
                loop {
                    let page = self.run_rows(&filter, next.as_deref()).await?;
                    take(page.runs);
                    match page.next_token {
                        Some(t) if next.as_deref() != Some(t.as_str()) => next = Some(t),
                        _ => break,
                    }
                }
                Ok(IndexedRead {
                    read: RunsRead::All(runs),
                    index,
                })
            }
            Pages::First => {
                let page = self.run_rows(&filter, None).await?;
                let has_more = page.next_token.is_some();
                take(page.runs);
                Ok(IndexedRead {
                    read: RunsRead::First { runs, has_more },
                    index,
                })
            }
        }
    }

    /// `POST /v1/tags/edit`: one batch of at most 40 edits, one result for each in request order. An edit
    /// is a set operation, so it is retried like the other state-idempotent POSTs: sent again it ends the same and answers `unchanged`. The caller
    /// judges success by the tags a result carries, not by the status alone. A stage without the route answers 404; see [`tags_failure`].
    pub async fn edit_tags(&self, req: &TagEditRequest) -> Result<TagEditResponse, ApiError> {
        req.validate().map_err(|e| ApiError::Decode {
            what: "TagEditRequest".into(),
            msg: e.to_string(),
        })?;
        let body = serde_json::to_vec(req).map_err(|e| ApiError::Decode {
            what: "TagEditRequest".into(),
            msg: e.to_string(),
        })?;
        self.retried(
            "TagEditResponse",
            "POST",
            "/v1/tags/edit",
            &json_headers(None),
            &body,
        )
        .await
    }

    pub async fn get_run(&self, run_id: &str) -> Result<RunDetail, ApiError> {
        self.retried("RunDetail", "GET", &format!("/v1/runs/{run_id}"), &[], &[])
            .await
    }

    /// One page of one shard after the cursor (a single attempt). An event of a type this client has
    /// no name for (a newer harness wrote it) is left out and the cursor moves past it; see
    /// [`RawEventsPage::into_page`].
    pub async fn events(
        &self,
        run_id: &str,
        shard: u8,
        after: Option<&str>,
    ) -> Result<EventsPage, ApiError> {
        self.events_counted(run_id, shard, after)
            .await
            .map(|(page, _)| page)
    }

    /// [`Api::events`] with the number of events the server sent, skipped ones included: a page is
    /// full by what the server sent.
    async fn events_counted(
        &self,
        run_id: &str,
        shard: u8,
        after: Option<&str>,
    ) -> Result<(EventsPage, usize), ApiError> {
        let mut path = format!("/v1/runs/{run_id}/events?shard={shard}");
        if let Some(a) = after {
            path.push_str("&after=");
            path.push_str(a);
        }
        let raw: RawEventsPage = self.once("EventsPage", "GET", &path, &[], &[]).await?;
        let sent = raw.events.len();
        raw.into_page().map(|page| (page, sent))
    }

    /// Every event of one shard after the cursor: full pages are followed at once.
    async fn drain(
        &self,
        run_id: &str,
        shard: u8,
        mut after: Option<String>,
    ) -> Result<EventsPage, ApiError> {
        let mut events = Vec::new();
        loop {
            let (page, sent) = self.events_counted(run_id, shard, after.as_deref()).await?;
            let full = sent >= FULL_PAGE;
            let advanced = page.next_after.is_some() && page.next_after != after;
            events.extend(page.events);
            after = page.next_after.or(after);
            if !(full && advanced) {
                return Ok(EventsPage {
                    events,
                    next_after: after,
                });
            }
        }
    }

    /// One read of each listed shard after its cursor, all shards at once. The pages that arrived
    /// are returned with the first error, so a partial cycle still makes progress.
    pub async fn fetch_pages(
        &self,
        run_id: &str,
        cursors: &[(u8, Option<String>)],
    ) -> (Vec<(u8, EventsPage)>, Option<ApiError>) {
        let mut set = tokio::task::JoinSet::new();
        for (shard, after) in cursors.iter().cloned() {
            let api = self.clone();
            let run_id = run_id.to_string();
            set.spawn(async move { (shard, api.drain(&run_id, shard, after).await) });
        }
        let mut pages = Vec::new();
        let mut first_error = None;
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((shard, Ok(page))) => pages.push((shard, page)),
                Ok((_, Err(e))) => {
                    first_error.get_or_insert(e);
                }
                Err(e) => {
                    first_error.get_or_insert(ApiError::Transport(TransportError::Network(
                        format!("event read task: {e}"),
                    )));
                }
            }
        }
        pages.sort_by_key(|(s, _)| *s);
        (pages, first_error)
    }

    /// One polling cycle over all 32 shards into `state`; returns the events it accepted.
    pub async fn poll(&self, state: &mut RunState) -> Result<usize, ApiError> {
        let before = state.view().events;
        let (pages, error) = self.fetch_pages(state.run_id(), &cursors(state)).await;
        let run_id = state.run_id().to_string();
        for (shard, page) in pages {
            // An envelope of another run is refused by the state; the API never sends one.
            debug_assert!(page.events.iter().all(|e| e.run_id == run_id));
            state.apply_page(shard, page);
        }
        match error {
            Some(e) => Err(e),
            None => Ok(state.view().events.saturating_sub(before)),
        }
    }

    /// One cycle of an adaptive watcher: reads the shards `plan` chooses at `now` (every shard on the first cycle, then the harness
    /// shard, the shards of the started agents, the shards that gave an event lately, and a cold sweep now and then), applies what
    /// arrived, tells the plan what was found, and returns the events accepted. `now` is the time since the watch began; the
    /// caller waits `plan.interval()` before the next cycle. A one-shot read of a run uses [`Api::poll`], which reads every shard.
    pub async fn poll_with(
        &self,
        state: &mut RunState,
        plan: &mut PollPlan,
        now: Duration,
    ) -> Result<usize, ApiError> {
        let before = state.event_count();
        let cursors = plan.cursors(state, now);
        let (pages, error) = self.fetch_pages(state.run_id(), &cursors).await;
        plan.record(now, &pages);
        let run_id = state.run_id().to_string();
        for (shard, page) in pages {
            debug_assert!(page.events.iter().all(|e| e.run_id == run_id));
            state.apply_page(shard, page);
        }
        match error {
            Some(e) => Err(e),
            None => Ok(state.event_count().saturating_sub(before)),
        }
    }

    pub async fn list_artifacts(&self, run_id: &str) -> Result<Vec<ArtifactView>, ApiError> {
        let r: ListArtifactsResponse = self
            .retried(
                "ListArtifactsResponse",
                "GET",
                &format!("/v1/runs/{run_id}/artifacts"),
                &[],
                &[],
            )
            .await?;
        Ok(r.artifacts)
    }

    /// A presigned GET of one artifact and its true expiry. The call writes nothing; it carries a
    /// fresh `Idempotency-Key` as every POST does.
    pub async fn download_url(
        &self,
        run_id: &str,
        artifact_id: &str,
    ) -> Result<DownloadUrlResponse, ApiError> {
        let key = crate::start::new_nonce();
        self.retried(
            "DownloadUrlResponse",
            "POST",
            &format!("/v1/runs/{run_id}/artifacts/{artifact_id}/download-url"),
            &json_headers(Some(&key)),
            b"{}",
        )
        .await
    }

    /// `POST /v1/runs/{runId}/retry`: the service starts the run again with the request it
    /// was started with, linked to the blocked run, and answers like a start. The body is empty and the client sends none of the request
    /// (`ham` keeps no copy of it). The key is `retry-<blocked run id>` (`ham_core::retry::start_key`), so a repeat, or a send that got no
    /// answer, returns the run the first one started; the call is retried like the reads for that reason.
    pub async fn retry_run(&self, run_id: &str) -> Result<StartRunResponse, ApiError> {
        self.retried(
            "StartRunResponse",
            "POST",
            &format!("/v1/runs/{run_id}/retry"),
            &json_headers(Some(&ham_core::retry::start_key(run_id))),
            b"{}",
        )
        .await
    }

    /// `POST /v1/runs/{runId}/resume`: the service starts a follow-up run from the code a paused or
    /// stalled run delivered, with the request that run was started with, and answers like a start. The key is `resume-<stopped run id>`
    /// (`ham_core::resume::start_key`), so a repeat, a second press or a send that got no answer returns the run the first one started; the call is
    /// retried like the reads for that reason.
    pub async fn resume_run(&self, run_id: &str) -> Result<StartRunResponse, ApiError> {
        self.retried(
            "StartRunResponse",
            "POST",
            &format!("/v1/runs/{run_id}/resume"),
            &json_headers(Some(&ham_core::resume::start_key(run_id))),
            b"{}",
        )
        .await
    }

    /// `POST /v1/runs/{runId}/cancel` (`ham cancel`, the dashboard's `c` twice): on the direct stage it asks Cline to stop and
    /// keeps the work. A cancel is state-idempotent, so it is retried like the reads and carries a fresh `Idempotency-Key`.
    pub async fn cancel_run(&self, run_id: &str) -> Result<RunControlResponse, ApiError> {
        let key = crate::start::new_nonce();
        self.retried(
            "RunControlResponse",
            "POST",
            &format!("/v1/runs/{run_id}/cancel"),
            &json_headers(Some(&key)),
            b"{}",
        )
        .await
    }

    /// `POST /v1/runs/{runId}/input` (`ham say`, `new-session`, `try-model`, `finish`, the dashboard's keys): the user's answer to the
    /// question a run waits on. The request id inside the answer is its nonce, so the same answer sent again is one answer: the call is
    /// retried like the reads and is sent again by the commands when Cline has not acknowledged it. A 200 or 202 means queued, not
    /// applied: the acknowledgement is an event.
    pub async fn answer_input(
        &self,
        run_id: &str,
        answer: &InputAnswer,
    ) -> Result<InputAccepted, ApiError> {
        let body = serde_json::to_vec(answer).map_err(|e| ApiError::Decode {
            what: "InputAnswer".into(),
            msg: e.to_string(),
        })?;
        let key = crate::start::new_nonce();
        self.retried(
            "InputAccepted",
            "POST",
            &format!("/v1/runs/{run_id}/input"),
            &json_headers(Some(&key)),
            &body,
        )
        .await
    }

    pub async fn models(&self) -> Result<ModelsResponse, ApiError> {
        self.retried("ModelsResponse", "GET", "/v1/models", &[], &[])
            .await
    }

    /// `GET /v1/direct/models`: the models a direct run may name, the default marked.
    pub async fn direct_models(&self) -> Result<ham_core::models::ModelList, ApiError> {
        self.retried("ModelList", "GET", "/v1/direct/models", &[], &[])
            .await
    }

    pub async fn whoami(&self) -> Result<WhoAmIResponse, ApiError> {
        self.retried("WhoAmIResponse", "GET", "/v1/whoami", &[], &[])
            .await
    }

    /// The repository allowlist.
    pub async fn list_repos(&self) -> Result<Vec<RepoAllowView>, ApiError> {
        let r: ListReposResponse = self
            .retried("ListReposResponse", "GET", "/v1/repos", &[], &[])
            .await?;
        Ok(r.repos)
    }

    /// `ham-admin repos allow|deny` (operator principals only). An allow or deny is the same
    /// entry again on a repeat, so it is retried like the reads.
    pub async fn admin_repos(&self, req: &RepoAdminRequest) -> Result<RepoAdminResponse, ApiError> {
        let body = serde_json::to_vec(req).map_err(|e| ApiError::Decode {
            what: "RepoAdminRequest".into(),
            msg: e.to_string(),
        })?;
        let key = crate::start::new_nonce();
        self.retried(
            "RepoAdminResponse",
            "POST",
            "/v1/admin/repos",
            &json_headers(Some(&key)),
            &body,
        )
        .await
    }
}

/// The API's error body; API Gateway's own `{"message": ...}` answers (an IAM refusal, a throttle)
/// keep their message.
fn wire_error(status: u16, body: &[u8]) -> WireError {
    if let Ok(e) = serde_json::from_slice::<WireError>(body) {
        return e;
    }
    let gateway = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("message")
                .or_else(|| v.get("Message"))
                .and_then(|m| m.as_str().map(str::to_string))
        });
    WireError {
        code: "http".into(),
        message: gateway
            .unwrap_or_else(|| format!("the API answered HTTP {status} without an error body")),
        request_id: None,
    }
}

/// The state's cursor for every shard.
pub fn cursors(state: &RunState) -> Vec<(u8, Option<String>)> {
    (0..EVENT_SHARDS)
        .map(|s| (s, state.cursor(s).map(str::to_string)))
        .collect()
}
