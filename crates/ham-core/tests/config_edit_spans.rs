//! Step 1 of the comment-keeping patcher (`ham_core::config_edit`): does the `toml` crate's own span API carry what the patcher needs?
//!
//! The patcher edits a file by bytes. It needs from the parser (a) the byte span of a key and of its value, (b) the byte end of the last
//! entry of a standard table, (c) a usable header position for an empty table `[x]`, and nothing else about the layout. This test runs
//! `toml::de::DeTable::parse` on one document that holds a comment, a table, a dotted key, a multi-line array and a trailing comment and
//! checks each point. It also pins the span facts the patcher builds its classification on, because the crate does not publish the flags
//! of a table (`is_dotted`, `is_implicit`, `is_inline`) or of an array (`is_array_of_tables`); they are `pub(crate)` in `toml` 1.1.6, which
//! `Cargo.toml` pins exactly. A bump of the crate that moves any of these facts fails here before it can corrupt a file.
//!
//! The facts: a standard table's span is its header (`[a.b]`) and holds no entry; an implicit table (made by a dotted key or by the path of
//! a header) has the span of one key segment; an inline table's span starts with `{`; an array of tables (`[[x]]`) has the span of its
//! first header, which is also the span of its first element, while the span of a plain array is strictly larger than the span of any
//! element in it; a scalar's span includes its quotes; an array's span runs from `[` to `]`.
#![allow(clippy::unwrap_used)]

use toml::Spanned;
use toml::de::{DeTable, DeValue};

/// A document with every shape the patcher meets. Byte offsets are found by searching, never written by hand.
const DOC: &str = "\
# top comment
apiEndpoint = \"https://x\" # trailing comment
region = 'us-east-1'
\"quoted key\" = true
d.e = 1

[run]
goal = true
tags = [
  \"a\",
  \"b\", # inner comment
]
a.b = 2   # dotted in a table
inl = { c = 3 }
ml = \"\"\"
first
#second\"\"\"

[empty]

[o.p]
x = 4

[[aot]]
z = 5
[[aot]]
z = 6
";

fn doc_tree() -> Spanned<DeTable<'static>> {
    DeTable::parse(DOC).unwrap()
}

fn text(span: std::ops::Range<usize>) -> &'static str {
    DOC.get(span).unwrap()
}

fn table<'a, 'i>(t: &'a DeTable<'i>, key: &str) -> &'a Spanned<DeValue<'i>> {
    t.get(key)
        .unwrap_or_else(|| panic!("no key {key:?} in the table"))
}

fn as_table<'a, 'i>(v: &'a Spanned<DeValue<'i>>) -> &'a DeTable<'i> {
    match v.get_ref() {
        DeValue::Table(t) => t,
        other => panic!("not a table: {other:?}"),
    }
}

