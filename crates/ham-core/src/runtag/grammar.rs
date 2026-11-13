//! The tag grammar and policy: what a stored tag may be, what a person may type, which words no tag may hold, the comparison key that
//! joins two spellings of one idea, and the project tag that a repository gives.
//!
//! A tag is `kind:value` with the kind `project`, `topic` or `tech`. It is display and search text (root invariant 1): never an
//! identifier, never an outcome label. Every function here is pure; the checks are small scanners over the characters, with no
//! regular-expression crate and no Unicode tables.
//!
//! Three origins decide how strict the value rules are. A [`Origin::Model`] tag (the utility model's, and the automatic tags a follow-up
//! inherits) has 1 to 3 words of at most 32 characters, and a word with a digit in it is at most 6 characters long, which keeps
//! a lower-cased key id, an account number or a phone number out. A [`Origin::User`] tag (typed by the person) and a [`Origin::Derived`] tag (the repository's) have 1 to 6
//! words of at most 40 characters and may hold a repository's own mixed word.

use crate::redact::redact_tokens;
use crate::sanitize::{marker, sanitize};
use std::collections::BTreeSet;
use std::fmt;

/// The kinds a tag may have. The model writes `topic` and `tech` only.
pub const KINDS: [&str; 3] = ["project", "topic", "tech"];

/// The most bytes a raw tag string may have before any other check (the longest valid tag is 48).
pub const RAW_MAX: usize = 64;

/// The most characters of one word of a value.
pub const WORD_MAX: usize = 16;

/// The words the harness never uses about a run (root invariant 1); no tag holds one as any hyphen-separated part of its value.
/// The protocol's own list, so there is one place that names them.
pub const OUTCOME_TOKENS: [&str; 4] = harness_protocol::direct::BANNED_CLAIM_WORDS;

/// Status words: a run's status comes from the run itself, so none of these is a tag value (the whole value, any origin).
pub const STATUS_WORDS: [&str; 22] = [
    "done",
    "complete",
    "completed",
    "finished",
    "failed",
    "failure",
    "running",
    "active",
    "blocked",
    "pending",
    "wip",
    "todo",
    "in-progress",
    "merged",
    "passed",
    "passing",
    "success",
    "successful",
    "needs-input",
    "needs-you",
    "cancelled",
    "aborted",
];

/// Generic words that say nothing about a run. The model may not write one as the whole value, and a follow-up does not inherit one; a
/// person may type any of them.
pub const GENERIC_WORDS: [&str; 40] = [
    "code",
    "coding",
    "software",
    "programming",
    "development",
    "dev",
    "project",
    "projects",
    "task",
    "tasks",
    "work",
    "feature",
    "features",
    "bug",
    "bugs",
    "fix",
    "fixes",
    "app",
    "application",
    "script",
    "scripts",
    "file",
    "files",
    "general",
    "misc",
    "other",
    "update",
    "change",
    "changes",
    "improvement",
    "enhancement",
    "implementation",
    "tool",
    "tools",
    "test",
    "tests",
    "testing",
    "refactor",
    "refactoring",
    "bugfix",
];

/// Words dropped from the comparison key.
pub const STOP_WORDS: [&str; 8] = ["and", "of", "the", "for", "to", "in", "a", "with"];

/// Vendor names dropped from the front of a `tech` value in the comparison key (`aws-s3` and `s3` are one tag).
pub const VENDOR_PREFIXES: [&str; 5] = ["aws", "amazon", "google", "gcp", "azure"];

/// The suffixes the comparison key folds, longest first; one is stripped only when 3 or more letters remain.
pub const SUFFIXES: [&str; 8] = ["ings", "ing", "ers", "er", "ed", "es", "s", "e"];

/// Who wrote a tag. The origin decides the value rules and whether the generic words and the `project` kind are open to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// The utility model, or an automatic tag a follow-up inherited.
    Model,
    /// The person, through `/tag` or the explorer.
    User,
    /// The code: the project tag a repository gives.
    Derived,
}

