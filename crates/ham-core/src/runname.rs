//! Run names: the 5 to 7 word label a run shows in the Runs list. Pure functions only, with no dependency besides `sanitize`,
//! `redact` and `utility`: the caller (`run-session`, the TUI) owns the call to the model.
//!
//! - [`plan`] builds what the utility model may see: the shared [`utility::excerpt`] (the prompt with credentials hidden and control
//!   bytes stripped, cut to its first 2,000 and last 1,000 characters, angle brackets escaped) wrapped in a `<request>` tag. A prompt of
//!   fewer than 4 words asks no model ([`utility::is_short`]).
//! - [`judge`] decides whether the model's reply may become a name: 19 steps in a fixed order, the first failure gives a [`Reason`]. The
//!   reply is untrusted input to a terminal, so the rejecting steps reject and never clean. The four safety checks on the raw reply are
//!   [`utility::guard_raw`], shared with the run tags.
//! - [`seed_name`] is the name built from the stored prompt head with no model. It is shown first and is the floor when the model is not
//!   asked, fails, or is refused by the judge.
//! - [`accept_stored`] and [`check_stored`] check a name again when it is read back.
//!
//! A run name is display text only. It is never an S3 key, a branch, a zip or folder name, a pull request title, a shell argument or an
//! outcome label (root invariant 1): `rg runName crates/` finds no delivery code that reads it.

use crate::redact::REDACTED;
use crate::sanitize::{COMBINING_MAX, marker};
use crate::utility::{
    self, GuardReason, UtilityFault, UtilityReply, non_space_chars, unspaced_majority,
};
pub use crate::utility::{redacted_prompt, word_count};
use std::collections::HashSet;
use std::fmt;
use std::sync::OnceLock;

/// The version of the system text. A change to `assets/run-name-system.txt` is a new version, measured again on the live model.
pub const PROMPT_VERSION: &str = "rn-1";
/// `maxTokens` of the request: the longest of 124 live replies with thinking off was 31 tokens.
pub const MAX_TOKENS: u32 = 64;
/// The stop sequence; the reply text does not contain it.
pub const STOP: &str = "</title>";
/// The name of a run whose prompt leaves nothing to build a seed from.
pub const UNTITLED: &str = "Untitled coding run";
/// A stored name is at most this many terminal columns (wide East Asian characters count 2) ...
pub const MAX_NAME_COLUMNS: usize = 64;
/// ... and this many bytes.
pub const MAX_NAME_BYTES: usize = 256;
/// The most characters of the user turn: 3,007 characters of excerpt, every `<` and `>` made four characters, and the wrapper.
pub const MAX_USER_TEXT_CHARS: usize = 12_100;
/// A prompt of this many characters or fewer goes to the model whole (the shared excerpt of `utility`).
pub const WHOLE_CHARS: usize = utility::EXCERPT_WHOLE_CHARS;
/// The head of a longer prompt that the model sees ...
pub const HEAD_CHARS: usize = 2000;
/// ... and its tail.
pub const TAIL_CHARS: usize = 1000;
/// What stands where the middle of a long prompt was left out.
pub const CUT_MARK: &str = utility::EXCERPT_CUT_MARK;
const _: () = assert!(HEAD_CHARS == utility::EXCERPT_HEAD_CHARS);
const _: () = assert!(TAIL_CHARS == utility::EXCERPT_TAIL_CHARS);
/// A reply longer than this is not a name.
const MAX_RAW_CHARS: usize = 400;
/// A word longer than this is not part of a name (the word limit of `utility::guard_raw`).
const MAX_WORD_CHARS: usize = utility::GUARD_MAX_WORD_CHARS;
/// The most characters of an unspaced script (Chinese, Japanese, Thai) in a name.
const UNSPACED_MAX: usize = 20;
/// The fewest characters of an unspaced script in a name, unless the prompt is short.
const UNSPACED_MIN: usize = 8;
/// The most words of a spaced name, the fewest, and the words from which a longer name is cut back.
const WORDS_MAX: usize = 7;
const WORDS_MIN: usize = 5;
/// Names of up to this many words (and prompts of fewer than `SHORT_PROMPT_WORDS`) may be shorter than `WORDS_MIN`.
const SHORT_PROMPT_WORDS: usize = 8;
/// A prompt of fewer than this many characters of an unspaced script may have a name shorter than `UNSPACED_MIN`.
const SHORT_PROMPT_UNSPACED: usize = 15;
const MARKUP: &str = "<>{}[]$\\@";
const ALLOWED_PUNCT: &str = "-_.+#/&'(),";

const SYSTEM_TEXT: &str = include_str!("../assets/run-name-system.txt");

/// The system text of the request, without the file's final newline.
pub fn system_text() -> &'static str {
    SYSTEM_TEXT.strip_suffix('\n').unwrap_or(SYSTEM_TEXT)
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Reasons
// ---------------------------------------------------------------------------------------------------------------------------------

/// How a Converse stop reason other than `stop_sequence` is named in a [`Reason`]. The set is closed so a log field has a bounded set of
/// values; anything this build does not know is [`StopKind::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StopKind {
    EndTurn,
    ToolUse,
    MaxTokens,
    GuardrailIntervened,
    ContentFiltered,
    MalformedModelOutput,
    MalformedToolUse,
    ModelContextWindowExceeded,
    /// Anthropic's `refusal`; how Converse reports a classifier decline is not documented.
    Refusal,
    Other,
}

