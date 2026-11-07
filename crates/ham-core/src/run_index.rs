//! The run index: a small search item per run, kept outside the run-list index (GSI1), and the rows and filter built on it.
//!
//! `RunSummary` lives in `harness-protocol`, so a new member there would move the worker image key. The list route instead returns a
//! [`RunRow`]: the summary's members, flattened, plus a title, the model, the last activity, the outcome word and the run's name.
//! The JSON is a superset of `ListRunsResponse`, so an old client parses it. The extras come from [`RunIndexItem`], one item per
//! run in the harness table:
//!
//! - key: `PK = OWNER#<sha256 of the caller's principal ARN>`, `SK = SRCH#<createdAt>#<runId>` ([`owner_pk`], [`search_sk`]);
//! - it has no `ownerPrincipalArn` attribute, so GSI1 (sparse on that attribute) never lists it;
//! - four writers each own their fields and write with `if_not_exists`, a monotonic time or a condition, so the order they run in does
//!   not matter: `control-api` at the start (the prompt head, model and repository) and in the backfill, `run-session` at the end of a
//!   run (the outcome word and the last activity), and `run-session`'s `name-run` once, after the run exists (`runName`,
//!   `runNameSource`, `runNameModel`, written only when `runName` is absent);
//! - the tag attributes belong to the tag writers, the tag branch of `name-run` and the tag edit route of `control-api`: the
//!   start, the end of the run and the backfill never name them. They are the automatic, typed and removed tag lists (`runTags`,
//!   `runTagsUser`, `runTagsOff`) and the bookkeeping (`runTagsAt`, `runTagsModel`, `runTagsVer`, `runTagsInh`, `runTagsRev`).
//!
//! The name a row shows is the stored name when it passes [`runname::accept_stored`], else the seed built from the stored prompt head
//! ([`runname::seed_name`]); it is display text only and the seed is never stored. The tags a row shows are
//! [`effective`] of the three lists and the repository, computed on every read and stored nowhere: the person's
//! removal wins, and a stored tag that fails the grammar is not shown.
//!
//! `control-api`, `run-session` and `ham` share the key, the item, the title rule and the filter.

