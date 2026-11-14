//! Plain-text renderings shared by the TUI and `--json`-less command output: the delivery card
//! and the one-line run status.

use crate::failover;
use crate::state::RunView;
use harness_protocol::goal::{GoalPhase, GoalStatus};
use std::time::Duration;

/// What a run used, as the stage reported it (client polish 2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CardUsage {
    pub tokens: u64,
    /// The price of the tokens; `None` when the stage could not price them (a model with no row in its price table), shown as
    /// "unknown" and never as 0.00.
    pub usd: Option<f64>,
}

impl CardUsage {
    /// `Tokens    <n>  USD <x.xx>` (or `USD unknown`).
    pub fn line(&self) -> String {
        match self.usd {
            Some(usd) => format!("Tokens    {}  USD {usd:.2}", self.tokens),
            None => format!("Tokens    {}  USD unknown", self.tokens),
        }
    }
}

/// Client-side facts for the delivery card.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FinalCard {
    pub local_path: String,
    pub presigned_url: Option<String>,
    pub expires_at: Option<String>,
    /// The tokens and price the stage reported for the run; `None` when it reported none (a turn that ended without engine-reported
    /// usage, or a detail the client could not read). The card shows what the stage reported or nothing at all: a "Tokens 0  USD
    /// 0.00" line for a run that used a million tokens is a fake zero.
    pub usage: Option<CardUsage>,
    /// How long `ham watch` waited for `run.completed` before it gave up: the card then says the outcome was not received.
    /// `None` for every other card, including one drawn while the label may still come.
    pub label_missing_after: Option<Duration>,
    /// The one sentence about the clean-checkout test report of the delivered zip, already clean for a screen: what the repository's
    /// own test commands did in a clean copy of the delivered head. `None` when the zip holds no report or has not been read; the card then has
    /// no `Tests` line. The line never changes the label.
    pub tests: Option<String>,
    /// The word of the flow the run declared: `quick`, `full` or `none`, read from `evidence/direct.json` of the delivered zip. `None` when
    /// the zip has no record or has not been read; the card then has no `Flow` row. The row never changes the label.
    pub flow: Option<String>,
    /// The number of the run's last session, when the run went on in a new session after the first one ended (2 or more); `None` for a run of one
    /// session, and for a detail the client could not read. The card then has a `Session` row that says how often the run continued.
    pub session: Option<u32>,
}

/// The `Session` row of the card: which session the run ended in and how often it continued in a new one.
pub fn session_text(session: u32) -> String {
    let renewed = session.saturating_sub(1);
    let times = if renewed == 1 { "time" } else { "times" };
    format!("{session} (the run continued in a new session {renewed} {times})")
}

/// A wait as the card and the notes write it: `30 s`, `0.6 s`.
pub fn wait_text(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{} s", d.as_secs())
    } else {
        format!("{:.1} s", d.as_secs_f32())
    }
}

fn status_str(s: Option<GoalStatus>) -> String {
    s.and_then(|s| serde_json::to_value(s).ok())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

fn phase_str(p: Option<GoalPhase>) -> String {
    p.and_then(|p| serde_json::to_value(p).ok())
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "-".into())
}

/// The words of a phase as the service stores them.
fn phase_word(p: GoalPhase) -> &'static str {
    match p {
        GoalPhase::Planning => "planning",
        GoalPhase::Red => "red",
        GoalPhase::Green => "green",
        GoalPhase::Refactor => "refactor",
        GoalPhase::Gating => "gating",
        GoalPhase::Evaluating => "evaluating",
        GoalPhase::Integrating => "integrating",
        GoalPhase::Delivering => "delivering",
    }
}

