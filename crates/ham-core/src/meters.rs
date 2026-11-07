//! The status line, its meters and the reconnect overlay of the run chat view. Pure: the app passes the clock in and draws
//! what comes back.
//!
//! * **Clocks.** [`elapsed_s`] counts from the start of the run and stops at its end; [`age_s`] counts from the last good read.
//! * **The line.** [`StatusLine`] words one row: model, state, elapsed time, tokens and cost, a context bar and the age of the last read
//!   (`Claude Sonnet 5.5 · active · 12m40s · 113k tok · $0.14 · as of turn 3 · ctx [####------] 42% · updated 2s ago`). Cline sends no
//!   usage update at the pin and usage is summed per answered prompt, so the totals are those of the last completed turn and say so
//!   ("as of turn 3"), unless the stream proves a live meter ([`StatusLine::live`]). On a narrow screen the line gives up, in order, the
//!   bar for a plain percentage, the age, the context, the totals (tokens, cost and their label go together), the elapsed time and the
//!   model, and only then is cut. Text handed in is cleaned (controls and escape sequences go, tokens are hidden).
//! * **The link.** [`Link`] decides when a failing poll shows "connection lost, retrying": after a grace of [`GRACE_LATER_MS`] since the
//!   first failure, or [`GRACE_FIRST_MS`] while no read has ever succeeded. The first good read clears it, and the next failure starts a
//!   new grace, so a failure that ends inside the grace is never seen.

use crate::redact::redact_tokens;
use crate::sanitize::display_line;

/// The grace before the overlay once a read has succeeded.
pub const GRACE_LATER_MS: u64 = 1_000;
/// The grace before the overlay while no read has succeeded yet.
pub const GRACE_FIRST_MS: u64 = 5_000;

/// Seconds from `started` to `ended` (or `now` while the run goes on, and never past `now`); never negative. `None` without a start.
pub fn elapsed_s(started: Option<i64>, ended: Option<i64>, now: i64) -> Option<u64> {
    let start = started?;
    let end = ended.map_or(now, |e| e.min(now));
    Some(span_s(start, end))
}

/// The seconds from `from` to `to`, never negative, and held at `u64::MAX` (the difference of two `i64` can need 65 bits).
fn span_s(from: i64, to: i64) -> u64 {
    let d = i128::from(to) - i128::from(from);
    u64::try_from(d.max(0)).unwrap_or(u64::MAX)
}

/// Seconds from the last good read to `now`; never negative. `None` before the first read.
pub fn age_s(last_read: Option<i64>, now: i64) -> Option<u64> {
    let last = last_read?;
    Some(span_s(last, now))
}

/// A token count in a few characters: `999`, `1.5k`, `113k`, `1.3M`.
pub fn tokens_text(n: u64) -> String {
    fn tenths(whole_tenths: u64, unit: &str) -> String {
        let (w, f) = (whole_tenths / 10, whole_tenths % 10);
        if f == 0 {
            format!("{w}{unit}")
        } else {
            format!("{w}.{f}{unit}")
        }
    }
    match n {
        0..=999 => n.to_string(),
        1_000..=9_999 => tenths((n + 50) / 100, "k"),
        10_000..=999_499 => format!("{}k", (n + 500) / 1_000),
        999_500..=999_999 => "1M".to_string(),
        _ => {
            let t = (u128::from(n) + 50_000) / 100_000;
            tenths(u64::try_from(t).unwrap_or(u64::MAX), "M")
        }
    }
}

fn safe(text: &str) -> String {
    redact_tokens(&display_line(text)).trim().to_string()
}

fn elapsed_text(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn age_text(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h", s / 3600),
    }
}

/// How much of the model's context window is in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub used: u64,
    pub window: u64,
}

impl Context {
    /// The percentage, rounded; `None` without a window. 100 means the window is full: a rounded 99.5 stays at 99.
    fn percent(self) -> Option<u64> {
        if self.window == 0 {
            return None;
        }
        let (used, window) = (u128::from(self.used), u128::from(self.window));
        if used >= window {
            return Some(100);
        }
        let p = (used * 100 + window / 2) / window;
        Some(u64::try_from(p.min(99)).unwrap_or(99))
    }

    /// `ctx [####------] 42%` (ten cells), or `ctx 42%` when `bar` is false.
    fn text(self, bar: bool) -> Option<String> {
        let p = self.percent()?;
        if !bar {
            return Some(format!("ctx {p}%"));
        }
        let filled = usize::try_from((p + 5) / 10).unwrap_or(10).min(10);
        Some(format!(
            "ctx [{}{}] {p}%",
            "#".repeat(filled),
            "-".repeat(10 - filled)
        ))
    }
}

