//! The comment-keeping patcher: it changes one key of a TOML file by bytes and leaves every
//! other byte alone, or it refuses and the caller's text is untouched. The registry supplies the rank of a new top-level key; the patcher
//! knows no key names. The tests use a rank list that follows the client keys of the design, nothing more.
//!
//! Files in the tests are written with `\n`; the CRLF variants are made with `crlf`.
#![allow(clippy::unwrap_used)]

use ham_core::config_edit::{
    Edit, EditValue, Outcome, Refusal, apply, apply_with_shifted_value_span, prove_pair,
};

/// The client keys in registry order, then the table keys; an unknown name ranks last.
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

fn path(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|p| (*p).to_string()).collect()
}

fn set(parts: &[&str], value: EditValue) -> Edit {
    Edit::Set {
        path: path(parts),
        value,
    }
}

fn clear(parts: &[&str]) -> Edit {
    Edit::Clear { path: path(parts) }
}

fn text(v: &str) -> EditValue {
    EditValue::Str(v.to_string())
}

fn crlf(s: &str) -> String {
    s.replace('\n', "\r\n")
}

/// The new text of an edit that must change the file.
fn changed(old: &str, edits: &[Edit]) -> String {
    match apply(old, edits, &rank) {
        Ok(Outcome::Changed(new)) => new,
        other => panic!("expected a change, got {other:?} for {old:?}"),
    }
}

fn refusal(old: &str, edits: &[Edit]) -> Refusal {
    match apply(old, edits, &rank) {
        Err(r) => r,
        other => panic!("expected a refusal, got {other:?} for {old:?}"),
    }
}

fn tree(s: &str) -> toml::Table {
    toml::from_str(s).unwrap()
}

/// `old` with the text `old_value`, which follows `key_prefix` (`mouse = `), replaced by `new_value`.
fn replaced(source: &str, key_prefix: &str, old_value: &str, new_value: &str) -> String {
    let at = source.find(&format!("{key_prefix}{old_value}")).unwrap() + key_prefix.len();
    format!(
        "{}{}{}",
        &source[..at],
        new_value,
        &source[at + old_value.len()..]
    )
}

const GOLDEN: &str = "\
# ham config, edited by hand
apiEndpoint = \"https://abc.execute-api.us-east-1.amazonaws.com/direct/v1\" # my stage
region = \"us-east-1\"

# the model I use
defaultModel = \"global.anthropic.claude-sonnet-5-5\"   # keep this note
inputWaitMinutes = 30
mouse = true

[run]
goal = true  # trailing
tags = [
  \"a\", # first
  \"b\",
]

# last words
";

#[test]
fn set_changes_exactly_one_value_span_and_keeps_every_other_byte() {
    for style in [str::to_string as fn(&str) -> String, crlf_style] {
        let golden = style(GOLDEN);
        let cases: Vec<(Edit, String)> = vec![
            (
                set(&["defaultModel"], text("global.openai.gpt-6-1-sol")),
                replaced(
                    &golden,
                    "defaultModel = ",
                    "\"global.anthropic.claude-sonnet-5-5\"",
                    "\"global.openai.gpt-6-1-sol\"",
                ),
            ),
            (
                set(&["inputWaitMinutes"], EditValue::Int(45)),
                replaced(&golden, "inputWaitMinutes = ", "30", "45"),
            ),
            (
                set(&["mouse"], EditValue::Bool(false)),
                replaced(&golden, "mouse = ", "true", "false"),
            ),
            (
                set(&["run", "goal"], EditValue::Bool(false)),
                replaced(&golden, "goal = ", "true", "false"),
            ),
            (
                set(&["run", "tags"], EditValue::StrList(vec!["c".to_string()])),
                replaced(
                    &golden,
                    "tags = ",
                    &style("[\n  \"a\", # first\n  \"b\",\n]"),
                    "[\"c\"]",
                ),
            ),
            (
                set(&["region"], text("eu-west-1")),
                replaced(&golden, "region = ", "\"us-east-1\"", "\"eu-west-1\""),
            ),
        ];
        for (edit, expected) in cases {
            let got = changed(&golden, std::slice::from_ref(&edit));
            // `expected` is the golden with one value span replaced, so equality is the claim: every other byte is the golden's own.
            assert_eq!(got, expected, "{edit:?}");
        }
    }
}

fn crlf_style(s: &str) -> String {
    crlf(s)
}

#[test]
fn a_noop_set_returns_unchanged() {
    let same: Vec<(&str, Edit)> = vec![
        ("a = \"x\"\n", set(&["a"], text("x"))),
        // A literal string and a basic string with the same value are the same value.
        ("a = 'x'\n", set(&["a"], text("x"))),
        ("f = 0.90\n", set(&["f"], EditValue::Float(0.9))),
        ("n = 0x10\n", set(&["n"], EditValue::Int(16))),
        (
            "t = [\n  \"a\",\n  \"b\",\n]\n",
            set(
                &["t"],
                EditValue::StrList(vec!["a".to_string(), "b".to_string()]),
            ),
        ),
        (
            "[run]\ngoal = true # c\n",
            set(&["run", "goal"], EditValue::Bool(true)),
        ),
        // Clearing a key that is not there changes nothing.
        ("a = 1\n", clear(&["b"])),
        ("a = 1\n", clear(&["run", "goal"])),
        ("", clear(&["a"])),
    ];
    for (old, edit) in same {
        assert_eq!(
            apply(old, std::slice::from_ref(&edit), &rank),
            Ok(Outcome::Unchanged),
            "{old:?} {edit:?}"
        );
    }
    assert_eq!(apply("a = 1\n", &[], &rank), Ok(Outcome::Unchanged));
    // Two edits that cancel give the original bytes back, which is also unchanged.
    assert_eq!(
        apply(
            "a = 1\n",
            &[set(&["b"], EditValue::Int(2)), clear(&["b"])],
            &rank
        ),
        Ok(Outcome::Unchanged)
    );
}