/// The phase of a run as the screens of the terminal app write it, or `None` when the run is over.
///
/// The service sets a run's phase three times (planning, green, delivering) and moves the status to `complete` or `blocked` without
/// touching it, so a run that finished keeps the last phase it reached: every finished run said `delivering`. The phase says something
/// while a run goes on, and for a blocked run or a run held by a limit it says where the run stopped. A `complete` or `cleared` run shows
/// none. A phase that is not known reads `-`, and a status that is not known leaves the phase as it is. The record the service stored,
/// `ham runs` and the JSON of the API keep the phase as stored; only the run screens ask this.
pub fn phase_shown(status: Option<GoalStatus>, phase: Option<GoalPhase>) -> Option<&'static str> {
    if matches!(status, Some(GoalStatus::Complete | GoalStatus::Cleared)) {
        return None;
    }
    Some(phase.map_or("-", phase_word))
}

/// The phase of a run as the command-line surfaces write it (`ham runs`, `ham status`, `ham watch`, the card the command prints): the stored
/// phase, except that a `complete` run whose last stored phase is `delivering` reads `delivered`. The service writes `delivering` once
/// and moves the status to `complete` without writing a phase again, so the stored word kept saying a delivery was going on hours after the zip
/// was in the bucket. Every other pair is written as stored (a `blocked` run still says where it stopped), and the JSON of the API and of
/// `ham runs --json` keep the stored value.
pub fn phase_listed(status: Option<GoalStatus>, phase: Option<GoalPhase>) -> String {
    match (status, phase) {
        (Some(GoalStatus::Complete), Some(GoalPhase::Delivering)) => "delivered".to_string(),
        _ => phase_str(phase),
    }
}

/// The `Zip` line: the local path with the delivered file's SHA-256 prefix and size, or `n/a` before delivery.
fn zip_text(v: &RunView, card: &FinalCard) -> String {
    v.zip.as_ref().map_or_else(
        || "n/a".to_string(),
        |z| {
            format!(
                "{} (sha256 {}, {} bytes)",
                card.local_path,
                &z.sha256[..z.sha256.len().min(12)],
                z.size_bytes
            )
        },
    )
}

/// The `Link` line: the presigned URL with its expiry, or `n/a`.
fn link_text(card: &FinalCard) -> String {
    match (&card.presigned_url, &card.expires_at) {
        (Some(u), Some(e)) => format!("{u} (expires {e})"),
        (Some(u), None) => u.clone(),
        _ => "n/a".to_string(),
    }
}

/// The name the shipped model list gives `id` (`Claude Sonnet 5.5`), else the id as the harness wrote it: a run started by a build
/// with another list still shows what it used.
pub fn model_name(id: &str) -> String {
    let models = crate::models::builtin();
    models
        .find(id)
        .or_else(|| models.find_route(id))
        .map_or_else(|| id.to_string(), |m| m.name.clone())
}

/// The card's model text: `Claude Sonnet 5.5 (global.anthropic.claude-sonnet-5-5)` for a model of the shipped list, so a person reads
/// the name and a script finds the id; an id the list does not know is shown alone. An alternate route of a listed model (a run that
/// switched route ends on one) is shown with its own id: `Claude Sonnet 5.5 (us.anthropic.claude-sonnet-5-5)`.
pub fn model_text(id: &str) -> String {
    let models = crate::models::builtin();
    if let Some(m) = models.find(id) {
        return format!("{} ({})", m.name, m.id);
    }
    match models.find_route(id) {
        Some(m) => format!("{} ({id})", m.name),
        None => id.to_string(),
    }
}

/// Where a run that switched model or route ended, when that is not the model the user asked for. It is read from the
/// `Ran on <id> after <n> switches.` sentence the harness writes first in the detail of `run.completed`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndedOn {
    /// The model id the run ended on, as the harness wrote it.
    pub model_id: String,
    /// Its display name; the id follows in parentheses when it is another route of the model the user asked for.
    pub text: String,
    pub switches: u32,
}

impl EndedOn {
    /// `ran on GLM 5.3 after 9 switches`, the words the status line adds after the model.
    pub fn phrase(&self) -> String {
        format!(
            "ran on {} after {}",
            self.text,
            failover::switches_text(self.switches)
        )
    }

