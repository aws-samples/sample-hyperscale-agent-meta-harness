//! Property tests of the comment-keeping patcher (`ham_core::config_edit`). Documents are generated from a model that holds every shape
//! the design names: comments, blank lines, trailing comments, quotes and `#` inside strings, multi-line strings and arrays, dotted
//! keys, inline tables, standard tables in any order, arrays of tables, LF and CRLF, with and without a final newline. For a generated
//! document and edit the patcher must do one of three things: refuse a construct it does not edit (a dotted key, an inline table, an array
//! of tables, a name that is a table), report that nothing changes, or return text that
//!
//! * reads as the old tree with exactly the edited key changed (checked here against a model of the edit, not against the patcher's own
//!   oracle),
//! * differs from the old text in one contiguous region whose shape fits the edit: a replaced value on one line, an inserted block, or
//!   whole lines removed, and
//! * is a fixed point: the same edit on the result changes nothing.
//!
//! An edit of a plain key never ends in `NotProvable`, and a file that is not TOML ends in `NotToml`.
#![allow(clippy::unwrap_used)]

use ham_core::config_edit::{Edit, EditValue, Outcome, Refusal, apply};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use std::collections::BTreeMap;

fn rank(path: &[String]) -> usize {
    const ORDER: [&str; 10] = [
        "apiEndpoint",
        "region",
        "awsProfile",
        "defaultRepo",
        "downloadDir",
        "defaultModel",
        "inputWaitMinutes",
        "reportTests",
        "mouse",
        "configVersion",
    ];
    path.first()
        .and_then(|k| ORDER.iter().position(|o| o == k))
        .unwrap_or(usize::MAX)
}

// ---- the document model -------------------------------------------------------------------------------------------------------------

const TOP_KEYS: &[&str] = &[
    "a",
    "b",
    "c",
    "region",
    "default_model",
    "mouse",
    "apiEndpoint",
    "inl",
];
const TABLE_KEYS: &[&str] = &["goal", "rules", "skills", "a", "tags", "k"];
const TABLE_NAMES: &[&str] = &["run", "research", "ui", "run.sub"];
const DOTTED: &[(&str, &str)] = &[("d", "x"), ("d", "y"), ("f", "g")];
const COMMENTS: &[&str] = &[
    "# a comment",
    "# key = 1 (commented out)",
    "#",
    "# [fake] table",
    "#  trailing ] [ \"quote",
];
const TAILS: &[&str] = &["# tail", "#x", "# a = \"b\""];
/// Source text of values, written with `\n` for line breaks inside them.
const VALUES: &[&str] = &[
    "true",
    "false",
    "42",
    "-7",
    "0x1F",
    "0.9",
    "1.50",
    "1e3",
    "\"plain\"",
    "\"has # hash\"",
    "\"quo\\\"te\"",
    "\"back\\\\slash\"",
    "'lit # eral'",
    "\"\u{e9}\"",
    "\"\"\"\nline one\n# line two\n\"\"\"",
    "'''\n# not a comment\n'''",
    "[]",
    "[\"a\", \"b\"]",
    "[\n  \"a\", # c\n  \"b\",\n]",
    "[1, 2, 3]",
    "[[1], [2]]",
    "[ {a = 1} ]",
    "{ c = 1 }",
    "{}",
];

