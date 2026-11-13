//! The reads of the Runs list when the client holds several backends: every backend is asked at once, each with its own deadline, the
//! answers are merged newest first in memory with a watermark ([`ham_core::backends::merge_first`]), a backend that fails or is slow leaves the
//! others on screen, and the rows of a backend that failed stay (marked stale) from its last good read. Nothing is written to disk.

use crate::api::{Api, ApiError, Retry};
use crate::http::{ControlPlane, TransportError};
use ham_core::backends::{Local, MergedRow, Page, merge_all, merge_first, second_of};
use ham_core::run_index::{RunFilter, RunRow};
use ham_core::runfilter::FIRST_PAGE;
use std::time::Duration;

/// How long one backend has for one page of a read on the timer.
pub const TIMER_DEADLINE: Duration = Duration::from_secs(5);

/// How long one backend has for one page of a read the person asked for (`r`, `ham runs`).
pub const MANUAL_DEADLINE: Duration = Duration::from_secs(15);

/// The rows kept for a backend after a read of its newest page that says more follow: the page, then the rows of the earlier read that are
/// not in the page and are not newer than the page's oldest row (to the second, as the list orders them). Rows of the earlier read inside the
/// range the page covers are replaced by the page; the older ones stay until a read of every page replaces them. One row for each run id,
/// newest first.
fn keep_older(fresh: &[RunRow], old: &[RunRow]) -> Vec<RunRow> {
    let Some(oldest) = fresh.iter().map(|r| second_of(&r.summary.created_at)).min() else {
        return old.to_vec();
    };
    let ids: std::collections::HashSet<&str> =
        fresh.iter().map(|r| r.summary.run_id.as_str()).collect();
    let mut kept: Vec<RunRow> = fresh.to_vec();
    kept.extend(
        old.iter()
            .filter(|r| {
                !ids.contains(r.summary.run_id.as_str())
                    && second_of(&r.summary.created_at) <= oldest
            })
            .cloned(),
    );
    kept.sort_by(|a, b| {
        second_of(&b.summary.created_at)
            .cmp(second_of(&a.summary.created_at))
            .then_with(|| b.summary.run_id.cmp(&a.summary.run_id))
    });
    kept
}

/// What one backend gave: its rows and whether more follow, or why it gave none.
type BackendRead = Result<(Vec<RunRow>, bool), ApiError>;

/// A repeated token or this many pages end a walk of one backend.
const MAX_PAGES: usize = 2000;

impl<P: ControlPlane> Api<P> {
    /// The reads of this Api go to the backend `alias` only (and its start, and every call about a run it makes).
    #[must_use]
    pub fn on(&self, alias: &str) -> Self {
        let mut a = self.clone();
        a.target = Some(std::sync::Arc::from(alias));
        a
    }

    /// The backend this Api is aimed at, if it is aimed at one.
    pub fn target(&self) -> Option<&str> {
        self.target.as_deref()
    }

    /// The deadlines of one page of a read on the timer and of a read the person asked for.
    #[must_use]
    pub fn with_list_deadlines(mut self, timer: Duration, manual: Duration) -> Self {
        self.list_deadlines = (timer, manual);
        self
    }

    /// The aliases to read when this Api names no backend and the plane holds two or more.
    pub(crate) fn multi_aliases(&self) -> Option<Vec<String>> {
        if self.target.is_some() {
            return None;
        }
        self.plane
            .backend_view()
            .map(|v| v.backends.into_iter().map(|b| b.alias).collect())
    }

    /// This Api aimed at the backend that holds `run_id`, when the plane knows it (a read of `GET /v1/runs?q=<run id>` is a list route).
    pub(crate) fn owner_of(&self, run_id: &str) -> Self {
        match (self.target.as_deref(), self.plane.backend_of(run_id)) {
            (None, Some(alias)) => self.on(&alias),
            _ => self.clone(),
        }
    }

