//! The connect check: did the engine connect to the tool server?
//!
//! The pinned engine reads its registration file at its first `session/prompt` of a session and, when the file is missing, unreadable or wrong, goes
//! on without the tools and says nothing. A file that was written proves nothing, so the module judges what the server saw.
//! The clock of a session starts at its first `PromptSent` (not at `session/new`, which returns in 5 ms, long before the engine connects). When the
//! window is over, 30 seconds by default, the counters of the server since the clock started give one word: `none` (nothing arrived), `handshake-only`
//! (an `initialize` and no `tools/list`: the failure of a read-only directory and of a 500 on `initialize` look like this) or `ok`.
//!
//! The counters belong to the one server of the run, so a clock is judged by the difference between the counters now and the counters at its first
//! prompt. [`Watch`] is the pure state of all clocks, with the time given as an offset, so that the boundary (29 seconds is not yet a verdict, 30
//! is) is a test and not a sleep. The hook runs one timer task per clock and calls [`Watch::judge_due`] when it fires.

use crate::direct::tools::stats::Stats;
use std::time::Duration;

/// The window after the first prompt of a session in which the engine is expected to connect.
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(30);

/// What the server saw of one session. Ordered from the worst to the best: `None < HandshakeOnly < Ok`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Connect {
    /// Nothing arrived.
    None,
    /// An `initialize` arrived and no `tools/list`.
    HandshakeOnly,
    /// The engine listed the tools.
    Ok,
}

impl Connect {
    /// The word of the summary (`DirectToolsNote.connect`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::HandshakeOnly => "handshake-only",
            Self::Ok => "ok",
        }
    }
}

/// The word for what happened between `before` and `now`.
pub fn classify(before: &Stats, now: &Stats) -> Connect {
    if now.lists > before.lists {
        Connect::Ok
    } else if now.initializes > before.initializes {
        Connect::HandshakeOnly
    } else {
        Connect::None
    }
}

#[derive(Debug)]
struct Clock {
    session: String,
    started: Duration,
    before: Stats,
    /// No word will be given for this clock any more: it has one, or it was closed without one.
    closed: bool,
}

/// All clocks of a run and the words they gave.
#[derive(Debug)]
pub struct Watch {
    window: Duration,
    clocks: Vec<Clock>,
    verdicts: Vec<(String, Connect)>,
}

impl Watch {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            clocks: Vec::new(),
            verdicts: Vec::new(),
        }
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// A prompt of `session` is about to be written at time `at` (an offset from any fixed start) with the server's counters `stats`. The first prompt
    /// of a session starts its clock and returns `true`; a retry, a resume or a nudge in the same session is not a new connect and returns `false`.
    pub fn prompt_sent(&mut self, session: &str, at: Duration, stats: &Stats) -> bool {
        if self.clocks.iter().any(|c| c.session == session) {
            return false;
        }
        self.clocks.push(Clock {
            session: session.to_string(),
            started: at,
            before: stats.clone(),
            closed: false,
        });
        true
    }

    fn judge(&mut self, session: &str, now: &Stats) -> Option<(String, Connect)> {
        let c = self
            .clocks
            .iter_mut()
            .find(|c| c.session == session && !c.closed)?;
        c.closed = true;
        let word = classify(&c.before, now);
        self.verdicts.push((session.to_string(), word));
        Some((session.to_string(), word))
    }

    /// Judges every clock whose window is over at time `at`; the words are returned and kept.
    pub fn judge_due(&mut self, at: Duration, now: &Stats) -> Vec<(String, Connect)> {
        let due: Vec<String> = self
            .clocks
            .iter()
            .filter(|c| !c.closed && at.saturating_sub(c.started) >= self.window)
            .map(|c| c.session.clone())
            .collect();
        due.iter().filter_map(|s| self.judge(s, now)).collect()
    }

    /// The engine (`only` one session, or `None` for every open clock) is going away at time `at`: the session ended, the engine's process was
    /// stopped, the run is over. A clock whose window is over is judged like any other. A clock whose window is not over is judged only when it
    /// is already certain: a `tools/list` arrived since it started, so `ok`. Any other is closed without a word: an engine that ended before its
    /// window was over did not get the chance to connect, and a short run or a dead engine is not a failure of the registration.
    pub fn close(
        &mut self,
        only: Option<&str>,
        at: Duration,
        now: &Stats,
    ) -> Vec<(String, Connect)> {
        let open: Vec<String> = self
            .clocks
            .iter()
            .filter(|c| !c.closed && only.is_none_or(|s| s == c.session))
            .map(|c| c.session.clone())
            .collect();
        let mut out = Vec::new();
        for session in open {
            let Some(c) = self
                .clocks
                .iter()
                .find(|c| c.session == session && !c.closed)
            else {
                continue;
            };
            let over = at.saturating_sub(c.started) >= self.window;
            if over || classify(&c.before, now) == Connect::Ok {
                out.extend(self.judge(&session, now));
            } else if let Some(c) = self
                .clocks
                .iter_mut()
                .find(|c| c.session == session && !c.closed)
            {
                c.closed = true;
            }
        }
        out
    }

    /// The run is ending: [`Watch::close`] for every open clock.
    pub fn finish(&mut self, at: Duration, now: &Stats) -> Vec<(String, Connect)> {
        self.close(None, at, now)
    }

    /// The worst word over the sessions judged so far, or `None` when no session was judged.
    pub fn worst(&self) -> Option<Connect> {
        self.verdicts.iter().map(|(_, w)| *w).min()
    }
}
