//! What `ham config set` and `ham config unset` do to the TEXT of `config.toml`: a pure function from the text of the
//! file, a key and a value to the new text, with no file, no environment and no clock. The registry decides what a key is and what a value may
//! be ([`crate::config::set_key`], [`crate::config::unset_key`]); the comment-keeping patcher ([`crate::config_edit`]) changes the bytes; this
//! module joins the two.
//!
//! - **Spelling.** The patcher knows no key names, so the key goes to it as the file spells it: a hand-written `default_model` is updated in
//!   place and no second spelling appears; a key that is not in the file is written in its canonical form (`defaultModel`, `run.goal` as the key
//!   `goal` of `[run]`).
//! - **No-op.** The value a key already has plans no text, so nothing is written and the modification time stays.
//! - **A file with a bad key is patched around.** A typo, a key in the wrong place, a wrong value: the edit is made on its own line, the rest is
//!   not touched, and [`Plan::still_wrong`] says what the new file still holds that [`crate::config::read_config`] finds wrong. A value the
//!   reader could not store (`mouse = "maybe"`) is replaced by `set` and removed by `unset`.
//! - **A file that is not TOML, or that a newer `ham` wrote, is never changed**: [`PlanError::Unreadable`], [`ConfigError::Newer`].
//! - **`configVersion`** is written with the first key beyond the nine of v4.02 and removed with the last one. A tables-only test of the text
//!   decides, so a table that holds a key the reader rejects still counts as a table: [`Plan::opens_the_door`] is true for the write that adds
//!   the first table, the one that older builds refuse.

use crate::config::{Config, ConfigError, MouseMode, get_key, read_config, set_key, unset_key};
use crate::config_edit::{self, Edit, EditValue, Outcome, Refusal};
use crate::settings::{CONFIG_VERSION, Finding, Key, ROWS, Row, Slot, TABLES};
use toml::de::{DeTable, DeValue};

/// What a `set` or `unset` would do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The name `ham config` prints for the key (`default_model`, `run.goal`).
    pub name: &'static str,
    /// The value after the change as `ham config get` prints it; `None` after an unset.
    pub stored: Option<String>,
    /// The new text of the file; `None` when nothing changes, and then nothing is written.
    pub text: Option<String>,
    /// True when the change adds the first table to a file that had none: from then on every `ham` before this one refuses the file.
    pub opens_the_door: bool,
    /// What the file still holds that is wrong after the change, one line each with its line number (the findings of the reader).
    pub still_wrong: Vec<String>,
}

/// Why no plan was made. Nothing was changed.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    /// An unknown key, a value the key refuses, or a file written by a newer `ham`.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The text is not usable as TOML (too large, a NUL byte, a byte-order mark, nested too deep, or a syntax error); the findings, one per line.
    #[error("{0}")]
    Unreadable(String),
    /// The patcher met a construct it does not edit, or could not show its result to be exactly the edit.
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// The order of a new top-level key: the legacy keys in registry order, then `configVersion`, which the patcher uses to place a new key
/// before the first existing key that follows it.
fn rank(path: &[String]) -> usize {
    let [name] = path else {
        return usize::MAX;
    };
    let legacy = ROWS.iter().filter(|r| r.file.0.is_empty()).count();
    if name == "configVersion" {
        return legacy;
    }
    ROWS.iter()
        .filter(|r| r.file.0.is_empty())
        .position(|r| r.file.1 == name || r.file_alias == Some(name.as_str()))
        .unwrap_or(usize::MAX)
}

fn parse(text: &str) -> Result<toml::Spanned<DeTable<'_>>, PlanError> {
    DeTable::parse(text).map_err(|_| PlanError::Refused(Refusal::NotProvable))
}

/// The top-level names the file spells.
fn top_names(doc: &DeTable<'_>) -> Vec<String> {
    doc.iter()
        .map(|(k, _)| {
            let name: &str = k.get_ref();
            name.to_string()
        })
        .collect()
}

