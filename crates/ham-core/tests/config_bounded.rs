//! `config.toml` read from bytes with the size on disk known: the reader of `ham` never holds more than the cap, and says how big the file
//! really is; and the vocabulary the verbs need: which findings make the text unusable
//! as TOML, and the seven classes of settings the harness locks.
#![allow(clippy::unwrap_used)]

use ham_core::config::{parse_config, parse_config_bytes, read_config, read_config_bytes};
use ham_core::settings::{Code, LOCKED, MAX_CONFIG_BYTES};

#[test]
fn a_file_over_the_cap_is_refused_with_its_real_size_from_the_bytes_that_were_read() {
    // The caller read at most cap + 1 bytes of a file that is 10 million bytes long.
    let read_so_far = vec![b'a'; MAX_CONFIG_BYTES + 1];
    let read = read_config_bytes(&read_so_far, 10_000_000);
    let f = read.findings.errors().next().expect("an error finding");
    assert_eq!(f.code, Code::TooLarge);
    assert!(
        f.message.contains("10000000") && f.message.contains(&MAX_CONFIG_BYTES.to_string()),
        "{}",
        f.message
    );
    assert!(parse_config_bytes(&read_so_far, 10_000_000).is_err());
    // A file at the cap is read, and its bytes read as `read_config` reads the text.
    let mut exact = String::from("# ");
    exact.push_str(&"a".repeat(MAX_CONFIG_BYTES - 3));
    exact.push('\n');
    let read = read_config_bytes(exact.as_bytes(), exact.len() as u64);
    assert!(!read.has_errors(), "{:?}", read.findings);
}

#[test]
fn bytes_that_are_not_utf8_are_an_error_that_names_no_content() {
    let bytes = b"region = \"\xff\xfe\"\n";
    let read = read_config_bytes(bytes, bytes.len() as u64);
    let f = read.findings.errors().next().expect("an error finding");
    assert!(f.message.contains("UTF-8"), "{}", f.message);
    assert!(parse_config_bytes(bytes, bytes.len() as u64).is_err());
}

#[test]
fn valid_bytes_read_like_the_text_they_hold() {
    let text = "# note\nregion = \"us-east-1\"\nconfigVersion = 1\n\n[ui]\nbell = false\n";
    let a = read_config(text);
    let b = read_config_bytes(text.as_bytes(), text.len() as u64);
    assert_eq!(a.config, b.config);
    assert_eq!(a.findings, b.findings);
    assert_eq!(
        parse_config_bytes(text.as_bytes(), text.len() as u64).unwrap(),
        parse_config(text).unwrap()
    );
}

#[test]
fn the_findings_that_make_the_text_unusable_as_toml_are_named() {
    let unusable = [
        Code::TooLarge,
        Code::Nul,
        Code::Bom,
        Code::TooDeep,
        Code::Syntax,
    ];
    let usable = [
        Code::UnknownKey,
        Code::UnknownTable,
        Code::MovedKey,
        Code::BadType,
        Code::OutOfRange,
        Code::DuplicateKey,
        Code::BadVersion,
        Code::NewerVersion,
    ];
    for c in unusable {
        assert!(c.is_unreadable(), "{}", c.as_str());
    }
    for c in usable {
        assert!(!c.is_unreadable(), "{}", c.as_str());
    }
}

#[test]
fn the_locked_classes_are_seven_each_with_a_name_a_short_label_and_one_reason() {
    assert_eq!(LOCKED.len(), 7);
    let mut labels = std::collections::BTreeSet::new();
    for class in &LOCKED {
        assert!(
            !class.name.is_empty() && !class.reason.is_empty(),
            "{class:?}"
        );
        assert!(class.reason.len() <= 100, "one short reason: {class:?}");
        assert!(labels.insert(class.label), "{} is used twice", class.label);
        assert!(!class.reason.chars().any(char::is_control));
    }
    let labels: Vec<_> = LOCKED.iter().map(|c| c.label).collect();
    for want in [
        "credentials",
        "provider",
        "telemetry",
        "processes",
        "approvals",
        "network",
        "owned elsewhere",
    ] {
        assert!(labels.contains(&want), "{want}: {labels:?}");
    }
}
