//! The DynamoDB expressions of the tag writers, and the check that a fake rejects what DynamoDB rejects.
//!
//! The tag lists live on the run's search item (`PK = OWNER#<sha256 of the caller's principal ARN>`, `SK = SRCH#<createdAt>#<runId>`).
//! Three plain `UpdateItem`s write them, none a transaction, and each names only its own attributes, so the writers that already exist
//! (the start's, the end's, the backfill's) and the tag writers never overwrite one another:
//!
//! - [`inherit_update`]: a follow-up copies its parent's user and off lists, once, before any model call;
//! - [`auto_update`]: the tag branch stores the automatic list and the `runTagsAt` marker, once;
//! - [`edit_update`]: the person's edit stores the user and off lists, compare-and-set on `runTagsRev`.
//!
//! [`vocabulary_query`] reads the owner's newest search items, projecting the three tag lists only. Every attribute name goes through
//! an expression attribute name, so a reserved word can never be read as one.
//!
//! The specs are data: the adapters of `run-session` and `control-api` send them as they are, and their in-memory stores call
//! [`expr_rejects`] on every spec they receive, so a fake refuses what DynamoDB refuses (a path set twice, an empty set, an unused or
//! undefined placeholder, a reserved word as a bare name, a `Select` of `SPECIFIC_ATTRIBUTES` without a projection, an item over 400 KB).

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

/// `runTags`: the automatic tags, at most 6.
pub const RUN_TAGS: &str = "runTags";
/// `runTagsAt`: the initialize-once marker; present when the automatic step finished.
pub const RUN_TAGS_AT: &str = "runTagsAt";
/// `runTagsModel`: the route that answered, present only if the model answered.
pub const RUN_TAGS_MODEL: &str = "runTagsModel";
/// `runTagsVer`: the prompt version, present only if the model answered.
pub const RUN_TAGS_VER: &str = "runTagsVer";
/// `runTagsUser`: the tags the person added, at most 12.
pub const RUN_TAGS_USER: &str = "runTagsUser";
/// `runTagsOff`: the tags the person removed, at most 32.
pub const RUN_TAGS_OFF: &str = "runTagsOff";
/// `runTagsRev`: the compare-and-set counter, shared by every tag writer.
pub const RUN_TAGS_REV: &str = "runTagsRev";
/// `runTagsInh`: the parent run whose user and off lists were copied.
pub const RUN_TAGS_INH: &str = "runTagsInh";
/// The eight attributes of the tag branch and the edit route.
pub const TAG_ATTRIBUTES: [&str; 8] = [
    RUN_TAGS,
    RUN_TAGS_AT,
    RUN_TAGS_MODEL,
    RUN_TAGS_VER,
    RUN_TAGS_USER,
    RUN_TAGS_OFF,
    RUN_TAGS_REV,
    RUN_TAGS_INH,
];
const RUN_ID: &str = "runId";
const CREATED_AT: &str = "createdAt";

/// The newest search items one vocabulary read looks at.
pub const VOCABULARY_WINDOW: u32 = 200;
/// DynamoDB's limit on the size of an item.
pub const ITEM_MAX_BYTES: usize = 400 * 1024;
/// DynamoDB's limit on the size of one expression.
pub const EXPRESSION_MAX_BYTES: usize = 4096;

/// An attribute value of the specs: a string, a number, a list of strings, or a set of strings (never written by the tag writers; it
/// exists so a fake can refuse an empty one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Av {
    S(String),
    N(i64),
    L(Vec<String>),
    Ss(Vec<String>),
}

/// One `UpdateItem`, without its key: the caller builds `PK` and `SK` ([`crate::run_index::owner_pk`] and
/// [`crate::run_index::search_sk`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateSpec {
    /// What the key must be, for a reader of the spec.
    pub key_sk_prefix_note: String,
    pub expression: String,
    pub condition: Option<String>,
    /// Alias (`#runTags`) to attribute name.
    pub names: BTreeMap<String, String>,
    /// Alias (`:tags`) to value.
    pub values: BTreeMap<String, Av>,
    /// The size the item would have after the update, when the caller knows it (a fake does); checked against 400 KB.
    pub item_bytes: Option<usize>,
}

