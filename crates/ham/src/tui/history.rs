//! The full-history chat view: the stored conversation of the focused run, read page by page from
//! `GET /v1/runs/{id}/transcript` and shown in place of the progress feed. A child module of `app` (it reads and writes the app's private
//! state); the pure parts are `ham_core::stored`.
//!
//! - **Open.** Opening a run asks for the newest 32 chunks (`before=end`), or, from a search hit, for the chunks after the hit's `open`
//!   token. The progress feed and its label are shown until the first page says the store is there; an older image (`unsupported`) keeps
//!   them.
//! - **Older.** Scrolling to the top (or a conversation that fits the screen with older chunks behind it) asks for the chunks below the
//!   oldest one read, and the view stays on the block it showed.
//! - **Newer.** A run that is still going is polled with the cursor of the last page every few seconds, one request at a time. A run that
//!   ended and was read to its end is not polled.
//! - **A failed read** keeps what is shown, says so once, and is tried again after a pause that grows; a first page that fails three times
//!   in a row (a stage without the route) is given up for the run.
//!
//! - **The run it continues from.** A follow-up run shows the stored conversation of that
//!   run above a divider, read-only. It is read once (the run is finished), page by page upward when the view reaches its top; a first read
//!   that fails three times is given up and the divider still says what the new session was given. The run's own older chunks are then
//!   read as the timer fires, because its top is below the divider.
//!
//! The view is a function of the pages read, the run's events and its messages (`stored::compose`), so reopening shows the same.

use super::*;
use ham_core::stored::{self, Context, Read, StoredChat};
use ham_core::transcript::{Before, TranscriptPage, TranscriptQuery, TranscriptState};

/// Chunks a page asks for.
const PAGE: u32 = 32;
/// The pause between polls of a live run.
const POLL_EVERY: Duration = Duration::from_secs(3);
/// The pause before a failed read is tried again (it doubles per failure, up to [`RETRY_MAX`]).
const RETRY_FIRST: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// Times the first page may fail in a row before the stored conversation is given up for this run.
const FIRST_READ_TRIES: u32 = 3;
/// A run that is still going answers `unsupported` before its worker has registered `transcript.v1`: the first page is asked again after
/// this pause, doubling to [`NOT_READY_MAX`], until the capability appears or the run ends.
const NOT_READY_FIRST: Duration = Duration::from_secs(2);
const NOT_READY_MAX: Duration = Duration::from_secs(30);
/// Polls of an ended run that bring nothing new before the polling stops.
const ENDED_IDLE_POLLS: u32 = 3;

/// The conversation of the run the focused run continues from.
#[derive(Debug, Default)]
struct Parent {
    run_id: String,
    stored: StoredChat,
    inflight: bool,
    failed: u32,
    retry_at: Option<Duration>,
    /// The store has nothing for it, or its first read failed three times: no more requests.
    gave_up: bool,
}

impl Parent {
    /// The first read has an answer (a page, or the give-up), so the divider may be drawn.
    fn settled(&self) -> bool {
        self.gave_up || self.stored.state().is_some()
    }
}

/// What the divider says: the blocks above it belong to the run this one continues from.
const DIVIDER: &str = "previous run, read-only. The new session received only the prompt you submitted, not this history.";

#[derive(Debug, Default)]
pub(crate) struct History {
    run_id: String,
    parent: Option<Parent>,
    stored: StoredChat,
    inflight: bool,
    failed: u32,
    /// How many times the store said `unsupported` to the first page of a run that was still going.
    not_ready: u32,
    retry_at: Option<Duration>,
    next_poll: Duration,
    idle_polls: u32,
    /// The store has nothing for this run (an older image, a purged run): no more requests.
    gave_up: bool,
    /// The `open` token of a search hit that the next `open_run` asks the first page from.
    pub(crate) next_open: Option<String>,
}