/// What the status line says. The app fills it every second from the run view, the run detail and the link.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusLine {
    pub model: Option<String>,
    pub state: String,
    pub elapsed_s: Option<u64>,
    pub tokens: Option<u64>,
    /// The cost as the card writes it (`$0.14`, `USD unknown`).
    pub cost: Option<String>,
    /// The turn the tokens and the cost are totals of.
    pub as_of_turn: Option<u32>,
    /// The tokens stream live (the totals are cumulative as they arrive): no "as of" label.
    pub live: bool,
    pub context: Option<Context>,
    /// Seconds since the last good read.
    pub age_s: Option<u64>,
}

impl StatusLine {
    /// The tokens and the cost with their label; none without either.
    fn totals(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(t) = self.tokens {
            parts.push(format!("{} tok", tokens_text(t)));
        }
        if let Some(c) = self.cost.as_deref().map(safe).filter(|c| !c.is_empty()) {
            parts.push(c);
        }
        if parts.is_empty() {
            return None;
        }
        if !self.live {
            parts.push(match self.as_of_turn {
                Some(k) => format!("as of turn {k}"),
                None => "as of the last turn".to_string(),
            });
        }
        Some(parts.join(" \u{b7} "))
    }

    /// The line in at most `width` characters (see the module documentation for what a narrow line gives up).
    pub fn line(&self, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let model = self.model.as_deref().map(safe).filter(|m| !m.is_empty());
        let state = Some(safe(&self.state)).filter(|s| !s.is_empty());
        // model, state, elapsed, totals, context, age
        let mut slots: [Option<String>; 6] = [
            model,
            state,
            self.elapsed_s.map(elapsed_text),
            self.totals(),
            self.context.and_then(|c| c.text(true)),
            self.age_s.map(|s| format!("updated {} ago", age_text(s))),
        ];
        let join = |slots: &[Option<String>; 6]| {
            slots
                .iter()
                .flatten()
                .cloned()
                .collect::<Vec<_>>()
                .join(" \u{b7} ")
        };
        let fits = |s: &str| s.chars().count() <= width;
        let line = join(&slots);
        if fits(&line) {
            return line;
        }
        slots[4] = self.context.and_then(|c| c.text(false));
        let line = join(&slots);
        if fits(&line) {
            return line;
        }
        for drop in [5usize, 4, 3, 2, 0] {
            slots[drop] = None;
            let line = join(&slots);
            if fits(&line) {
                return line;
            }
        }
        let line = join(&slots);
        let mut out: String = line.chars().take(width.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

/// `5 s`, `1 min 12 s`, `2 h 5 min`.
fn duration_text(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => match s % 60 {
            0 => format!("{} min", s / 60),
            r => format!("{} min {r} s", s / 60),
        },
        _ => match (s % 3600) / 60 {
            0 => format!("{} h", s / 3600),
            m => format!("{} h {m} min", s / 3600),
        },
    }
}

/// How the link to the server looks at one moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// The last read worked, or nothing has failed yet.
    Connected,
    /// A read failed, inside the grace: nothing is shown.
    Quiet,
    /// A read has been failing for longer than the grace: the overlay shows.
    Lost { for_ms: u64, first_connect: bool },
}

/// The reconnect state machine, over a clock the caller passes in (milliseconds from any fixed start).
#[derive(Clone, Copy, Debug, Default)]
pub struct Link {
    /// A read has succeeded at least once.
    connected_once: bool,
    /// When the current run of failures began.
    failing_since: Option<u64>,
    last_ok: Option<u64>,
}

impl Link {
    pub fn new() -> Self {
        Self::default()
    }

    /// A read worked.
    pub fn on_ok(&mut self, now_ms: u64) {
        self.connected_once = true;
        self.failing_since = None;
        self.last_ok = Some(now_ms);
    }

    /// A read failed. Only the first failure of a run counts for the grace.
    pub fn on_err(&mut self, now_ms: u64) {
        self.failing_since.get_or_insert(now_ms);
    }

    pub fn state(&self, now_ms: u64) -> LinkState {
        let Some(since) = self.failing_since else {
            return LinkState::Connected;
        };
        let grace = if self.connected_once {
            GRACE_LATER_MS
        } else {
            GRACE_FIRST_MS
        };
        let for_ms = now_ms.saturating_sub(since);
        if for_ms >= grace {
            LinkState::Lost {
                for_ms,
                first_connect: !self.connected_once,
            }
        } else {
            LinkState::Quiet
        }
    }

    /// The overlay's words, once the grace is over.
    pub fn overlay(&self, now_ms: u64) -> Option<String> {
        match self.state(now_ms) {
            LinkState::Lost {
                for_ms,
                first_connect,
            } => {
                let what = if first_connect {
                    "Cannot connect yet, retrying"
                } else {
                    "Connection lost, retrying"
                };
                Some(format!("{what} ({})", duration_text(for_ms)))
            }
            _ => None,
        }
    }

    /// Seconds since the last good read; a failing poll does not reset it.
    pub fn age_s(&self, now_ms: u64) -> Option<u64> {
        self.last_ok.map(|t| now_ms.saturating_sub(t) / 1000)
    }
}
