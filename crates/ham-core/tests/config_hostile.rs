//! Hostile `config.toml` text: too large, NUL, a byte-order mark, nesting past the parser's limit, and a parse error on a line that must not be
//! echoed. Every input here is TOML text read in memory with ordinary characters; nothing is executed or interpreted.

#![allow(clippy::unwrap_used)]

use ham_core::config::{parse_config, read_config};
use ham_core::settings::{MAX_CONFIG_BYTES, MAX_NESTING};

fn first_error_code(text: &str) -> String {
    let read = read_config(text);
    let finding = read
        .findings
        .errors()
        .next()
        .unwrap_or_else(|| panic!("no error finding for {} bytes", text.len()));
    finding.code.as_str().to_string()
}

#[test]
fn sixty_four_kib_plus_one_nul_and_bom_are_refused_before_parsing() {
    assert_eq!(MAX_CONFIG_BYTES, 64 * 1024);
    // One byte over the limit: refused by size, whatever the text is.
    let mut big = String::from("# ");
    big.push_str(&"a".repeat(MAX_CONFIG_BYTES - 1));
    big.push('\n');
    assert_eq!(big.len(), MAX_CONFIG_BYTES + 2);
    assert_eq!(first_error_code(&big), "too_large");
    assert!(parse_config(&big).is_err());
    // Exactly the limit is read.
    let mut exact = String::from("# ");
    exact.push_str(&"a".repeat(MAX_CONFIG_BYTES - 3));
    exact.push('\n');
    assert_eq!(exact.len(), MAX_CONFIG_BYTES);
    let read = read_config(&exact);
    assert!(!read.has_errors(), "{:?}", read.findings);
    assert!(parse_config(&exact).is_ok());
    // A NUL byte and a byte-order mark are named for what they are, not as a syntax error.
    assert_eq!(first_error_code("region = \"a\0b\"\n"), "nul");
    assert_eq!(first_error_code("\u{feff}region = \"us-east-1\"\n"), "bom");
    assert!(parse_config("region = \"a\0b\"\n").is_err());
    assert!(parse_config("\u{feff}region = \"us-east-1\"\n").is_err());
}

/// The same body as a guard: the parser's recursion limit keeps a deeply nested value from overflowing a small stack. The depth is far
/// past the limit of 80; the thread has 256 KiB, a tenth of the usual 2 MiB.
#[test]
fn deep_nesting_is_an_error_and_not_a_stack_overflow() {
    const STACK: usize = 256 * 1024;
    let handle = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(|| {
            let depth = 5000;
            let arrays = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
            let tables = format!(
                "{}{}",
                "{a=".repeat(depth),
                "1".to_string() + &"}".repeat(depth)
            );
            let mut codes = Vec::new();
            for text in [
                format!("region = {arrays}\n"),
                format!("region = {tables}\n"),
            ] {
                let read = read_config(&text);
                codes.push(
                    read.findings
                        .errors()
                        .next()
                        .map(|f| f.code.as_str().to_string()),
                );
                assert!(parse_config(&text).is_err());
            }
            codes
        })
        .unwrap();
    let codes = handle.join().expect("the reader returned on a small stack");
    for code in codes {
        assert_eq!(
            code.as_deref(),
            Some("too_deep"),
            "an error finding for the nested value"
        );
    }
    // At the limit the file is parsed (here as a value of the wrong type for `region`, a finding of its own); a brace or bracket inside
    // a string or a comment does not count.
    let at_limit = format!(
        "region = {}{}\n",
        "[".repeat(MAX_NESTING),
        "]".repeat(MAX_NESTING)
    );
    assert_eq!(first_error_code(&at_limit), "bad_type");
    let in_text = format!(
        "# {}\nregion = \"{}\"\nawsProfile = \'{}\'\n",
        "[".repeat(100),
        "{[".repeat(100),
        "[".repeat(100)
    );
    assert!(parse_config(&in_text).is_ok(), "{in_text:.60}");
}

#[test]
fn a_parse_error_prints_line_and_column_and_never_the_source_line() {
    // A credential-shaped line, assembled at run time (no key-shaped literal in the source).
    let head = ["aws_secret_access", "_key"].concat();
    let secret = ["wJalrXUtn", "FEMI/K7MDENG", "/bPxRfiCY", "EXAMPLEKEY"].concat();
    let text = format!("[default]\n{head} = {secret}\n");
    let read = read_config(&text);
    let finding = read.findings.errors().next().expect("a syntax finding");
    assert_eq!(finding.code.as_str(), "syntax");
    assert_eq!(finding.line, Some(2));
    assert!(finding.column.is_some());
    let shown = finding.render();
    assert!(
        shown.contains("line 2") && shown.contains("column"),
        "{shown}"
    );
    for part in [&secret[..], &secret[4..14], &head[..]] {
        assert!(!shown.contains(part), "the source line leaked: {shown}");
    }
    let err = parse_config(&text).unwrap_err().to_string();
    for part in [&secret[..], &secret[4..14]] {
        assert!(!err.contains(part), "the source line leaked: {err}");
    }

    // The same text behind a symbolic link: the reader sees the target's bytes and prints the same.
    let dir = std::env::temp_dir().join(format!("ham-hostile-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("credentials");
    std::fs::write(&target, &text).unwrap();
    let link = dir.join("config.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let via_link = std::fs::read_to_string(&link).unwrap();
    let err = parse_config(&via_link).unwrap_err().to_string();
    assert!(err.contains("line 2"), "{err}");
    assert!(!err.contains(&secret[4..14]), "{err}");

    // A value of the wrong type for a known key is named by key and line, with no part of the value.
    let typed = format!("region = 5\nmouse = \"{secret}\"\n");
    let err = parse_config(&typed).unwrap_err().to_string();
    assert!(err.contains("line 1") && err.contains("line 2"), "{err}");
    assert!(!err.contains(&secret[4..14]), "{err}");
}