/// One `Query`, without the table name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuerySpec {
    pub key_condition: String,
    pub names: BTreeMap<String, String>,
    pub values: BTreeMap<String, Av>,
    pub projection: Option<String>,
    pub select: Option<String>,
    pub filter: Option<String>,
    pub consistent_read: bool,
    pub limit: u32,
    pub scan_index_forward: bool,
}

/// The facts of a model answer that the automatic write stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Answered<'a> {
    /// The route that answered.
    pub route: &'a str,
    /// The prompt version.
    pub version: &'a str,
}

const KEY_NOTE: &str = "PK = OWNER#<sha256 of the caller's principal ARN>, SK = SRCH#<createdAt>#<runId> (the search item of the run)";

#[derive(Default)]
struct Builder {
    sets: Vec<String>,
    removes: Vec<String>,
    names: BTreeMap<String, String>,
    values: BTreeMap<String, Av>,
}

impl Builder {
    fn name(&mut self, attribute: &str) -> String {
        let alias = format!("#{attribute}");
        self.names.insert(alias.clone(), attribute.to_string());
        alias
    }

    fn set(&mut self, attribute: &str, value_alias: &str, value: Av) {
        let name = self.name(attribute);
        self.sets.push(format!("{name} = {value_alias}"));
        self.values.insert(value_alias.to_string(), value);
    }

    fn remove(&mut self, attribute: &str) {
        let name = self.name(attribute);
        self.removes.push(name);
    }

    /// `runId` and `createdAt` are set only if the item has none: the op can reach the table before the start's own write does.
    fn creation(&mut self, run_id: &str, created_at: &str) {
        for (attribute, alias, value) in
            [(RUN_ID, ":rid", run_id), (CREATED_AT, ":cat", created_at)]
        {
            let name = self.name(attribute);
            self.sets
                .push(format!("{name} = if_not_exists({name}, {alias})"));
            self.values
                .insert(alias.to_string(), Av::S(value.to_string()));
        }
    }

    fn bump_revision(&mut self) {
        let name = self.name(RUN_TAGS_REV);
        self.sets
            .push(format!("{name} = if_not_exists({name}, :zero) + :one"));
        self.values.insert(":zero".to_string(), Av::N(0));
        self.values.insert(":one".to_string(), Av::N(1));
    }

    fn finish(self, condition: &str) -> UpdateSpec {
        let mut expression = format!("SET {}", self.sets.join(", "));
        if !self.removes.is_empty() {
            expression.push_str(&format!(" REMOVE {}", self.removes.join(", ")));
        }
        UpdateSpec {
            key_sk_prefix_note: KEY_NOTE.to_string(),
            expression,
            condition: Some(condition.to_string()),
            names: self.names,
            values: self.values,
            item_bytes: None,
        }
    }
}

fn strings(tags: &[String]) -> Av {
    Av::L(tags.to_vec())
}

/// A follow-up copies its parent's user and off lists (an empty list is left out), once: the condition fails if an edit, or an earlier
/// copy, came first, and the person's edit wins. `None` when the parent has neither list, because there is nothing to write.
pub fn inherit_update(
    run_id: &str,
    created_at: &str,
    parent_run_id: &str,
    user: &[String],
    off: &[String],
) -> Option<UpdateSpec> {
    if user.is_empty() && off.is_empty() {
        return None;
    }
    let mut b = Builder::default();
    b.creation(run_id, created_at);
    if !user.is_empty() {
        b.set(RUN_TAGS_USER, ":user", strings(user));
    }
    if !off.is_empty() {
        b.set(RUN_TAGS_OFF, ":off", strings(off));
    }
    b.set(RUN_TAGS_INH, ":inh", Av::S(parent_run_id.to_string()));
    b.bump_revision();
    Some(b.finish("attribute_not_exists(#runTagsInh) AND attribute_not_exists(#runTagsRev)"))
}