impl StopKind {
    fn parse(stop: &str) -> StopKind {
        match stop {
            "end_turn" => StopKind::EndTurn,
            "tool_use" => StopKind::ToolUse,
            "max_tokens" => StopKind::MaxTokens,
            "guardrail_intervened" => StopKind::GuardrailIntervened,
            "content_filtered" => StopKind::ContentFiltered,
            "malformed_model_output" => StopKind::MalformedModelOutput,
            "malformed_tool_use" => StopKind::MalformedToolUse,
            "model_context_window_exceeded" => StopKind::ModelContextWindowExceeded,
            "refusal" => StopKind::Refusal,
            _ => StopKind::Other,
        }
    }
}

/// Why a run has no model-written name, or why a name is refused: a fixed set of codes, so a log field and a metric stay bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    // What the operation decides before or around the call.
    ShortPrompt,
    Disabled,
    NoRun,
    Kept,
    Timeout,
    Throttle,
    ServiceError,
    ConfigError,
    OpDeadline,
    // The judge, in the order of its steps.
    Stop(StopKind),
    NoText,
    RawTooLong,
    ControlChars,
    InvisibleChars,
    CredentialShape,
    LongToken,
    Untagged,
    /// The model said `NONE`, or nothing was left after cleaning (code `none`).
    NoName,
    Multiline,
    MarkupOrLink,
    Meta,
    OutcomeWord,
    EchoesInstructions,
    TooWide,
    TooLong,
    TooShort,
    Ungrounded,
    Charset,
}

/// Every code [`Reason::as_str`] can return.
pub const REASON_CODES: &[&str] = &[
    "short_prompt",
    "disabled",
    "no_run",
    "kept",
    "timeout",
    "throttle",
    "service_error",
    "config_error",
    "op_deadline",
    "stop_end_turn",
    "stop_tool_use",
    "stop_max_tokens",
    "stop_guardrail_intervened",
    "stop_content_filtered",
    "stop_malformed_model_output",
    "stop_malformed_tool_use",
    "stop_model_context_window_exceeded",
    "stop_refusal",
    "stop_other",
    "no_text",
    "raw_too_long",
    "control_chars",
    "invisible_chars",
    "credential_shape",
    "long_token",
    "untagged",
    "none",
    "multiline",
    "markup_or_link",
    "meta",
    "outcome_word",
    "echoes_instructions",
    "too_wide",
    "too_long",
    "too_short",
    "ungrounded",
    "charset",
];

impl Reason {
    /// The fixed code.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::ShortPrompt => "short_prompt",
            Reason::Disabled => "disabled",
            Reason::NoRun => "no_run",
            Reason::Kept => "kept",
            Reason::Timeout => "timeout",
            Reason::Throttle => "throttle",
            Reason::ServiceError => "service_error",
            Reason::ConfigError => "config_error",
            Reason::OpDeadline => "op_deadline",
            Reason::Stop(StopKind::EndTurn) => "stop_end_turn",
            Reason::Stop(StopKind::ToolUse) => "stop_tool_use",
            Reason::Stop(StopKind::MaxTokens) => "stop_max_tokens",
            Reason::Stop(StopKind::GuardrailIntervened) => "stop_guardrail_intervened",
            Reason::Stop(StopKind::ContentFiltered) => "stop_content_filtered",
            Reason::Stop(StopKind::MalformedModelOutput) => "stop_malformed_model_output",
            Reason::Stop(StopKind::MalformedToolUse) => "stop_malformed_tool_use",
            Reason::Stop(StopKind::ModelContextWindowExceeded) => {
                "stop_model_context_window_exceeded"
            }
            Reason::Stop(StopKind::Refusal) => "stop_refusal",
            Reason::Stop(StopKind::Other) => "stop_other",
            Reason::NoText => "no_text",
            Reason::RawTooLong => "raw_too_long",
            Reason::ControlChars => "control_chars",
            Reason::InvisibleChars => "invisible_chars",
            Reason::CredentialShape => "credential_shape",
            Reason::LongToken => "long_token",
            Reason::Untagged => "untagged",
            Reason::NoName => "none",
            Reason::Multiline => "multiline",
            Reason::MarkupOrLink => "markup_or_link",
            Reason::Meta => "meta",
            Reason::OutcomeWord => "outcome_word",
            Reason::EchoesInstructions => "echoes_instructions",
            Reason::TooWide => "too_wide",
            Reason::TooLong => "too_long",
            Reason::TooShort => "too_short",
            Reason::Ungrounded => "ungrounded",
            Reason::Charset => "charset",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).as_str())
    }
}

impl From<GuardReason> for Reason {
    fn from(reason: GuardReason) -> Reason {
        match reason {
            GuardReason::ControlChars => Reason::ControlChars,
            GuardReason::InvisibleChars => Reason::InvisibleChars,
            GuardReason::CredentialShape => Reason::CredentialShape,
            GuardReason::LongToken => Reason::LongToken,
        }
    }
}