    /// `Ran on GLM 5.3 after 9 switches`, the row of the dashboard's status block.
    pub fn row(&self) -> String {
        failover::ran_on_phrase(&self.text, self.switches)
    }
}

/// The display text of the model a run ended on: its name, with the id when the name is the one of the model the user asked for (another
/// route of the same model), and the id alone when the list does not know it.
fn ended_text(final_id: &str, chosen: Option<&str>) -> String {
    let name = model_name(final_id);
    let other_route =
        name != final_id && chosen.is_some_and(|c| c != final_id && model_name(c) == name);
    if other_route {
        format!("{name} ({final_id})")
    } else {
        name
    }
}

/// The model the run ended on, when it switched (the detail of `run.completed` has the `Ran on` sentence) and ended on another model
/// or route than the one the session started on. `None` for every other run.
pub fn ended_on(v: &RunView) -> Option<EndedOn> {
    let chosen = v.direct_model_id()?;
    let detail = v.result.as_ref()?.detail.as_deref()?;
    let ran_on = failover::split(detail)?.ran_on?;
    (ran_on.model_id != chosen).then(|| EndedOn {
        model_id: ran_on.model_id.to_string(),
        text: ended_text(ran_on.model_id, Some(chosen)),
        switches: ran_on.switches,
    })
}

/// The label of the card's clone line, as the TUI replaces it with the clone's state.
pub const CLONE_LABEL: &str = "Clone     ";

/// The label of the card's push hint line (`ham push <runId> <git-url>`).
pub const PUSH_LABEL: &str = "Push      ";

/// The label of the card's link line, which the TUI replaces with [`link_line`].
pub const LINK_LABEL: &str = "Link      ";

/// The label of the card's test report line (`FinalCard::tests`).
pub const TESTS_LABEL: &str = "Tests     ";

/// The label of the card's flow line (`FinalCard::flow`).
pub const FLOW_LABEL: &str = "Flow      ";

/// `HH:MM UTC` for Unix seconds. The TUI shows the expiry of the download link in the local zone when it can find it
/// (`ham::link::local_clock`) and falls back to this.
pub fn utc_clock(epoch: i64) -> String {
    let secs = epoch.rem_euclid(86_400);
    format!("{:02}:{:02} UTC", secs / 3_600, secs % 3_600 / 60)
}

/// The short host label of a download link: `s3` for an S3 host, else the host itself. User info, port, path and query are never
/// part of it, so the label can sit on a card without showing the link. A string that is not a URL gives an empty label.
pub fn link_host(url: &str) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return String::new();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6
            .split(']')
            .next()
            .map_or(String::new(), |h| format!("[{h}]")),
        None => host_port.split(':').next().unwrap_or_default().to_string(),
    };
    if host.is_empty()
        || !host.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'[' | b']' | b':')
        })
    {
        return String::new();
    }
    let lower = host.to_ascii_lowercase();
    let s3 = lower.ends_with(".amazonaws.com")
        && lower
            .split('.')
            .any(|label| label == "s3" || label.starts_with("s3-"));
    if s3 { "s3".to_string() } else { host }
}

/// What the dashboard's `Link` line shows about the delivered zip's download link.
#[derive(Clone, Copy, Debug)]
pub struct LinkLine<'a> {
    /// [`link_host`] of the link.
    pub host: &'a str,
    /// The zip's file name.
    pub name: &'a str,
    /// When the link stops working, as the reader's clock writes it (`06:12 MDT`); `None` when it is not known.
    pub expires: Option<&'a str>,
    /// The link has lapsed: the line says `expired` instead of `expires`.
    pub expired: bool,
    /// The keys that act on the link (`o open · d download`); the last part of the line.
    pub keys: &'a str,
}

const PART_SEPARATOR: &str = " · ";

/// The shortest zip name the line keeps before it drops the host and then the name.
const MIN_NAME: usize = 12;

