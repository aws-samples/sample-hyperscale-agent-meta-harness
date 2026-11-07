//! The shared redaction function of the stored conversation:
//! secret classes found as spans, a [`Stream`] that holds back a trailing window of streamed text so a secret split across two
//! flushes is caught before any part of it is published, and the limits.
//!
//! The text comes from an engine and is stored for as long as the bucket lives, so the function is a guard,
//! not a promise: it hides the shapes below and the exact session secrets, and an arbitrary secret in an unknown encoding passes.
//! Every rule is a regular expression of the linear-time `regex` crate (no look-around, no backtracking), run over the text once; the
//! test `long_repetitive_text_is_scanned_in_linear_time` holds the cost to the length of the text.
//!
//! Classes: GitHub, GitLab, Slack, npm, OpenAI, Stripe and Google tokens; AWS access key ids; JWTs; `Authorization` with any scheme and
//! bearer credentials; `Cookie` and `Set-Cookie` lines; `_authToken`, `_auth` and `_password` lines; assignments of a secret-named key
//! (`password`, `passwd`, `pwd`, `pass`, `secret`, `token`, `api_key`, `credentials`, `private_key`, `access_key` and their compounds);
//! URL user information; `X-Amz-*`, `Signature=` and `AWSAccessKeyId=` query values; a long base64 or hex value after `=` or `:`
//! (hex digests and mixed-case runs; a 40-hex git commit id after `commit`, `blob` or `HEAD` and a run that looks like a path stay); a
//! PEM private key block (and a lone `BEGIN` marker, which hides everything to the end of the text); and the exact session secrets.
//!
//! `refusal::redact` and the scrub of `engine_diagnostic` stay as they are: they work on one line of an engine's stderr, with the
//! heuristics of that use (any run of 40 token characters mixing upper case, lower case and digits is hidden). A transcript is mostly
//! code and tool output, so the long-value rule here is limited to the context of `=` or `:`.

use regex::Regex;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

/// What a hidden value is replaced by.
pub const REDACTED: &str = "<redacted>";
/// The smallest window a [`Stream`] holds back: longer than the longest context a class needs in front of its value.
pub const MIN_WINDOW: usize = 256;
/// A session secret shorter than this is ignored: it would eat ordinary words.
pub const SECRET_MIN_BYTES: usize = 8;
/// The bytes kept from a hidden private key block, in case its end marker is split across two pushes.
const PEM_TAIL: usize = 64;

/// How a shape goes on when it is hidden before its end was seen (a [`Stream`] that holds too much).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cont {
    /// The shape has a fixed length: nothing follows.
    None,
    /// A private key block: everything up to its end marker.
    Pem,
    /// A run of token characters.
    Token,
    /// A value that ends at a blank or one of `"',;)}&`.
    Value,
    /// A value that ends at the end of the line.
    Line,
}

/// A found secret: `text[lead..end]` is the whole shape (its context included, so a stream does not cut inside it), `text[start..end]`
/// is what is replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub lead: usize,
    pub start: usize,
    pub end: usize,
    pub cont: Cont,
}

/// Why a text could not be redacted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("redaction failed")]
pub struct RedactFailed;

struct Rule {
    re: Regex,
    /// The group that starts the shape (0: the whole match) and the group that is replaced.
    lead: usize,
    hide: usize,
    cont: Cont,
    /// A value this says is code or prose, not a secret, is left alone.
    skip: Option<fn(&str) -> bool>,
}

fn rule(pattern: &str, lead: usize, hide: usize, cont: Cont) -> Rule {
    Rule {
        re: Regex::new(pattern).unwrap_or_else(|e| panic!("redact pattern {pattern}: {e}")),
        lead,
        hide,
        cont,
        skip: None,
    }
}