impl From<UtilityFault> for Reason {
    fn from(fault: UtilityFault) -> Reason {
        match fault {
            UtilityFault::Timeout => Reason::Timeout,
            UtilityFault::Throttle => Reason::Throttle,
            UtilityFault::Service => Reason::ServiceError,
            UtilityFault::Config => Reason::ConfigError,
            UtilityFault::NoText => Reason::NoText,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Characters, widths, words
// ---------------------------------------------------------------------------------------------------------------------------------

/// The common combining marks (the blocks of `sanitize`, plus the kana voicing marks, the Thai vowel and tone marks and the Devanagari
/// signs). The table is short on purpose: a mark it lacks fails the character check, which falls back to the seed.
fn is_mark(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036f
            | 0x0591..=0x05bd
            | 0x05bf
            | 0x05c1..=0x05c2
            | 0x05c4..=0x05c5
            | 0x05c7
            | 0x0610..=0x061a
            | 0x064b..=0x065f
            | 0x0670
            | 0x06d6..=0x06dc
            | 0x06df..=0x06e4
            | 0x06e7..=0x06e8
            | 0x06ea..=0x06ed
            | 0x0900..=0x0903
            | 0x093a..=0x093c
            | 0x093e..=0x094f
            | 0x0951..=0x0957
            | 0x0962..=0x0963
            | 0x0e31
            | 0x0e34..=0x0e3a
            | 0x0e47..=0x0e4e
            | 0x1ab0..=0x1aff
            | 0x1dc0..=0x1dff
            | 0x20d0..=0x20ff
            | 0x3099..=0x309a
            | 0xfe20..=0xfe2f
    )
}

/// East Asian wide and fullwidth characters, and the emoji blocks, take two columns.
fn is_wide(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x115f
            | 0x2e80..=0x303e
            | 0x3041..=0x33ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xa000..=0xa4cf
            | 0xa960..=0xa97f
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1f64f
            | 0x1f680..=0x1f6ff
            | 0x1f900..=0x1f9ff
            | 0x1fa70..=0x1faff
            | 0x20000..=0x3fffd
    )
}

/// Emoji and pictographs, which a model adds out of habit and which a name does without.
fn is_pictograph(c: char) -> bool {
    matches!(
        c as u32,
        0x1f000..=0x1faff | 0x2600..=0x27bf | 0x2b00..=0x2bff | 0x2300..=0x23ff
    )
}

/// The terminal columns of `s`: 2 for a wide character, 0 for a combining mark, 1 otherwise.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            if is_mark(c) {
                0
            } else if is_wide(c) {
                2
            } else {
                1
            }
        })
        .sum()
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The first `n` characters of `s` that are not blanks, with the blanks between them, trimmed.
fn cut_non_space(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for c in s.chars() {
        if !c.is_whitespace() {
            if count == n {
                break;
            }
            count += 1;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// Words that carry no topic: English, and the short Spanish, German, French and Italian ones.
const FUNCTION_WORDS: &[&str] = &[
    "a", "an", "the", "to", "of", "for", "in", "on", "with", "and", "or", "by", "from", "into",
    "at", "as", "per", "via", "over", "under", "when", "while", "than", "so", "but", "then",
    "that", "which", "is", "it", "this", "be", "if", "who", "because", "also", "about", "after",
    "before", "de", "la", "el", "los", "las", "y", "para", "por", "con", "del", "en", "un", "una",
    "o", "al", "que", "und", "der", "die", "das", "zu", "mit", "den", "dem", "ein", "eine", "von",
    "im", "le", "les", "et", "du", "des", "pour", "au", "aux", "avec", "sur", "dans", "da", "di",
    "il", "che",
];

fn is_function_word(w: &str) -> bool {
    let w = w.trim_matches(|c: char| !c.is_alphanumeric());
    FUNCTION_WORDS.contains(&w.to_lowercase().as_str())
}

/// Lower-case words made of letters and digits.
fn lower_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Plan: what the model may see
// ---------------------------------------------------------------------------------------------------------------------------------

/// The decision to ask the model, and the text to ask it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// False when the prompt is too short to name: the seed stays and no call is made.
    pub ask: bool,
    /// `short_prompt` when `ask` is false.
    pub reason: Option<&'static str>,
    /// The user turn: `<request>`, the excerpt with `& < >` escaped, `</request>` and the instruction. Built even when `ask` is false.
    pub user_text: String,
}

/// Builds the user turn for `prompt` from the shared [`utility::excerpt`]: the redacted text, whole up to 3,000 characters and otherwise
/// its first 2,000 and last 1,000 around [`CUT_MARK`], escaped and wrapped. At most [`MAX_USER_TEXT_CHARS`] characters whatever the size
/// of the prompt. The run tags build their user turn from the same excerpt.
pub fn plan(prompt: &str) -> Plan {
    let ex = utility::excerpt(prompt);
    let short = utility::is_short(&ex);
    let user_text = format!(
        "<request>\n{}\n</request>\n\nWrite the title for the run above.",
        ex.escaped
    );
    Plan {
        ask: !short,
        reason: short.then_some(Reason::ShortPrompt.as_str()),
        user_text,
    }
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Judge: may this reply become a name?
// ---------------------------------------------------------------------------------------------------------------------------------

/// A name the judge accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Judged {
    pub name: String,
    /// The reply had 8 to 10 words and was cut back to 5 to 7.
    pub squeezed: bool,
}

const META: [&str; 9] = [
    "too vague",
    "cannot",
    "can't",
    "unable",
    "nothing to name",
    "not enough",
    "no request",
    "untitled",
    "unclear",
];

const OUTCOME_WORDS: [&str; 4] = ["verified", "tested", "gated", "evaluated"];

/// Steps 2 to 5, which run on the whole raw reply before any text is extracted from it: the name's own length limit, then the four
/// checks of [`utility::guard_raw`] that every helper shares.
fn raw_checks(raw: &str) -> Result<(), Reason> {
    if raw.chars().count() > MAX_RAW_CHARS {
        return Err(Reason::RawTooLong);
    }
    utility::guard_raw(raw)?;
    Ok(())
}

/// Step 6: the text after the one `<title>` tag, or the whole reply when it is one clean line.
fn title_of_reply(raw: &str) -> Result<&str, Reason> {
    match raw.matches("<title>").count() {
        0 => {
            let t = raw.trim();
            if t.contains('\n') || t.contains('<') {
                Err(Reason::Untagged)
            } else {
                Ok(t)
            }
        }
        1 => Ok(raw.split_once("<title>").map_or("", |(_, rest)| rest)),
        _ => Err(Reason::Untagged),
    }
}

/// Step 9: markup, variables, mentions and links are refused, never cleaned.
fn has_markup(t: &str) -> bool {
    t.chars().any(|c| MARKUP.contains(c))
        || t.contains("://")
        || t.to_ascii_lowercase().contains("www.")
}

/// A single quote between two letters or digits is an apostrophe; anywhere else it is decoration.
fn apostrophe_or_space(chars: &[char], i: usize) -> char {
    let before = i.checked_sub(1).and_then(|p| chars.get(p));
    let after = chars.get(i + 1);
    if before.is_some_and(|c| c.is_alphanumeric()) && after.is_some_and(|c| c.is_alphanumeric()) {
        '\''
    } else {
        ' '
    }
}

/// Inside the text, `**`, backticks, double quotes, guillemets and the characters `: ; | ! ? = % ~ ^` become blanks; a single quote
/// stays where it is an apostrophe.
fn inner_replace(s: &str) -> String {
    let s = s.replace("**", " ");
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '`' | '"' | '\u{201c}' | '\u{201d}' | '\u{ab}' | '\u{bb}' | '\u{300c}' | '\u{300d}'
            | ':' | ';' | '|' | '!' | '?' | '=' | '%' | '~' | '^' => out.push(' '),
            '\'' | '\u{2018}' | '\u{2019}' => out.push(apostrophe_or_space(&chars, i)),
            _ => out.push(c),
        }
    }
    out
}