#[derive(Clone, Debug)]
enum Item {
    Kv(&'static str, &'static str, Option<&'static str>),
    Dotted(&'static str, &'static str),
    Comment(&'static str),
    Blank,
}

#[derive(Clone, Debug)]
struct TableSpec {
    name: &'static str,
    above: Vec<Item>,
    header_tail: Option<&'static str>,
    items: Vec<Item>,
}

#[derive(Clone, Debug)]
struct Spec {
    crlf: bool,
    final_newline: bool,
    top: Vec<Item>,
    tables: Vec<TableSpec>,
    aot: usize,
}

fn item(keys: &'static [&'static str]) -> impl Strategy<Value = Item> {
    prop_oneof![
        5 => (
            prop::sample::select(keys),
            prop::sample::select(VALUES),
            prop::option::of(prop::sample::select(TAILS)),
        )
            .prop_map(|(k, v, c)| Item::Kv(k, v, c)),
        1 => prop::sample::select(DOTTED).prop_map(|(p, k)| Item::Dotted(p, k)),
        2 => prop::sample::select(COMMENTS).prop_map(Item::Comment),
        2 => Just(Item::Blank),
    ]
}

fn spec_strategy() -> impl Strategy<Value = Spec> {
    let table = (
        prop::sample::select(TABLE_NAMES),
        prop::collection::vec(
            prop_oneof![
                prop::sample::select(COMMENTS).prop_map(Item::Comment),
                Just(Item::Blank)
            ],
            0..3,
        ),
        prop::option::of(prop::sample::select(TAILS)),
        // Many tables hold one key, so that removing it removes the table.
        prop_oneof![
            2 => prop::collection::vec(item(TABLE_KEYS), 0..6),
            2 => (prop::sample::select(TABLE_KEYS), prop::sample::select(VALUES))
                .prop_map(|(k, v)| vec![Item::Kv(k, v, None)]),
        ],
    )
        .prop_map(|(name, above, header_tail, items)| TableSpec {
            name,
            above,
            header_tail,
            items,
        });
    (
        any::<bool>(),
        any::<bool>(),
        prop::collection::vec(item(TOP_KEYS), 0..7),
        prop::collection::vec(table, 0..4),
        0usize..3,
    )
        .prop_map(|(crlf, final_newline, top, tables, aot)| Spec {
            crlf,
            final_newline,
            top,
            tables,
            aot,
        })
}

/// The lines of a block of items; a repeated key, repeated dotted pair or key that already stands for a table is dropped, so the
/// document is valid TOML by construction.
fn block(items: &[Item], seen: &mut Vec<(String, String)>, out: &mut Vec<String>) {
    for it in items {
        match it {
            Item::Kv(k, v, tail) => {
                if seen.iter().any(|(p, key)| p.is_empty() && key == k) {
                    continue;
                }
                seen.push((String::new(), (*k).to_string()));
                let mut line = format!("{k} = {v}");
                if let Some(t) = tail {
                    line.push(' ');
                    line.push_str(t);
                }
                out.push(line);
            }
            Item::Dotted(p, k) => {
                if seen.iter().any(|(sp, key)| sp.as_str() == *p && key == k) {
                    continue;
                }
                seen.push(((*p).to_string(), (*k).to_string()));
                out.push(format!("{p}.{k} = 1"));
            }
            Item::Comment(c) => out.push((*c).to_string()),
            Item::Blank => out.push(String::new()),
        }
    }
}

fn render(spec: &Spec) -> String {
    let mut lines = Vec::new();
    block(&spec.top, &mut Vec::new(), &mut lines);
    let mut names: Vec<&str> = Vec::new();
    for t in &spec.tables {
        if names.contains(&t.name) {
            continue;
        }
        names.push(t.name);
        block(&t.above, &mut Vec::new(), &mut lines);
        let mut header = format!("[{}]", t.name);
        if let Some(tail) = t.header_tail {
            header.push(' ');
            header.push_str(tail);
        }
        lines.push(header);
        block(&t.items, &mut Vec::new(), &mut lines);
    }
    for n in 0..spec.aot {
        lines.push("[[aot]]".to_string());
        lines.push(format!("z = {n}"));
    }
    let eol = if spec.crlf { "\r\n" } else { "\n" };
    let mut text = lines.join("\n").replace('\n', eol);
    if !lines.is_empty() && spec.final_newline {
        text.push_str(eol);
    }
    text
}

// ---- the edits and the model of an edit ---------------------------------------------------------------------------------------------

const EDIT_PATHS: &[&[&str]] = &[
    &["a"],
    &["b"],
    &["c"],
    &["region"],
    &["default_model"],
    &["mouse"],
    &["apiEndpoint"],
    &["configVersion"],
    &["newkey"],
    &["inl"],
    &["d"],
    &["d", "x"],
    &["run", "goal"],
    &["run", "rules"],
    &["run", "skills"],
    &["run", "tags"],
    &["run", "brand_new"],
    &["run", "sub", "k"],
    &["research", "enabled"],
    &["ui", "bell"],
    &["fresh", "k"],
    &["aot", "z"],
    &["run"],
    &["research"],
    &["a", "b"],
];

#[derive(Clone, Debug)]
enum Val {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(&'static str),
    List(&'static [&'static str]),
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    let value = prop_oneof![
        any::<bool>().prop_map(Val::Bool),
        prop::sample::select(vec![0i64, -7, 42, i64::MAX]).prop_map(Val::Int),
        prop::sample::select(vec![0.9f64, 1.0, -2.5, 1e21]).prop_map(Val::Float),
        prop::sample::select(vec![
            "x",
            "",
            "has # hash",
            "q\"uote",
            "back\\slash",
            "two\nlines",
            "[inject]\nz = 1",
        ])
        .prop_map(Val::Text),
        prop::sample::select(vec![&["a", "b"][..], &[][..], &["x\"y", "]\n[inject]"][..]])
            .prop_map(Val::List),
    ];
    let path = prop::sample::select(EDIT_PATHS)
        .prop_map(|p| p.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
    prop_oneof![
        3 => (path.clone(), value).prop_map(|(path, v)| Edit::Set {
            path,
            value: match v {
                Val::Bool(b) => EditValue::Bool(b),
                Val::Int(i) => EditValue::Int(i),
                Val::Float(f) => EditValue::Float(f),
                Val::Text(t) => EditValue::Str(t.to_string()),
                Val::List(l) => EditValue::StrList(l.iter().map(|s| (*s).to_string()).collect()),
            },
        }),
        2 => path.prop_map(|path| Edit::Clear { path }),
    ]
}

/// Paths of the keys a tree holds (values that are not tables, and empty tables).
fn existing_paths(t: &toml::Table) -> Vec<Vec<String>> {
    fn walk(t: &toml::Table, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        for (k, v) in t {
            prefix.push(k.clone());
            match v {
                toml::Value::Table(inner) if !inner.is_empty() => walk(inner, prefix, out),
                _ => out.push(prefix.clone()),
            }
            prefix.pop();
        }
    }
    let mut out = Vec::new();
    walk(t, &mut Vec::new(), &mut out);
    out
}

/// The same edit aimed at the `pick`th key the document has, if it has any: most real edits name a key that is there.
fn aimed(edit: Edit, tree: &toml::Table, pick: Option<usize>) -> Edit {
    let paths = existing_paths(tree);
    let Some(pick) = pick.filter(|_| !paths.is_empty()) else {
        return edit;
    };
    let path = paths[pick % paths.len()].clone();
    match edit {
        Edit::Set { value, .. } => Edit::Set { path, value },
        Edit::Clear { .. } => Edit::Clear { path },
    }
}

fn to_toml(value: &EditValue) -> toml::Value {
    match value {
        EditValue::Bool(b) => toml::Value::Boolean(*b),
        EditValue::Int(i) => toml::Value::Integer(*i),
        EditValue::Float(f) => toml::Value::Float(*f),
        EditValue::Str(s) => toml::Value::String(s.clone()),
        EditValue::StrList(l) => {
            toml::Value::Array(l.iter().cloned().map(toml::Value::String).collect())
        }
    }
}

/// The tree an edit must produce, written without the patcher; `None` when the edit cannot be made (a value where a table is needed,
/// or a table where a value is set).
fn model(old: &toml::Table, edit: &Edit) -> Option<toml::Table> {
    let mut t = old.clone();
    match edit {
        Edit::Set { path, value } => {
            let (last, parents) = path.split_last()?;
            let mut cur = &mut t;
            for seg in parents {
                let entry = cur
                    .entry(seg.clone())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                cur = entry.as_table_mut()?;
            }
            if matches!(cur.get(last), Some(toml::Value::Table(_))) {
                return None;
            }
            cur.insert(last.clone(), to_toml(value));
            Some(t)
        }
        Edit::Clear { path } => {
            let (last, parents) = path.split_last()?;
            let mut cur = &mut t;
            for seg in parents {
                match cur.get_mut(seg) {
                    Some(toml::Value::Table(inner)) => cur = inner,
                    _ => return Some(t),
                }
            }
            match cur.get(last) {
                Some(toml::Value::Table(_)) => None,
                Some(_) => {
                    cur.remove(last);
                    Some(t)
                }
                None => Some(t),
            }
        }
    }
}

/// Every leaf with its path; an empty table is a leaf too. Tables above the edited key may appear or vanish with it, so those paths are
/// left out of the comparison.
fn flat(t: &toml::Table, edited: &[String]) -> BTreeMap<Vec<String>, String> {
    fn walk(t: &toml::Table, prefix: &mut Vec<String>, out: &mut BTreeMap<Vec<String>, String>) {
        for (k, v) in t {
            prefix.push(k.clone());
            match v {
                toml::Value::Table(inner) if !inner.is_empty() => walk(inner, prefix, out),
                toml::Value::Table(_) => {
                    out.insert(prefix.clone(), "empty table".to_string());
                }
                other => {
                    out.insert(prefix.clone(), format!("{other:?}"));
                }
            }
            prefix.pop();
        }
    }
    let mut out = BTreeMap::new();
    walk(t, &mut Vec::new(), &mut out);
    for k in 1..edited.len() {
        out.remove(&edited[..k]);
    }
    out
}

/// The lines two texts share at the start and at the end (line breaks kept), and how many lines each has between them.
fn line_diff(old: &str, new: &str) -> (usize, usize, usize, usize) {
    let o: Vec<&str> = old.split_inclusive('\n').collect();
    let n: Vec<&str> = new.split_inclusive('\n').collect();
    let p = o.iter().zip(&n).take_while(|(a, b)| a == b).count();
    let room = o.len().min(n.len()) - p;
    let s = o
        .iter()
        .rev()
        .zip(n.iter().rev())
        .take(room)
        .take_while(|(a, b)| a == b)
        .count();
    (p, s, o.len() - p - s, n.len() - p - s)
}

fn edit_path(edit: &Edit) -> &[String] {
    match edit {
        Edit::Set { path, .. } | Edit::Clear { path } => path,
    }
}

/// The paths of the keys of a generated document whose value is an inline table, which the patcher does not edit (nor anything under them).
fn inline_paths(spec: &Spec) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut add = |scope: &[String], items: &[Item]| {
        for it in items {
            if let Item::Kv(k, v, _) = it
                && v.starts_with('{')
            {
                let mut path = scope.to_vec();
                path.push((*k).to_string());
                out.push(path);
            }
        }
    };
    add(&[], &spec.top);
    for t in &spec.tables {
        let scope: Vec<String> = t.name.split('.').map(str::to_string).collect();
        add(&scope, &t.items);
    }
    out
}

/// The whole property for one text and one edit. `strict` also demands that every plain key is editable, which only holds for the
/// documents of the generator (the fragment soup may define a plain-looking key with dotted keys); `inline` lists the paths of the
/// generated inline tables.
fn check(
    text: &str,
    edit: &Edit,
    strict: bool,
    inline: &[Vec<String>],
) -> Result<(), TestCaseError> {
    // A leading byte order mark parses in the `toml` crate and a NUL does not; the patcher refuses both as "not TOML".
    let refused_by_rule = text.starts_with('\u{feff}') || text.contains('\0');
    let old_tree = match toml::from_str::<toml::Table>(text) {
        Ok(t) if !refused_by_rule => t,
        _ => {
            // Not TOML: the patcher says so with a position and writes nothing.
            match apply(text, std::slice::from_ref(edit), &rank) {
                Err(Refusal::NotToml { line, column }) => {
                    prop_assert!(line >= 1 && column >= 1, "{line}:{column}");
                }
                other => prop_assert!(false, "not TOML but {other:?}: {text:?}"),
            }
            return Ok(());
        }
    };
    let path = edit_path(edit);
    let result = apply(text, std::slice::from_ref(edit), &rank);
    let wanted = model(&old_tree, edit);
    // Dotted keys and the array of tables are generated under the names `d`, `f` and `aot`; an inline table is known from the spec.
    let touches_unsupported = path
        .iter()
        .any(|seg| matches!(seg.as_str(), "d" | "f" | "aot"))
        || inline.iter().any(|p| path.starts_with(p));
    let leaf_exists = {
        let mut table = &old_tree;
        let mut found = false;
        for (i, seg) in path.iter().enumerate() {
            match table.get(seg) {
                Some(toml::Value::Table(inner)) if i + 1 < path.len() => table = inner,
                Some(v) if i + 1 == path.len() => found = !v.is_table(),
                _ => break,
            }
        }
        found
    };

    match result {
        Err(Refusal::NotProvable) => prop_assert!(false, "not provable: {edit:?} on {text:?}"),
        Err(Refusal::NotToml { .. }) => {
            prop_assert!(false, "valid TOML refused as not TOML: {text:?}")
        }
        Err(Refusal::Unsupported { line, reason }) => {
            prop_assert!(line <= text.lines().count().max(1), "{line} {reason}");
            // A plain key is always editable: refusing it would be a missing feature.
            let plain_set = strict
                && matches!(edit, Edit::Set { .. })
                && wanted.is_some()
                && !touches_unsupported;
            let plain_clear =
                strict && matches!(edit, Edit::Clear { .. }) && leaf_exists && !touches_unsupported;
            prop_assert!(
                !(plain_set || plain_clear),
                "refused a supported edit ({reason}, line {line}): {edit:?} on {text:?}"
            );
        }
        Ok(Outcome::Unchanged) => {
            let want = wanted.expect("an edit that changes nothing is one that can be made");
            prop_assert_eq!(
                flat(&want, path),
                flat(&old_tree, path),
                "{:?} on {:?}",
                edit,
                text
            );
        }
        Ok(Outcome::Changed(new)) => {
            prop_assert_ne!(new.as_str(), text);
            let want = wanted.expect("a change that cannot be made must be refused");
            let new_tree: toml::Table = toml::from_str(&new)
                .map_err(|e| TestCaseError::fail(format!("result is not TOML: {e}: {new:?}")))?;
            prop_assert_eq!(
                flat(&want, path),
                flat(&new_tree, path),
                "{:?} on {:?} gave {:?}",
                edit,
                text,
                new
            );

            // The lines that changed are one block, and its shape fits the edit. A missing final line break is added to both texts first,
            // so that it does not count as a changed line.
            let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
            let whole = |t: &str| {
                if t.is_empty() || t.ends_with('\n') {
                    t.to_string()
                } else {
                    format!("{t}{eol}")
                }
            };
            let (old_whole, new_whole) = (whole(text), whole(&new));
            let (_, _, gone, came) = line_diff(&old_whole, &new_whole);
            match edit {
                Edit::Set { .. } if leaf_exists => {
                    prop_assert_eq!(
                        came,
                        1,
                        "a replaced value is one line: {:?} -> {:?}",
                        text,
                        new
                    );
                    prop_assert!(gone >= 1);
                }
                Edit::Set { .. } => {
                    prop_assert_eq!(
                        gone,
                        0,
                        "an added key only adds lines: {:?} -> {:?}",
                        text,
                        new
                    );
                    prop_assert!((1..=4).contains(&came), "{came} lines added");
                }
                Edit::Clear { .. } => {
                    prop_assert_eq!(
                        came,
                        0,
                        "a removed key only removes lines: {:?} -> {:?}",
                        text,
                        new
                    );
                    prop_assert!(gone >= 1);
                }
            }

            // A fixed point.
            prop_assert_eq!(
                apply(&new, std::slice::from_ref(edit), &rank),
                Ok(Outcome::Unchanged),
                "{:?} again on {:?}",
                edit,
                new
            );
        }
    }
    Ok(())
}

/// Cases per property: 1,500, or the number in `HAMH_PROP_CASES` for a longer run.
fn cases() -> u32 {
    std::env::var("HAMH_PROP_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500)
}

/// Arbitrary printable text (no control characters).
fn any_text() -> impl Strategy<Value = String> {
    prop::string::string_regex("\\PC{0,120}").unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(), max_global_rejects: 100_000, ..ProptestConfig::default() })]

    #[test]
    fn proptest_any_registry_set_or_clear_changes_exactly_one_key_or_refuses(
        spec in spec_strategy(),
        edit in edit_strategy(),
        pick in prop::option::weighted(0.7, 0usize..64),
    ) {
        let text = render(&spec);
        let Ok(tree) = toml::from_str::<toml::Table>(&text) else {
            return Err(TestCaseError::reject("the generated document is not TOML"));
        };
        check(&text, &aimed(edit, &tree, pick), true, &inline_paths(&spec))?;
    }

    /// A new key, then its removal, gives the original bytes back when the file ends in a line break (a file without one gets one) and
    /// the key did not need the blank line that separates a first top-level key from the tables below it.
    #[test]
    fn proptest_adding_a_new_key_and_removing_it_restores_the_bytes(
        spec in spec_strategy(),
        which in 0usize..8,
        fresh_table in any::<bool>(),
    ) {
        let spec = Spec { final_newline: true, ..spec };
        let mut text = render(&spec);
        // A blank line that ends the file is the separator of a new table and goes with it; end the file with a comment line instead.
        if text.lines().last().is_some_and(|l| l.trim().is_empty()) {
            text.push_str(if spec.crlf { "# end\r\n" } else { "# end\n" });
        }
        prop_assume!(toml::from_str::<toml::Table>(&text).is_ok());
        // A first top-level key goes above the tables and brings its own blank line, which the removal leaves; a new table does not.
        let top_only = render(&Spec { tables: vec![], aot: 0, ..spec.clone() });
        let has_top_assignment = top_only
            .lines()
            .any(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'));
        let has_headers = !spec.tables.is_empty() || spec.aot > 0;
        let path: Vec<String> = if fresh_table || (!has_top_assignment && has_headers) {
            vec![format!("fresh{which}"), "k".to_string()]
        } else {
            vec![format!("newkey{which}")]
        };
        let set = Edit::Set { path: path.clone(), value: EditValue::Int(7) };
        let clear = Edit::Clear { path };
        let Ok(Outcome::Changed(with)) = apply(&text, std::slice::from_ref(&set), &rank) else {
            return Err(TestCaseError::fail(format!("a new key must be added to {text:?}")));
        };
        match apply(&with, std::slice::from_ref(&clear), &rank) {
            Ok(Outcome::Changed(back)) => prop_assert_eq!(back, text),
            Ok(Outcome::Unchanged) => prop_assert_eq!(with, text),
            other => prop_assert!(false, "{other:?}"),
        }
    }

    /// Whatever text arrives, the patcher returns a refusal or a proven change and never panics.
    #[test]
    fn proptest_fragment_soup_never_panics_and_obeys_the_same_rules(
        lines in prop::collection::vec(prop::sample::select(SOUP), 0..10),
        crlf in any::<bool>(),
        edit in edit_strategy(),
    ) {
        let eol = if crlf { "\r\n" } else { "\n" };
        let text = lines.join(eol);
        check(&text, &edit, false, &[])?;
    }

    #[test]
    fn proptest_arbitrary_text_never_panics(text in any_text(), edit in edit_strategy()) {
        // The annotation fixes the closure's argument type before `&text` is used as a `&str`.
        let text: String = text;
        check(&text, &edit, false, &[])?;
    }
}

