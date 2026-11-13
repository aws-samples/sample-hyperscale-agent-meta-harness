//! The person's edits and what a follow-up inherits.
//!
//! A run holds three lists: `auto` (the utility model's), `user` (tags the person added) and `off` (tags the person removed). An edit
//! changes `user` and `off` only; `effective` hides every tag whose key is on `off`, so a removal wins whoever wrote the tag and no
//! automatic step can bring it back. An add is the only thing that clears an `off` entry. A list that would pass its cap makes the edit
//! invalid and nothing is dropped to make room.
//!
//! The derived project is stored nowhere: removing it writes `off`, adding it back only clears `off`.

use super::grammar::{Reason, key_of, normalize_typed, project_tag_of};
use super::wire::MAX_TAGS_PER_EDIT;
use super::{MAX_AUTO, MAX_OFF, MAX_USER};
use std::fmt;

/// The three tag lists of one run, as stored on its search item.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagState {
    pub auto: Vec<String>,
    pub user: Vec<String>,
    pub off: Vec<String>,
}

/// The words of a `/tag` line: tags to add and tags to remove, each already a normalised tag.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TagWords {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

/// Why an edit was refused: the rule, and the tag it is about when there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditError {
    pub reason: Reason,
    pub tag: Option<String>,
}

impl EditError {
    fn new(reason: Reason, tag: Option<&str>) -> Self {
        Self {
            reason,
            tag: tag.map(str::to_string),
        }
    }

    /// The rule in plain words, then the tag it is about in parentheses.
    pub fn message(&self) -> String {
        match &self.tag {
            Some(tag) => format!("{} ({tag})", self.reason.message()),
            None => self.reason.message().to_string(),
        }
    }
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for EditError {}

/// Reads the words of a `/tag` line: `+kind:value` or a bare word adds, `-kind:value` removes, a bare word without a kind is a topic.
/// Every word is normalised ([`normalize_typed`]); a sign with nothing after it, a word outside the grammar, or more than 12 words is
/// an error that names its rule. A line with no word is no edit.
pub fn parse_tag_words(line: &str) -> Result<TagWords, Reason> {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.len() > MAX_TAGS_PER_EDIT {
        return Err(Reason::TooMany);
    }
    let mut out = TagWords::default();
    for word in words {
        let (list, text) = match word.as_bytes()[0] {
            b'-' => (&mut out.remove, &word[1..]),
            b'+' => (&mut out.add, &word[1..]),
            _ => (&mut out.add, word),
        };
        if text.is_empty() {
            return Err(Reason::NoTag);
        }
        let tag = normalize_typed(text)?;
        if !list.contains(&tag) {
            list.push(tag);
        }
    }
    Ok(out)
}

/// Normalises the tags of one side of an edit; spellings of one key count once.
fn normalise_all(raw: &[String]) -> Result<Vec<String>, EditError> {
    let mut out: Vec<String> = Vec::new();
    for text in raw {
        let tag = normalize_typed(text).map_err(|reason| EditError::new(reason, Some(text)))?;
        if !out.iter().any(|t| key_of(t) == key_of(&tag)) {
            out.push(tag);
        }
    }
    Ok(out)
}

/// Applies one edit to a run's lists. `repository` is the run's target text (it names the derived project). The result is the new
/// state; when it equals the old one the edit changed nothing (an empty edit and a replay both end there). Any invalid tag, a tag that
/// is both added and removed, more than 12 tags, or a list past its cap refuses the whole edit and leaves the state as it was.
pub fn apply_edit(
    state: &TagState,
    repository: &str,
    add: &[String],
    remove: &[String],
) -> Result<TagState, EditError> {
    if add.len() + remove.len() > MAX_TAGS_PER_EDIT {
        return Err(EditError::new(Reason::TooMany, None));
    }
    let adds = normalise_all(add)?;
    let removes = normalise_all(remove)?;
    for tag in &adds {
        if removes.iter().any(|r| key_of(r) == key_of(tag)) {
            return Err(EditError::new(Reason::AddAndRemove, Some(tag)));
        }
    }
    let derived_key = project_tag_of(repository).map(|t| key_of(&t));
    let mut next = state.clone();
    for tag in &removes {
        let key = key_of(tag);
        next.user.retain(|t| key_of(t) != key);
        if !next.off.iter().any(|t| key_of(t) == key) {
            next.off.push(tag.clone());
        }
    }
    for tag in &adds {
        let key = key_of(tag);
        next.off.retain(|t| key_of(t) != key);
        if derived_key.as_deref() == Some(key.as_str()) {
            // The repository gives this tag on every read; a copy in the list would be a second source of the same fact.
            next.user.retain(|t| key_of(t) != key);
        } else if !next.user.iter().any(|t| key_of(t) == key) {
            next.user.push(tag.clone());
        }
    }
    if next.user.len() > MAX_USER && next.user.len() > state.user.len() {
        return Err(EditError::new(Reason::UserCap, None));
    }
    if next.off.len() > MAX_OFF && next.off.len() > state.off.len() {
        return Err(EditError::new(Reason::OffCap, None));
    }
    Ok(next)
}

/// What a follow-up takes from its parent: the person's tags and removals as they are, and the parent's automatic tags, which the tag
/// branch adds after the follow-up's own. Every list is cut to its cap.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inherited {
    pub user: Vec<String>,
    pub off: Vec<String>,
    pub auto: Vec<String>,
}

pub fn inherit(parent: &TagState) -> Inherited {
    let cut = |list: &[String], max: usize| list.iter().take(max).cloned().collect();
    Inherited {
        user: cut(&parent.user, MAX_USER),
        off: cut(&parent.off, MAX_OFF),
        auto: cut(&parent.auto, MAX_AUTO),
    }
}