/// Leading `#`, `-`, `*` and a number with `.` or `)`.
fn strip_leading_marker(s: &str) -> &str {
    let s = s.trim_start();
    let t = s.trim_start_matches(['#', '-', '*']);
    if t.len() != s.len() {
        return t.trim_start();
    }
    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if (1..=3).contains(&digits) {
        let rest = &s[digits..];
        if let Some(after) = rest.strip_prefix(['.', ')'])
            && after.starts_with(char::is_whitespace)
        {
            return after.trim_start();
        }
    }
    s
}

/// One pass of steps 10 and 11: pictographs out, decoration to blanks, blanks collapsed, a leading marker and trailing punctuation off,
/// and every piece without a letter or a digit (a stray `-` or `&`) dropped, since it is not a word.
fn clean_pass(s: &str) -> String {
    let dropped: String = s.chars().filter(|c| !is_pictograph(*c)).collect();
    let collapsed = collapse(&inner_replace(&dropped));
    let head = strip_leading_marker(&collapsed);
    head.trim_end_matches(['.', ',', '\u{2026}', '*'])
        .trim()
        .split(' ')
        .filter(|w| w.chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Steps 10 and 11 until nothing changes (stripping one edge can expose the next).
fn clean(text: &str) -> String {
    let mut s = text.trim().to_string();
    for _ in 0..8 {
        let next = clean_pass(&s);
        if next == s {
            break;
        }
        s = next;
    }
    s
}

/// Step 13: a name never says verified, tested, gated or evaluated unless the prompt did.
fn outcome_word(name: &str, prompt: &str) -> Result<(), Reason> {
    let in_prompt = lower_tokens(prompt);
    for t in lower_tokens(name) {
        if OUTCOME_WORDS.contains(&t.as_str()) && !in_prompt.contains(&t) {
            return Err(Reason::OutcomeWord);
        }
    }
    Ok(())
}

/// The four-word runs of the system text, lower-cased.
fn instruction_runs() -> &'static HashSet<String> {
    static RUNS: OnceLock<HashSet<String>> = OnceLock::new();
    RUNS.get_or_init(|| {
        lower_tokens(system_text())
            .windows(4)
            .map(|w| w.join(" "))
            .collect()
    })
}

/// Step 14: a name that holds four consecutive words of the system text is the instructions read back.
fn echoes_instructions(name: &str) -> bool {
    let runs = instruction_runs();
    lower_tokens(name)
        .windows(4)
        .any(|w| runs.contains(&w.join(" ")))
}

/// Steps 15 and 16 for a name in a spaced script. A name of 11 words or more fails; one wider than 64 columns loses trailing words
/// while 5 remain; 8 to 10 words are squeezed; 4 or fewer pass only for a short prompt.
fn fit_spaced(name: &str, prompt: &str) -> Result<(String, bool), Reason> {
    let mut ws: Vec<&str> = name.split(' ').collect();
    if ws.len() > WORDS_MAX + 3 {
        return Err(Reason::TooLong);
    }
    while display_width(&ws.join(" ")) > MAX_NAME_COLUMNS && ws.len() > WORDS_MIN {
        ws.pop();
    }
    let joined = ws.join(" ");
    if display_width(&joined) > MAX_NAME_COLUMNS {
        return Err(Reason::TooWide);
    }
    match ws.len() {
        WORDS_MIN..=WORDS_MAX => Ok((joined, false)),
        n if n > WORDS_MAX => Ok((squeeze(&joined), true)),
        _ if word_count(prompt) < SHORT_PROMPT_WORDS => Ok((joined, false)),
        _ => Err(Reason::TooShort),
    }
}

/// Steps 15 and 16 for a name in an unspaced script: 8 to 20 characters; more are cut at 20; fewer pass only for a short prompt.
fn fit_unspaced(name: &str, prompt: &str) -> Result<(String, bool), Reason> {
    let n = non_space_chars(name);
    let name = if n > UNSPACED_MAX {
        cut_non_space(name, UNSPACED_MAX)
    } else if n < UNSPACED_MIN && non_space_chars(prompt) >= SHORT_PROMPT_UNSPACED {
        return Err(Reason::TooShort);
    } else {
        name.to_string()
    };
    if display_width(&name) > MAX_NAME_COLUMNS {
        return Err(Reason::TooWide);
    }
    Ok((name, false))
}

/// Cuts a name of 8 to 10 words back to 5 to 7: keep the first 7; drop the seventh if it is a function word, else cut before the sixth
/// if that is one; then drop trailing function words while more than 5 remain. A name of any other length is returned as it is.
pub fn squeeze(name: &str) -> String {
    let w: Vec<&str> = name.split_whitespace().collect();
    if !(WORDS_MAX + 1..=WORDS_MAX + 3).contains(&w.len()) {
        return name.to_string();
    }
    let mut keep = if is_function_word(w[6]) {
        6
    } else if is_function_word(w[5]) {
        5
    } else {
        7
    };
    while keep > WORDS_MIN && is_function_word(w[keep - 1]) {
        keep -= 1;
    }
    w[..keep].join(" ")
}

/// The content stems of a text: the first five letters of each word of four letters or more that is not a function word.
fn stems(s: &str) -> HashSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| w.chars().count() >= 4 && !FUNCTION_WORDS.contains(w))
        .map(|w| w.chars().take(5).collect())
        .collect()
}