/// The automatic step, once: the list (left out when empty), the marker `runTagsAt`, and the route and version when the model answered.
/// The condition `attribute_not_exists(runTagsAt)` makes a second op, a retry or a duplicate event, a no-op.
pub fn auto_update(
    run_id: &str,
    created_at: &str,
    tags: &[String],
    at: &str,
    answered: Option<Answered<'_>>,
) -> UpdateSpec {
    let mut b = Builder::default();
    b.creation(run_id, created_at);
    if !tags.is_empty() {
        b.set(RUN_TAGS, ":tags", strings(tags));
    }
    b.set(RUN_TAGS_AT, ":at", Av::S(at.to_string()));
    if let Some(a) = answered {
        b.set(RUN_TAGS_MODEL, ":route", Av::S(a.route.to_string()));
        b.set(RUN_TAGS_VER, ":ver", Av::S(a.version.to_string()));
    }
    b.bump_revision();
    b.finish("attribute_not_exists(#runTagsAt)")
}

/// The person's edit: both lists set, or removed when empty (never both for one attribute), compare-and-set on the revision the edit
/// read (`None` when the item had none). A lost race makes the condition fail; the caller re-reads once.
pub fn edit_update(
    run_id: &str,
    created_at: &str,
    user: &[String],
    off: &[String],
    expected_rev: Option<u32>,
) -> UpdateSpec {
    let mut b = Builder::default();
    b.creation(run_id, created_at);
    for (attribute, alias, list) in [(RUN_TAGS_USER, ":user", user), (RUN_TAGS_OFF, ":off", off)] {
        if list.is_empty() {
            b.remove(attribute);
        } else {
            b.set(attribute, alias, strings(list));
        }
    }
    b.bump_revision();
    b.values.insert(
        ":r".to_string(),
        Av::N(i64::from(expected_rev.unwrap_or(0))),
    );
    b.finish("attribute_not_exists(#runTagsRev) OR #runTagsRev = :r")
}

/// The vocabulary read: the owner's newest 200 search items, eventually consistent, only the three tag lists. No filter expression, so
/// a policy that limits the attributes can be proved against exactly this request.
pub fn vocabulary_query(owner_pk: &str) -> QuerySpec {
    let names = [
        ("#pk", "PK"),
        ("#sk", "SK"),
        ("#runTags", RUN_TAGS),
        ("#runTagsUser", RUN_TAGS_USER),
        ("#runTagsOff", RUN_TAGS_OFF),
    ];
    QuerySpec {
        key_condition: "#pk = :pk AND begins_with(#sk, :sk)".to_string(),
        names: names
            .iter()
            .map(|(a, n)| (a.to_string(), n.to_string()))
            .collect(),
        values: BTreeMap::from([
            (":pk".to_string(), Av::S(owner_pk.to_string())),
            (
                ":sk".to_string(),
                Av::S(crate::run_index::SEARCH_PREFIX.to_string()),
            ),
        ]),
        projection: Some("#runTags, #runTagsUser, #runTagsOff".to_string()),
        select: Some("SPECIFIC_ATTRIBUTES".to_string()),
        filter: None,
        consistent_read: false,
        limit: VOCABULARY_WINDOW,
        scan_index_forward: false,
    }
}

// ---- the rules DynamoDB applies ----

static RESERVED: OnceLock<HashSet<&'static str>> = OnceLock::new();

