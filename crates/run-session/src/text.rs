//! The words of the pull request. They say what Cline reported and that
//! the harness checked nothing: no sentence here claims the work was verified, tested, gated or evaluated.

use harness_protocol::common::Engine;
use harness_protocol::direct::{DirectOutcome, head_chars};

/// CodeCommit limits (`delivery::checks`): a title of 150 characters, a description or comment of 10,240.
pub const PR_TITLE_MAX_CHARS: usize = 150;
pub const PR_DESCRIPTION_MAX_CHARS: usize = 10_240;
pub const COMMENT_MAX_CHARS: usize = 10_240;
/// The most characters of Cline's message in the description; a longer message goes whole into comments of at most [`COMMENT_MAX_CHARS`].
pub const DESCRIPTION_MESSAGE_CHARS: usize = 6_000;
/// The most characters of the prompt in the package request (`objective`).
pub const OBJECTIVE_MAX_CHARS: usize = 8_000;
/// The first line of the prompt cut to this many characters in the title.
pub const TITLE_LINE_CHARS: usize = 120;

fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// `ham: ` and the first non-blank line of the prompt, cut to [`TITLE_LINE_CHARS`] characters with `…`. A prompt with
/// no text gives `ham: direct run`. Never starts `[goal:`.
pub fn title(prompt: &str) -> String {
    let first = prompt
        .lines()
        .map(one_line)
        .find(|l| !l.is_empty())
        .unwrap_or_else(|| "direct run".to_string());
    let cut = if first.chars().count() > TITLE_LINE_CHARS {
        format!("{}…", head_chars(&first, TITLE_LINE_CHARS))
    } else {
        first
    };
    let t = format!("ham: {cut}");
    head_chars(&t, PR_TITLE_MAX_CHARS).to_string()
}

/// `3731` as `1h 2m 11s`; the units that are zero at the front are left out.
pub fn wall_time(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
}

/// `text` as a block quote, every line (an empty one as a bare `>`).
pub fn quote(text: &str) -> String {
    text.trim_end_matches(['\n', '\r'])
        .lines()
        .map(|l| {
            let l = l.trim_end_matches('\r');
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the pull request is made of.
#[derive(Debug)]
pub struct PrFacts<'a> {
    pub run_id: &'a str,
    /// The engine that ran the session: its display name is in the label and in the lines that say whose message this is.
    pub engine: Engine,
    pub outcome: DirectOutcome,
    pub model_id: &'a str,
    pub wall_s: Option<u64>,
    pub message: Option<&'a str>,
    /// The specifics behind the outcome; a stop by the harness reads in the label (`DirectOutcome::label_for`).
    pub detail: Option<&'a str>,
    /// The head is the session's last saved snapshot, not its final bundle (a lost session).
    pub from_snapshot: bool,
    /// When that snapshot was taken, as `2026-10-13 18:50 UTC`.
    pub snapshot_at: Option<&'a str>,
    /// The earlier run this run continues.
    pub continues: Option<Continues<'a>>,
}

/// The earlier run a follow-up run continues, and the link of its pull request when it opened one.
#[derive(Clone, Copy, Debug)]
pub struct Continues<'a> {
    pub run_id: &'a str,
    pub pull_request_url: Option<&'a str>,
}

/// The description: the label on its own line; `Run <runId>. Model <modelId>. Wall time <h m s>.`; a blank line; for a follow-up run, the sentence
/// that names the run it continues and that run's pull request, and a blank line;
/// `Cline's last message:` and the message as a quote, cut to [`DESCRIPTION_MESSAGE_CHARS`]; a blank line; for a head that is
/// a snapshot, the sentence that says so and when it was taken, and a blank line; and the sentence that the harness checked nothing. At most [`PR_DESCRIPTION_MAX_CHARS`] characters.
pub fn description(f: &PrFacts<'_>) -> String {
    description_with(f, false)
}

/// The closing sentence of the description when a clean-checkout test report exists. The harness then ran the repository's own
/// test commands and recorded what they did, so "nothing was checked" would be wrong; it still made no judgment of the work.
fn with_report_closing(name: &str) -> String {
    format!(
        "The harness made no judgment of the work. The zip carries what {name} reported and the clean-checkout test report (EVIDENCE.md); a comment below gives the report's result."
    )
}

/// [`description`], with the closing sentence for a run that has a clean-checkout test report when `with_report` is set.
pub fn description_with(f: &PrFacts<'_>, with_report: bool) -> String {
    let name = f.engine.display_name();
    let mut out = String::new();
    out.push_str(&f.outcome.label_for_engine(f.engine, f.detail));
    out.push('\n');
    out.push_str(&format!("Run {}. Model {}.", f.run_id, f.model_id));
    if let Some(w) = f.wall_s {
        out.push_str(&format!(" Wall time {}.", wall_time(w)));
    }
    out.push_str("\n\n");
    if let Some(c) = &f.continues {
        match c.pull_request_url {
            Some(url) => out.push_str(&format!(
                "Follow-up of run {}; it continues the pull request {url}.",
                c.run_id
            )),
            None => out.push_str(&format!(
                "Follow-up of run {}; that run opened no pull request.",
                c.run_id
            )),
        }
        out.push_str("\n\n");
    }
    match f.message.map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => {
            out.push_str(&format!("{name}'s last message:\n"));
            if m.chars().count() > DESCRIPTION_MESSAGE_CHARS {
                out.push_str(&quote(head_chars(m, DESCRIPTION_MESSAGE_CHARS)));
                out.push_str("\n> … (cut here; the whole message is in the first comment and in EVIDENCE.md)");
            } else {
                out.push_str(&quote(m));
            }
        }
        None => out.push_str(&format!("{name} left no final message.")),
    }
    if f.from_snapshot {
        out.push_str("\n\nThis pull request holds the last saved snapshot of the work");
        if let Some(t) = f.snapshot_at {
            out.push_str(&format!(", taken at {t}"));
        }
        out.push_str(&format!(", not {name}'s final result."));
    }
    out.push_str("\n\n");
    out.push_str(&if with_report {
        with_report_closing(name)
    } else {
        format!(
            "Nothing in this pull request was checked by the harness. The zip carries what {name} reported (EVIDENCE.md)."
        )
    });
    head_chars(&out, PR_DESCRIPTION_MAX_CHARS).to_string()
}

