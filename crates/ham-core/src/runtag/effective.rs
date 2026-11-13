//! The effective tags of a run: the one function that says which tags a run shows.
//!
//! `derived project ++ the person's tags ++ the automatic tags`, each checked again as it is read, equal keys joined (the earlier
//! spelling stays), minus every tag whose key is on the off list. The person's removal wins on every read, whoever wrote the tag. The
//! derived project is computed here from the repository text and stored nowhere. The function is the same one the list row, the client
//! (for a row without a `tags` member), `ham runs --json`, the vocabulary counter and the explorer call.

use super::MAX_EFFECTIVE;
use super::grammar::{Origin, check_stored, key_of, project_tag_of};
use std::collections::HashSet;

/// The tags a run shows, in order, and how many stored entries were not shown because they failed the grammar or the policy, or
/// because the run already showed [`MAX_EFFECTIVE`] tags. An entry that repeats a shown tag or is on the off list is not counted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effective {
    pub tags: Vec<String>,
    pub dropped: usize,
}

/// `repository` is the run's target text as the run list shows it (`cc:calc`, `new cc:calc`, `s3-only`). `auto` is the automatic list,
/// `user` the person's added tags, `off` the tags the person removed from this run. Applying the function to its own output (as the
/// `user` list, with the same `off`) changes nothing.
pub fn effective(repository: &str, auto: &[String], user: &[String], off: &[String]) -> Effective {
    let off_keys: HashSet<String> = off.iter().map(|t| key_of(t)).collect();
    let derived = project_tag_of(repository);
    let candidates = derived
        .iter()
        .map(|t| (t, Origin::Derived))
        .chain(user.iter().map(|t| (t, Origin::User)))
        .chain(auto.iter().map(|t| (t, Origin::Model)));
    let mut tags: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut dropped = 0;
    for (tag, origin) in candidates {
        if check_stored(tag, origin).is_err() {
            dropped += 1;
            continue;
        }
        let key = key_of(tag);
        if off_keys.contains(&key) || seen.contains(&key) {
            continue;
        }
        if tags.len() >= MAX_EFFECTIVE {
            dropped += 1;
            continue;
        }
        seen.insert(key);
        tags.push(tag.clone());
    }
    Effective { tags, dropped }
}
