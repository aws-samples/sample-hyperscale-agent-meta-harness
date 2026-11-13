//! The tag call: the prompt, the Converse request, the reply parser and the judge.
//!
//! The utility model reads the person's request (as data, already redacted, cut and escaped by the caller) and the person's recent topic
//! and tech tags, and answers with a JSON array of tags inside `<tags>`. Its reply is untrusted text: nothing in it is stored until the
//! judge has checked it tag by tag, and the judge keeps a tag only if it passes the model grammar ([`check_stored`] with
//! [`Origin::Model`]). A failed tag is dropped with a reason code and never fails the run. The functions are pure: the call itself and the
//! writes belong to the callers. The checks on the whole raw text of a live reply (size, control and invisible characters, credential
//! shapes) are [`read_reply`], which [`judge_reply`] runs before [`judge`]; the word rule of the name guard is not among them (see
//! [`read_reply`]).

use super::MAX_AUTO;
use super::grammar::{Origin, Reason, check_stored, cosmetic, key_of, kind_of, raw_checks};
use super::vocab::Vocabulary;
use crate::utility::{GuardReason, guard_chars};
use serde_json::{Value, json};
use std::collections::HashSet;

/// The version of the prompt, written to the search item with the tags.
pub const PROMPT_VERSION: &str = "rt-1";
/// The most tokens the model may write; a reply is a few dozen.
pub const MAX_TOKENS: u32 = 400;
/// The stop sequence: the model's closing tag ends the call.
pub const STOP: &str = "</tags>";
/// The most strings a reply may hold; more than this and the whole reply is refused.
pub const MAX_REPLY_TAGS: usize = 12;
/// The most automatic topic tags and tech tags a run keeps from one reply.
pub const MAX_TOPICS: usize = 3;
pub const MAX_TECHS: usize = 3;
/// The last line of the user turn.
pub const ASK: &str = "Tag the run above.";
/// The most bytes a reply may have. A reply of the 400 tokens the call allows is a few kilobytes at most; a longer one is refused before
/// anything is read, so the work on a reply is bounded whatever the service sends.
pub const MAX_REPLY_BYTES: usize = 16 * 1024;

const SYSTEM: &str = include_str!("../../assets/run-tags-system.txt");

/// The system text of prompt `rt-1`, byte for byte as measured (the asset file without its one trailing newline).
pub fn system_text() -> &'static str {
    SYSTEM.strip_suffix('\n').unwrap_or(SYSTEM)
}

/// The user turn: the vocabulary, then the request, then the ask. `escaped_excerpt` is the run-names excerpt with `&`, `<` and `>`
/// escaped; one that still holds `<` or `>` could close the request element or forge an answer, so it is refused.
pub fn user_text(vocab: &Vocabulary, escaped_excerpt: &str) -> Result<String, Reason> {
    if escaped_excerpt.contains(['<', '>']) {
        return Err(Reason::UnescapedExcerpt);
    }
    Ok(format!(
        "<vocabulary>\n{}\n</vocabulary>\n\n<request>\n{}\n</request>\n\n{ASK}",
        vocab.block(),
        escaped_excerpt
    ))
}

/// The Converse request body. `extra_fields` is the catalog row's `additionalModelRequestFields` (thinking off); `Null` leaves the
/// member out. No sampling field, no assistant turn, no tool: the last message is the user's.
pub fn request(
    extra_fields: &Value,
    vocab: &Vocabulary,
    escaped_excerpt: &str,
) -> Result<Value, Reason> {
    let user = user_text(vocab, escaped_excerpt)?;
    let mut body = json!({
        "system": [{"text": system_text()}],
        "messages": [{"role": "user", "content": [{"text": user}]}],
        "inferenceConfig": {"maxTokens": MAX_TOKENS, "stopSequences": [STOP]},
    });
    if !extra_fields.is_null() {
        body["additionalModelRequestFields"] = extra_fields.clone();
    }
    Ok(body)
}

/// The strings of the last `<tags>` element of a reply: text before it is skipped, a closing tag (the stop sequence normally cuts it)
/// ends the element, an empty element is an empty list. More than [`MAX_REPLY_TAGS`] strings, a non-string element, text that is not
/// a JSON array, or no element at all, refuses the whole reply.
pub fn parse_reply(text: &str) -> Result<Vec<String>, Reason> {
    if text.trim().is_empty() {
        return Err(Reason::NoText);
    }
    let open = "<tags>";
    let Some(at) = text.rfind(open) else {
        return Err(Reason::BadJson);
    };
    let body = &text[at + open.len()..];
    let body = body.split(STOP).next().unwrap_or("").trim();
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<Value> = serde_json::from_str(body).map_err(|_| Reason::BadJson)?;
    if items.len() > MAX_REPLY_TAGS {
        return Err(Reason::TooMany);
    }
    items
        .into_iter()
        .map(|item| match item {
            Value::String(s) => Ok(s),
            _ => Err(Reason::BadJson),
        })
        .collect()
}

fn guard_reason(g: GuardReason) -> Reason {
    match g {
        GuardReason::ControlChars => Reason::ControlChars,
        GuardReason::InvisibleChars => Reason::InvisibleChars,
        GuardReason::CredentialShape => Reason::CredentialShape,
        // `guard_chars` has no word rule; the arm keeps the match total.
        GuardReason::LongToken => Reason::RawShape,
    }
}