use crate::runname;
use crate::runtag::effective::effective;
use harness_protocol::api::v1::RunSummary;
use harness_protocol::common::{is_utc_timestamp, parse_utc_unix};
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The characters of the prompt the item keeps (the full prompt is on the run's goal item and in `request.json`).
pub const PROMPT_HEAD_CHARS: usize = 2000;
/// The characters of a row's title.
pub const TITLE_CHARS: usize = 80;
/// The longest `q`, after its spaces are collapsed.
pub const MAX_QUERY_CHARS: usize = 200;
/// The most words in `q`.
pub const MAX_QUERY_WORDS: usize = 8;
/// A filtered list stops after the first page of 100 runs that brings it to this many matches.
pub const FILTER_FILL: usize = 25;
/// A filtered list reads at most this many pages of 100 runs in one call; past it the answer is `partial` with a token.
pub const FILTER_MAX_PAGES: usize = 10;
/// One backfill call writes at most this many items.
pub const BACKFILL_MAX_WRITES: usize = 40;
/// One backfill call reads at most this many pages of 100 runs.
pub const BACKFILL_MAX_PAGES: usize = 5;
/// The sort-key prefix of the search items.
pub const SEARCH_PREFIX: &str = "SRCH#";
/// The `nameSource` of a name the utility model wrote and the judge accepted (stored by `name-run`).
pub const NAME_SOURCE_AI: &str = "ai";
/// The `nameSource` of a name built from the prompt head with no model (never stored; computed when the list is read).
pub const NAME_SOURCE_SEED: &str = "seed";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `OWNER#<sha256 hex of the principal ARN>`: the partition of one caller's search items. Derived on the server from the authenticated
/// caller and never taken from a request.
pub fn owner_pk(owner_arn: &str) -> String {
    format!("OWNER#{}", hex(&Sha256::digest(owner_arn.as_bytes())))
}

/// `SRCH#<createdAt>#<runId>`: sorts by creation time, so one run list page is one contiguous range of the partition.
pub fn search_sk(created_at: &str, run_id: &str) -> String {
    format!("{SEARCH_PREFIX}{created_at}#{run_id}")
}

/// The first [`PROMPT_HEAD_CHARS`] characters of `prompt` and whether it was longer.
pub fn prompt_head(prompt: &str) -> (String, bool) {
    let mut chars = prompt.chars();
    let head: String = chars.by_ref().take(PROMPT_HEAD_CHARS).collect();
    (head, chars.next().is_some())
}

/// The first non-blank line of the prompt head, control characters out, runs of white space made one space, at most [`TITLE_CHARS`]
/// characters (cut with an ellipsis). Empty when the head has no text.
pub fn title_of(head: &str) -> String {
    let line = head
        .lines()
        .map(|l| {
            l.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect::<String>()
        })
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.chars().count() <= TITLE_CHARS {
        return line;
    }
    let mut cut: String = line.chars().take(TITLE_CHARS - 1).collect();
    cut.push('\u{2026}');
    cut
}

/// How the list shows a run's target: the repository, `new <repository>`, or `s3-only`.
pub fn target_text(t: &RunTarget) -> String {
    match t {
        RunTarget::Repo { repo } => repo.to_string(),
        RunTarget::NewRepo { repo } => format!("new {repo}"),
        RunTarget::S3Only => "s3-only".to_string(),
    }
}

/// Reads a tag attribute without letting a wrong type fail the whole item: a value of another type than the field's (a number where a
/// list belongs, a list holding a number, a number past `u32`) is `None`. The value is read whole before its type is checked, so a
/// wrong one leaves the reader at the next attribute. Before this, one bad attribute made the item unreadable and the row lost its
/// title, model and name with it. The caller that knows the raw item counts what was dropped (`control-api`'s window read).
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned,
{
    Ok(Value::deserialize(d)
        .ok()
        .and_then(|v| serde_json::from_value::<Option<T>>(v).ok())
        .flatten())
}

/// [`lenient`] for the list of removed tags, with one difference: it keeps what it can read. The person's removal wins on every read, so
/// a removal list that is only partly readable must not forget the removals it holds, or a tag the person removed would be shown again.
/// The text entries of a list are kept, and a lone text value is a list of one. Only a value that is not a list of text at all (a
/// number, a map) is `None`, and `control-api` counts that apart. The typed and automatic lists stay all or nothing: losing an entry
/// there only hides a tag.
fn lenient_off<'de, D>(d: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match Value::deserialize(d) {
        Ok(Value::Array(items)) => Some(
            items
                .into_iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
        ),
        Ok(Value::String(s)) => Some(vec![s]),
        _ => None,
    })
}

/// The search item of one run, without its keys. The start fills the prompt head, the model and the repository; the end of the run adds
/// the outcome word and moves the last activity. An item the end wrote first has an empty prompt head until the backfill completes it.
/// `Default` is for tests that build a partial item; no production path writes a default item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunIndexItem {
    pub run_id: String,
    /// The run's own `createdAt`, byte for byte: it is half of the sort key.
    pub created_at: String,
    #[serde(default)]
    pub prompt_head: String,
    #[serde(default)]
    pub prompt_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// [`target_text`] of the run's target.
    #[serde(default)]
    pub repository: String,
    /// The harness label of how Cline stopped (`Cline reported done`), set when the run ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default)]
    pub last_activity: String,
    /// The checked name of the run, written once by run-session's `name-run`: at most 64 columns and 256 bytes, passes
    /// [`runname::check_stored`]. Absent until a model name is accepted. The attribute is `runName` (`name` is a DynamoDB reserved word).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_name: Option<String>,
    /// Where `run_name` came from: `ai` for a model name. `user` is reserved for a rename feature that nothing writes yet; a reader
    /// treats any other value as "not ai".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_name_source: Option<String>,
    /// The route id that answered (`us.anthropic.claude-haiku-5-5`): provenance for the operator, never a wire member of the list row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_name_model: Option<String>,
    /// The automatic tags: the utility model's, in its order, then the parent run's; at most 6. Written once by the tag branch of
    /// `name-run`, under the condition that `runTagsAt` is absent. Absent when none. A tag is display and search text.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub run_tags: Option<Vec<String>>,
    /// The tags the person added, in order; at most 12. Written by the tag edit route, and copied from the parent run by a follow-up.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub run_tags_user: Option<Vec<String>>,
    /// The tags the person removed from this run; at most 32. The person's removal wins on every read, whoever wrote the tag.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_off"
    )]
    pub run_tags_off: Option<Vec<String>>,
    /// When the automatic step finished (RFC 3339 UTC): the model answered, even with an empty list, or inherited tags were written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub run_tags_at: Option<String>,
    /// The route id that answered. Provenance for the operator, never a wire member of the list row.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub run_tags_model: Option<String>,
    /// The version of the tag prompt that answered (`rt-1`). Never a wire member of the list row.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub run_tags_ver: Option<String>,
    /// The id of the parent run whose typed and removed lists a follow-up copied. Never a wire member of the list row.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub run_tags_inh: Option<String>,
    /// The compare-and-set base of the tag writers; the first write sets 1. Never a wire member of the list row.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub run_tags_rev: Option<u32>,
}