#[test]
fn toml_span_api_supports_the_patcher() {
    let root = doc_tree();
    let root = root.get_ref();

    // (a) The byte span of a key and of its value; a scalar's span includes its quotes, and the trailing comment is outside it.
    let (k, v) = root.get_key_value("apiEndpoint").unwrap();
    assert_eq!(text(k.span()), "apiEndpoint");
    assert_eq!(text(v.span()), "\"https://x\"");
    let (k, v) = root.get_key_value("region").unwrap();
    assert_eq!(text(k.span()), "region");
    assert_eq!(text(v.span()), "'us-east-1'");
    let (k, v) = root.get_key_value("quoted key").unwrap();
    assert_eq!(text(k.span()), "\"quoted key\"");
    assert_eq!(text(v.span()), "true");

    // A multi-line array: the span runs from `[` to `]` and covers the inner comment; a multi-line string covers its delimiters.
    let run = as_table(table(root, "run"));
    let tags = table(run, "tags");
    assert_eq!(
        text(tags.span()),
        "[\n  \"a\",\n  \"b\", # inner comment\n]"
    );
    assert_eq!(
        text(table(run, "ml").span()),
        "\"\"\"\nfirst\n#second\"\"\""
    );

    // (b) The end of the last entry of a standard table is the largest end of an entry's value span in it (a dotted key counts); the
    // table's own span is its header and ends before the first entry.
    let run_value = table(root, "run");
    assert_eq!(text(run_value.span()), "[run]");
    assert!(run_value.span().end <= table(run, "goal").span().start);
    let mut last_end = 0;
    for (_, v) in run.iter() {
        match v.get_ref() {
            // An implicit table made by a dotted key carries the entries of the dotted path.
            DeValue::Table(inner) if !text(v.span()).starts_with(['[', '{']) => {
                for (_, leaf) in inner.iter() {
                    last_end = last_end.max(leaf.span().end);
                }
            }
            _ => last_end = last_end.max(v.span().end),
        }
    }
    assert!(DOC[..last_end].ends_with("#second\"\"\""), "{last_end}");
    assert!(DOC[last_end..].starts_with("\n\n[empty]"), "{last_end}");

    // (c) An empty table has a usable header position and no entry.
    let empty = table(root, "empty");
    assert_eq!(text(empty.span()), "[empty]");
    assert!(as_table(empty).is_empty());

    // The classification facts. A dotted key makes an implicit table whose span is the key segment, not a bracket.
    let d = table(root, "d");
    assert_eq!(text(d.span()), "d");
    assert_eq!(text(table(as_table(d), "e").span()), "1");
    let a = table(run, "a");
    assert_eq!(text(a.span()), "a");
    // The path of a header makes an implicit ancestor with the span of one key segment; the last segment holds the header span.
    let o = table(root, "o");
    assert_eq!(text(o.span()), "o");
    assert_eq!(text(table(as_table(o), "p").span()), "[o.p]");
    // An inline table is a value whose span starts with `{`.
    assert_eq!(text(table(run, "inl").span()), "{ c = 3 }");

    // An array of tables: the array and its first element share the first header's span; later elements carry their own header span.
    let aot = table(root, "aot");
    let items = match aot.get_ref() {
        DeValue::Array(items) => items,
        other => panic!("not an array: {other:?}"),
    };
    assert_eq!(items.len(), 2);
    assert_eq!(aot.span(), items[0].span());
    assert_eq!(text(items[0].span()), "[[aot]]");
    assert_eq!(text(items[1].span()), "[[aot]]");
    assert_ne!(items[0].span(), items[1].span());
    // A plain array never has that shape: its span is strictly larger than any element's.
    let plain = DeTable::parse("p = [[1], [2]]\nq = [ {a = 1} ]\n").unwrap();
    for key in ["p", "q"] {
        let v = table(plain.get_ref(), key);
        let items = match v.get_ref() {
            DeValue::Array(items) => items,
            other => panic!("not an array: {other:?}"),
        };
        assert!(
            items.iter().all(|e| e.span() != v.span()),
            "a plain array must differ from its elements"
        );
    }
    // The document's own span is empty, so the patcher never reads it.
    assert_eq!(root_span(), 0..0);
}

fn root_span() -> std::ops::Range<usize> {
    doc_tree().span()
}

#[test]
fn spans_of_a_crlf_file_stop_before_the_carriage_return() {
    let crlf = "a = \"x\" # c\r\n[t]\r\nk = 1\r\n";
    let tree = DeTable::parse(crlf).unwrap();
    let a = tree.get_ref().get("a").unwrap();
    assert_eq!(&crlf[a.span()], "\"x\"");
    let t = tree.get_ref().get("t").unwrap();
    assert_eq!(&crlf[t.span()], "[t]");
    let k = as_table(t).get("k").unwrap();
    assert_eq!(&crlf[k.span()], "1");
}

#[test]
fn parse_failures_carry_a_span_and_a_leading_bom_is_not_one() {
    // A syntax error has a byte span the patcher turns into a line and a column.
    for (bad, start) in [
        ("a = \n", 4),
        ("a = 1\rb = 2\n", 6),
        ("[x]\n[x]\n", 5),
        ("a = 1\na = 2\n", 6),
    ] {
        let err = DeTable::parse(bad).unwrap_err();
        assert_eq!(err.span().unwrap().start, start, "{bad:?}");
    }
    let nul = DeTable::parse("a = 1\u{0}\n").unwrap_err();
    assert_eq!(nul.span().unwrap().start, 4);
    // The parser accepts a byte order mark, so the patcher refuses it itself (the reader of `ham` does the same).
    assert!(DeTable::parse("\u{feff}a = 1\n").is_ok());
}