impl App {
    /// The run is open: the first page, from the end (or from a search hit).
    pub(crate) fn hist_open(&mut self, run_id: &str) -> Vec<Effect> {
        let token = self.hist.next_open.take();
        self.hist = History {
            run_id: run_id.to_string(),
            inflight: true,
            ..History::default()
        };
        let query = match token {
            Some(t) => TranscriptQuery {
                after: Some(t),
                before: None,
                limit: Some(PAGE),
            },
            None => TranscriptQuery {
                after: None,
                before: Some(Before::End),
                limit: Some(PAGE),
            },
        };
        vec![Effect::Ext(Job::Transcript {
            run_id: run_id.to_string(),
            query,
            read: Read::First,
        })]
    }

    fn hist_request(&mut self, query: TranscriptQuery, read: Read) -> Vec<Effect> {
        self.hist.inflight = true;
        vec![Effect::Ext(Job::Transcript {
            run_id: self.hist.run_id.clone(),
            query,
            read,
        })]
    }

    /// Applies the later capability updates of the stored conversation over `caps`.
    pub(crate) fn hist_apply_capability_updates(&self, caps: &mut ham_core::engines::Capabilities) {
        for r in self.hist.stored.capability_updates() {
            caps.apply_update(&r.kind, &r.data);
        }
    }

    /// The run is going: its conversation may still grow.
    pub(crate) fn run_is_live(&self) -> bool {
        self.cached
            .as_ref()
            .is_none_or(|v| v.status.is_none_or(|s| !s.is_terminal()))
    }

    /// Older chunks are wanted: the view is at its top, or it fits the screen and there is more behind it.
    fn wants_older(&self) -> bool {
        if self.hist.stored.older().is_none() {
            return false;
        }
        let chat = self.chat.borrow();
        chat.fits() || chat.at_top()
    }

    fn older_query(&self) -> Option<TranscriptQuery> {
        Some(TranscriptQuery {
            after: None,
            before: Some(Before::Seq(self.hist.stored.older()?)),
            limit: Some(PAGE),
        })
    }

    /// The run it continues from: older chunks of its conversation are wanted when the view is at its top or fits the screen.
    fn parent_older(&self) -> Option<TranscriptQuery> {
        let p = self.hist.parent.as_ref()?;
        if p.inflight || p.gave_up {
            return None;
        }
        let before = p.stored.older()?;
        let chat = self.chat.borrow();
        (chat.fits() || chat.at_top()).then_some(TranscriptQuery {
            after: None,
            before: Some(Before::Seq(before)),
            limit: Some(PAGE),
        })
    }

    /// The id of the run the focused run continues from, once its detail (which is the focused run's) says so.
    fn parent_wanted(&self) -> Option<String> {
        let d = self.detail.as_ref()?;
        (d.summary.run_id == self.hist.run_id)
            .then(|| d.summary.continue_from.clone())
            .flatten()
    }

    /// The first read of the run it continues from, and its tries again; empty when there is nothing to ask.
    fn parent_tick(&mut self) -> Vec<Effect> {
        let Some(id) = self.parent_wanted() else {
            return vec![];
        };
        let now = self.clock;
        let first = TranscriptQuery {
            after: None,
            before: Some(Before::End),
            limit: Some(PAGE),
        };
        match self.hist.parent.as_mut() {
            Some(p) if p.run_id == id => {
                if p.inflight
                    || p.gave_up
                    || p.stored.state().is_some()
                    || p.retry_at.is_some_and(|at| now < at)
                {
                    return vec![];
                }
                p.retry_at = None;
                p.inflight = true;
            }
            _ => {
                self.hist.parent = Some(Parent {
                    run_id: id.clone(),
                    inflight: true,
                    ..Parent::default()
                });
            }
        }
        vec![Effect::Ext(Job::Transcript {
            run_id: id,
            query: first,
            read: Read::First,
        })]
    }