#[test]
fn a_new_top_key_lands_in_rank_order_before_the_first_table() {
    // A fresh file gets exactly the line the struct writer wrote before.
    assert_eq!(
        changed("", &[set(&["defaultModel"], text("global.x.y"))]),
        "defaultModel = \"global.x.y\"\n"
    );
    // The three connection keys land in struct order, whatever the order of the edits.
    let want =
        "apiEndpoint = \"https://h/direct/v1\"\nregion = \"us-east-1\"\nawsProfile = \"p\"\n";
    let api = set(&["apiEndpoint"], text("https://h/direct/v1"));
    let region = set(&["region"], text("us-east-1"));
    let profile = set(&["awsProfile"], text("p"));
    assert_eq!(
        changed("", &[api.clone(), region.clone(), profile.clone()]),
        want
    );
    assert_eq!(changed("", &[profile, region, api]), want);

    // Before the first existing key that follows it, else after the last top-level assignment.
    assert_eq!(
        changed(
            "apiEndpoint = \"a\"\nmouse = true\n\n[run]\ngoal = true\n",
            &[set(&["region"], text("r"))]
        ),
        "apiEndpoint = \"a\"\nregion = \"r\"\nmouse = true\n\n[run]\ngoal = true\n"
    );
    assert_eq!(
        changed(
            "apiEndpoint = \"a\"\nmouse = true\n\n[run]\ngoal = true\n",
            &[set(&["configVersion"], EditValue::Int(1))]
        ),
        "apiEndpoint = \"a\"\nmouse = true\nconfigVersion = 1\n\n[run]\ngoal = true\n"
    );
    // A comment directly above the key it follows stays with that key.
    assert_eq!(
        changed(
            "apiEndpoint = \"a\"\n# the mouse\nmouse = true\n",
            &[set(&["region"], text("r"))]
        ),
        "apiEndpoint = \"a\"\nregion = \"r\"\n# the mouse\nmouse = true\n"
    );
    // A blank line between a comment and the key keeps the comment where it is.
    assert_eq!(
        changed(
            "apiEndpoint = \"a\"\n# about the file\n\nmouse = true\n",
            &[set(&["region"], text("r"))]
        ),
        "apiEndpoint = \"a\"\n# about the file\n\nregion = \"r\"\nmouse = true\n"
    );
    // With no top-level key at all, the key goes above the first table and gets a blank line after it.
    assert_eq!(
        changed(
            "[run]\ngoal = true\n",
            &[set(&["configVersion"], EditValue::Int(1))]
        ),
        "configVersion = 1\n\n[run]\ngoal = true\n"
    );
    // With only comments, it goes below them.
    assert_eq!(
        changed("# nothing yet\n", &[set(&["region"], text("r"))]),
        "# nothing yet\nregion = \"r\"\n"
    );
    // A dotted key at the top counts as a top-level assignment of its first name. A name the rank does not know never follows a new key.
    assert_eq!(
        changed("d.e = 1\n", &[set(&["region"], text("r"))]),
        "d.e = 1\nregion = \"r\"\n"
    );
}

const OWNER_FILE: &str = "\
# my ham settings
# edited on 2026-10-01
apiEndpoint = \"https://abc.example/direct/v1\"
region = \"us-east-1\"
awsProfile = \"ham-acct-a01\"
";

#[test]
fn a_new_table_is_appended_at_the_end_after_one_blank_line_and_unset_restores_the_bytes() {
    for style in [str::to_string as fn(&str) -> String, crlf_style] {
        let eol = style("\n");
        let original = style(OWNER_FILE);
        let add = [
            set(&["configVersion"], EditValue::Int(1)),
            set(&["semantic_search", "enabled"], EditValue::Bool(false)),
        ];
        let gone = [
            clear(&["configVersion"]),
            clear(&["semantic_search", "enabled"]),
        ];
        let with = changed(&original, &add);
        let expected = format!(
            "{original}configVersion = 1{eol}{eol}[semantic_search]{eol}enabled = false{eol}"
        );
        assert_eq!(with, expected);
        // Set then clear gives the original bytes.
        assert_eq!(changed(&with, &gone), original);
        // The order of the two edits does not matter.
        let reversed = [add[1].clone(), add[0].clone()];
        assert_eq!(changed(&original, &reversed), expected);
        // Clearing the table key first and the version second also restores the bytes.
        assert_eq!(
            changed(&with, &[gone[1].clone(), gone[0].clone()]),
            original
        );

        // A second table goes after the first, with one blank line between.
        let two = changed(&with, &[set(&["run", "goal"], EditValue::Bool(false))]);
        assert_eq!(two, format!("{expected}{eol}[run]{eol}goal = false{eol}"));
        assert_eq!(changed(&two, &[clear(&["run", "goal"])]), with);
    }

    // A file that already has a table gets the new one after it.
    let with_table = "x = 1\n\n[run]\ngoal = true\n";
    let both = changed(with_table, &[set(&["ui", "bell"], EditValue::Bool(false))]);
    assert_eq!(both, "x = 1\n\n[run]\ngoal = true\n\n[ui]\nbell = false\n");
    assert_eq!(changed(&both, &[clear(&["ui", "bell"])]), with_table);

    // A nested path makes one header with a dotted name, and a name with a dot in it is quoted.
    assert_eq!(
        changed("a = 1\n", &[set(&["t", "u", "k"], EditValue::Int(2))]),
        "a = 1\n\n[t.u]\nk = 2\n"
    );
    let quoted = changed("", &[set(&["a.b"], EditValue::Int(2))]);
    assert_eq!(quoted, "\"a.b\" = 2\n");
    assert_eq!(tree(&quoted).len(), 1);
}