/// Why a tag, a reply or an edit was refused. [`Reason::as_str`] is the code a log line carries; [`Reason::message`] is the rule in plain
/// words, for the line a person reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Reason {
    RawShape,
    ControlChars,
    InvisibleChars,
    CredentialShape,
    UrlShape,
    Grammar,
    UnknownKind,
    EmptyValue,
    NonAscii,
    BadChar,
    NotLowercase,
    HyphenShape,
    ValueTooLong,
    TagTooLong,
    TooManyWords,
    WordShape,
    ModelProject,
    OutcomeToken,
    StatusWord,
    Generic,
    RemovedByUser,
    OverQuota,
    UnescapedExcerpt,
    NoText,
    BadJson,
    TooMany,
    StopMaxTokens,
    StopRefusal,
    StopContentFiltered,
    StopEndTurn,
    StopOther,
    AddAndRemove,
    UserCap,
    OffCap,
    NoTag,
}

impl Reason {
    /// The code of a log line (design 7.4 and the grammar rules): lower-case words joined by underscores.
    pub fn as_str(&self) -> &'static str {
        match self {
            Reason::RawShape => "raw_shape",
            Reason::ControlChars => "control_chars",
            Reason::InvisibleChars => "invisible_chars",
            Reason::CredentialShape => "credential_shape",
            Reason::UrlShape => "url_shape",
            Reason::Grammar => "grammar",
            Reason::UnknownKind => "unknown_kind",
            Reason::EmptyValue => "empty_value",
            Reason::NonAscii => "non_ascii",
            Reason::BadChar => "bad_char",
            Reason::NotLowercase => "not_lowercase",
            Reason::HyphenShape => "hyphen_shape",
            Reason::ValueTooLong => "value_too_long",
            Reason::TagTooLong => "tag_too_long",
            Reason::TooManyWords => "too_many_words",
            Reason::WordShape => "word_shape",
            Reason::ModelProject => "model_project",
            Reason::OutcomeToken => "outcome_token",
            Reason::StatusWord => "status_word",
            Reason::Generic => "generic",
            Reason::RemovedByUser => "removed_by_user",
            Reason::OverQuota => "over_quota",
            Reason::UnescapedExcerpt => "unescaped_excerpt",
            Reason::NoText => "no_text",
            Reason::BadJson => "bad_json",
            Reason::TooMany => "too_many",
            Reason::StopMaxTokens => "stop_max_tokens",
            Reason::StopRefusal => "stop_refusal",
            Reason::StopContentFiltered => "stop_content_filtered",
            Reason::StopEndTurn => "stop_end_turn",
            Reason::StopOther => "stop_other",
            Reason::AddAndRemove => "add_and_remove",
            Reason::UserCap => "user_cap",
            Reason::OffCap => "off_cap",
            Reason::NoTag => "no_tag",
        }
    }

    /// The rule in plain words: what a person sees when a typed tag is refused.
    pub fn message(&self) -> &'static str {
        match self {
            Reason::RawShape => "a tag is kind:value text of 1 to 64 characters",
            Reason::ControlChars => "a tag holds no control or escape characters",
            Reason::InvisibleChars => "a tag holds no invisible or direction-changing characters",
            Reason::CredentialShape => {
                "a tag holds nothing that looks like a key, token or password"
            }
            Reason::UrlShape => "a tag holds no web address",
            Reason::Grammar => "a tag is kind:value, with the kind project, topic or tech",
            Reason::UnknownKind => "the kind of a tag is project, topic or tech",
            Reason::EmptyValue => "a tag needs a value after the kind",
            Reason::NonAscii => {
                "a tag uses English letters, digits and hyphens only; write it in English"
            }
            Reason::BadChar => "a tag value uses lowercase letters, digits and single hyphens only",
            Reason::NotLowercase => "a tag value is lowercase",
            Reason::HyphenShape => {
                "hyphens join words: a tag value has no leading, trailing or doubled hyphen"
            }
            Reason::ValueTooLong => {
                "a tag value is too long (an automatic tag 32 characters, a typed tag 40)"
            }
            Reason::TagTooLong => {
                "a tag is too long (an automatic tag 40 characters, a typed tag 48)"
            }
            Reason::TooManyWords => {
                "a tag value has too many words (an automatic tag 3, a typed tag 6)"
            }
            Reason::WordShape => {
                "a word of a tag is at most 16 characters, and an automatic word with a digit in it at most 6"
            }
            Reason::ModelProject => {
                "the project of a run is its repository or a tag you type, never an automatic tag"
            }
            Reason::OutcomeToken => {
                "a tag cannot hold the words verified, tested, gated or evaluated"
            }
            Reason::StatusWord => "a status word such as done or blocked is not a tag",
            Reason::Generic => "a generic word such as testing or app is not an automatic tag",
            Reason::RemovedByUser => "you removed this tag from the run",
            Reason::OverQuota => "at most 3 topic tags, 3 tech tags and 6 tags in all",
            Reason::UnescapedExcerpt => {
                "the request text must be escaped before it enters the prompt"
            }
            Reason::NoText => "the model gave no text",
            Reason::BadJson => "the reply is not a JSON array of strings inside tags",
            Reason::TooMany => "too many tags (at most 12 in a reply or in one edit)",
            Reason::StopMaxTokens => "the model reply was cut at the token limit",
            Reason::StopRefusal => "the model refused",
            Reason::StopContentFiltered => "the reply was filtered",
            Reason::StopEndTurn => "the model ended its turn before the closing tag",
            Reason::StopOther => "the model stopped for another reason",
            Reason::AddAndRemove => "one edit cannot add and remove the same tag",
            Reason::UserCap => "a run holds at most 12 tags that you added",
            Reason::OffCap => "a run holds at most 32 tags that you removed",
            Reason::NoTag => "a + or - needs a tag after it",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for Reason {}

/// The value limits of one origin.
struct Limits {
    words: usize,
    value: usize,
    tag: usize,
    /// The most characters of a word that has a digit in it; `None` when the rule does not apply.
    digit_word: Option<usize>,
}

fn limits(origin: Origin) -> Limits {
    match origin {
        Origin::Model => Limits {
            words: 3,
            value: 32,
            tag: 40,
            digit_word: Some(6),
        },
        Origin::User | Origin::Derived => Limits {
            words: 6,
            value: 40,
            tag: 48,
            digit_word: None,
        },
    }
}

/// The checks every origin passes on the raw string, before any fix: a size limit, no invisible or direction-changing character, nothing
/// the sanitizer would remove, nothing token-shaped (judged before lower-casing, which would hide a key shape), no `://`.
pub fn raw_checks(raw: &str) -> Result<(), Reason> {
    if raw.is_empty() || raw.len() > RAW_MAX {
        return Err(Reason::RawShape);
    }
    if raw.chars().any(|c| marker(c).is_some()) {
        return Err(Reason::InvisibleChars);
    }
    if sanitize(raw) != raw {
        return Err(Reason::ControlChars);
    }
    if redact_tokens(raw) != raw {
        return Err(Reason::CredentialShape);
    }
    if raw.contains("://") {
        return Err(Reason::UrlShape);
    }
    Ok(())
}

/// The cosmetic fixes and nothing else: trim, lower-case (ASCII only), a space or underscore in the value becomes a hyphen, runs of
/// hyphens collapse to one. A string that needed none comes back unchanged. The kind is trimmed and lower-cased as well.
pub fn cosmetic(raw: &str) -> String {
    let t = raw.trim();
    match t.split_once(':') {
        Some((kind, value)) => format!(
            "{}:{}",
            kind.trim().to_ascii_lowercase(),
            cosmetic_value(value)
        ),
        None => cosmetic_value(t),
    }
}

fn cosmetic_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut hyphen = false;
    for c in value.trim().chars() {
        let c = if c == ' ' || c == '_' {
            '-'
        } else {
            c.to_ascii_lowercase()
        };
        if c == '-' {
            if !hyphen {
                out.push('-');
            }
            hyphen = true;
        } else {
            hyphen = false;
            out.push(c);
        }
    }
    out
}