    /// A page (or its failure) of the run it continues from came back.
    fn parent_done(&mut self, read: Read, result: Result<TranscriptPage, String>) -> Vec<Effect> {
        let now = self.clock;
        let Some(p) = self.hist.parent.as_mut() else {
            return vec![];
        };
        p.inflight = false;
        match result {
            Ok(page) => {
                p.failed = 0;
                p.retry_at = None;
                if matches!(
                    page.state,
                    TranscriptState::Unsupported | TranscriptState::Purged
                ) {
                    p.gave_up = true;
                }
                if read == Read::Older {
                    self.chat.get_mut().with_state(|st, _| st.keep_view());
                }
                if let Some(p) = self.hist.parent.as_mut() {
                    p.stored.absorb(&page, read);
                }
            }
            Err(_) => {
                p.failed += 1;
                if p.stored.state().is_none() && p.failed >= FIRST_READ_TRIES {
                    p.gave_up = true;
                }
                let pause = RETRY_FIRST
                    .saturating_mul(1 << (p.failed - 1).min(3))
                    .min(RETRY_MAX);
                p.retry_at = Some(now + pause);
            }
        }
        self.rebuild_chat();
        vec![]
    }

    /// The conversation was scrolled: older chunks are asked for at the top.
    pub(crate) fn hist_scrolled(&mut self) -> Vec<Effect> {
        // With a previous run above the divider, the top of the view is the previous run's.
        if self.screen == Screen::Dashboard
            && let Some(q) = self.parent_older()
        {
            let id = self.hist.parent.as_ref().map(|p| p.run_id.clone());
            if let (Some(id), Some(p)) = (id, self.hist.parent.as_mut()) {
                p.inflight = true;
                return vec![Effect::Ext(Job::Transcript {
                    run_id: id,
                    query: q,
                    read: Read::Older,
                })];
            }
        }
        if self.screen != Screen::Dashboard
            || self.hist.inflight
            || self.hist.gave_up
            || self.hist.run_id.is_empty()
            || !self.wants_older()
        {
            return vec![];
        }
        match self.older_query() {
            Some(q) => self.hist_request(q, Read::Older),
            None => vec![],
        }
    }

    /// The 1 s timer: a failed read again, older chunks, newer chunks of a live run.
    pub(crate) fn hist_tick(&mut self) -> Vec<Effect> {
        if self.screen != Screen::Dashboard
            || self.hist.inflight
            || self.hist.gave_up
            || self.hist.run_id.is_empty()
        {
            return vec![];
        }
        if let Some(at) = self.hist.retry_at {
            if self.clock < at {
                return vec![];
            }
            self.hist.retry_at = None;
            if self.hist.stored.state().is_none() {
                let q = TranscriptQuery {
                    after: None,
                    before: Some(Before::End),
                    limit: Some(PAGE),
                };
                return self.hist_request(q, Read::First);
            }
        }
        if self.hist.stored.state().is_none() {
            return vec![];
        }
        let previous = self.parent_tick();
        if !previous.is_empty() {
            return previous;
        }
        // With a previous run above the divider the top of the view is not this run's, so its own older chunks are read as the timer fires.
        let below_a_divider = self
            .hist
            .parent
            .as_ref()
            .is_some_and(|p| p.settled() && !p.inflight);
        if (self.wants_older() || (below_a_divider && self.hist.stored.older().is_some()))
            && let Some(q) = self.older_query()
        {
            return self.hist_request(q, Read::Older);
        }
        if let Some(q) = self.parent_older() {
            let id = self.hist.parent.as_ref().map(|p| p.run_id.clone());
            if let (Some(id), Some(p)) = (id, self.hist.parent.as_mut()) {
                p.inflight = true;
                return vec![Effect::Ext(Job::Transcript {
                    run_id: id,
                    query: q,
                    read: Read::Older,
                })];
            }
        }
        let live = self.run_is_live();
        if let Some(after) = self.hist.stored.newer()
            && self.clock >= self.hist.next_poll
            && (live || self.hist.idle_polls < ENDED_IDLE_POLLS)
        {
            let q = TranscriptQuery {
                after: Some(after),
                before: None,
                limit: Some(PAGE),
            };
            return self.hist_request(q, Read::Newer);
        }
        vec![]
    }