#[test]
fn a_new_key_in_an_existing_table_goes_after_the_last_key_of_that_table() {
    assert_eq!(
        changed(
            "[run]\ngoal = true\nrules = false # c\n\n[research]\nenabled = true\n",
            &[set(&["run", "skills"], EditValue::Bool(false))]
        ),
        "[run]\ngoal = true\nrules = false # c\nskills = false\n\n[research]\nenabled = true\n"
    );
    // The last key may end on a later line than it starts.
    assert_eq!(
        changed(
            "[run]\ntags = [\n  \"a\",\n]\n\n[other]\n",
            &[set(&["run", "x"], EditValue::Int(1))]
        ),
        "[run]\ntags = [\n  \"a\",\n]\nx = 1\n\n[other]\n"
    );
    // A dotted key counts as an entry of the table.
    assert_eq!(
        changed(
            "[run]\nd.e = 1\n\n[other]\n",
            &[set(&["run", "x"], EditValue::Int(1))]
        ),
        "[run]\nd.e = 1\nx = 1\n\n[other]\n"
    );
    // An empty table gets the key straight after its header line, comment and all.
    assert_eq!(
        changed(
            "[run] # about run\n\n[x]\n",
            &[set(&["run", "goal"], EditValue::Bool(true))]
        ),
        "[run] # about run\ngoal = true\n\n[x]\n"
    );
    // The last table of a file without a final newline.
    assert_eq!(
        changed(
            "[run]\ngoal = true",
            &[set(&["run", "rules"], EditValue::Bool(true))]
        ),
        "[run]\ngoal = true\nrules = true\n"
    );
    // A table named only through a sub-table gets its own header at the end.
    assert_eq!(
        changed("[a.b]\nx = 1\n", &[set(&["a", "c"], EditValue::Int(2))]),
        "[a.b]\nx = 1\n\n[a]\nc = 2\n"
    );
    // A key of a sub-table of a table goes into the sub-table.
    assert_eq!(
        changed(
            "[a]\nx = 1\n[a.b]\ny = 2\n",
            &[set(&["a", "b", "z"], EditValue::Int(3))]
        ),
        "[a]\nx = 1\n[a.b]\ny = 2\nz = 3\n"
    );
}

#[test]
fn unset_removes_only_its_line_and_its_trailing_comment_and_keeps_comment_lines() {
    assert_eq!(
        changed("a = 1 # note\n# about b\nb = 2\n", &[clear(&["b"])]),
        "a = 1 # note\n# about b\n"
    );
    assert_eq!(
        changed("a = 1 # note\n# about b\nb = 2\n", &[clear(&["a"])]),
        "# about b\nb = 2\n"
    );
    // A multi-line value goes whole, with the comment after its closing bracket.
    assert_eq!(
        changed("a = [\n  1,\n  2,\n] # trailing\nb = 3\n", &[clear(&["a"])]),
        "b = 3\n"
    );
    // The last line of a file without a final newline.
    assert_eq!(changed("a = 1\nb = 2", &[clear(&["b"])]), "a = 1\n");
    assert_eq!(changed("a = 1\r\nb = 2\r\n", &[clear(&["a"])]), "b = 2\r\n");

    // A table left with no key and no comment goes with its header and the blank line before it.
    assert_eq!(
        changed(
            "x = 1\n\n[run]\ngoal = false\n\n[other]\ny = 2\n",
            &[clear(&["run", "goal"])]
        ),
        "x = 1\n\n[other]\ny = 2\n"
    );
    // With no blank line above the header, the first blank line after the table goes instead (at the top of the file and elsewhere).
    assert_eq!(
        changed(
            "[run]\ngoal = false\n\n[other]\ny = 1\n",
            &[clear(&["run", "goal"])]
        ),
        "[other]\ny = 1\n"
    );
    assert_eq!(
        changed(
            "k = 1\n[run]\ngoal = false\n\n[other]\ny = 1\n",
            &[clear(&["run", "goal"])]
        ),
        "k = 1\n[other]\ny = 1\n"
    );
    assert_eq!(
        changed(
            "[run]\ngoal = false\n\n# about other\n[other]\ny = 1\n",
            &[clear(&["run", "goal"])]
        ),
        "# about other\n[other]\ny = 1\n"
    );
    assert_eq!(
        changed("k = 1\n\n[run]\ngoal = false", &[clear(&["run", "goal"])]),
        "k = 1\n"
    );
    // A table that still has a key, a comment in its body, a comment on its header line or a comment above it stays.
    assert_eq!(
        changed(
            "[run]\ngoal = false\nrules = true\n",
            &[clear(&["run", "goal"])]
        ),
        "[run]\nrules = true\n"
    );
    assert_eq!(
        changed("[run]\n# why\ngoal = false\n", &[clear(&["run", "goal"])]),
        "[run]\n# why\n"
    );
    assert_eq!(
        changed(
            "[run]\ngoal = false\n# hint\n\n[o]\n",
            &[clear(&["run", "goal"])]
        ),
        "[run]\n# hint\n\n[o]\n"
    );
    assert_eq!(
        changed("[run] # about\ngoal = false\n", &[clear(&["run", "goal"])]),
        "[run] # about\n"
    );
    assert_eq!(
        changed(
            "# about run\n[run]\ngoal = false\n",
            &[clear(&["run", "goal"])]
        ),
        "# about run\n[run]\n"
    );
    // Both spellings of the same table empty in one call.
    assert_eq!(
        changed(
            "a = 1\n\n[run]\ngoal = false\nrules = true\n",
            &[clear(&["run", "goal"]), clear(&["run", "rules"])]
        ),
        "a = 1\n"
    );
}

#[test]
fn a_snake_spelling_is_updated_in_place_and_no_second_spelling_is_created() {
    // The patcher writes the name the caller gives it and never rewrites a key it finds; a hand-written snake spelling keeps its line.
    let old = "# model\ndefault_model = \"x\" # mine\nregion = \"r\"\n";
    let got = changed(old, &[set(&["default_model"], text("global.new.model"))]);
    assert_eq!(
        got,
        "# model\ndefault_model = \"global.new.model\" # mine\nregion = \"r\"\n"
    );
    assert!(!got.contains("defaultModel"));
    assert_eq!(got.lines().count(), old.lines().count());
    // Clearing the snake spelling removes that line and nothing else.
    assert_eq!(
        changed(old, &[clear(&["default_model"])]),
        "# model\nregion = \"r\"\n"
    );
}