/// The strings a reply proposes, or the reason the whole reply is refused. In this order: the stop reason (only `stop_sequence` ends a good
/// reply), the size ([`MAX_REPLY_BYTES`]), the checks on the whole raw text ([`guard_chars`]: control and bidirectional characters, invisible
/// characters, a credential shape; the reply is refused and never cleaned), then [`parse_reply`].
///
/// The word rule of the name guard (`utility::guard_raw`) is left out on purpose. The model writes the list either as `["a", "b"]` or
/// compact as `["a","b"]`, and the compact list is one blank-separated word of 80 to 110 characters: the rule refused 16 of 152 valid
/// replies of the live smoke run. What the rule guards against, a blob in a tag, is caught per item (raw length at most 64, grammar, word
/// shape), and text outside the list is never read.
pub fn read_reply(stop_reason_text: &str, text: &str) -> Result<Vec<String>, Reason> {
    if stop_reason_text != "stop_sequence" {
        return Err(stop_reason(stop_reason_text));
    }
    if text.len() > MAX_REPLY_BYTES {
        return Err(Reason::RawShape);
    }
    guard_chars(text).map_err(guard_reason)?;
    parse_reply(text)
}

/// What the judge made of a reply: the tags to store, in the model's order, and a reason for every tag or reply it refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Judged {
    pub kept: Vec<String>,
    pub rejects: Vec<Reason>,
}

fn stop_reason(stop: &str) -> Reason {
    match stop {
        "max_tokens" => Reason::StopMaxTokens,
        "refusal" => Reason::StopRefusal,
        "content_filtered" | "guardrail_intervened" => Reason::StopContentFiltered,
        "end_turn" => Reason::StopEndTurn,
        _ => Reason::StopOther,
    }
}

/// Judges one reply. `stop_reason` is the Converse stop reason as text and must be `stop_sequence`; `text` is the concatenated text of
/// the reply; `vocab` is the vocabulary the call used (its key map is used to snap spellings); `off` holds the tags the person removed,
/// the run's and the parent's together. This is the per-tag judge: it checks each tag on its own and drops the ones that fail, whatever the
/// rest of the reply holds. The checks on the whole raw text come first, in [`judge_reply`], which is what a caller of a live reply uses.
///
/// Per tag, in order: the raw string checks, the cosmetic fixes, the model grammar (including the rule that the model writes no
/// project and the policy words), the off list by key, the snap to the vocabulary spelling of the same key (kept only if the snapped
/// spelling passes the model grammar too), then duplicates by key and the quotas.
pub fn judge(stop: &str, text: &str, vocab: &Vocabulary, off: &[String]) -> Judged {
    let mut out = Judged::default();
    if stop != "stop_sequence" {
        out.rejects.push(stop_reason(stop));
        return out;
    }
    let proposed = match parse_reply(text) {
        Ok(tags) => tags,
        Err(reason) => {
            out.rejects.push(reason);
            return out;
        }
    };
    let off_keys: HashSet<String> = off.iter().map(|t| key_of(t)).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let (mut topics, mut techs) = (0, 0);
    for raw in proposed {
        if let Err(reason) = raw_checks(&raw) {
            out.rejects.push(reason);
            continue;
        }
        let mut tag = cosmetic(&raw);
        if let Err(reason) = check_stored(&tag, Origin::Model) {
            out.rejects.push(reason);
            continue;
        }
        let key = key_of(&tag);
        if off_keys.contains(&key) {
            out.rejects.push(Reason::RemovedByUser);
            continue;
        }
        if let Some(spelling) = vocab.keys.get(&key)
            && *spelling != tag
            && check_stored(spelling, Origin::Model).is_ok()
        {
            tag = spelling.clone();
        }
        if !seen.insert(key) {
            continue;
        }
        let topic = kind_of(&tag) == Some("topic");
        if out.kept.len() >= MAX_AUTO
            || (topic && topics >= MAX_TOPICS)
            || (!topic && techs >= MAX_TECHS)
        {
            out.rejects.push(Reason::OverQuota);
            continue;
        }
        if topic {
            topics += 1;
        } else {
            techs += 1;
        }
        out.kept.push(tag);
    }
    out
}

/// Judges the reply of a live call: the checks on the whole raw text first ([`read_reply`]: the stop reason, the size, control and
/// invisible characters and credential shapes anywhere in the text), and only then [`judge`] tag by tag. A reply that fails the first part
/// keeps nothing and gives its reason alone, and nothing of it is cleaned. A caller that holds the text of a Converse answer calls this
/// function and not [`judge`] alone, so no reply is read before its whole text has been checked.
pub fn judge_reply(stop: &str, text: &str, vocab: &Vocabulary, off: &[String]) -> Judged {
    match read_reply(stop, text) {
        Ok(_) => judge(stop, text, vocab, off),
        Err(reason) => Judged {
            kept: Vec::new(),
            rejects: vec![reason],
        },
    }
}

/// The automatic list to store: the model's kept tags first, then the parent's automatic tags that are not already there (by key), not
/// on the off list and still valid as automatic tags, up to [`MAX_AUTO`] in all.
pub fn compose_auto(model: &[String], parent_auto: &[String], off: &[String]) -> Vec<String> {
    let off_keys: HashSet<String> = off.iter().map(|t| key_of(t)).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let from_model = model.iter().map(|t| (t, false));
    let from_parent = parent_auto.iter().map(|t| (t, true));
    for (tag, check) in from_model.chain(from_parent) {
        if out.len() >= MAX_AUTO {
            break;
        }
        if check && check_stored(tag, Origin::Model).is_err() {
            continue;
        }
        let key = key_of(tag);
        if off_keys.contains(&key) || !seen.insert(key) {
            continue;
        }
        out.push(tag.clone());
    }
    out
}