/// The numbers of two digits or more in a text.
fn numbers(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|n| n.len() >= 2)
        .map(str::to_string)
        .collect()
}

/// Step 17: a model fills a vague prompt with a plausible sentence; a name that shares nothing with the prompt is that sentence.
/// Skipped when the prompt has nothing to share.
fn grounded(name: &str, prompt: &str) -> bool {
    if unspaced_majority(name) {
        if non_space_chars(prompt) < 2 {
            return true;
        }
        let chars: Vec<char> = name.chars().collect();
        return chars.windows(2).any(|w| {
            !w[0].is_whitespace()
                && !w[1].is_whitespace()
                && prompt.contains(&w.iter().collect::<String>())
        });
    }
    let (ps, pn) = (stems(prompt), numbers(prompt));
    if ps.is_empty() && pn.is_empty() {
        return true;
    }
    stems(name).iter().any(|s| ps.contains(s)) || numbers(name).iter().any(|n| pn.contains(n))
}

/// The cased letters of `s` are all capitals and there are more than four of them.
fn is_all_caps(s: &str) -> bool {
    let (mut upper, mut lower) = (0usize, 0usize);
    for c in s.chars() {
        if c.is_uppercase() {
            upper += 1;
        } else if c.is_lowercase() {
            lower += 1;
        }
    }
    lower == 0 && upper > 4
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Step 18.
fn sentence_case(name: String) -> String {
    if is_all_caps(&name) {
        capitalise(&name.to_lowercase())
    } else {
        name
    }
}

/// Step 19 (and the stored-name check): every character is a letter, a digit, a blank, one of `- _ . + # / & ' ( ) ,`, or a combining
/// mark; no more than three marks in a row.
fn charset_ok(s: &str) -> bool {
    let mut marks = 0;
    for c in s.chars() {
        if is_mark(c) {
            marks += 1;
            if marks > COMBINING_MAX {
                return false;
            }
            continue;
        }
        marks = 0;
        if !(c.is_alphabetic() || c.is_numeric() || c == ' ' || ALLOWED_PUNCT.contains(c)) {
            return false;
        }
    }
    true
}

/// The raw-text shape checks that a name must pass wherever it is read: no control or invisible character, no credential shape or long
/// word, one line, no markup, within 64 columns and 256 bytes, and only the allowed characters.
fn stored_shape(name: &str) -> Result<(), Reason> {
    if name.trim().is_empty() {
        return Err(Reason::NoName);
    }
    utility::guard_raw(name)?;
    if name.contains('\n') {
        return Err(Reason::Multiline);
    }
    if has_markup(name) {
        return Err(Reason::MarkupOrLink);
    }
    if display_width(name) > MAX_NAME_COLUMNS || name.len() > MAX_NAME_BYTES {
        return Err(Reason::TooWide);
    }
    if !charset_ok(name) {
        return Err(Reason::Charset);
    }
    Ok(())
}

/// The length rule of a stored name (a name that needed the squeeze is not stored as it was said): 5 to 7 words, or 8 to 20 characters
/// of an unspaced script, and fewer only for a short prompt.
fn length_rule(name: &str, prompt: &str) -> Result<(), Reason> {
    if unspaced_majority(name) {
        let n = non_space_chars(name);
        return if n > UNSPACED_MAX {
            Err(Reason::TooLong)
        } else if n < UNSPACED_MIN && non_space_chars(prompt) >= SHORT_PROMPT_UNSPACED {
            Err(Reason::TooShort)
        } else {
            Ok(())
        };
    }
    match word_count(name) {
        WORDS_MIN..=WORDS_MAX => Ok(()),
        n if n > WORDS_MAX => Err(Reason::TooLong),
        _ if word_count(prompt) < SHORT_PROMPT_WORDS => Ok(()),
        _ => Err(Reason::TooShort),
    }
}

/// True when `name` is safe to show: the checks of a stored name that need no prompt. The list builder runs this on every `runName` it
/// reads, so a hand-edited item, or a name made by an older judge, cannot reach a terminal; a name that fails shows as the seed.
pub fn accept_stored(name: &str) -> bool {
    stored_shape(name).is_ok()
}

/// [`accept_stored`] and the length rule for the prompt the name belongs to.
pub fn check_stored(name: &str, prompt: &str) -> Result<(), Reason> {
    stored_shape(name)?;
    length_rule(name, prompt)
}

/// Decides whether the utility model's reply may become the name of a run. `redacted_prompt` is [`redacted_prompt`] of the run's prompt.
/// The steps, in order, with the reason code of the first that fails:
///
/// 1. only a reply that ended on the stop sequence counts (`stop_<reason>`), and it must have text (`no_text`);
/// 2. a raw reply over 400 characters (`raw_too_long`);
/// 3. to 5. on the whole raw reply, before anything is extracted: an escape sequence or control character (`control_chars`), an
///    invisible character (`invisible_chars`), a credential shape or a word over 40 characters (`credential_shape`, `long_token`);
/// 6. exactly one `<title>` tag, or one clean line without it (`untagged`);
/// 7. `NONE` or nothing (`none`); 8. a second line (`multiline`); 9. markup, a variable, a mention or a link (`markup_or_link`);
/// 10. and 11. decoration and emoji are cleaned;
/// 12. meta text (`meta`); 13. an outcome word the prompt does not hold (`outcome_word`); 14. the instructions read back
///     (`echoes_instructions`);
/// 15. and 16. width, then length (`too_wide`, `too_long`, `too_short`), with the squeeze for 8 to 10 words;
/// 17. a name that shares nothing with the prompt (`ungrounded`); 18. all capitals become sentence case; 19. the characters (`charset`).
///
/// Last, the name that will be stored is read the way [`check_stored`] reads it: its shape, then the length rule of its final script
/// (a squeeze can turn a spaced name into an unspaced one), so every `Ok` name passes [`check_stored`].
///
/// Steps 3 to 5 and 9 reject and never clean: a reply with an escape byte, a link or a token shape is broken or steered, and cleaning it
/// would store a label chosen by the input.
pub fn judge(reply: &UtilityReply, redacted_prompt: &str) -> Result<Judged, Reason> {
    if reply.stop_reason != "stop_sequence" {
        return Err(Reason::Stop(StopKind::parse(&reply.stop_reason)));
    }
    let raw = reply.text.as_str();
    if raw.trim().is_empty() {
        return Err(Reason::NoText);
    }
    raw_checks(raw)?;
    let body = title_of_reply(raw)?.trim();
    if body.is_empty() || body.eq_ignore_ascii_case("none") {
        return Err(Reason::NoName);
    }
    if body.contains('\n') {
        return Err(Reason::Multiline);
    }
    if has_markup(body) {
        return Err(Reason::MarkupOrLink);
    }
    let name = clean(body);
    if name.is_empty() || name.eq_ignore_ascii_case("none") {
        return Err(Reason::NoName);
    }
    let low = name.to_lowercase();
    if META.iter().any(|m| low.contains(m)) {
        return Err(Reason::Meta);
    }
    outcome_word(&name, redacted_prompt)?;
    if echoes_instructions(&name) {
        return Err(Reason::EchoesInstructions);
    }
    let (name, squeezed) = if unspaced_majority(&name) {
        fit_unspaced(&name, redacted_prompt)?
    } else {
        fit_spaced(&name, redacted_prompt)?
    };
    if !grounded(&name, redacted_prompt) {
        return Err(Reason::Ungrounded);
    }
    let name = sentence_case(name);
    stored_shape(&name)?;
    // The squeeze and the cut can change the script of a mixed name, so the length rule is read again from the name as it will be
    // stored: `Ok(name)` means `check_stored(name, prompt)` passes.
    length_rule(&name, redacted_prompt)?;
    Ok(Judged { name, squeezed })
}

// ---------------------------------------------------------------------------------------------------------------------------------
// Seed: the name with no model
// ---------------------------------------------------------------------------------------------------------------------------------

/// One line of the prompt head that could name the run.
struct Candidate {
    text: String,
    /// A heading, or a line that ends in a colon.
    label: bool,
    /// Code, a stack frame or a data literal.
    code: bool,
    words: usize,
    unspaced: bool,
}

/// `#`, `-`, `*`, `+`, `>`, `1.`, `1)` and `[ ]` before the text of a line, each followed by a blank.
fn strip_list_markers(line: &str) -> &str {
    let mut s = line.trim_start();
    loop {
        let before = s;
        let t = s.trim_start_matches('#');
        if t.len() != s.len() && t.starts_with(char::is_whitespace) {
            s = t.trim_start();
        }
        for m in ['-', '*', '+', '>'] {
            if let Some(r) = s.strip_prefix(m)
                && r.starts_with(char::is_whitespace)
            {
                s = r.trim_start();
            }
        }
        let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if (1..=3).contains(&digits)
            && let Some(r) = s[digits..].strip_prefix(['.', ')'])
            && r.starts_with(char::is_whitespace)
        {
            s = r.trim_start();
        }
        for b in ["[ ]", "[x]", "[X]"] {
            if let Some(r) = s.strip_prefix(b)
                && (r.is_empty() || r.starts_with(char::is_whitespace))
            {
                s = r.trim_start();
            }
        }
        if s == before {
            return s;
        }
    }
}

/// A link or an address ends the clause it stands in: what was said after it is not part of the name.
fn is_link_or_address(tok: &str) -> bool {
    let low = tok.to_ascii_lowercase();
    if low.contains("://") || low.starts_with("www.") {
        return true;
    }
    tok.find('@').is_some_and(|i| i > 0 && i + 1 < tok.len())
}

/// The words of the line up to the first link or address; a line that starts with one continues after it.
fn first_clause(line: &str) -> String {
    let mut clause: Vec<&str> = Vec::new();
    for tok in line.split_whitespace() {
        if is_link_or_address(tok) {
            if clause.is_empty() {
                continue;
            }
            break;
        }
        clause.push(tok);
    }
    clause.join(" ")
}

/// A token shaped like a dotted path of identifiers (`com.shop.cart.CartService.total(...)`), three parts or more.
fn dotted_path(tok: &str) -> bool {
    let head = tok.split('(').next().unwrap_or("");
    let parts: Vec<&str> = head
        .trim_matches(|c: char| !(c.is_alphanumeric() || c == '_'))
        .split('.')
        .collect();
    parts.len() >= 3
        && parts.iter().all(|p| {
            p.chars()
                .next()
                .is_some_and(|c| c.is_alphabetic() || c == '_')
                && p.chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
        })
}

fn prose_ratio(line: &str) -> f64 {
    let (mut all, mut prose) = (0usize, 0usize);
    for c in line.chars().filter(|c| !c.is_whitespace()) {
        all += 1;
        if c.is_alphabetic() || ".,'-?!".contains(c) {
            prose += 1;
        }
    }
    if all == 0 {
        0.0
    } else {
        prose as f64 / all as f64
    }
}

/// Code, a stack frame, a data literal or a dotted path. A line that starts with a keyword counts only when it also has code symbols,
/// so "import the data from S3" and "class names should be unique" are prose.
fn looks_like_code(line: &str) -> bool {
    let t = line.trim_start();
    let first = t.split_whitespace().next().unwrap_or("");
    let symbols = t.chars().any(|c| "{}();=<>[]".contains(c));
    if t.starts_with(['{', '}', '"']) {
        return true;
    }
    let keyword = matches!(
        first,
        "def"
            | "class"
            | "fn"
            | "func"
            | "function"
            | "public"
            | "private"
            | "protected"
            | "static"
            | "const"
            | "let"
            | "var"
    );
    if keyword && (symbols || t.ends_with(':')) {
        return true;
    }
    if first == "import" && word_count(t) <= 3 {
        return true;
    }
    if first == "from" && t.contains(" import ") {
        return true;
    }
    if first == "at"
        && t.split_whitespace()
            .nth(1)
            .is_some_and(|n| n.contains('(') || dotted_path(n))
    {
        return true;
    }
    if t.split_whitespace().any(dotted_path) {
        return true;
    }
    symbols && prose_ratio(t) < 0.85
}

/// The lines of the head that could name the run, fenced code dropped.
fn candidates(text: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let heading = trimmed.starts_with('#')
            && trimmed
                .trim_start_matches('#')
                .starts_with(char::is_whitespace);
        // Code is judged before the emphasis marks are stripped: a `*` is a symbol of code (`t += i.price * i.qty`) as well as emphasis.
        let clause = first_clause(strip_list_markers(raw));
        let code = looks_like_code(&clause);
        let line = clause
            .replace("**", "")
            .replace("__", "")
            .replace(['`', '*'], "");
        if !line.chars().any(char::is_alphanumeric) {
            continue;
        }
        out.push(Candidate {
            label: heading || line.ends_with(':'),
            code,
            words: line.split_whitespace().count(),
            unspaced: unspaced_majority(&line),
            text: line,
        });
    }
    out
}