/// The paths a key goes by in this file: for a key of a table, the table and the name; for a legacy key, every spelling the file uses, else
/// the canonical one.
fn paths_of(row: &Row, top: &[String]) -> Vec<Vec<String>> {
    match row.file {
        ("", canonical) => {
            let mut names: Vec<&str> = Vec::new();
            if top.iter().any(|n| n == canonical) {
                names.push(canonical);
            }
            if let Some(alias) = row.file_alias
                && top.iter().any(|n| n == alias)
            {
                names.push(alias);
            }
            if names.is_empty() {
                names.push(canonical);
            }
            names.into_iter().map(|n| vec![n.to_string()]).collect()
        }
        (table, name) => vec![vec![table.to_string(), name.to_string()]],
    }
}

/// The value a key holds in `cfg`, as the patcher writes it.
fn value_of(key: Key, cfg: &Config) -> Option<EditValue> {
    let mut copy = cfg.clone();
    match key.slot(&mut copy) {
        Slot::Text(s) => s.clone().map(EditValue::Str),
        Slot::Bool(s) => s.map(EditValue::Bool),
        Slot::Minutes(s) | Slot::Number(s) => s.map(|m| EditValue::Int(i64::from(m))),
        Slot::Mascot(s) => s.map(|m| EditValue::Str(m.word().to_string())),
        Slot::Flow(s) => s.map(|m| EditValue::Str(m.word().to_string())),
        Slot::Review(s) => s.map(|m| EditValue::Str(m.word().to_string())),
        Slot::Compute(s) => s.map(|m| EditValue::Str(m.word().to_string())),
        Slot::Mouse(s) => s.map(|m| match m {
            MouseMode::On => EditValue::Bool(true),
            MouseMode::Off => EditValue::Bool(false),
            MouseMode::Auto => EditValue::Str("auto".to_string()),
        }),
    }
}

/// True while one of the tables of the file holds a key (a key the reader rejects counts): the file then needs `configVersion` and older
/// builds refuse it.
fn tables_in_use(doc: &DeTable<'_>) -> bool {
    doc.iter().any(|(k, v)| {
        let name: &str = k.get_ref();
        TABLES.contains(&name)
            && matches!(v.get_ref(), DeValue::Table(entries) if entries.iter().next().is_some())
    })
}

/// True for an integer `configVersion` this build knows (1 up to [`CONFIG_VERSION`]).
fn is_known_version(v: &DeValue<'_>) -> bool {
    match v {
        DeValue::Integer(i) => i64::from_str_radix(i.as_str(), i.radix())
            .is_ok_and(|n| (1..=i64::from(CONFIG_VERSION)).contains(&n)),
        _ => false,
    }
}

/// The edit that makes `configVersion` right for the file in `doc`: written while a table is in use, removed when none is.
fn version_edit(doc: &DeTable<'_>) -> Option<Edit> {
    let at = doc.iter().find(|(k, _)| {
        let name: &str = k.get_ref();
        name == "configVersion"
    });
    let path = vec!["configVersion".to_string()];
    match (tables_in_use(doc), at) {
        (true, None) => Some(version_set()),
        // Only a version this build writes is taken away; anything else in that place is not ours to remove.
        (false, Some((_, v))) if is_known_version(v.get_ref()) => Some(Edit::Clear { path }),
        _ => None,
    }
}

/// The findings of the reader on `text`, one line each.
fn findings_of(text: &str) -> Vec<String> {
    read_config(text)
        .findings
        .iter()
        .map(Finding::render)
        .collect()
}

/// Refuses a text that cannot be used as TOML, with the findings that say why.
fn refuse_unreadable(text: &str) -> Result<crate::config::ConfigRead, PlanError> {
    let read = read_config(text);
    let why: Vec<String> = read
        .findings
        .errors()
        .filter(|f| f.code.is_unreadable())
        .map(Finding::render)
        .collect();
    if why.is_empty() {
        Ok(read)
    } else {
        Err(PlanError::Unreadable(why.join("\n")))
    }
}

/// The edit that writes `configVersion` at the version this build knows.
fn version_set() -> Edit {
    Edit::Set {
        path: vec!["configVersion".to_string()],
        value: EditValue::Int(i64::from(CONFIG_VERSION)),
    }
}

/// True when the file has no top-level `configVersion`.
fn version_missing(doc: &DeTable<'_>) -> bool {
    !doc.iter().any(|(k, _)| {
        let name: &str = k.get_ref();
        name == "configVersion"
    })
}