impl RunIndexItem {
    /// The item `control-api` writes when a run starts: `last_activity` is the creation time.
    pub fn at_start(
        run_id: &str,
        created_at: &str,
        prompt: &str,
        model: Option<&str>,
        target: &RunTarget,
    ) -> Self {
        let (head, truncated) = prompt_head(prompt);
        Self {
            run_id: run_id.to_string(),
            created_at: created_at.to_string(),
            prompt_head: head,
            prompt_truncated: truncated,
            model: model.map(str::to_string),
            repository: target_text(target),
            outcome: None,
            last_activity: created_at.to_string(),
            run_name: None,
            run_name_source: None,
            run_name_model: None,
            run_tags: None,
            run_tags_user: None,
            run_tags_off: None,
            run_tags_at: None,
            run_tags_model: None,
            run_tags_ver: None,
            run_tags_inh: None,
            run_tags_rev: None,
        }
    }

    /// The repository text that gives the run's derived project tag: the item's own, else the run's target as the list shows it. The item
    /// of a run that `run-session` wrote first has no repository yet. They are the same string wherever both exist, since the start writes
    /// [`target_text`] of the target. Every reader and writer of tags uses this one rule, so a removal recorded against the derived project
    /// always matches the tag it hides.
    pub fn tag_repository(&self, target: &RunTarget) -> String {
        if self.repository.is_empty() {
            target_text(target)
        } else {
            self.repository.clone()
        }
    }

    /// The item lacks what the start writes (the end of the run created it first). A real prompt is never empty.
    pub fn is_partial(&self) -> bool {
        self.prompt_head.is_empty()
    }
}

/// One row of the run list: the `RunSummary`, flattened, and what the index adds. Every extra is left out when unknown, so a row of a run
/// without an index item serializes as the bare summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRow {
    #[serde(flatten)]
    pub summary: RunSummary,
    /// The first line of the prompt ([`title_of`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The newest of the index item's last activity and, for an active run, its newest event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity: Option<String>,
    /// The harness label of how Cline stopped, once the run has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    /// The run's name: the stored name when it passes [`runname::accept_stored`], else the seed built from the prompt head.
    /// Absent for a run with no item, and for an item with neither a name nor a prompt head. Display text only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `ai` for a stored model name, `seed` for a name built from the prompt. Any other stored value (`user`) is carried as it is; a
    /// stored value that is not a plain lowercase word is left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_source: Option<String>,
    /// The run's effective tags: [`effective`] of the item's three lists and the repository. Present, possibly empty, on every
    /// row that has an item; absent on a row without one and from a backend that predates tags. Display and search text only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// The prompt head the server matched `q` against. Never sent.
    #[serde(skip)]
    search_text: String,
}

impl PartialEq for RunRow {
    /// Rows are equal when their wire members are: the search text is not one.
    fn eq(&self, other: &Self) -> bool {
        self.summary == other.summary
            && self.title == other.title
            && self.model == other.model
            && self.last_activity == other.last_activity
            && self.outcome == other.outcome
            && self.name == other.name
            && self.name_source == other.name_source
            && self.tags == other.tags
    }
}