#[test]
fn a_multi_line_array_value_is_replaced_whole() {
    let old = "tags = [\n  \"a\", # x\n  \"b\",\n]\nother = 1\n";
    assert_eq!(
        changed(
            old,
            &[set(&["tags"], EditValue::StrList(vec!["c".to_string()]))]
        ),
        "tags = [\"c\"]\nother = 1\n"
    );
    assert_eq!(
        changed(old, &[set(&["tags"], EditValue::StrList(vec![]))]),
        "tags = []\nother = 1\n"
    );
    assert_eq!(
        changed(old, &[set(&["tags"], text("one"))]),
        "tags = \"one\"\nother = 1\n"
    );
    // A multi-line string is one value too, and a hash inside it is not a comment.
    let ml = "s = \"\"\"\nfirst\n# second\"\"\" # tail\nz = 1\n";
    assert_eq!(
        changed(ml, &[set(&["s"], EditValue::Int(7))]),
        "s = 7 # tail\nz = 1\n"
    );
    assert_eq!(changed(ml, &[clear(&["s"])]), "z = 1\n");
    // The same holds in a file with CRLF line endings.
    assert_eq!(
        changed(
            &crlf(old),
            &[set(
                &["tags"],
                EditValue::StrList(vec!["c".to_string(), "d".to_string()])
            )]
        ),
        crlf("tags = [\"c\", \"d\"]\nother = 1\n")
    );
}

#[test]
fn a_dotted_key_an_inline_table_and_an_array_of_tables_are_unsupported_with_their_line() {
    let unsupported = |old: &str, edit: Edit, line: usize| {
        let got = refusal(old, std::slice::from_ref(&edit));
        match got {
            Refusal::Unsupported { line: l, .. } => {
                assert_eq!(l, line, "{old:?} {edit:?}");
            }
            other => panic!("expected Unsupported for {old:?} {edit:?}, got {other:?}"),
        }
    };
    let dotted = "a = 1\nd.e = 2\n";
    unsupported(dotted, set(&["d", "e"], EditValue::Int(3)), 2);
    unsupported(dotted, clear(&["d", "e"]), 2);
    unsupported(dotted, set(&["d"], EditValue::Int(3)), 2);
    unsupported(dotted, set(&["d", "f"], EditValue::Int(3)), 2);
    let in_table = "[t]\n\nx.y = 1\n";
    unsupported(in_table, set(&["t", "x", "z"], EditValue::Int(2)), 3);
    unsupported(in_table, set(&["t", "x"], EditValue::Int(2)), 3);

    let inline = "x = 1\ninl = { c = 3 }\n";
    unsupported(inline, set(&["inl"], EditValue::Int(1)), 2);
    unsupported(inline, clear(&["inl"]), 2);
    unsupported(inline, set(&["inl", "c"], EditValue::Int(1)), 2);
    unsupported(inline, set(&["inl", "d"], EditValue::Int(1)), 2);

    let aot = "[[aot]]\nz = 1\n\n[[aot]]\nz = 2\n";
    unsupported(aot, set(&["aot", "z"], EditValue::Int(5)), 1);
    unsupported(aot, set(&["aot", "y"], EditValue::Int(5)), 1);
    unsupported(aot, clear(&["aot", "z"]), 1);
    unsupported(aot, set(&["aot"], EditValue::Int(5)), 1);

    // A value where a table is needed, and a table where a value is meant.
    unsupported("a = 1\n", set(&["a", "b"], EditValue::Int(2)), 1);
    unsupported("[t]\nk = 1\n", set(&["t"], EditValue::Int(2)), 1);
    unsupported("[t.u]\nk = 1\n", set(&["t"], EditValue::Int(2)), 1);
    unsupported("[t]\nk = 1\n", clear(&["t"]), 1);

    // The refusal changes nothing: the same text with the same edit refuses the same way again.
    let first = apply(dotted, &[set(&["d", "e"], EditValue::Int(3))], &rank);
    let second = apply(dotted, &[set(&["d", "e"], EditValue::Int(3))], &rank);
    assert_eq!(first, second);
    assert!(matches!(first, Err(Refusal::Unsupported { line: 2, .. })));

    // Those constructs elsewhere in the file do not stop an edit of a plain key.
    let mixed =
        "d.e = 1\ninl = { c = 1 }\nregion = \"r\"\n\n[t]\nx.y = 1\nk = 2\n\n[[aot]]\nz = 1\n";
    let got = changed(
        mixed,
        &[
            set(&["region"], text("s")),
            set(&["t", "k"], EditValue::Int(3)),
            set(&["mouse"], EditValue::Bool(true)),
        ],
    );
    assert_eq!(
        got,
        "d.e = 1\ninl = { c = 1 }\nregion = \"s\"\nmouse = true\n\n[t]\nx.y = 1\nk = 3\n\n[[aot]]\nz = 1\n"
    );
}

/// The `setup.sh` reader: `sed -n "s/^KEY[[:space:]]*=[[:space:]]*\"\(.*\)\"[[:space:]]*$/\1/p"`.
fn sed_reads(file: &str, key: &str) -> Option<String> {
    file.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?;
        let rest = rest.trim_start_matches([' ', '\t']).strip_prefix('=')?;
        let rest = rest.trim_start_matches([' ', '\t']).strip_prefix('"')?;
        let value = rest.trim_end_matches([' ', '\t']).strip_suffix('"')?;
        Some(value.to_string())
    })
}