/// The offset after `phrase` at the start of `text`: ASCII, any case, one or more blanks between its words, and a word boundary after it.
fn eat_phrase(text: &str, phrase: &str) -> Option<usize> {
    let mut pos = 0;
    for (i, word) in phrase.split(' ').enumerate() {
        if i > 0 {
            let rest = &text[pos..];
            let blanks = rest.len() - rest.trim_start().len();
            if blanks == 0 {
                return None;
            }
            pos += blanks;
        }
        let n = word.len();
        let rest = &text[pos..];
        if rest.len() < n || !rest.is_char_boundary(n) || !rest[..n].eq_ignore_ascii_case(word) {
            return None;
        }
        pos += n;
    }
    match text[pos..].chars().next() {
        Some(c) if c.is_alphanumeric() || c == '_' || c == '\'' => None,
        _ => Some(pos),
    }
}

const ASK_MARKERS: [&str; 8] = [
    "what i want",
    "i want",
    "please",
    "your task",
    "task",
    "goal",
    "request",
    "need",
];

const FILLER: [&str; 14] = [
    "please",
    "pls",
    "can you",
    "could you",
    "would you",
    "i want you to",
    "i need you to",
    "i would like you to",
    "help me",
    "let us",
    "we need to",
    "your task is to",
    "thanks",
    "thank",
];