/// A stored `runNameSource` the row may carry: one to sixteen characters, a lowercase letter first, then lowercase letters, digits and
/// hyphens. Anything else (a hand-edited item) is not carried, so a row never forwards arbitrary text in this member.
fn is_plain_source(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= 16
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The name and its source that the row of `item` shows. A stored name is checked again on every read ([`runname::accept_stored`]), so a
/// hand-edited item or a name made by an older judge cannot reach a terminal; a name that fails shows as the seed. The seed is built
/// from the stored prompt head, the same bytes the client has when it starts a run, and is `None` when the head is empty.
fn shown_name(item: &RunIndexItem) -> (Option<String>, Option<String>) {
    if let Some(stored) = item.run_name.as_deref()
        && runname::accept_stored(stored)
    {
        let source = item
            .run_name_source
            .as_deref()
            .filter(|s| is_plain_source(s))
            .map(str::to_string);
        return (Some(stored.to_string()), source);
    }
    if item.prompt_head.is_empty() {
        return (None, None);
    }
    (
        Some(runname::seed_name(&item.prompt_head)),
        Some(NAME_SOURCE_SEED.to_string()),
    )
}

impl RunRow {
    pub fn new(summary: RunSummary, item: Option<&RunIndexItem>) -> Self {
        let mut row = Self {
            summary,
            title: None,
            model: None,
            last_activity: None,
            outcome: None,
            name: None,
            name_source: None,
            tags: None,
            search_text: String::new(),
        };
        if let Some(i) = item {
            row.title = Some(title_of(&i.prompt_head)).filter(|t| !t.is_empty());
            row.model.clone_from(&i.model);
            row.last_activity = Some(i.last_activity.clone()).filter(|t| !t.is_empty());
            row.outcome.clone_from(&i.outcome);
            row.search_text.clone_from(&i.prompt_head);
            (row.name, row.name_source) = shown_name(i);
            row.tags = Some(
                effective(
                    &i.tag_repository(&row.summary.target),
                    i.run_tags.as_deref().unwrap_or_default(),
                    i.run_tags_user.as_deref().unwrap_or_default(),
                    i.run_tags_off.as_deref().unwrap_or_default(),
                )
                .tags,
            );
        }
        row
    }

    /// A row from an item that exists but has not been completed (or none at all) shows no title: the client may offer the backfill.
    pub fn is_indexed(&self) -> bool {
        self.title.is_some()
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// `GET /v1/runs` answer: rows, the token of the next page, and whether the scan stopped for time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRowsPage {
    pub runs: Vec<RunRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
    /// A filtered scan stopped at its time or page limit; `next_token` continues it. Zero rows with a token is a normal answer.
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    /// How many of these rows are runs that predate the index (no title): `POST /v1/runs/index` makes their items.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unindexed: u32,
}

/// Why a filter is refused; the text names the parameter.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct FilterError(pub String);

/// The server-side filter of the run list. All four members narrow; none is required.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunFilter {
    /// Words, all of which must appear (any case) in the run id, the prompt head, the name, the model, the repository, the outcome, the status
    /// or one of the run's tags.
    pub q: Option<String>,
    /// A case-insensitive piece of the target text.
    pub target: Option<String>,
    pub status: Option<GoalStatus>,
    /// An RFC 3339 UTC time: only runs created at or after it.
    pub since: Option<String>,
}

fn has_control(s: &str) -> bool {
    s.chars().any(char::is_control)
}

fn status_words() -> String {
    [
        GoalStatus::Active,
        GoalStatus::Paused,
        GoalStatus::Blocked,
        GoalStatus::UsageLimited,
        GoalStatus::BudgetLimited,
        GoalStatus::Complete,
        GoalStatus::Cleared,
    ]
    .iter()
    .map(|s| status_word(*s))
    .collect::<Vec<_>>()
    .join(", ")
}

/// The wire word of a status (`usage_limited`).
pub fn status_word(s: GoalStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl RunFilter {
    pub fn is_empty(&self) -> bool {
        self.q.is_none() && self.target.is_none() && self.status.is_none() && self.since.is_none()
    }

    /// The filter in the query parameters of `GET /v1/runs`. A blank value is no filter; unknown parameters (`nextToken`) are ignored.
    pub fn from_params(params: &BTreeMap<String, String>) -> Result<Self, FilterError> {
        let trimmed = |name: &str| params.get(name).map(|v| v.trim()).filter(|v| !v.is_empty());
        let q = match trimmed("q") {
            None => None,
            Some(raw) => {
                if has_control(raw) {
                    return Err(FilterError("q must be plain text".into()));
                }
                let words: Vec<&str> = raw.split_whitespace().collect();
                let collapsed = words.join(" ");
                if collapsed.chars().count() > MAX_QUERY_CHARS {
                    return Err(FilterError(format!(
                        "q is longer than {MAX_QUERY_CHARS} characters"
                    )));
                }
                if words.len() > MAX_QUERY_WORDS {
                    return Err(FilterError(format!(
                        "q has {} words; at most {MAX_QUERY_WORDS} words are searched",
                        words.len()
                    )));
                }
                Some(collapsed)
            }
        };
        let target = match trimmed("target") {
            None => None,
            Some(t) if has_control(t) || t.chars().count() > MAX_QUERY_CHARS => {
                return Err(FilterError(format!(
                    "target must be plain text of at most {MAX_QUERY_CHARS} characters"
                )));
            }
            Some(t) => Some(t.to_string()),
        };
        let status = match trimmed("status") {
            None => None,
            Some(s) => Some(
                serde_json::from_value::<GoalStatus>(serde_json::Value::String(s.to_string()))
                    .map_err(|_| {
                        FilterError(format!("status must be one of {}", status_words()))
                    })?,
            ),
        };
        let since = match trimmed("since") {
            None => None,
            Some(s) if is_utc_timestamp(s) => Some(s.to_string()),
            Some(_) => {
                return Err(FilterError(
                    "since must be a UTC time such as 2030-01-01T00:00:00Z".into(),
                ));
            }
        };
        Ok(Self {
            q,
            target,
            status,
            since,
        })
    }

    /// The query string of the filter (no leading `?`, no token), percent-encoded; empty for no filter.
    pub fn to_query(&self) -> String {
        let mut parts = Vec::new();
        if let Some(q) = &self.q {
            parts.push(format!("q={}", percent_encode(q)));
        }
        if let Some(t) = &self.target {
            parts.push(format!("target={}", percent_encode(t)));
        }
        if let Some(s) = self.status {
            parts.push(format!("status={}", status_word(s)));
        }
        if let Some(s) = &self.since {
            parts.push(format!("since={}", percent_encode(s)));
        }
        parts.join("&")
    }

    /// Whether the row passes every member of the filter.
    pub fn matches(&self, row: &RunRow) -> bool {
        if let Some(s) = self.status
            && row.summary.status != s
        {
            return false;
        }
        if let Some(t) = &self.target
            && !target_text(&row.summary.target)
                .to_lowercase()
                .contains(&t.to_lowercase())
        {
            return false;
        }
        if let Some(since) = &self.since {
            let created = parse_utc_unix(&row.summary.created_at);
            match (created, parse_utc_unix(since)) {
                (Some(c), Some(s)) if c >= s => {}
                _ => return false,
            }
        }
        if let Some(q) = &self.q {
            // The tags that the row shows, not the stored lists: a tag the person removed finds nothing.
            let tags = row.tags.as_deref().map(|t| t.join(" ")).unwrap_or_default();
            let hay = [
                row.summary.run_id.as_str(),
                row.title.as_deref().unwrap_or_default(),
                row.search_text.as_str(),
                row.name.as_deref().unwrap_or_default(),
                row.model.as_deref().unwrap_or_default(),
                row.outcome.as_deref().unwrap_or_default(),
                &target_text(&row.summary.target),
                &status_word(row.summary.status),
                tags.as_str(),
            ]
            .join("\n")
            .to_lowercase();
            if !q.to_lowercase().split_whitespace().all(|w| hay.contains(w)) {
                return false;
            }
        }
        true
    }
}

/// What one `POST /v1/runs/index` call did. The client calls again while `more` is true, with `next_token` when there is one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackfillReport {
    /// Items made or completed.
    pub indexed: u32,
    /// Runs whose item was complete, among the runs this call read.
    pub already_indexed: u32,
    /// Runs the backfill cannot describe (no goal item, or a goal with no text); they are left alone.
    pub skipped: u32,
    /// Runs this call read. A call that stopped inside a page is followed by one that reads the page again, so the sum of these over
    /// calls counts some runs twice.
    pub scanned: u32,
    /// The call stopped before the end of the runs.
    pub more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_token: Option<String>,
}

impl BackfillReport {
    /// Folds in a later call. A call that stops inside a page is followed by one that reads the page again from its start, so the three
    /// read counts are per call and only `indexed` adds up: the read counts, the token and `more` are the later call's.
    pub fn absorb(&mut self, later: &BackfillReport) {
        self.indexed += later.indexed;
        self.already_indexed = later.already_indexed;
        self.skipped = later.skipped;
        self.scanned = later.scanned;
        self.more = later.more;
        self.next_token.clone_from(&later.next_token);
    }
}

/// Percent-encoding of a query value: everything but the unreserved characters, as SigV4 signs it.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The inverse of [`percent_encode`].
pub fn percent_decode(s: &str) -> Result<String, FilterError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| FilterError("a percent escape is not two hex digits".into()))?;
            out.push(h);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| FilterError("the text is not UTF-8".into()))
}