fn driver_port(file: &str) -> Option<u16> {
    let at = file.find("apiEndpoint")? + "apiEndpoint".len();
    let rest = file[at..].trim_start().strip_prefix('=')?;
    let rest = rest.trim_start().strip_prefix('"')?;
    let rest = rest
        .strip_prefix("https://")
        .or_else(|| rest.strip_prefix("http://"))?;
    let host_end = rest.find(['"', ':', '/'])?;
    let rest = rest[host_end..].strip_prefix(':')?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[test]
fn strings_are_always_basic_double_quoted_with_escapes_and_floats_keep_a_fraction() {
    let awkward = [
        "",
        "plain",
        "a#b",
        "say \"hi\"",
        "back\\slash",
        "tab\there",
        "line\nbreak",
        "bell\u{7}\u{1}\u{1b}[31m",
        "del\u{7f}",
        "c1\u{9b}31m",
        "h\u{e9}llo \u{2603} \u{65e5}\u{672c}",
        "[inject]\nx = 1",
        "ends with backslash\\",
        "\u{202e}rtl\u{200b}",
    ];
    for value in awkward {
        let got = changed("", &[set(&["k"], text(value))]);
        assert!(got.starts_with("k = \""), "{got:?}");
        assert!(got.ends_with("\"\n"), "{got:?}");
        assert_eq!(got.matches('\n').count(), 1, "one line for {value:?}");
        assert!(
            !got.chars().any(|c| c.is_control() && c != '\n'),
            "no raw control character in {got:?}"
        );
        assert_eq!(tree(&got)["k"].as_str(), Some(value));
    }
    // Floats and integers render so that they read back as the same number, and a float always has a fraction or an exponent.
    for (value, line) in [
        (0.9, "k = 0.9\n"),
        (1.0, "k = 1.0\n"),
        (-2.5, "k = -2.5\n"),
        (0.0, "k = 0.0\n"),
    ] {
        assert_eq!(changed("", &[set(&["k"], EditValue::Float(value))]), line);
    }
    for value in [1e21, 1.5e-7, f64::MAX, f64::MIN_POSITIVE, 123456.789, -0.0] {
        let got = changed("", &[set(&["k"], EditValue::Float(value))]);
        let read = tree(&got)["k"].as_float().unwrap();
        assert_eq!(read.to_bits(), value.to_bits(), "{got:?}");
        let body = got.trim_start_matches("k = ").trim_end();
        assert!(body.contains(['.', 'e', 'E']), "{got:?}");
    }
    for value in [0, -1, i64::MAX, i64::MIN] {
        let got = changed("", &[set(&["k"], EditValue::Int(value))]);
        assert_eq!(tree(&got)["k"].as_integer(), Some(value));
    }
    // The shell reader of setup.sh and the Python driver regex both read a legacy line the patcher wrote, new or replaced.
    let fresh = changed(
        "",
        &[
            set(&["apiEndpoint"], text("http://127.0.0.1:8080/direct/v1")),
            set(&["region"], text("us-east-1")),
        ],
    );
    assert_eq!(
        sed_reads(&fresh, "apiEndpoint").as_deref(),
        Some("http://127.0.0.1:8080/direct/v1")
    );
    assert_eq!(sed_reads(&fresh, "region").as_deref(), Some("us-east-1"));
    assert_eq!(driver_port(&fresh), Some(8080));
    let edited = changed(
        &fresh,
        &[set(&["apiEndpoint"], text("https://10.0.0.5:9443/x"))],
    );
    assert_eq!(
        sed_reads(&edited, "apiEndpoint").as_deref(),
        Some("https://10.0.0.5:9443/x")
    );
    assert_eq!(driver_port(&edited), Some(9443));
    // A commented file the patcher touched is still read the same way.
    let commented = changed(
        "# stage\napiEndpoint = \"http://localhost:7000/d\" # mine\nregion = \"r\"\n",
        &[set(&["region"], text("us-west-2"))],
    );
    assert_eq!(
        sed_reads(&commented, "region").as_deref(),
        Some("us-west-2")
    );
    assert_eq!(driver_port(&commented), Some(7000));
}

#[test]
fn a_file_without_a_final_newline_gets_its_newline_before_the_appended_text() {
    for style in [str::to_string as fn(&str) -> String, crlf_style] {
        let eol = style("\n");
        let two_lines = style("x = 0\na = 1");
        // A new top-level key after the last key.
        assert_eq!(
            changed(&two_lines, &[set(&["region"], text("r"))]),
            format!("x = 0{eol}a = 1{eol}region = \"r\"{eol}")
        );
        // A new table.
        assert_eq!(
            changed(&two_lines, &[set(&["t", "k"], EditValue::Int(1))]),
            format!("x = 0{eol}a = 1{eol}{eol}[t]{eol}k = 1{eol}")
        );
        // A new key in the last table.
        assert_eq!(
            changed(&style("[t]\nk = 1"), &[set(&["t", "j"], EditValue::Int(2))]),
            format!("[t]{eol}k = 1{eol}j = 2{eol}")
        );
        // The last key removed.
        assert_eq!(changed(&two_lines, &[clear(&["a"])]), format!("x = 0{eol}"));
    }
    // A file with no line ending at all gets the default one.
    assert_eq!(
        changed("# c", &[set(&["region"], text("r"))]),
        "# c\nregion = \"r\"\n"
    );
    assert_eq!(
        changed("a = 1", &[set(&["t", "k"], EditValue::Int(1))]),
        "a = 1\n\n[t]\nk = 1\n"
    );
}

#[test]
fn invalid_toml_bom_and_nul_are_refused_and_the_text_is_not_touched() {
    let edit = set(&["a"], EditValue::Int(1));
    let at = |input: &str| match apply(input, std::slice::from_ref(&edit), &rank) {
        Err(Refusal::NotToml { line, column }) => (line, column),
        other => panic!("expected NotToml for {input:?}, got {other:?}"),
    };
    assert_eq!(at("a = \n"), (1, 5));
    assert_eq!(at("b = 1\na = 1\na = 2\n"), (3, 1));
    assert_eq!(at("[x]\n[x]\n"), (2, 2));
    assert_eq!(at("a = 1\rb = 2\n"), (1, 7));
    // The byte order mark parses in the `toml` crate, so the patcher refuses it itself, at the first column.
    assert_eq!(at("\u{feff}a = 1\n"), (1, 1));
    // A NUL is refused where it stands, with the column counted in characters.
    assert_eq!(at("a = 1\u{0}\n"), (1, 6));
    // A value that is out of range parses as syntax but cannot be read, so the file is refused where the value stands.
    assert_eq!(at("a = 1\nbig = 99999999999999999999\n").0, 2);
    assert_eq!(at("low = -9223372036854775809\n").0, 1);
    assert_eq!(at("h\u{e9}llo = 1\nb = 2 \u{0}\n"), (2, 7));
    // A clear edit refuses the same way, and an empty list of edits still checks the file.
    assert_eq!(
        apply("a = \n", &[clear(&["a"])], &rank),
        Err(Refusal::NotToml { line: 1, column: 5 })
    );
    assert_eq!(
        apply("a = \n", &[], &rank),
        Err(Refusal::NotToml { line: 1, column: 5 })
    );
    // The messages never carry the source line.
    let secret_line = format!("{} = ", "pass".to_string() + "word");
    let shown = apply(&secret_line, &[edit], &rank).unwrap_err().to_string();
    assert!(!shown.contains("pass"), "{shown}");
}

#[test]
fn the_oracle_refuses_what_it_cannot_prove() {
    let old = "a = \"x\"\nb = 2 # c\n";
    let edit = set(&["a"], text("yy"));
    // Control: with no fault the same call works.
    assert_eq!(
        apply_with_shifted_value_span(old, std::slice::from_ref(&edit), &rank, 0),
        Ok(Outcome::Changed("a = \"yy\"\nb = 2 # c\n".to_string()))
    );
    // A value span that is off by one byte either breaks the TOML or changes another key, and the oracle says so.
    for shift in [-1, 1, 2, 5] {
        assert_eq!(
            apply_with_shifted_value_span(old, std::slice::from_ref(&edit), &rank, shift),
            Err(Refusal::NotProvable),
            "shift {shift}"
        );
    }
    // A shifted span can also leave valid TOML that changed another key; the oracle sees the other key moved.
    assert_eq!(
        apply_with_shifted_value_span(
            "a = 1\nb = 22\n",
            &[set(&["a"], EditValue::Int(5))],
            &rank,
            6
        ),
        Err(Refusal::NotProvable)
    );
    // The same through the table of a longer file.
    let long = "x = 1\n[t]\nk = true # c\nj = 2\n";
    for shift in [-1, 1, 3] {
        assert_eq!(
            apply_with_shifted_value_span(
                long,
                &[set(&["t", "k"], EditValue::Bool(false))],
                &rank,
                shift
            ),
            Err(Refusal::NotProvable),
            "shift {shift}"
        );
    }
}

#[test]
fn a_hostile_edit_value_never_changes_another_key() {
    let hostile = "x\"\n[inject]\nz = 1\n# \"";
    let got = changed("a = 1\n", &[set(&["k"], text(hostile))]);
    let parsed = tree(&got);
    assert_eq!(parsed.len(), 2, "{got:?}");
    assert_eq!(parsed["k"].as_str(), Some(hostile));
    assert!(!parsed.contains_key("inject"));

    // Inside a table, as a list entry and as a key.
    let in_table = changed(
        "[t]\nq = 1\n",
        &[
            set(&["t", "k"], text(hostile)),
            set(
                &["t", "list"],
                EditValue::StrList(vec![hostile.to_string(), "]\n[inject2]".to_string()]),
            ),
        ],
    );
    let parsed = tree(&in_table);
    assert_eq!(parsed.len(), 1);
    let t = parsed["t"].as_table().unwrap();
    assert_eq!(t.len(), 3);
    assert_eq!(t["k"].as_str(), Some(hostile));
    assert_eq!(t["list"].as_array().unwrap().len(), 2);

    let odd_key = "we ird\n[inject3]\n\"q\" = ";
    let keyed = changed("", &[set(&[odd_key], EditValue::Int(1))]);
    let parsed = tree(&keyed);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[odd_key].as_integer(), Some(1));

    // The value replaced in an existing line keeps the line's other keys.
    let replaced_text = changed("a = 1\nb = 2\n", &[set(&["a"], text(hostile))]);
    let parsed = tree(&replaced_text);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed["b"].as_integer(), Some(2));
    assert_eq!(parsed["a"].as_str(), Some(hostile));
}