/// `name` cut to `max` characters with an ellipsis in the middle, keeping more of the tail (the sha and the extension tell zips apart).
fn middle_cut(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        return name.to_string();
    }
    let keep = max.saturating_sub(1);
    let head = keep * 2 / 5;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(&chars[chars.len() - tail..]);
    out
}

/// The `Link` line of the dashboard's delivery card: `Link      s3 · <zip name> · expires 06:12 MDT · o open · d download`, fitted to
/// `width` characters (the card's inner width) so it never wraps. It gives up the zip name first (shortened in the middle to at least
/// 12 characters), then the host, the name and the expiry; the label and the keys stay while they fit. The line is built from the
/// host label, the name, a time and the keys only, so it cannot carry the link itself.
pub fn link_line(l: &LinkLine<'_>, width: usize) -> String {
    let expiry = l
        .expires
        .filter(|e| !e.is_empty())
        .map(|e| format!("{} {e}", if l.expired { "expired" } else { "expires" }));
    let join = |host: Option<&str>, name: Option<&str>, expiry: Option<&str>| {
        let mut parts: Vec<&str> = Vec::new();
        parts.extend(host.filter(|h| !h.is_empty()));
        parts.extend(name.filter(|n| !n.is_empty()));
        parts.extend(expiry);
        parts.push(l.keys);
        format!("{LINK_LABEL}{}", parts.join(PART_SEPARATOR))
    };
    let len = |s: &str| s.chars().count();
    let host = Some(l.host).filter(|h| !h.is_empty());
    // The line with everything, then with the name cut to what is left; then without the host, without the name, without the expiry.
    let tries: [(Option<&str>, bool, Option<&str>); 4] = [
        (host, true, expiry.as_deref()),
        (None, true, expiry.as_deref()),
        (None, false, expiry.as_deref()),
        (None, false, None),
    ];
    for (host, with_name, expiry) in tries {
        let without_name = join(host, None, expiry);
        if !with_name {
            if len(&without_name) <= width {
                return without_name;
            }
            continue;
        }
        let with_full_name = join(host, Some(l.name), expiry);
        if len(&with_full_name) <= width {
            return with_full_name;
        }
        if l.name.is_empty() {
            continue;
        }
        // Room for the name: the line without it, plus one separator.
        let used = len(&without_name) + PART_SEPARATOR.chars().count();
        let room = width.saturating_sub(used);
        if room >= MIN_NAME {
            return join(host, Some(&middle_cut(l.name, room)), expiry);
        }
    }
    let line = join(None, None, None);
    if len(&line) <= width {
        return line;
    }
    // Too narrow for the label and the keys: one line, cut.
    let mut cut: String = line.chars().take(width.saturating_sub(1)).collect();
    if width > 0 {
        cut.push('…');
    }
    cut
}

/// The card shown on `delivery.ready`: the run, how Cline ended, the pull request, the zip, the link, the clone hint and the usage.
/// A direct run has no evidence to summarize: its card carries the harness's own label, "Cline reported done" or
/// "Cline stopped: <reason>", exactly as delivered, and the specifics under it.
pub fn final_card(v: &RunView, card: &FinalCard) -> String {
    card_text(v, card, false)
}

/// [`final_card`] as the run screen draws it: the `Run` line of a run that is over carries its status alone ([`phase_shown`]). The command
/// line keeps [`final_card`], the record as the service stored it.
pub fn final_card_shown(v: &RunView, card: &FinalCard) -> String {
    card_text(v, card, true)
}