fn reserved_words() -> &'static HashSet<&'static str> {
    RESERVED.get_or_init(|| {
        include_str!("../../assets/dynamodb-reserved-words.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    })
}

/// True for a DynamoDB reserved word (the list of the developer guide), compared without case.
pub fn is_reserved(word: &str) -> bool {
    reserved_words().contains(word.to_ascii_lowercase().as_str())
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    Name(String),
    Value(String),
    Num(String),
    Punct(char),
}

fn tokenize(text: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '#' || c == ':' {
            let start = i;
            i += 1;
            while i < chars.len() && ident(chars[i]) {
                i += 1;
            }
            if i == start + 1 {
                return Err(format!("a lone {c} in the expression"));
            }
            let alias: String = chars[start..i].iter().collect();
            out.push(if c == '#' {
                Tok::Name(alias)
            } else {
                Tok::Value(alias)
            });
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && ident(chars[i]) {
                i += 1;
            }
            out.push(Tok::Word(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            out.push(Tok::Num(chars[start..i].iter().collect()));
        } else if ",()=+-.[]<>".contains(c) {
            out.push(Tok::Punct(c));
            i += 1;
        } else {
            return Err(format!("unexpected character {c:?} in the expression"));
        }
    }
    Ok(out)
}

const SECTIONS: [&str; 4] = ["SET", "REMOVE", "ADD", "DELETE"];
const OPERATORS: [&str; 5] = ["and", "or", "not", "between", "in"];

fn section_of(tok: &Tok) -> Option<&'static str> {
    match tok {
        Tok::Word(w) => SECTIONS.iter().copied().find(|s| s.eq_ignore_ascii_case(w)),
        _ => None,
    }
}

/// The reserved words used as bare attribute names. A word followed by `(` is a function; `operators` says whether AND, OR, NOT,
/// BETWEEN and IN are operators here (a condition) or names (an update expression); `sections` are the tokens that start a clause.
fn reserved_bare(tokens: &[Tok], operators: bool, sections: &HashSet<usize>) -> Option<String> {
    for (i, tok) in tokens.iter().enumerate() {
        let Tok::Word(w) = tok else { continue };
        if sections.contains(&i) || tokens.get(i + 1) == Some(&Tok::Punct('(')) {
            continue;
        }
        if operators && OPERATORS.contains(&w.to_ascii_lowercase().as_str()) {
            continue;
        }
        if is_reserved(w) {
            return Some(format!(
                "attribute name {w} is a reserved keyword; use an expression attribute name"
            ));
        }
    }
    None
}

/// The segments of a document path (`#a.#b[0]`), alias names resolved when defined.
fn path_segments(tokens: &[Tok], names: &BTreeMap<String, String>) -> Result<Vec<String>, String> {
    let mut segments = Vec::new();
    let mut want_name = true;
    let mut i = 0;
    while i < tokens.len() {
        match (&tokens[i], want_name) {
            (Tok::Word(w), true) => {
                segments.push(w.clone());
                want_name = false;
            }
            (Tok::Name(a), true) => {
                segments.push(names.get(a).cloned().unwrap_or_else(|| a.clone()));
                want_name = false;
            }
            (Tok::Punct('.'), false) => want_name = true,
            (Tok::Punct('['), false) => match (tokens.get(i + 1), tokens.get(i + 2)) {
                (Some(Tok::Num(n)), Some(Tok::Punct(']'))) => {
                    segments.push(format!("[{n}]"));
                    i += 2;
                }
                _ => return Err("invalid list index in a document path".to_string()),
            },
            (other, _) => return Err(format!("invalid document path near {other:?}")),
        }
        i += 1;
    }
    if segments.is_empty() || want_name {
        return Err("incomplete document path".to_string());
    }
    Ok(segments)
}

fn overlaps(a: &[String], b: &[String]) -> bool {
    let n = a.len().min(b.len());
    a[..n] == b[..n]
}

/// The clauses of an update expression and the document paths each one writes.
/// What an update expression writes: the document paths, the positions of the clause keywords, and the value placeholder of each ADD
/// and DELETE action (the type of their operand is checked against the values).
struct UpdateParts {
    paths: Vec<Vec<String>>,
    sections: HashSet<usize>,
    operands: Vec<(&'static str, String)>,
}

fn update_targets(tokens: &[Tok], names: &BTreeMap<String, String>) -> Result<UpdateParts, String> {
    let mut starts: Vec<(usize, &'static str)> = Vec::new();
    let mut depth = 0i32;
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth -= 1,
            _ if depth == 0 => {
                if let Some(section) = section_of(tok) {
                    if starts.iter().any(|(_, s)| *s == section) {
                        return Err(format!(
                            "the {section} section can only be used once in an update expression"
                        ));
                    }
                    starts.push((i, section));
                }
            }
            _ => {}
        }
    }
    if starts.first().map(|(i, _)| *i) != Some(0) {
        return Err("an update expression starts with SET, REMOVE, ADD or DELETE".to_string());
    }
    let mut paths = Vec::new();
    let mut operands: Vec<(&'static str, String)> = Vec::new();
    for (n, (start, section)) in starts.iter().enumerate() {
        let end = starts.get(n + 1).map_or(tokens.len(), |(i, _)| *i);
        let body = &tokens[start + 1..end];
        if body.is_empty() {
            return Err(format!("the {section} section is empty"));
        }
        let mut action: Vec<Tok> = Vec::new();
        let mut actions: Vec<Vec<Tok>> = Vec::new();
        let mut depth = 0i32;
        for tok in body {
            match tok {
                Tok::Punct('(') => depth += 1,
                Tok::Punct(')') => depth -= 1,
                _ => {}
            }
            if depth == 0 && *tok == Tok::Punct(',') {
                actions.push(std::mem::take(&mut action));
            } else {
                action.push(tok.clone());
            }
        }
        actions.push(action);
        for action in actions {
            let path_tokens: &[Tok] = match *section {
                "SET" => {
                    let eq = action
                        .iter()
                        .position(|t| *t == Tok::Punct('='))
                        .ok_or("a SET action is path = value")?;
                    if eq + 1 >= action.len() {
                        return Err("a SET action is path = value".to_string());
                    }
                    &action[..eq]
                }
                "REMOVE" => &action,
                _ => {
                    if action.len() < 2
                        || !matches!(action.last(), Some(Tok::Value(_) | Tok::Num(_)))
                    {
                        return Err(format!("a {section} action is path value"));
                    }
                    if let Some(Tok::Value(alias)) = action.last() {
                        operands.push((section, alias.clone()));
                    }
                    &action[..action.len() - 1]
                }
            };
            paths.push(path_segments(path_tokens, names)?);
        }
    }
    let sections = starts.iter().map(|(i, _)| *i).collect();
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            if overlaps(a, b) {
                return Err(format!(
                    "two document paths overlap with each other: {} and {}",
                    a.join("."),
                    b.join(".")
                ));
            }
        }
    }
    Ok(UpdateParts {
        paths,
        sections,
        operands,
    })
}

/// DynamoDB expressions have no literals: every value is a placeholder. The one number allowed is a list index, `[0]`.
fn literal_problem(tokens: &[Tok]) -> Option<String> {
    tokens.iter().enumerate().find_map(|(i, tok)| match tok {
        Tok::Num(n) if i == 0 || tokens[i - 1] != Tok::Punct('[') => {
            Some(format!("a literal ({n}) in an expression; every value is an expression attribute value placeholder"))
        }
        _ => None,
    })
}

/// The key attributes of the table cannot be written by an update.
const KEY_ATTRIBUTES: [&str; 2] = ["PK", "SK"];

fn alias_problems(
    texts: &[&str],
    names: &BTreeMap<String, String>,
    values: &BTreeMap<String, Av>,
) -> Option<String> {
    let mut used_names: HashSet<String> = HashSet::new();
    let mut used_values: HashSet<String> = HashSet::new();
    for text in texts {
        for tok in tokenize(text).unwrap_or_default() {
            match tok {
                Tok::Name(a) => {
                    used_names.insert(a);
                }
                Tok::Value(a) => {
                    used_values.insert(a);
                }
                _ => {}
            }
        }
    }
    for a in names.keys() {
        if !a.starts_with('#') {
            return Some(format!("expression attribute name {a} must start with #"));
        }
    }
    for a in values.keys() {
        if !a.starts_with(':') {
            return Some(format!("expression attribute value {a} must start with :"));
        }
    }
    if let Some(a) = used_names.iter().filter(|a| !names.contains_key(*a)).min() {
        return Some(format!(
            "expression attribute name {a} is used in an expression and is not defined"
        ));
    }
    if let Some(a) = used_values
        .iter()
        .filter(|a| !values.contains_key(*a))
        .min()
    {
        return Some(format!(
            "expression attribute value {a} is used in an expression and is not defined"
        ));
    }
    if let Some(a) = names.keys().filter(|a| !used_names.contains(*a)).min() {
        return Some(format!(
            "expression attribute name {a} is defined and unused in the expressions"
        ));
    }
    if let Some(a) = values.keys().filter(|a| !used_values.contains(*a)).min() {
        return Some(format!(
            "expression attribute value {a} is defined and unused in the expressions"
        ));
    }
    None
}

fn value_problems(values: &BTreeMap<String, Av>) -> Option<String> {
    for (alias, value) in values {
        if let Av::Ss(items) = value {
            if items.is_empty() {
                return Some(format!(
                    "an attribute value cannot be an empty set ({alias})"
                ));
            }
            let distinct: HashSet<&String> = items.iter().collect();
            if distinct.len() != items.len() {
                return Some(format!(
                    "a set value cannot hold duplicate elements ({alias})"
                ));
            }
        }
    }
    None
}

fn too_long(texts: &[(&str, &str)]) -> Option<String> {
    texts
        .iter()
        .find(|(_, t)| t.len() > EXPRESSION_MAX_BYTES)
        .map(|(what, _)| {
            format!(
                "the {what} is over the limit of {EXPRESSION_MAX_BYTES} bytes for an expression"
            )
        })
}

fn update_rejects(spec: &UpdateSpec) -> Option<String> {
    if spec.expression.trim().is_empty() {
        return Some("an update expression is required".to_string());
    }
    let condition = spec.condition.as_deref().unwrap_or("");
    if let Some(problem) = too_long(&[
        ("update expression", &spec.expression),
        ("condition expression", condition),
    ]) {
        return Some(problem);
    }
    let tokens = match tokenize(&spec.expression) {
        Ok(t) => t,
        Err(e) => return Some(e),
    };
    let condition_tokens = match tokenize(condition) {
        Ok(t) => t,
        Err(e) => return Some(e),
    };
    if let Some(problem) = literal_problem(&tokens).or_else(|| literal_problem(&condition_tokens)) {
        return Some(problem);
    }
    let parts = match update_targets(&tokens, &spec.names) {
        Ok(parts) => parts,
        Err(e) => return Some(e),
    };
    if let Some(path) = parts
        .paths
        .iter()
        .find(|p| KEY_ATTRIBUTES.contains(&p[0].as_str()))
    {
        return Some(format!("the key attribute {} cannot be updated", path[0]));
    }
    if let Some(problem) = reserved_bare(&tokens, false, &parts.sections)
        .or_else(|| reserved_bare(&condition_tokens, true, &HashSet::new()))
    {
        return Some(problem);
    }
    for (section, alias) in &parts.operands {
        let allowed = matches!(
            (*section, spec.values.get(alias)),
            (_, None) | ("ADD", Some(Av::N(_) | Av::Ss(_))) | ("DELETE", Some(Av::Ss(_)))
        );
        if !allowed {
            let wants = if *section == "ADD" {
                "a number or a set"
            } else {
                "a set"
            };
            return Some(format!("{section} takes {wants} as its operand ({alias})"));
        }
    }
    if let Some(problem) = alias_problems(&[&spec.expression, condition], &spec.names, &spec.values)
    {
        return Some(problem);
    }
    if let Some(problem) = value_problems(&spec.values) {
        return Some(problem);
    }
    match spec.item_bytes {
        Some(bytes) if bytes > ITEM_MAX_BYTES => Some(format!(
            "item size {bytes} has exceeded the 400 KB limit of an item"
        )),
        _ => None,
    }
}

fn query_rejects(spec: &QuerySpec) -> Option<String> {
    if spec.key_condition.trim().is_empty() {
        return Some("a key condition expression is required".to_string());
    }
    let filter = spec.filter.as_deref().unwrap_or("");
    let projection = spec.projection.as_deref().unwrap_or("");
    if let Some(problem) = too_long(&[
        ("key condition", &spec.key_condition),
        ("filter expression", filter),
        ("projection expression", projection),
    ]) {
        return Some(problem);
    }
    let key_tokens = match tokenize(&spec.key_condition) {
        Ok(t) => t,
        Err(e) => return Some(e),
    };
    let filter_tokens = match tokenize(filter) {
        Ok(t) => t,
        Err(e) => return Some(e),
    };
    let projection_tokens = match tokenize(projection) {
        Ok(t) => t,
        Err(e) => return Some(e),
    };
    if let Some(problem) = literal_problem(&key_tokens)
        .or_else(|| literal_problem(&filter_tokens))
        .or_else(|| literal_problem(&projection_tokens))
    {
        return Some(problem);
    }
    let none = HashSet::new();
    if let Some(problem) = reserved_bare(&key_tokens, true, &none)
        .or_else(|| reserved_bare(&filter_tokens, true, &none))
        .or_else(|| reserved_bare(&projection_tokens, false, &none))
    {
        return Some(problem);
    }
    if spec.projection.is_some() {
        let mut paths: Vec<Vec<String>> = Vec::new();
        for part in projection_tokens.split(|t| *t == Tok::Punct(',')) {
            match path_segments(part, &spec.names) {
                Ok(p) => paths.push(p),
                Err(e) => return Some(format!("invalid projection expression: {e}")),
            }
        }
        for (i, a) in paths.iter().enumerate() {
            if paths[i + 1..].iter().any(|b| overlaps(a, b)) {
                return Some(format!(
                    "invalid projection expression: two document paths overlap with each other ({})",
                    a.join(".")
                ));
            }
        }
    }
    match (spec.select.as_deref(), spec.projection.is_some()) {
        (Some("SPECIFIC_ATTRIBUTES"), false) => {
            return Some("Select SPECIFIC_ATTRIBUTES needs a ProjectionExpression".to_string());
        }
        (Some(s @ ("ALL_ATTRIBUTES" | "COUNT" | "ALL_PROJECTED_ATTRIBUTES")), true) => {
            return Some(format!(
                "a ProjectionExpression cannot be used with Select {s}"
            ));
        }
        (
            Some("SPECIFIC_ATTRIBUTES" | "ALL_ATTRIBUTES" | "COUNT" | "ALL_PROJECTED_ATTRIBUTES")
            | None,
            _,
        ) => {}
        (Some(other), _) => return Some(format!("unknown Select value {other}")),
    }
    if spec.limit == 0 {
        return Some("Limit must be at least 1".to_string());
    }
    if let Some(problem) = alias_problems(
        &[&spec.key_condition, filter, projection],
        &spec.names,
        &spec.values,
    ) {
        return Some(problem);
    }
    value_problems(&spec.values)
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for super::UpdateSpec {}
    impl Sealed for super::QuerySpec {}
}

/// A request spec that [`expr_rejects`] can judge: an [`UpdateSpec`] or a [`QuerySpec`].
pub trait ExprSpec: sealed::Sealed {
    fn rejects(&self) -> Option<String>;
}

impl ExprSpec for UpdateSpec {
    fn rejects(&self) -> Option<String> {
        update_rejects(self)
    }
}

impl ExprSpec for QuerySpec {
    fn rejects(&self) -> Option<String> {
        query_rejects(self)
    }
}

/// Why DynamoDB would refuse this request, or `None`. The in-memory stores of the tag writers call it on every update and query they
/// receive and fail the test on `Some`, so a fake is as strict as the service in what a tag writer can plausibly get wrong.
pub fn expr_rejects<T: ExprSpec>(spec: &T) -> Option<String> {
    spec.rejects()
}