#[test]
fn an_empty_path_and_a_non_finite_float_are_refused_and_edits_apply_in_order() {
    assert!(matches!(
        refusal("a = 1\n", &[set(&[], EditValue::Int(1))]),
        Refusal::Unsupported { line: 0, .. }
    ));
    assert!(matches!(
        refusal("a = 1\n", &[clear(&[])]),
        Refusal::Unsupported { line: 0, .. }
    ));
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            refusal("a = 1\n", &[set(&["f"], EditValue::Float(bad))]),
            Refusal::Unsupported { line: 0, .. }
        ));
    }
    // Each edit sees the text the one before it produced.
    assert_eq!(
        changed(
            "a = 1\n",
            &[
                set(&["b"], EditValue::Int(2)),
                set(&["a"], EditValue::Int(3)),
                clear(&["b"]),
                set(&["b"], EditValue::Int(4)),
            ]
        ),
        "a = 3\nb = 4\n"
    );
    // A refusal in the middle of a list refuses the whole call.
    assert!(matches!(
        apply(
            "d.e = 1\n",
            &[
                set(&["x"], EditValue::Int(1)),
                set(&["d", "e"], EditValue::Int(2))
            ],
            &rank
        ),
        Err(Refusal::Unsupported { .. })
    ));
}

#[test]
fn the_rank_decides_only_where_a_new_top_level_key_goes() {
    // The rank is asked for the names of top-level keys only; a key of a table is placed after the table's last key whatever it ranks.
    let seen = std::cell::RefCell::new(Vec::<Vec<String>>::new());
    let spy = |p: &[String]| {
        seen.borrow_mut().push(p.to_vec());
        rank(p)
    };
    let out = apply(
        "mouse = true\n\n[run]\ngoal = true\n",
        &[
            set(&["run", "alpha"], EditValue::Int(1)),
            set(&["region"], text("r")),
        ],
        &spy,
    )
    .unwrap();
    assert_eq!(
        out,
        Outcome::Changed(
            "region = \"r\"\nmouse = true\n\n[run]\ngoal = true\nalpha = 1\n".to_string()
        )
    );
    assert!(seen.borrow().iter().all(|p| p.len() == 1));
    // An unknown top-level key ranks last, so it lands after the last assignment.
    assert_eq!(
        changed("mouse = true\n", &[set(&["zzz"], EditValue::Int(1))]),
        "mouse = true\nzzz = 1\n"
    );
}