    /// One backend's rows: its newest page (`all` false) or every page. A page that runs past `deadline` is a timeout.
    async fn backend_rows(
        &self,
        alias: &str,
        all: bool,
        deadline: Duration,
    ) -> Result<(Vec<RunRow>, bool), ApiError> {
        let api = self.on(alias).with_retry(Retry {
            attempts: 2,
            base: Duration::from_millis(200),
        });
        let filter = RunFilter::default();
        let mut rows: Vec<RunRow> = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = match tokio::time::timeout(deadline, api.run_rows(&filter, token.as_deref()))
                .await
            {
                Ok(r) => r?,
                Err(_) => {
                    self.plane.note_slow(alias);
                    return Err(ApiError::Transport(TransportError::Timeout(format!(
                        "{alias}: no answer in {} s",
                        deadline.as_secs().max(1)
                    ))));
                }
            };
            rows.extend(page.runs);
            match page.next_token {
                Some(t) if all && token.as_deref() != Some(t.as_str()) => token = Some(t),
                Some(_) => return Ok((rows, !all)),
                None => return Ok((rows, false)),
            }
        }
        Ok((rows, false))
    }

    /// The merged rows of every backend and whether more exist than they show. `all` reads every page of every backend; otherwise the newest
    /// page of each is merged with the watermark. A backend that fails, is slow or is paused (its login lapsed, on a timer read) contributes
    /// the rows of its last good read, marked stale, or none; the read fails only when no backend gave anything and some failed.
    pub(crate) async fn multi_rows(
        &self,
        aliases: Vec<String>,
        all: bool,
        timer: bool,
    ) -> Result<(Vec<RunRow>, bool), ApiError> {
        if !timer {
            self.plane.retry_down();
        }
        let deadline = if timer {
            self.list_deadlines.0
        } else {
            self.list_deadlines.1
        };
        let mut set = tokio::task::JoinSet::new();
        let mut results: Vec<Option<BackendRead>> = (0..aliases.len()).map(|_| None).collect();
        for (i, alias) in aliases.iter().cloned().enumerate() {
            if timer && self.plane.paused(&alias) {
                results[i] = Some(Err(ApiError::Transport(TransportError::Local(
                    Local::BackendDown {
                        alias: alias.clone(),
                        reason: "its sign-in has lapsed; this read leaves it alone".to_string(),
                    },
                ))));
                continue;
            }
            let api = self.clone();
            set.spawn(async move {
                let r = api.backend_rows(&alias, all, deadline).await;
                (i, r)
            });
        }
        while let Some(joined) = set.join_next().await {
            if let Ok((i, r)) = joined {
                results[i] = Some(r);
            }
        }
        let mut pages: Vec<Page<RunRow>> = Vec::new();
        let mut failures: Vec<(String, String)> = Vec::new();
        for (alias, result) in aliases.iter().zip(results) {
            let (rows, more) = match result {
                Some(Ok((rows, more))) => {
                    {
                        // A read of every page, or a page with nothing after it, is the whole list. A first page that says more follow
                        // keeps the rows older than itself, so a later failure shows what the last full read showed.
                        let mut seen = crate::api::lock_seen(&self.seen);
                        let kept = match seen.get(alias) {
                            Some(old) if !all && more => keep_older(&rows, old),
                            _ => rows.clone(),
                        };
                        seen.insert(alias.clone(), kept);
                    }
                    self.plane.set_stale(alias, false);
                    (rows, more)
                }
                other => {
                    if let Some(Err(e)) = other {
                        failures.push((alias.clone(), e.to_string()));
                    }
                    let kept = crate::api::lock_seen(&self.seen).get(alias).cloned();
                    match kept {
                        Some(rows) => {
                            self.plane.set_stale(alias, true);
                            let more = !all && rows.len() > FIRST_PAGE;
                            let rows = if all {
                                rows
                            } else {
                                rows.into_iter().take(FIRST_PAGE).collect()
                            };
                            (rows, more)
                        }
                        None => continue,
                    }
                }
            };
            pages.push(Page {
                alias: alias.clone(),
                rows: rows
                    .into_iter()
                    .map(|r| (r.summary.created_at.clone(), r.summary.run_id.clone(), r))
                    .collect(),
                more,
            });
        }
        if pages.is_empty() && !failures.is_empty() {
            let reasons = failures
                .iter()
                .map(|(a, why)| format!("{a}: {why}"))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(ApiError::Transport(TransportError::Local(
                Local::AllFailed { reasons },
            )));
        }
        if all {
            let rows: Vec<MergedRow<RunRow>> = merge_all(pages);
            return Ok((rows.into_iter().map(|m| m.payload).collect(), false));
        }
        let merged = merge_first(pages);
        Ok((
            merged.rows.into_iter().map(|m| m.payload).collect(),
            merged.has_more,
        ))
    }
}
