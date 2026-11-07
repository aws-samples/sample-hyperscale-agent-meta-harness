//! A hostile engine file: a file other tools can write, read on the Mac before anything is
//! sent. Size, shape and type are bounded, a parse error never prints the source line, and no control character or escape sequence of a key reaches
//! a message. The hostile text here is ordinary TOML with ordinary characters; nothing in it is run or interpreted, and a key-shaped literal is
//! assembled at run time.
#![allow(clippy::unwrap_used)]

use ham_core::engine_file::{self, Disk, EngineFile, MAX_BYTES, Reading};
use std::time::{Duration, Instant};

const ENGINE: &str = "cline";

fn read(text: &str) -> Reading {
    engine_file::read_text(text, ENGINE)
}

fn codes(r: &Reading) -> Vec<&str> {
    r.problems.iter().map(|p| p.code.as_str()).collect()
}

fn scratch(label: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("ham-engine-hostile-{label}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---- size, shape and type caps -------------------------------------------------------------------------------------------------------

#[test]
fn size_shape_and_type_caps() {
    // 64 KiB plus one byte is refused before it is parsed, whatever it holds.
    let big = format!("schema = 1\n# {}\n", "x".repeat(MAX_BYTES));
    assert!(big.len() > MAX_BYTES);
    let r = engine_file::read_bytes(big.as_bytes(), big.len() as u64, ENGINE);
    assert_eq!(codes(&r), ["too_large"]);
    // The size on disk can be larger than the bytes held: the message names it.
    let held = &big.as_bytes()[..=MAX_BYTES];
    let r = engine_file::read_bytes(held, 10_000_000, ENGINE);
    assert_eq!(codes(&r), ["too_large"]);
    assert!(
        r.problems[0].message.contains("10000000"),
        "{}",
        r.problems[0].message
    );

    // Many short keys finish in bounded time: 16,000 of them do not fit the cap, and 10,000 that do are refused by the size of the member.
    let started = Instant::now();
    let names: Vec<String> = (0..17_000usize)
        .map(|i| {
            let (a, b, c) = (i / 676, (i / 26) % 26, i % 26);
            format!(
                "{}{}{}",
                (b'a' + (a % 26) as u8) as char,
                (b'a' + b as u8) as char,
                (b'a' + c as u8) as char
            )
        })
        .collect();
    let many: String = names
        .iter()
        .take(16_000)
        .map(|n| format!("{n}=1\n"))
        .collect();
    let r = read(&format!("schema = 1\n{many}"));
    assert_eq!(codes(&r), ["too_large"], "16,000 keys");
    let fits: String = names
        .iter()
        .take(9_000)
        .map(|n| format!("{n}=1\n"))
        .collect();
    assert!(fits.len() < MAX_BYTES);
    let r = read(&format!("schema = 1\n{fits}"));
    assert_eq!(codes(&r), ["too_large"], "9,000 keys that fit the file cap");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );

    // NUL, a byte-order mark and bytes that are not UTF-8.
    assert_eq!(codes(&read("schema = 1\n\0")), ["nul"]);
    assert_eq!(codes(&read("\u{feff}schema = 1\n")), ["bom"]);
    let r = engine_file::read_bytes(b"schema = 1\n# \xff\xfe\n", 16, ENGINE);
    assert_eq!(codes(&r), ["not_utf8"]);

    // Types the engine table cannot hold: a datetime, nan and inf, an array of tables, an inline table in an array, an array of numbers.
    for (text, line) in [
        (
            "schema = 1\n[commands]\ntimeout_seconds = 1979-05-27T07:32:00Z\n",
            3,
        ),
        ("schema = 1\n[context]\ncompaction_trigger = nan\n", 3),
        ("schema = 1\n[context]\ncompaction_trigger = inf\n", 3),
        ("schema = 1\n[context]\ncompaction_trigger = -inf\n", 3),
        ("schema = 1\n[[tools]]\ndisable = []\n", 2),
        ("schema = 1\n[tools]\ndisable = [{ a = 1 }]\n", 3),
        ("schema = 1\n[tools]\ndisable = [[\"spawn_agent\"]]\n", 3),
        ("schema = 1\n[tools]\ndisable = [1]\n", 3),
    ] {
        let r = read(text);
        assert!(r.config.is_none(), "{text:?}");
        assert_eq!(codes(&r), ["bad_type"], "{text:?}: {:?}", r.problems);
        assert_eq!(r.problems[0].line, Some(line), "{text:?}");
    }
    // An integer that does not fit 64 bits is a type problem, not a crash.
    let r = read("schema = 1\n[commands]\ntimeout_seconds = 99999999999999999999\n");
    assert!(r.config.is_none() && !r.ok(), "{:?}", r.problems);
    assert_eq!(r.problems[0].line, Some(3), "{:?}", r.problems);
}

#[test]
fn deep_nesting_is_an_error_and_not_a_stack_overflow() {
    // Guard: the reader refuses nesting past its limit before it parses, and the parser has its own limit. The thread has 256 KiB, a tenth of the
    // usual 2 MiB, so a recursion without a bound would fail here.
    let handle = std::thread::Builder::new()
        .stack_size(256 * 1024)
        .spawn(|| {
            let depth = 5000;
            let arrays = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            let tables = format!(
                "{}{}",
                "{a=".repeat(depth),
                "1".to_string() + &"}".repeat(depth)
            );
            [
                format!("schema = 1\nx = {arrays}\n"),
                format!("schema = 1\nx = {tables}\n"),
            ]
            .iter()
            .map(|text| {
                let r = read(text);
                assert!(r.config.is_none());
                r.problems.first().map(|p| p.code.clone())
            })
            .collect::<Vec<_>>()
        })
        .unwrap();
    let codes = handle.join().expect("the reader returned on a small stack");
    for code in codes {
        assert_eq!(code.as_deref(), Some("too_deep"));
    }
}

#[test]
fn a_quoted_key_with_a_dot_is_an_unknown_root_key_and_not_a_path() {
    let r = read("schema = 1\n\"reasoning.effort\" = \"high\"\n");
    assert!(r.config.is_none());
    assert_eq!(codes(&r), ["unknown_key"], "{:?}", r.problems);
    assert_eq!(r.problems[0].line, Some(2));
    // The name has a character the messages do not repeat, so it is not echoed.
    assert_eq!(r.problems[0].key, None);
    assert!(
        r.problems[0].message.contains("unusual characters"),
        "{}",
        r.problems[0].message
    );
    // The same file with the table form is a setting.
    assert!(read("schema = 1\n[reasoning]\neffort = \"high\"\n").ok());
}

#[test]
fn duplicate_keys_and_redefined_tables_are_not_toml() {
    for text in [
        "schema = 1\nschema = 1\n",
        "schema = 1\n[tools]\ndisable = []\n[tools]\n",
        "schema = 1\n[reasoning]\neffort = \"low\"\neffort = \"high\"\n",
        "schema = 1\ntools = 1\n[tools]\n",
    ] {
        let r = read(text);
        assert!(r.config.is_none(), "{text:?}");
        assert_eq!(codes(&r), ["syntax"], "{text:?}: {:?}", r.problems);
        assert!(r.problems[0].line.is_some(), "{text:?}");
        assert!(r.problems[0].column.is_some(), "{text:?}");
    }
}

#[test]
fn a_parse_error_prints_line_and_column_and_never_the_source_line() {
    // A line that looks like a credential, assembled at run time (nothing in this file is key-shaped), with a syntax error after it.
    let id: String = ["A", "K", "I", "A"].concat() + &"Q".repeat(16);
    let line = format!("api_access = {id} oops");
    let text = format!("schema = 1\n[tools]\n{line}\n");
    let dir = scratch("secret");
    let real = dir.join("target.toml");
    std::fs::write(&real, &text).unwrap();
    let link = dir.join("engines-cline.toml");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let file = EngineFile::load(&link, &Disk).unwrap();
    let r = file.read(ENGINE);
    assert_eq!(codes(&r), ["syntax"]);
    let p = &r.problems[0];
    assert_eq!(p.line, Some(3));
    assert!(p.column.is_some());
    let shown = p.render("engines/cline.toml");
    let whole = r.refusal("engines/cline.toml");
    for out in [&shown, &whole, &p.message] {
        assert!(!out.contains(&id), "the source line was repeated: {out}");
        assert!(!out.contains("api_access"), "{out}");
    }
    assert!(
        shown.starts_with("engines/cline.toml line 3 column"),
        "{shown}"
    );
}

#[test]
fn control_characters_and_escape_sequences_in_a_key_never_reach_a_message() {
    // A key made of an escape sequence, a bell, a backspace and a right-to-left mark, written with TOML escapes, in a table and at the root.
    let evil = "\\u001b[31mred\\u0007\\u0008\\u202ex";
    for text in [
        format!("schema = 1\n\"{evil}\" = 1\n"),
        format!("schema = 1\n[\"{evil}\"]\nvalue = 1\n"),
        format!("schema = 1\n[reasoning]\n\"{evil}\" = \"low\"\n"),
        format!("schema = 1\n[tools]\ndisable = [\"{evil}\"]\n"),
    ] {
        let r = read(&text);
        assert!(!r.ok(), "{text:?}");
        for out in r
            .problems
            .iter()
            .map(|p| p.render("engines/cline.toml"))
            .chain([r.refusal("engines/cline.toml")])
        {
            for c in out.chars() {
                assert!(
                    !c.is_control() || c == '\n',
                    "a control character {:?} in {out:?}",
                    c
                );
            }
            assert!(!out.contains('\u{202e}'), "{out:?}");
            assert!(!out.contains("red"), "the name was repeated: {out:?}");
        }
    }
}

#[test]
fn a_hostile_value_is_never_repeated_in_a_message() {
    // A TOML escape: a raw control character is not allowed inside a TOML string.
    let value = "x".repeat(300) + "\\u001b[2J";
    let r = read(&format!("schema = 1\n[reasoning]\neffort = \"{value}\"\n"));
    assert_eq!(codes(&r), ["out_of_range"]);
    let out = r.refusal("engines/cline.toml");
    assert!(!out.contains("xxxx") && !out.contains('\u{1b}'), "{out}");
}