/// A word with a digit in it looks like an identifier (a key id, an account number, a phone number) when it is long.
fn has_digit(word: &str) -> bool {
    word.bytes().any(|b| b.is_ascii_digit())
}

/// The grammar of an already-cleaned tag: kind, alphabet, hyphens, lengths, words, word shapes.
fn check_grammar(tag: &str, origin: Origin) -> Result<(), Reason> {
    let Some((kind, value)) = tag.split_once(':') else {
        return Err(Reason::Grammar);
    };
    if !KINDS.contains(&kind) {
        return Err(Reason::UnknownKind);
    }
    if value.is_empty() {
        return Err(Reason::EmptyValue);
    }
    for c in value.chars() {
        if !c.is_ascii() {
            return Err(Reason::NonAscii);
        }
        if c.is_ascii_uppercase() {
            return Err(Reason::NotLowercase);
        }
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(Reason::BadChar);
        }
    }
    if value.starts_with('-') || value.ends_with('-') || value.contains("--") {
        return Err(Reason::HyphenShape);
    }
    let lim = limits(origin);
    if value.len() > lim.value {
        return Err(Reason::ValueTooLong);
    }
    let words: Vec<&str> = value.split('-').collect();
    if words.len() > lim.words {
        return Err(Reason::TooManyWords);
    }
    for w in words {
        if w.len() > WORD_MAX {
            return Err(Reason::WordShape);
        }
        if let Some(max) = lim.digit_word
            && w.len() > max
            && has_digit(w)
        {
            return Err(Reason::WordShape);
        }
    }
    if tag.len() > lim.tag {
        return Err(Reason::TagTooLong);
    }
    Ok(())
}