/// Applies the key edits, then the `configVersion` edit the result needs, and builds the plan. `adds_a_table_key` is true for a `set` of a key
/// of a table: when the file has no `configVersion` yet it is written first, so that the table lands after it and an unset takes both away and
/// leaves the bytes the file had (in a file with no key at the top, the other order leaves a blank line behind).
fn finish(
    text: &str,
    row: &'static Row,
    stored: Option<String>,
    edits: &[Edit],
    adds_a_table_key: bool,
) -> Result<Plan, PlanError> {
    let mut first = match config_edit::apply(text, edits, &rank)? {
        Outcome::Unchanged => {
            return Ok(Plan {
                name: row.name,
                stored,
                text: None,
                opens_the_door: false,
                still_wrong: findings_of(text),
            });
        }
        Outcome::Changed(t) => t,
    };
    let before = parse(text)?;
    if adds_a_table_key
        && version_missing(before.get_ref())
        && let Outcome::Changed(with_version) = config_edit::apply(text, &[version_set()], &rank)?
        && let Outcome::Changed(redone) = config_edit::apply(&with_version, edits, &rank)?
    {
        first = redone;
    }
    let doc = parse(&first)?;
    let next = match version_edit(doc.get_ref()) {
        Some(edit) => match config_edit::apply(&first, &[edit], &rank)? {
            Outcome::Changed(t) => t,
            Outcome::Unchanged => first,
        },
        None => first,
    };
    let after = parse(&next)?;
    let opens_the_door = !tables_in_use(before.get_ref()) && tables_in_use(after.get_ref());
    let still_wrong = findings_of(&next);
    Ok(Plan {
        name: row.name,
        stored,
        text: Some(next),
        opens_the_door,
        still_wrong,
    })
}

/// The plan for `ham config set <key> <value>` on the file `text` (empty for no file). The key is checked against the registry and the value
/// against its kind (a model is checked against the model list and stored as its full id).
pub fn plan_set(text: &str, key: &str, value: &str) -> Result<Plan, PlanError> {
    let read = refuse_unreadable(text)?;
    let after = set_key(&read.config, key, value)?;
    let key = Key::lookup(key).ok_or(PlanError::Refused(Refusal::NotProvable))?;
    let row = key.row();
    let stored = get_key(&after, row.name)?;
    let value = value_of(key, &after).ok_or(PlanError::Refused(Refusal::NotProvable))?;
    let doc = parse(text)?;
    let paths = paths_of(row, &top_names(doc.get_ref()));
    let path = paths
        .into_iter()
        .next()
        .ok_or(PlanError::Refused(Refusal::NotProvable))?;
    finish(
        text,
        row,
        stored,
        &[Edit::Set { path, value }],
        !row.file.0.is_empty(),
    )
}

/// The plan for `ham config unset <key>` on the file `text`. The key is removed from the file even when the reader could not store its value.
pub fn plan_unset(text: &str, key: &str) -> Result<Plan, PlanError> {
    let read = refuse_unreadable(text)?;
    unset_key(&read.config, key)?;
    let key = Key::lookup(key).ok_or(PlanError::Refused(Refusal::NotProvable))?;
    let row = key.row();
    let doc = parse(text)?;
    let edits: Vec<Edit> = paths_of(row, &top_names(doc.get_ref()))
        .into_iter()
        .map(|path| Edit::Clear { path })
        .collect();
    finish(text, row, None, &edits, false)
}

/// The note printed after the write that opened the door, and shown before it in the message box: what older builds do with the file and
/// how to go back. `client_version` is this build's (`4.07`), `name` the key just set (`run.goal`).
pub fn door_note(client_version: &str, name: &str) -> String {
    let table = Key::lookup(name)
        .map(|k| k.row().file.0)
        .filter(|t| !t.is_empty());
    let lines = match table {
        Some(t) => format!("`[{t}]` and `configVersion`"),
        None => "`configVersion`".to_string(),
    };
    format!(
        "ham versions before v{client_version} refuse this file. To go back, run `ham config unset {name}` or delete the {lines} lines."
    )
}