fn card_text(v: &RunView, card: &FinalCard, on_screen: bool) -> String {
    let na = || "n/a".to_string();
    let (zip, link) = (zip_text(v, card), link_text(card));
    let run = match (on_screen, phase_shown(v.status, v.phase)) {
        (true, None) => status_str(v.status),
        (true, Some(phase)) => format!("{} ({phase})", status_str(v.status)),
        (false, _) => format!(
            "{} ({})",
            status_str(v.status),
            phase_listed(v.status, v.phase)
        ),
    };
    let mut lines = vec![format!("Run       {} {run}", v.run_id)];
    if let Some(r) = &v.result {
        lines.push(format!("Outcome   {}", r.label));
        // The facts the harness read at the end of the run, directly under the label, as it wrote them.
        for o in &v.observations {
            lines.push(format!("Observed  {}", o.text));
        }
        if let Some(d) = &r.detail {
            let chosen = v.direct_model_id();
            let detail = failover::tidy(d, |id| ended_text(id, chosen));
            lines.push(format!("Detail    {detail}"));
        }
    } else if let Some(waited) = card.label_missing_after {
        // `ham watch` waited and `run.completed` did not come.
        lines.push(format!(
            "Outcome   not received: run.completed did not arrive within {}; ham watch {} shows it once it does",
            wait_text(waited),
            v.run_id
        ));
    }
    // The card names the model the run used. A run that switched names the model it ended on,
    // and the model the user asked for on the next line.
    if let Some(id) = v.direct_model_id() {
        match ended_on(v) {
            Some(e) => {
                lines.push(format!("Model     {}", model_text(&e.model_id)));
                lines.push(format!("Requested {}", model_text(id)));
            }
            None => lines.push(format!("Model     {}", model_text(id))),
        }
    }
    if let Some(n) = card.session.filter(|n| *n > 1) {
        lines.push(format!("Session   {}", session_text(n)));
    }
    lines.push(format!("PR        {}", v.pr_url.clone().unwrap_or_else(na)));
    lines.push(format!("Zip       {zip}"));
    lines.push(format!("{LINK_LABEL}{link}"));
    if v.zip.is_some() {
        lines.push(format!("{CLONE_LABEL}ham clone {}", v.run_id));
        lines.push(format!("{PUSH_LABEL}ham push {} <git-url>", v.run_id));
    }
    // Where the latest saved copy of the work is, whatever the run delivered: a blocked run has no zip and still has its work.
    if let Some(copy) = &v.saved {
        lines.push(crate::saved_copy::card_line(
            copy,
            crate::engines::of_view(v),
            v.has_ended(),
            &v.run_id,
        ));
    }
    if let Some(u) = card.usage {
        lines.push(u.line());
    }
    if let Some(t) = card.tests.as_deref().filter(|t| !t.trim().is_empty()) {
        lines.push(format!("{TESTS_LABEL}{t}"));
    }
    if let Some(f) = card.flow.as_deref().filter(|f| !f.trim().is_empty()) {
        lines.push(format!("{FLOW_LABEL}{f}"));
    }
    lines.join("\n")
}

/// One line per run for the watch loop and the dashboard header: no gate counts (a direct run has none), and the harness's label
/// last, once it has said how Cline ended.
pub fn status_line(v: &RunView) -> String {
    // While the clean-checkout test report runs, the stored phase (still `green`) is not what the run is doing.
    let phase = if v.report_running() {
        crate::report_progress::PHASE_WORDS.to_string()
    } else {
        phase_listed(v.status, v.phase)
    };
    let mut s = format!(
        "run {} {} ({}) agents {}",
        v.run_id,
        status_str(v.status),
        phase,
        v.agents.len()
    );
    if let Some(id) = v.direct_model_id() {
        s.push_str(&format!(" model {}", model_name(id)));
    }
    if let Some(e) = ended_on(v) {
        s.push_str(&format!(" {}", e.phrase()));
    }
    if let Some(pr) = &v.pr_url {
        s.push_str(&format!(" pr {pr}"));
    }
    if v.untrusted > 0 {
        s.push_str(&format!(" ignored {}", v.untrusted));
    }
    if let Some(r) = &v.result {
        s.push_str(&format!(" outcome: {}", r.label));
    }
    if let Some(q) = v.input.waiting() {
        s.push_str(&format!(" needs input: {}", q.title));
    }
    s
}