/// The policy words: an outcome word as any hyphen-separated part of the value (every origin, no exception), a status word as the
/// whole value (every origin), a generic word as the whole value (model tags only). A string without a kind has none of them.
pub fn forbidden(tag: &str, origin: Origin) -> Option<Reason> {
    let (_, value) = tag.split_once(':')?;
    let value = value.to_ascii_lowercase();
    if value.split('-').any(|part| OUTCOME_TOKENS.contains(&part)) {
        return Some(Reason::OutcomeToken);
    }
    if STATUS_WORDS.contains(&value.as_str()) {
        return Some(Reason::StatusWord);
    }
    if origin == Origin::Model && GENERIC_WORDS.contains(&value.as_str()) {
        return Some(Reason::Generic);
    }
    None
}

/// The full check of a tag as it is stored or read back: raw checks, grammar, the model's kinds, then the policy words. A tag that passes
/// here is plain ASCII in the grammar and safe to draw, search and send to a model as data.
pub fn check_stored(tag: &str, origin: Origin) -> Result<(), Reason> {
    raw_checks(tag)?;
    check_grammar(tag, origin)?;
    if origin == Origin::Model && tag.starts_with("project:") {
        return Err(Reason::ModelProject);
    }
    match forbidden(tag, origin) {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// A tag the person typed, made a stored tag: the raw checks first (so a key shape is seen before the lower-casing), the cosmetic
/// fixes, a bare word becomes `topic:<word>`, leading and trailing hyphens of the value go, and the result passes [`check_stored`]
/// as a [`Origin::User`] tag. Non-ASCII text is refused with the rule in words.
pub fn normalize_typed(raw: &str) -> Result<String, Reason> {
    let typed = raw.trim();
    raw_checks(typed)?;
    if !typed.is_ascii() {
        return Err(Reason::NonAscii);
    }
    let fixed = cosmetic(typed);
    let tag = match fixed.split_once(':') {
        Some((kind, value)) => format!("{kind}:{}", value.trim_matches('-')),
        None => format!("topic:{}", fixed.trim_matches('-')),
    };
    check_stored(&tag, Origin::User)?;
    Ok(tag)
}

/// The token as the comparison key folds it: one suffix of [`SUFFIXES`] stripped when 3 or more characters remain.
pub fn fold_token(token: &str) -> String {
    for suffix in SUFFIXES {
        if let Some(stem) = token.strip_suffix(suffix)
            && stem.chars().count() >= 3
        {
            return stem.to_string();
        }
    }
    token.to_string()
}

/// The comparison key of a tag: equal keys are one tag. It is the kind, then the sorted set of the value's tokens with the stop words
/// removed, a vendor prefix removed from the front of a `tech` value, and each token folded by a small suffix stripper. Total: any
/// string has a key, and junk has a junk key that matches no valid tag. A value whose tokens are all stop words or vendor names keeps
/// them, so `tech:aws` and `tech:azure` stay two tags.
pub fn key_of(tag: &str) -> String {
    let lower = tag.to_ascii_lowercase();
    let (kind, value) = match lower.split_once(':') {
        Some((kind, value)) => (kind.trim().to_string(), value),
        None => (String::new(), lower.as_str()),
    };
    let all: Vec<&str> = value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let mut tokens: Vec<&str> = all
        .iter()
        .copied()
        .filter(|t| !STOP_WORDS.contains(t))
        .collect();
    if tokens.is_empty() {
        tokens = all;
    }
    if kind == "tech" && tokens.len() > 1 && VENDOR_PREFIXES.contains(&tokens[0]) {
        tokens.remove(0);
    }
    let folded: BTreeSet<String> = tokens.into_iter().map(fold_token).collect();
    format!(
        "{kind}:{}",
        folded.into_iter().collect::<Vec<_>>().join("-")
    )
}

/// The kind of a tag, when it has a known one.
pub fn kind_of(tag: &str) -> Option<&str> {
    let (kind, _) = tag.split_once(':')?;
    KINDS.contains(&kind).then_some(kind)
}

/// The project tag that a repository gives, from the run's target text as the run list shows it: `cc:<name>`, `new cc:<name>`,
/// `gh:<owner>/<name>`, `gl:<group>/<project>` or `s3-only`. The name is cut to its last path segment, made lower-case ASCII words
/// (any other character is a separator, a word is cut at 16 characters), and kept while it fits 6 words and 40 characters; it is cut at
/// a word boundary, so it never ends with a hyphen. `None` when the run has no repository, when the name holds no word, or when the
/// tag would break the policy (an outcome word, a token shape). Computed on every read and stored nowhere.
pub fn project_tag_of(target_text: &str) -> Option<String> {
    let text = target_text.trim();
    let text = text.strip_prefix("new ").unwrap_or(text).trim();
    if text.is_empty() || text == "s3-only" {
        return None;
    }
    let name = text.split_once(':').map_or(text, |(_, rest)| rest);
    let name = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
    let lim = limits(Origin::Derived);
    let mut value = String::new();
    let words = name
        .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
        .filter(|w| !w.is_empty());
    for (count, word) in words.enumerate() {
        let word = &word[..word.len().min(WORD_MAX)];
        let extra = if value.is_empty() {
            word.len()
        } else {
            word.len() + 1
        };
        if count == lim.words || value.len() + extra > lim.value {
            break;
        }
        if !value.is_empty() {
            value.push('-');
        }
        value.push_str(word);
    }
    if value.is_empty() {
        return None;
    }
    let tag = format!("project:{value}");
    check_stored(&tag, Origin::Derived).ok()?;
    Some(tag)
}