/// Fragments that are valid, nearly valid and wrong, in any order.
const SOUP: &[&str] = &[
    "a = 1",
    "a = 2",
    "b = \"x\"",
    "[run]",
    "[run.sub]",
    "[[aot]]",
    "[ui]",
    "run.goal = true",
    "d.x = 1",
    "d.x.y = 1",
    "k = [",
    "  \"a\",",
    "]",
    "s = \"\"\"",
    "text # not a comment",
    "\"\"\"",
    "# comment",
    "",
    "   ",
    "inl = { a = 1, b = { c = 2 } }",
    "\"quoted key\" = true",
    "'lit' = 'x'",
    "[ run ]",
    "= 1",
    "a = ",
    "[",
    "[[",
    "x = 1 y = 2",
    "t = 1979-05-27T07:32:00Z",
    "f = inf",
    "n = nan",
    "big = 99999999999999999999",
    "low = -9223372036854775809",
    "huge = 1e999",
    "z = 0o17",
    "\u{feff}a = 1",
    "u = \"\u{e9}\u{4e16}\"",
];

#[test]
fn the_generator_makes_valid_documents_most_of_the_time() {
    // A sanity check of the model itself: a fixed spec renders to the text its fields describe.
    let spec = Spec {
        crlf: true,
        final_newline: true,
        top: vec![
            Item::Comment("# c"),
            Item::Kv("a", "1", Some("# tail")),
            Item::Dotted("d", "x"),
        ],
        tables: vec![TableSpec {
            name: "run",
            above: vec![Item::Blank],
            header_tail: None,
            items: vec![Item::Kv("goal", "true", None)],
        }],
        aot: 1,
    };
    assert_eq!(
        render(&spec),
        "# c\r\na = 1 # tail\r\nd.x = 1\r\n\r\n[run]\r\ngoal = true\r\n[[aot]]\r\nz = 0\r\n"
    );
    assert!(toml::from_str::<toml::Table>(&render(&spec)).is_ok());
}