#[test]
fn characters_above_the_basic_plane_and_invisible_ones_are_escaped_and_read_back() {
    // Every control character, the separators and every character the terminal sanitizer marks, and the edges of the planes.
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{2028}' | '\u{2029}')
            || ham_core::sanitize::marker(c).is_some()
    };
    let mut chars: Vec<char> = (0u32..=0x10ffff)
        .filter_map(char::from_u32)
        .filter(|c| hidden(*c))
        .collect();
    assert!(chars.len() > 400, "{}", chars.len());
    assert!(
        chars.contains(&'\u{e0001}') && chars.contains(&'\u{1d173}') && chars.contains(&'\u{34f}')
    );
    chars.extend(['\u{ffff}', '\u{10000}', '\u{10ffff}', '\u{1f600}']);
    for c in chars {
        let value = format!("a{c}b");
        let got = changed("", &[set(&["k"], text(&value))]);
        assert_eq!(
            tree(&got)["k"].as_str(),
            Some(value.as_str()),
            "U+{:04X}",
            u32::from(c)
        );
        if hidden(c) {
            // The line break that ends the file is the file's own.
            let body = got.trim_end_matches('\n');
            assert!(!body.contains(c), "U+{:04X} written raw", u32::from(c));
        }
        // The same character in a key.
        let keyed = changed("", &[set(&[value.as_str()], EditValue::Int(1))]);
        assert_eq!(
            tree(&keyed).get(&value).and_then(toml::Value::as_integer),
            Some(1)
        );
    }
    // Four digits below U+10000 and eight above.
    assert_eq!(
        changed("", &[set(&["k"], text("\u{e0001}\u{200b}"))]),
        "k = \"\\U000E0001\\u200B\"\n"
    );
}

#[test]
fn the_oracle_compares_values_above_the_edited_key_and_ignores_only_empty_tables() {
    let drop_x = clear(&["a", "x"]);
    // What a clear may leave: the table gone, or an empty table.
    assert_eq!(prove_pair("[a]\nx = 1\n", "", &drop_x), Ok(()));
    assert_eq!(prove_pair("[a]\nx = 1\n", "[a]\n", &drop_x), Ok(()));
    // A value, an array or a table with content where the table was is not the intended change.
    for bad in [
        "a = 2\n",
        "a = [1]\n",
        "a = { y = 1 }\n",
        "[a]\ny = 1\n",
        "[a]\nx = 1\n",
    ] {
        assert_eq!(
            prove_pair("[a]\nx = 1\n", bad, &drop_x),
            Err(Refusal::NotProvable),
            "{bad:?}"
        );
    }
    // A set may make the tables above the key.
    let add_b = set(&["a", "b"], EditValue::Int(1));
    assert_eq!(prove_pair("", "[a]\nb = 1\n", &add_b), Ok(()));
    assert_eq!(prove_pair("[a]\n", "[a]\nb = 1\n", &add_b), Ok(()));
    assert_eq!(
        prove_pair("", "a = 5\nb = 1\n", &add_b),
        Err(Refusal::NotProvable)
    );
    assert_eq!(
        prove_pair("", "[a]\nb = 2\n", &add_b),
        Err(Refusal::NotProvable)
    );
    // Another key changed or lost.
    let keep_a = set(&["a"], EditValue::Int(1));
    assert_eq!(
        prove_pair("a = 1\nb = 2\n", "a = 1\nb = 3\n", &keep_a),
        Err(Refusal::NotProvable)
    );
    assert_eq!(
        prove_pair("a = 1\nb = 2\n", "a = 1\n", &keep_a),
        Err(Refusal::NotProvable)
    );
    assert_eq!(
        prove_pair("a = 1\nb = 2\n", "a = 1\nb = 2\nc = 3\n", &keep_a),
        Err(Refusal::NotProvable)
    );
    // Text that is not TOML cannot be proven.
    assert_eq!(
        prove_pair("a = 1\n", "a = \n", &keep_a),
        Err(Refusal::NotProvable)
    );
}

#[test]
fn deep_nesting_is_an_error_and_not_a_stack_overflow() {
    // The parser limits nesting (80 levels), so the walks of the patcher and of the oracle are bounded. A debug build needs about half a
    // megabyte for 60 levels (`toml::from_str` into a table is the hungry part; `DeTable::parse` alone fits 128 KiB), so the thread gets 2 MiB
    // and the test shows that a document nested 300 deep is refused instead of overflowing it.
    let handle = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let dotted = |n: usize| format!("{} = 1\n", vec!["a"; n].join("."));
            let arrays = |n: usize| format!("x = {}1{}\n", "[".repeat(n), "]".repeat(n));
            let headers = |n: usize| format!("[{}]\nx = 1\n", vec!["a"; n].join("."));
            let add = [set(&["z"], EditValue::Int(1))];
            for ok in [dotted(60), arrays(60), headers(60)] {
                match apply(&ok, &add, &rank) {
                    Ok(Outcome::Changed(new)) => assert!(new.contains("z = 1"), "{new:?}"),
                    other => panic!("expected a change, got {other:?}"),
                }
            }
            for deep in [dotted(300), arrays(300), headers(300)] {
                assert!(
                    matches!(
                        apply(&deep, &add, &rank),
                        Err(Refusal::NotToml { line: 1, .. })
                    ),
                    "a document nested 300 deep is refused as not TOML"
                );
            }
        })
        .unwrap();
    handle.join().unwrap();
}