/// The value of a secret-named key that is plainly not a secret: a type (`Option<String>`, `ProcessCredentials`), a call (`format!(`),
/// a field path or name (`self.passes`, `page_token`), a constant (`TOKEN_OPEN`) or a status word (`required`). Everything else after
/// such a key is hidden.
fn plain_code_value(v: &str) -> bool {
    const WORDS: [&str; 17] = [
        "required",
        "optional",
        "string",
        "number",
        "boolean",
        "none",
        "null",
        "undefined",
        "true",
        "false",
        "hidden",
        "masked",
        "missing",
        "invalid",
        "default",
        "unset",
        "empty",
    ];
    let v = v.trim_matches(|c| c == '"' || c == '\'');
    if v.contains('(') {
        return true;
    }
    // A field path or a snake_case name (`self.passes`, `page_token`): lower-case words joined by `.` or `_`, at most two digits.
    let parts: Vec<&str> = v.split(['.', '_']).collect();
    if parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
        && v.bytes().filter(u8::is_ascii_digit).count() <= 2
    {
        return true;
    }
    if WORDS.iter().any(|w| v.eq_ignore_ascii_case(w)) {
        return true;
    }
    let ident =
        |c: char| c.is_ascii_alphabetic() || matches!(c, '_' | ':' | '<' | '>' | '&' | '[' | ']');
    let first_upper = v.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    if first_upper && v.chars().all(ident) {
        return true;
    }
    v.contains('_')
        && v.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && v.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|e| panic!("redact pattern {pattern}: {e}"))
}

static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // Prefixed tokens, hidden whole.
        rule(r"\bgh[pousr]_[A-Za-z0-9]{30,255}", 0, 0, Cont::Token),
        rule(r"\bgithub_pat_[A-Za-z0-9_]{22,255}", 0, 0, Cont::Token),
        rule(r"\bglpat-[A-Za-z0-9_-]{20,}", 0, 0, Cont::Token),
        rule(r"\bxox[abprso]-[A-Za-z0-9-]{10,}", 0, 0, Cont::Token),
        rule(r"\bnpm_[A-Za-z0-9]{36}", 0, 0, Cont::Token),
        rule(r"\bsk-[A-Za-z0-9][A-Za-z0-9_-]{30,}", 0, 0, Cont::Token),
        rule(r"\bsk_(?:live|test)_[A-Za-z0-9]{16,}", 0, 0, Cont::Token),
        rule(r"\bAIza[0-9A-Za-z_-]{35}", 0, 0, Cont::Token),
        rule(r"\b(?:AKIA|ASIA)[A-Z0-9]{12,}", 0, 0, Cont::Token),
        rule(
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]*",
            0,
            0,
            Cont::Token,
        ),
        // `Authorization` with any scheme: the credential after the scheme goes, the header and the scheme stay.
        rule(
            r#"(?i)\bauthorization["']?[ \t]*[:=][ \t]*["']?(?:[A-Za-z][A-Za-z0-9-]*[ \t]+)?([^\s"',;]{6,})"#,
            0,
            1,
            Cont::Value,
        ),
        rule(
            r"(?i)\bbearer[ \t]+([A-Za-z0-9._~+/=-]{16,})",
            0,
            1,
            Cont::Token,
        ),
        // `Cookie` and `Set-Cookie`: the value to the end of the line.
        rule(
            r"(?i)\b(?:set-)?cookie[ \t]*:[ \t]*([^\r\n]{4,})",
            0,
            1,
            Cont::Line,
        ),
        // `.npmrc` lines.
        rule(
            r#"(?mi)(?:^|[\s/:'"])(_auth(?:token)?|_password)[ \t]*=[ \t]*["']?([^\s"']{4,})"#,
            1,
            2,
            Cont::Value,
        ),
        // A secret-named key and its value (6 or more characters).
        Rule {
            skip: Some(plain_code_value),
            ..rule(
                concat!(
                    r#"(?mi)(?:^|[^A-Za-z0-9])("#,
                    r"password|passwd|pwd|pass|secret[_-]?access[_-]?key|secret[_-]?key|secret|token|api[_-]?key|credentials?|",
                    r"private[_-]?key|access[_-]?key|client[_-]?secret|auth[_-]?token|access[_-]?token|refresh[_-]?token|session[_-]?token",
                    r#")["']?[ \t]*[:=][ \t]*["']?([^\s"',;)}&]{6,})"#
                ),
                1,
                2,
                Cont::Value,
            )
        },
        // URL user information: the password between the first colon and the `@`.
        rule(
            r"(?i)\b[a-z][a-z0-9+.-]{1,15}://[^/\s:@]*:([^/\s@]+)@",
            0,
            1,
            Cont::Value,
        ),
        // Presigned and signed query values.
        rule(
            r#"(?i)\bX-Amz-[A-Za-z-]+=([^&\s"']{20,})"#,
            0,
            1,
            Cont::Value,
        ),
        rule(
            r"(?i)\bSignature=([A-Za-z0-9%+/=_-]{8,})",
            0,
            1,
            Cont::Value,
        ),
        rule(r"(?i)\bAWSAccessKeyId=([A-Z0-9]{16,})", 0, 1, Cont::Token),
    ]
});