    /// A page (or its failure) came back.
    pub(crate) fn hist_done(
        &mut self,
        run_id: &str,
        read: Read,
        result: Result<TranscriptPage, String>,
    ) -> Vec<Effect> {
        if self
            .hist
            .parent
            .as_ref()
            .is_some_and(|p| p.run_id == run_id)
            && self.hist.run_id != run_id
        {
            return self.parent_done(read, result);
        }
        if self.hist.run_id != run_id {
            return vec![];
        }
        self.hist.inflight = false;
        match result {
            Ok(page)
                if read == Read::First
                    && page.state == TranscriptState::Unsupported
                    && self.run_is_live() =>
            {
                // The worker has not registered the capability yet: nothing is kept, the progress feed stays, and the first page is asked
                // again with a growing pause. The run ending while the store still says so is the answer for good (the branch below).
                self.hist.not_ready += 1;
                let pause = NOT_READY_FIRST
                    .saturating_mul(1 << (self.hist.not_ready - 1).min(4))
                    .min(NOT_READY_MAX);
                self.hist.retry_at = Some(self.clock + pause);
            }
            Ok(page) => {
                self.hist.failed = 0;
                self.hist.retry_at = None;
                if matches!(
                    page.state,
                    TranscriptState::Unsupported | TranscriptState::Purged
                ) {
                    self.hist.gave_up = true;
                }
                if read == Read::Older {
                    self.chat.get_mut().with_state(|st, _| st.keep_view());
                }
                let before = self.hist.stored.len();
                self.hist.stored.absorb(&page, read);
                if read == Read::Newer {
                    if self.hist.stored.len() == before {
                        self.hist.idle_polls += 1;
                    } else {
                        self.hist.idle_polls = 0;
                    }
                }
                self.hist.next_poll = self.clock + POLL_EVERY;
                self.caps_refresh();
                self.rebuild_chat();
            }
            Err(e) => {
                self.hist.failed += 1;
                // A stage that has no route to read (deployed before it existed) is not asked for ever: the first page failing three times
                // in a row leaves the progress feed in place.
                if self.hist.stored.state().is_none() && self.hist.failed >= FIRST_READ_TRIES {
                    self.hist.gave_up = true;
                }
                let pause = RETRY_FIRST
                    .saturating_mul(1 << (self.hist.failed - 1).min(3))
                    .min(RETRY_MAX);
                self.hist.retry_at = Some(self.clock + pause);
                if self.hist.failed == 1 {
                    self.notice = Some(format!(
                        "could not read the stored conversation yet ({}); showing what the run reported, and trying again",
                        sanitize_line(&e)
                    ));
                }
            }
        }
        vec![]
    }

    /// The conversation as the chat view shows it: the stored one with the user's items and the harness's notices when the store has the
    /// run, else the progress feed (with its label).
    pub(crate) fn hist_model(&self, progress: ChatModel) -> ChatModel {
        if !stored::uses_stored(&self.hist.stored) {
            // No stored conversation to compose into: the user's own items (delivered messages, answers) join the progress feed.
            return self.with_previous_run(self.send_with_user_items(progress));
        }
        let user = self.send_user_items();
        let own = stored::compose(
            &self.hist.stored,
            &Context {
                progress: &progress,
                user: &user,
                live: self.run_is_live(),
            },
        );
        self.with_previous_run(own)
    }

    /// The conversation of the run this one continues from above a divider, then `own`. Shown once the first read of the
    /// previous run has an answer: a previous run with no stored conversation still gets the divider, which says what the new session was given.
    fn with_previous_run(&self, own: ChatModel) -> ChatModel {
        let Some(p) = self.hist.parent.as_ref().filter(|p| p.settled()) else {
            return own;
        };
        let mut blocks: Vec<ham_core::chat::Block> = Vec::new();
        if stored::uses_stored(&p.stored) {
            let none = ChatModel::from_blocks(ham_core::chat::Source::ProgressFeed, "", Vec::new());
            let previous = stored::compose(
                &p.stored,
                &Context {
                    progress: &none,
                    user: &[],
                    live: false,
                },
            );
            // The ids of the two runs' blocks never meet: the widget keys its folds, wrap cache and selection by id.
            blocks.extend(previous.blocks.into_iter().map(|mut b| {
                b.id = format!("prev:{}", b.id);
                b
            }));
        }
        blocks.push(ham_core::chat::Block::notice(
            "previous-run",
            ham_core::notice::NoticeKind::RunStatus,
            DIVIDER,
        ));
        blocks.extend(own.blocks);
        ChatModel { blocks, ..own }
    }
}
