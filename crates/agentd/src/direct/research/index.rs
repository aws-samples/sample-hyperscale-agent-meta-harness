//! The index of what the run stored. One entry per placed item, derived from the route's manifest:
//! origin, retrieval date, licence and the SHA-256 of the manifest. `INDEX.json` is the machine copy; `INDEX.md` is the table whose
//! rows the run copies into `.ham/context/INDEX.md` (`ham-research index --md`), so the run never has to invent a hash. The columns are
//! the ones the lifecycle pack and its scorer read: Origin, Date read, sha256, Licence, Why it matters.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `schema` of `INDEX.json`.
pub const INDEX_SCHEMA: &str = "hamh-research-index/1";

/// One placed item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub item_id: String,
    /// `page` or `repo`.
    pub kind: String,
    pub origin: String,
    /// The pinned commit of a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// RFC 3339, as the route recorded it.
    pub retrieved_at: String,
    /// An SPDX id, or `NOASSERTION` when the route could not tell. Evidence, not clearance.
    pub licence: String,
    pub licence_source: String,
    /// Where the route got it (`tavily`, `github-archive`, ...).
    pub provider: String,
    /// SHA-256 of the manifest's bytes: the item's number in the index.
    pub manifest_sha256: String,
    /// SHA-256 of the body the manifest names (the page text, or the repository archive).
    pub body_sha256: String,
    /// Bytes placed: the page, or the unpacked files of a repository.
    pub bytes: u64,
    /// Files placed (1 for a page).
    pub files: u32,
    /// Absolute path of the page file or the repository directory.
    pub path: String,
    /// The same, relative to the context directory (`research/pages/i-0001.txt`).
    pub rel: String,
}

/// `INDEX.json` for these items (in item order), with a closing newline.
pub fn render_json(run: &str, items: &BTreeMap<String, Item>) -> String {
    let list: Vec<&Item> = items.values().collect();
    let mut text = serde_json::to_string(&serde_json::json!({
        "schema": INDEX_SCHEMA,
        "run": run,
        "items": list,
    }))
    .unwrap_or_default();
    text.push('\n');
    text
}

/// A table cell: one line, no pipe, no control character.
fn cell(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .replace('|', "%7C")
        .trim()
        .to_string()
}

/// The first ten characters of an RFC 3339 time (`YYYY-MM-DD`).
pub fn day_of(retrieved_at: &str) -> &str {
    retrieved_at.get(..10).unwrap_or(retrieved_at)
}

/// The origin as the index shows it: a repository carries its commit, like package@version.
fn origin_cell(item: &Item) -> String {
    match &item.commit {
        Some(c) if item.kind == "repo" => cell(&format!("{}@{c}", item.origin)),
        _ => cell(&item.origin),
    }
}

/// The table for `.ham/context/INDEX.md`: a header, a separator and one row per item, in item order.
pub fn render_md(items: &BTreeMap<String, Item>) -> String {
    let mut out = String::from(
        "| Origin | Date read | sha256 | Licence | Why it matters |\n| --- | --- | --- | --- | --- |\n",
    );
    for item in items.values() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | stored copy at {} (item {}) |\n",
            origin_cell(item),
            cell(day_of(&item.retrieved_at)),
            item.manifest_sha256,
            cell(&item.licence),
            cell(&item.rel),
            cell(&item.item_id),
        ));
    }
    out
}
