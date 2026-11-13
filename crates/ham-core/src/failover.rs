//! The failover sentences in the `detail` of a direct run's `run.completed`.
//!
//! agentd writes the detail as sentences joined with one space (`agentd::direct::summary::build_summary`):
//! `Ran on <model id> after <n> switch(es).` when the run switched model or route, then `Every route was down for the last <s> s.` when
//! every route was down, then the classifier's cause, which starts with a lowercase letter and has no full stop. The wire text stays as
//! the harness wrote it. The client reads the lead sentences to show the model the run ended on by its display name, and writes the
//! sentences for a person: a capital letter and a full stop on the cause. A detail that starts with neither lead sentence is not touched.

/// What agentd's `ran_on_sentence` wrote: the model id the run ended on and how many switches it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RanOn<'a> {
    pub model_id: &'a str,
    pub switches: u32,
}

/// A detail split into the sentences agentd joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lead<'a> {
    pub ran_on: Option<RanOn<'a>>,
    /// `Every route was down for the last <s> s.` as written.
    pub outage: Option<&'a str>,
    /// What follows the lead sentences, trimmed; empty when nothing does.
    pub cause: &'a str,
}

/// `Ran on <id> after <n> switch.` or `switches.`: the id, `n` (at least 1) and the text after the sentence.
fn take_ran_on(s: &str) -> Option<(RanOn<'_>, &str)> {
    let s = s.strip_prefix("Ran on ")?;
    let (model_id, s) = s.split_once(' ')?;
    let s = s.strip_prefix("after ")?;
    let digits = s.find(|c: char| !c.is_ascii_digit())?;
    let switches: u32 = s[..digits].parse().ok().filter(|n| *n > 0)?;
    let s = s[digits..].strip_prefix(" switch")?;
    let s = s.strip_prefix("es").unwrap_or(s).strip_prefix('.')?;
    Some((RanOn { model_id, switches }, s.trim_start()))
}

/// `Every route was down for the last <s> s.`: the sentence and the text after it.
fn take_outage(s: &str) -> Option<(&str, &str)> {
    let tail = s.strip_prefix("Every route was down for the last ")?;
    let digits = tail
        .find(|c: char| !c.is_ascii_digit())
        .filter(|d| *d > 0)?;
    let after = tail[digits..].strip_prefix(" s.")?;
    Some((&s[..s.len() - after.len()], after.trim_start()))
}

/// Splits `detail` into agentd's lead sentences and the cause; `None` when it starts with neither.
pub fn split(detail: &str) -> Option<Lead<'_>> {
    let rest = detail.trim();
    let (ran_on, rest) = match take_ran_on(rest) {
        Some((r, rest)) => (Some(r), rest),
        None => (None, rest),
    };
    let (outage, rest) = match take_outage(rest) {
        Some((o, rest)) => (Some(o), rest),
        None => (None, rest),
    };
    (ran_on.is_some() || outage.is_some()).then_some(Lead {
        ran_on,
        outage,
        cause: rest,
    })
}

/// `Ran on <name> after <n> switch(es)` (no full stop).
pub fn ran_on_phrase(name: &str, switches: u32) -> String {
    format!("Ran on {name} after {}", switches_text(switches))
}

/// `1 switch`, `9 switches`.
pub fn switches_text(switches: u32) -> String {
    format!("{switches} switch{}", if switches == 1 { "" } else { "es" })
}

/// `cause` as a sentence: the first letter in capitals and a full stop unless it ends with one.
fn sentence(cause: &str) -> String {
    let mut chars = cause.chars();
    let mut s: String = chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if !s.ends_with(['.', '!', '?']) {
        s.push('.');
    }
    s
}

/// The detail a person reads: the lead sentences with `name_of` giving the model id of `Ran on` its display text, and the cause as a
/// sentence. A detail with no lead sentence is returned as the harness wrote it.
pub fn tidy(detail: &str, name_of: impl Fn(&str) -> String) -> String {
    let Some(lead) = split(detail) else {
        return detail.to_string();
    };
    let mut parts = Vec::new();
    if let Some(r) = lead.ran_on {
        parts.push(format!(
            "{}.",
            ran_on_phrase(&name_of(r.model_id), r.switches)
        ));
    }
    if let Some(o) = lead.outage {
        parts.push(o.to_string());
    }
    if !lead.cause.is_empty() {
        parts.push(sentence(lead.cause));
    }
    parts.join(" ")
}