/// How an `apply` call ended, in words that tell the kinds of change apart.
fn outcome_kind(text: &str, edit: &Edit) -> &'static str {
    let headers = |t: &str| {
        t.lines()
            .filter(|l| l.trim_start().starts_with('['))
            .count()
    };
    match apply(text, std::slice::from_ref(edit), &rank) {
        Err(Refusal::Unsupported { .. }) => "refused as unsupported",
        Err(_) => "refused as not TOML or not provable",
        Ok(Outcome::Unchanged) => "unchanged",
        Ok(Outcome::Changed(new)) => match edit {
            Edit::Set { .. } if headers(&new) > headers(text) => "added a table",
            Edit::Set { .. }
                if new.len() >= text.len() && new.lines().count() > text.lines().count() =>
            {
                "added a key"
            }
            Edit::Set { .. } => "replaced a value",
            Edit::Clear { .. } if headers(&new) < headers(text) => "removed a table with its key",
            Edit::Clear { .. } => "removed a key",
        },
    }
}

/// The generators reach every kind of outcome, so a property that passes is not passing on a single easy shape. Fixed seed; counts are
/// of 4,000 generated cases.
#[test]
fn the_generated_cases_reach_every_kind_of_outcome() {
    let mut runner = TestRunner::deterministic();
    let strategy = (
        spec_strategy(),
        edit_strategy(),
        prop::option::weighted(0.7, 0usize..64),
    );
    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..4000 {
        let (spec, edit, pick) = strategy.new_tree(&mut runner).unwrap().current();
        let text = render(&spec);
        let Ok(tree) = toml::from_str::<toml::Table>(&text) else {
            continue;
        };
        let edit = aimed(edit, &tree, pick);
        *tally.entry(outcome_kind(&text, &edit)).or_default() += 1;
    }
    for kind in [
        "unchanged",
        "replaced a value",
        "added a key",
        "added a table",
        "removed a key",
        "removed a table with its key",
        "refused as unsupported",
    ] {
        assert!(
            tally.get(kind).copied().unwrap_or(0) >= 30,
            "kind {kind:?} is too rare: {tally:?}"
        );
    }
    assert!(
        !tally.contains_key("refused as not TOML or not provable"),
        "{tally:?}"
    );
}