static PEM_BEGIN: LazyLock<Regex> =
    LazyLock::new(|| re(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----"));
static PEM_END: LazyLock<Regex> =
    LazyLock::new(|| re(r"-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----"));
/// A long run of base64 or hex characters after `=` or `:`; [`looks_secret`] decides.
static LONG_RUN: LazyLock<Regex> =
    LazyLock::new(|| re(r#"[=:][ \t]*["']?([A-Za-z0-9+/_-]{40,}={0,2})"#));

/// Whether a long run found after `=` or `:` is a secret: a hex digest (32 or more), or a mix of upper case, lower case and digits. A
/// run with many slashes is a path; a 40-hex run after `commit`, `blob` or `HEAD` is a git object id.
fn looks_secret(run: &str, before: &str) -> bool {
    let body = run.trim_end_matches('=');
    let len = body.len();
    if body.matches('/').count() * 20 > len {
        return false;
    }
    if body.bytes().all(|b| b.is_ascii_hexdigit()) {
        if len == 40 {
            let from = before.len().saturating_sub(24);
            let from = (from..=before.len())
                .find(|i| before.is_char_boundary(*i))
                .unwrap_or(before.len());
            let ctx = before[from..].to_ascii_lowercase();
            if ctx.contains("commit") || ctx.contains("blob") || ctx.contains("head") {
                return false;
            }
        }
        return len >= 32;
    }
    body.bytes().any(|b| b.is_ascii_uppercase())
        && body.bytes().any(|b| b.is_ascii_lowercase())
        && body.bytes().any(|b| b.is_ascii_digit())
}

/// The exact values to hide, besides the shapes.
#[derive(Clone, Debug, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    /// Values shorter than [`SECRET_MIN_BYTES`] are ignored, repeats are kept once.
    pub fn new(secrets: Vec<String>) -> Self {
        let mut kept: Vec<String> = Vec::new();
        for s in secrets {
            if s.len() >= SECRET_MIN_BYTES && !kept.contains(&s) {
                kept.push(s);
            }
        }
        Self { secrets: kept }
    }

    /// Adds one more exact value (the session credentials rotate).
    pub fn add_secret(&mut self, s: &str) {
        if s.len() >= SECRET_MIN_BYTES && !self.secrets.iter().any(|k| k == s) {
            self.secrets.push(s.to_string());
        }
    }

    /// The length of the longest exact value: a [`Stream`] holds back at least this much.
    pub fn longest_secret(&self) -> usize {
        self.secrets.iter().map(String::len).max().unwrap_or(0)
    }

    /// Every secret in `text`, ordered and disjoint (overlapping shapes are one span).
    pub fn spans(&self, text: &str) -> Vec<Span> {
        let mut found: Vec<Span> = Vec::new();
        for r in RULES.iter() {
            for caps in r.re.captures_iter(text) {
                let (Some(lead), Some(hide)) = (caps.get(r.lead), caps.get(r.hide)) else {
                    continue;
                };
                if r.skip.is_some_and(|skip| skip(hide.as_str())) {
                    continue;
                }
                found.push(Span {
                    lead: lead.start(),
                    start: hide.start(),
                    end: hide.end(),
                    cont: r.cont,
                });
            }
        }
        // Private key blocks: from a BEGIN marker to the next END marker, or to the end of the text.
        let ends: Vec<(usize, usize)> = PEM_END
            .find_iter(text)
            .map(|m| (m.start(), m.end()))
            .collect();
        let mut at = 0;
        for b in PEM_BEGIN.find_iter(text) {
            if b.start() < at {
                continue;
            }
            let i = ends.partition_point(|(s, _)| *s < b.end());
            match ends.get(i) {
                Some((_, e)) => {
                    found.push(Span {
                        lead: b.start(),
                        start: b.start(),
                        end: *e,
                        cont: Cont::Pem,
                    });
                    at = *e;
                }
                None => {
                    found.push(Span {
                        lead: b.start(),
                        start: b.start(),
                        end: text.len(),
                        cont: Cont::Pem,
                    });
                    break;
                }
            }
        }
        // Long base64 and hex values after `=` or `:`.
        for caps in LONG_RUN.captures_iter(text) {
            let (Some(whole), Some(run)) = (caps.get(0), caps.get(1)) else {
                continue;
            };
            if looks_secret(run.as_str(), &text[..whole.start()]) {
                found.push(Span {
                    lead: whole.start(),
                    start: run.start(),
                    end: run.end(),
                    cont: Cont::Token,
                });
            }
        }
        // The exact values.
        for s in &self.secrets {
            for (i, _) in text.match_indices(s.as_str()) {
                found.push(Span {
                    lead: i,
                    start: i,
                    end: i + s.len(),
                    cont: Cont::None,
                });
            }
        }
        merge(found)
    }

    /// `text` with every secret replaced by [`REDACTED`].
    pub fn redact(&self, text: &str) -> String {
        apply(text, &self.spans(text))
    }
}

/// Spans sorted by position, overlapping ones joined: the join starts at the earliest lead and replacement start and ends at the
/// latest end, and goes on as the span that reaches furthest.
fn merge(mut found: Vec<Span>) -> Vec<Span> {
    found.sort_by_key(|s| (s.lead, s.end));
    let mut out: Vec<Span> = Vec::with_capacity(found.len());
    for s in found {
        match out.last_mut() {
            Some(last) if s.lead < last.end => {
                last.start = last.start.min(s.start);
                if s.end > last.end {
                    last.end = s.end;
                    last.cont = s.cont;
                }
            }
            _ => out.push(s),
        }
    }
    out
}

/// `text` with the replaced part of each span (in order, disjoint, inside the text) changed to [`REDACTED`].
fn apply(text: &str, spans: &[Span]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for s in spans {
        out.push_str(&text[at..s.start]);
        out.push_str(REDACTED);
        at = s.end;
    }
    out.push_str(&text[at..]);
    out
}

/// [`Redactor::redact`] that turns a panic into an error: the caller stores a marker and records a gap, never the text.
pub fn redact_checked(r: &Redactor, text: &str) -> Result<String, RedactFailed> {
    catch_unwind(AssertUnwindSafe(|| r.redact(text))).map_err(|_| RedactFailed)
}

/// One call for a list of exact values.
pub fn redact_with_secrets(text: &str, secrets: &[String]) -> String {
    Redactor::new(secrets.to_vec()).redact(text)
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Streamed text that is redacted as it goes. Each [`Stream::push`] returns the text that is safe to publish: everything before a
/// trailing window of the text held, moved back to the start of any shape that the cut would split or that is still open at the end of
/// the text. The next push scans the window together with the new text, so a secret split across two pushes is seen whole before any
/// part of it leaves. Joined with [`Stream::finish`], the output equals the whole text redacted in one call, for shapes shorter than
/// the hold limit; a longer shape is hidden to its end ([`Cont`]) and the text after it comes back.
#[derive(Debug)]
pub struct Stream {
    window: usize,
    max_hold: usize,
    held: String,
    cont: Option<Cont>,
}

impl Stream {
    /// `window` is raised to [`MIN_WINDOW`]; `max_hold`, the most a single open shape is held, to twice the window.
    pub fn new(window: usize, max_hold: usize) -> Self {
        let window = window.max(MIN_WINDOW);
        Self {
            window,
            max_hold: max_hold.max(window * 2),
            held: String::new(),
            cont: None,
        }
    }

    pub fn window(&self) -> usize {
        self.window
    }

    pub fn max_hold(&self) -> usize {
        self.max_hold
    }

    /// The bytes held back now.
    pub fn held_len(&self) -> usize {
        self.held.len()
    }

    /// Adds `text` and returns what is safe to publish.
    pub fn push(&mut self, r: &Redactor, text: &str) -> String {
        self.held.push_str(text);
        self.step(r, false)
    }

    /// The end of the message: everything held is redacted (an open shape ends with the text) and returned.
    pub fn finish(&mut self, r: &Redactor) -> String {
        let out = self.step(r, true);
        self.held.clear();
        self.cont = None;
        out
    }

    /// Drops the start of the held text that continues a shape that was hidden early. True when the shape has not ended yet.
    fn consume_continuation(&mut self, end: bool) -> bool {
        let Some(cont) = self.cont else {
            return false;
        };
        let stop = match cont {
            Cont::None => Some(0),
            Cont::Pem => PEM_END.find(&self.held).map(|m| m.end()),
            Cont::Token => self
                .held
                .find(|c: char| {
                    !(c.is_ascii_alphanumeric()
                        || matches!(c, '.' | '_' | '~' | '+' | '/' | '=' | '-' | '%'))
                })
                .or_else(|| (end).then_some(self.held.len())),
            Cont::Value => self
                .held
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '"' | '\'' | ',' | ';' | ')' | '}' | '&')
                })
                .or_else(|| (end).then_some(self.held.len())),
            Cont::Line => self
                .held
                .find('\n')
                .or_else(|| (end).then_some(self.held.len())),
        };
        match stop {
            Some(i) => {
                self.held.drain(..i);
                self.cont = None;
                false
            }
            None => {
                if cont == Cont::Pem {
                    let keep = floor_boundary(&self.held, self.held.len().saturating_sub(PEM_TAIL));
                    self.held.drain(..keep);
                } else {
                    self.held.clear();
                }
                true
            }
        }
    }

    fn step(&mut self, r: &Redactor, end: bool) -> String {
        if self.consume_continuation(end) {
            return String::new();
        }
        let len = self.held.len();
        let window = self.window.max(r.longest_secret() + 16);
        if !end && len <= window {
            return String::new();
        }
        let spans = r.spans(&self.held);
        let mut cut = if end {
            len
        } else {
            floor_boundary(&self.held, len.saturating_sub(window))
        };
        if !end {
            // The held text starts at a word, so a rule that needs a non-letter in front of its name sees the same text it would in the
            // whole. A word that starts more than 128 bytes before the cut is cut where it is.
            let raw = cut;
            let aligned = self.held[..raw]
                .rfind(char::is_whitespace)
                .map(|p| p + self.held[p..].chars().next().map_or(1, char::len_utf8));
            cut = match aligned {
                Some(a) if raw - a <= 128 => a,
                None if raw <= 128 => 0,
                _ => raw,
            };
        }
        let mut pulled: Option<Span> = None;
        if !end {
            // A cut inside a shape, or in front of one that is still open, moves back to where the shape starts.
            for s in &spans {
                if s.lead < cut && (cut < s.end || s.end == len) {
                    cut = s.lead;
                    pulled = Some(*s);
                    break;
                }
            }
        }
        if let Some(s) = pulled
            && s.end == len
            && len - cut > self.max_hold
        {
            // Held too long: hide the shape to its end now and drop its continuation as it arrives.
            let before: Vec<Span> = spans.iter().filter(|x| x.end <= cut).copied().collect();
            let mut out = apply(&self.held[..cut], &before);
            out.push_str(&self.held[cut..s.start]);
            out.push_str(REDACTED);
            if s.cont == Cont::Pem {
                let keep = floor_boundary(&self.held, len.saturating_sub(PEM_TAIL));
                self.held.drain(..keep);
            } else {
                self.held.clear();
            }
            self.cont = (s.cont != Cont::None).then_some(s.cont);
            return out;
        }
        let inside: Vec<Span> = spans.iter().filter(|x| x.end <= cut).copied().collect();
        let out = apply(&self.held[..cut], &inside);
        self.held.drain(..cut);
        out
    }
}