#[test]
fn spaced_headers_quoted_names_and_comment_boundaries_are_read_from_the_spans() {
    // Whitespace inside a header and a table named in two parts.
    let old = "[ a . b ]\nx = 1\n\n[ a ]\ny = 2\n";
    assert_eq!(
        changed(old, &[set(&["a", "z"], EditValue::Int(3))]),
        "[ a . b ]\nx = 1\n\n[ a ]\ny = 2\nz = 3\n"
    );
    assert_eq!(
        changed(old, &[set(&["a", "b", "z"], EditValue::Int(3))]),
        "[ a . b ]\nx = 1\nz = 3\n\n[ a ]\ny = 2\n"
    );
    // Quoted segments of a dotted key whose names look like a header or an inline table are still implicit tables made by dotted keys.
    let quoted = "\"[x]\".y = 1\n\"{z}\".w = 2\nk = 3\n";
    assert_eq!(
        changed(quoted, &[set(&["k"], EditValue::Int(4))]),
        "\"[x]\".y = 1\n\"{z}\".w = 2\nk = 4\n"
    );
    assert!(matches!(
        refusal(quoted, &[set(&["[x]", "y"], EditValue::Int(5))]),
        Refusal::Unsupported { line: 1, .. }
    ));
    assert!(matches!(
        refusal(quoted, &[clear(&["{z}", "w"])]),
        Refusal::Unsupported { line: 2, .. }
    ));

    // A multi-line string whose lines look like a comment and a header is one value; comments attach by the layout, not by their text.
    for style in [str::to_string as fn(&str) -> String, crlf_style] {
        let with_comment = style(
            "x = \"\"\"\n# string content\n[not_a_header]\n\"\"\"\n# keep with a\n[a]\ny = 1\n\n# keep with b\n[b]\nz = 2\n",
        );
        assert_eq!(
            changed(&with_comment, &[clear(&["a", "y"])]),
            style(
                "x = \"\"\"\n# string content\n[not_a_header]\n\"\"\"\n# keep with a\n[a]\n\n# keep with b\n[b]\nz = 2\n"
            )
        );
        let without = style(
            "x = \"\"\"\n# string content\n[not_a_header]\n\"\"\"\n[a]\ny = 1\n\n# keep with b\n[b]\nz = 2\n",
        );
        assert_eq!(
            changed(&without, &[clear(&["a", "y"])]),
            style(
                "x = \"\"\"\n# string content\n[not_a_header]\n\"\"\"\n# keep with b\n[b]\nz = 2\n"
            )
        );
        // A new top-level key goes above the string's comment-like lines only if they are comments: here they are not.
        assert_eq!(
            changed(&without, &[set(&["a2"], EditValue::Int(1))]),
            style(
                "x = \"\"\"\n# string content\n[not_a_header]\n\"\"\"\na2 = 1\n[a]\ny = 1\n\n# keep with b\n[b]\nz = 2\n"
            )
        );
    }
}

#[test]
fn floats_are_the_same_value_only_when_their_bits_are() {
    assert_eq!(
        apply("x = -0.0\n", &[set(&["x"], EditValue::Float(-0.0))], &rank),
        Ok(Outcome::Unchanged)
    );
    assert_eq!(
        changed("x = -0.0\n", &[set(&["x"], EditValue::Float(0.0))]),
        "x = 0.0\n"
    );
    // A NaN already in the file does not make every edit fail.
    assert_eq!(
        changed("n = nan\n", &[set(&["x"], EditValue::Int(1))]),
        "n = nan\nx = 1\n"
    );
}

#[test]
fn a_name_that_only_holds_arrays_of_tables_is_a_table_and_is_refused_with_its_line() {
    let doc = "[[a.b]]\nx = 1\n[[a.b]]\nx = 2\n";
    for edit in [
        set(&["a"], EditValue::Int(1)),
        clear(&["a"]),
        set(&["a", "b"], EditValue::Int(1)),
        clear(&["a", "b", "x"]),
    ] {
        assert!(
            matches!(
                refusal(doc, std::slice::from_ref(&edit)),
                Refusal::Unsupported { line: 1, .. }
            ),
            "{edit:?}"
        );
    }
    // A key beside them is still editable.
    assert_eq!(
        changed(doc, &[set(&["z"], EditValue::Int(3))]),
        "z = 3\n\n[[a.b]]\nx = 1\n[[a.b]]\nx = 2\n"
    );
}

#[test]
fn a_blank_line_at_the_end_of_the_file_is_the_separator_of_a_new_table() {
    let add = [set(&["t", "k"], EditValue::Int(1))];
    // One blank line between the content and the new table, not two.
    assert_eq!(changed("x = 1\n\n", &add), "x = 1\n\n[t]\nk = 1\n");
    assert_eq!(
        changed("x = 1\r\n\r\n", &add),
        "x = 1\r\n\r\n[t]\r\nk = 1\r\n"
    );
    // A line of spaces counts as blank, with or without a line break after it.
    assert_eq!(changed("x = 1\n  \n", &add), "x = 1\n  \n[t]\nk = 1\n");
    assert_eq!(changed("x = 1\n  ", &add), "x = 1\n  \n[t]\nk = 1\n");
    // A file of one line break is nothing but the separator.
    assert_eq!(changed("\n", &add), "\n[t]\nk = 1\n");
    // More blank lines than one stay as they are.
    assert_eq!(changed("x = 1\n\n\n", &add), "x = 1\n\n\n[t]\nk = 1\n");
    // The blank line went with the table, so taking the key out gives the file back without it.
    assert_eq!(
        changed("x = 1\n\n[t]\nk = 1\n", &[clear(&["t", "k"])]),
        "x = 1\n"
    );
}