/// The end of a polite opening at the start of `t`, if there is one.
fn filler_end(t: &str) -> Option<usize> {
    if let Some(e) = FILLER.iter().find_map(|p| eat_phrase(t, p)) {
        return Some(e);
    }
    for greeting in ["hey", "hi", "hello"] {
        if let Some(e) = eat_phrase(t, greeting) {
            let extra: usize = t[e..]
                .chars()
                .take_while(|c| "!,.".contains(*c))
                .map(char::len_utf8)
                .sum();
            return Some(e + extra);
        }
    }
    if let Some(e) = eat_phrase(t, "hope you")
        && let Some(i) = t[e..].find(['.', '!', '?'])
    {
        return Some(e + i + 1);
    }
    eat_phrase(t, "what i want from you").or_else(|| eat_phrase(t, "what i want"))
}

/// Removes polite openings ("Hey! Hope you are well. Could you please help me ...") one after the other.
fn strip_filler(text: &str) -> String {
    let mut t = text.trim().to_string();
    while let Some(end) = filler_end(&t) {
        let rest = t[end..]
            .trim_matches(|c: char| c.is_whitespace() || " ,.!;:".contains(c))
            .to_string();
        if rest.len() == t.len() {
            break;
        }
        t = rest;
    }
    t
}

/// The first words of a sentence: up to seven, or up to the end of a sentence once five are taken; a word that cannot be part of a name
/// is skipped; trailing function words go while more than one word remains.
fn take_words(text: &str) -> Vec<&str> {
    let mut words = Vec::new();
    for w in text.split_whitespace() {
        if w.chars().count() > MAX_WORD_CHARS || !w.chars().any(char::is_alphanumeric) {
            continue;
        }
        words.push(w);
        if words.len() >= WORDS_MIN && w.ends_with(['.', '?', '!', ';', ':']) {
            break;
        }
        if words.len() == WORDS_MAX {
            break;
        }
    }
    while words.len() > 1 && words.last().is_some_and(|w| is_function_word(w)) {
        words.pop();
    }
    words
}