/// The whole message, quoted, as comments of at most [`COMMENT_MAX_CHARS`] characters each: one comment headed `Cline's last message, whole:` when it
/// fits, else parts headed `Cline's last message, whole (part i of n):`, each filled to the limit. Joined without their headers the parts are the
/// quoted message.
fn whole_message_comments(name: &str, m: &str) -> Vec<String> {
    let quoted: Vec<char> = quote(m).chars().collect();
    let one = format!("{name}'s last message, whole:\n");
    if one.chars().count() + quoted.len() <= COMMENT_MAX_CHARS {
        return vec![format!("{one}{}", quoted.iter().collect::<String>())];
    }
    let header = |i: usize, n: usize| format!("{name}'s last message, whole (part {i} of {n}):\n");
    // The fewest parts whose room, after their headers, holds the message.
    let mut n = 2;
    while (1..=n)
        .map(|i| COMMENT_MAX_CHARS - header(i, n).chars().count())
        .sum::<usize>()
        < quoted.len()
    {
        n += 1;
    }
    let mut out = Vec::new();
    let mut at = 0;
    for i in 1..=n {
        let h = header(i, n);
        let room = COMMENT_MAX_CHARS - h.chars().count();
        let end = (at + room).min(quoted.len());
        if end == at {
            break;
        }
        out.push(format!("{h}{}", quoted[at..end].iter().collect::<String>()));
        at = end;
    }
    out
}

/// Comments with the whole message when the description had to cut it; none otherwise.
pub fn comments(message: Option<&str>) -> Vec<String> {
    comments_with_report(message, None)
}

/// [`comments`], then one comment with the sentence of the clean-checkout test report when there is one. The report only records what the
/// repository's own commands did in a clean checkout of this head.
pub fn comments_with_report(message: Option<&str>, report: Option<&str>) -> Vec<String> {
    comments_for(Engine::CLINE, message, report)
}

/// [`comments_with_report`] for a run of `engine`: the headings of the whole message name the engine. For Cline it is byte for byte the same.
pub fn comments_for(engine: Engine, message: Option<&str>, report: Option<&str>) -> Vec<String> {
    let mut out = match message.map(str::trim) {
        Some(m) if m.chars().count() > DESCRIPTION_MESSAGE_CHARS => {
            whole_message_comments(engine.display_name(), m)
        }
        _ => Vec::new(),
    };
    if let Some(sentence) = report {
        let body = format!(
            "Clean-checkout test report: {sentence}\nIt records what the repository's own test commands did in a clean checkout of this head. It does not say the tests are meaningful."
        );
        out.push(head_chars(&body, COMMENT_MAX_CHARS).to_string());
    }
    out
}