/// Keeps what a name may hold. Invisible characters and combining marks past three in a row are deleted; everything else that a name may
/// not hold becomes a blank.
fn charset_filter(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut marks = 0;
    for c in s.chars() {
        if c.is_whitespace() {
            out.push(' ');
            marks = 0;
            continue;
        }
        // Template braces (`/orders/{id}`) go; the other characters a name may not hold become blanks.
        if marker(c).is_some() || c.is_control() || c == '{' || c == '}' {
            continue;
        }
        if is_mark(c) {
            marks += 1;
            if marks <= COMBINING_MAX {
                out.push(c);
            }
            continue;
        }
        marks = 0;
        if c.is_alphabetic() || c.is_numeric() || ALLOWED_PUNCT.contains(c) {
            out.push(c);
        } else {
            out.push(' ');
        }
    }
    out
}

/// One pass of the limits of a seed: at most 7 words of a spaced script or 20 characters of an unspaced one (the rule is chosen from the
/// script of `text`, or forced to the unspaced rule by `unspaced`), sentence case for all capitals, a capital first letter, and 64 columns.
fn limit_seed(text: &str, unspaced: bool) -> String {
    let mut seed = if unspaced || unspaced_majority(text) {
        cut_non_space(text, UNSPACED_MAX)
    } else {
        let mut words: Vec<&str> = text
            .split(' ')
            .filter(|w| !w.is_empty() && w.chars().count() <= MAX_WORD_CHARS)
            .take(WORDS_MAX)
            .collect();
        while words.len() > 1 && words.last().is_some_and(|w| is_function_word(w)) {
            words.pop();
        }
        words.join(" ")
    };
    if is_all_caps(&seed) {
        seed = seed.to_lowercase();
    }
    let mut seed = capitalise(&seed);
    while display_width(&seed) > MAX_NAME_COLUMNS {
        match seed.rsplit_once(' ') {
            Some((head, _)) => seed = head.to_string(),
            None => {
                seed.pop();
            }
        }
    }
    seed
}

/// The most passes of [`limit_seed`] in [`finish_seed`]. Each pass either changes nothing or shortens the text, and a text is stable
/// after three or four.
const SEED_PASSES: usize = 6;

/// Cleans, limits and checks a seed. The limits run again on their own result until it stops changing: the 64-column cut drops
/// trailing words and can turn a mostly Latin line into a mostly Japanese one, and the cut at 20 characters can do the opposite, so the
/// seed obeys the count rule of the script it ends in. Anything that survives the cleaning and still fails a check of a stored name is
/// not shown: the constant [`UNTITLED`] stands in.
fn finish_seed(text: &str, unspaced: bool) -> String {
    let mut seed = collapse(&charset_filter(&clean(text)));
    let mut unspaced = unspaced;
    for _ in 0..SEED_PASSES {
        let next = limit_seed(&seed, unspaced);
        unspaced = false;
        if next == seed {
            break;
        }
        seed = next;
    }
    if seed.is_empty() || stored_shape(&seed).is_err() {
        return UNTITLED.to_string();
    }
    seed
}

/// The name of a run with no model: built from the stored prompt head (the first 2,000 characters of the prompt, so the client that has
/// just started a run and the server that reads the stored head feed it the same bytes). It picks the line that asks for something (an
/// ask marker such as "what I want", else the first line of prose), drops the polite opening, takes up to seven words and cleans them.
/// Code, fenced blocks, headings and links are never the name. The result passes every check of a stored name that needs no prompt, and
/// is [`UNTITLED`] when nothing is left. It is never stored.
pub fn seed_name(head: &str) -> String {
    let head: String = head.chars().take(HEAD_CHARS).collect();
    let text = redacted_prompt(&head)
        .replace("[redacted private key]", " ")
        .replace(REDACTED, " ");
    let cands = candidates(&text);
    let pick = cands
        .iter()
        .find(|c| !c.code && ASK_MARKERS.iter().any(|m| eat_phrase(&c.text, m).is_some()))
        .or_else(|| {
            cands
                .iter()
                .find(|c| !c.code && !c.label && (c.words >= 3 || c.unspaced))
        })
        .or_else(|| cands.iter().find(|c| !c.code))
        .or_else(|| cands.first());
    let Some(pick) = pick else {
        return UNTITLED.to_string();
    };
    if pick.unspaced {
        return finish_seed(&pick.text, true);
    }
    let sentence = strip_filler(&pick.text);
    finish_seed(&take_words(&sentence).join(" "), false)
}
