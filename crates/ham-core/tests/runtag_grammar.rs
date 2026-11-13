//! The tag grammar and policy: what may be stored, what a person may type, which words no tag may hold, and the project tag
//! that a repository gives. A tag is display and search text; these rules are the only place that decides what one may be.
#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::runtag::grammar::{
    GENERIC_WORDS, OUTCOME_TOKENS, Origin, Reason, STATUS_WORDS, check_stored, forbidden,
    normalize_typed, project_tag_of,
};
use ham_core::sanitize::sanitize;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::path::PathBuf;

const ORIGINS: [Origin; 3] = [Origin::Model, Origin::User, Origin::Derived];

fn fixture(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/runtag")
        .join(rel)
}

/// An access-key-shaped literal, built at run time so the source holds no key-shaped text.
fn key_shaped() -> String {
    ["AK", "IA", "IOSFODNN7", "EXAMPLE"].concat()
}

/// The tags of every recorded V5 reply: the last `<tags>` element, read as a JSON array of strings.
fn v5_tags() -> Vec<(String, Vec<String>)> {
    let text = std::fs::read_to_string(fixture("replies/v5.json")).unwrap();
    let raw: BTreeMap<String, String> = serde_json::from_str(&text).unwrap();
    raw.into_iter()
        .map(|(id, reply)| {
            let body = reply
                .rsplit("<tags>")
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let tags: Vec<String> = if body.is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&body).unwrap()
            };
            (id, tags)
        })
        .collect()
}

#[test]
fn grammar_takes_the_recorded_tags_and_refuses_the_rest() {
    let replies = v5_tags();
    assert_eq!(replies.len(), 38);
    let mut checked = 0;
    for (id, tags) in &replies {
        for tag in tags {
            if tag.starts_with("project:") {
                // The model writes topic and tech only: a project tag is refused for the model, with its own code.
                assert_eq!(
                    check_stored(tag, Origin::Model),
                    Err(Reason::ModelProject),
                    "{id}: {tag}"
                );
            } else {
                assert_eq!(check_stored(tag, Origin::Model), Ok(()), "{id}: {tag}");
                checked += 1;
            }
        }
    }
    assert!(
        checked >= 90,
        "only {checked} recorded non-project tags were checked"
    );

    let refused: [(&str, Reason); 7] = [
        ("lang:python", Reason::UnknownKind),
        ("topic:one-two-three-four", Reason::TooManyWords),
        (
            "topic:aaaaaaaaaaa-bbbbbbbbbbb-ccccccccc",
            Reason::ValueTooLong,
        ),
        ("topic:", Reason::EmptyValue),
        ("tech:日本語", Reason::NonAscii),
        ("topic:UPPER", Reason::NotLowercase),
        ("topic:a--b", Reason::HyphenShape),
    ];
    let mut codes = std::collections::BTreeSet::new();
    for (tag, want) in refused {
        assert_eq!(check_stored(tag, Origin::Model), Err(want), "{tag}");
        codes.insert(want.as_str());
    }
    assert_eq!(codes.len(), 7, "each refusal has its own reason code");

    for (tag, want) in [
        ("topic:-x", Reason::HyphenShape),
        ("topic:x-", Reason::HyphenShape),
        ("topic:a_b", Reason::BadChar),
        ("topic:a b", Reason::BadChar),
        ("topic:a:b", Reason::BadChar),
        ("nocolon", Reason::Grammar),
        ("", Reason::RawShape),
    ] {
        assert_eq!(check_stored(tag, Origin::Model), Err(want), "{tag:?}");
    }
    let long = format!("topic:{}", "a".repeat(100));
    assert_eq!(check_stored(&long, Origin::Model), Err(Reason::RawShape));
}

#[test]
fn model_tags_are_one_to_three_words_and_user_tags_one_to_six() {
    assert_eq!(check_stored("topic:one-two-three", Origin::Model), Ok(()));
    assert_eq!(
        check_stored("topic:one-two-three-four", Origin::Model),
        Err(Reason::TooManyWords)
    );
    assert_eq!(
        check_stored("topic:one-two-three-four-five-six", Origin::User),
        Ok(())
    );
    assert_eq!(
        check_stored("topic:one-two-three-four-five-six-seven", Origin::User),
        Err(Reason::TooManyWords)
    );
    // The longest user value is 40 characters and the longest user tag 48.
    let v40 = format!(
        "{}-{}-{}-{}",
        "a".repeat(9),
        "b".repeat(9),
        "c".repeat(9),
        "d".repeat(9)
    );
    assert_eq!(v40.len(), 39);
    assert_eq!(
        check_stored(&format!("project:{v40}x"), Origin::User),
        Ok(())
    );
    assert_eq!(
        check_stored(&format!("project:{v40}xy"), Origin::User),
        Err(Reason::ValueTooLong)
    );
}

#[test]
fn typed_tags_are_normalised() {
    assert_eq!(
        normalize_typed("CSV Export"),
        Ok("topic:csv-export".to_string())
    );
    assert_eq!(normalize_typed("Tech:K8s"), Ok("tech:k8s".to_string()));
    assert_eq!(
        normalize_typed("project:Billing_Export"),
        Ok("project:billing-export".to_string())
    );
    assert_eq!(normalize_typed("  topic:x  "), Ok("topic:x".to_string()));
    assert_eq!(
        normalize_typed("topic: csv  export "),
        Ok("topic:csv-export".to_string())
    );

    let err = normalize_typed("日本語").unwrap_err();
    assert_eq!(err, Reason::NonAscii);
    assert!(
        err.message().contains("letters"),
        "the message names the rule: {}",
        err.message()
    );
    assert_eq!(normalize_typed("foo:bar"), Err(Reason::UnknownKind));
    assert_eq!(normalize_typed("topic:"), Err(Reason::EmptyValue));
    assert_eq!(normalize_typed("   "), Err(Reason::RawShape));

    // The key shape is judged on the raw text, before the lower-casing that would hide it.
    let key = key_shaped();
    assert_eq!(
        normalize_typed(&format!("tech:{key}")),
        Err(Reason::CredentialShape)
    );
    assert_eq!(normalize_typed(&key), Err(Reason::CredentialShape));
    // A typed tag follows the same policy as a stored one.
    assert_eq!(normalize_typed("verified login"), Err(Reason::OutcomeToken));
    assert_eq!(normalize_typed("topic:blocked"), Err(Reason::StatusWord));
    // A person may type a generic word.
    assert_eq!(normalize_typed("testing"), Ok("topic:testing".to_string()));
}

#[test]
fn word_shape_rule_is_for_model_tags_only() {
    // A lower-cased 20-character key id: a word of more than 16 characters is refused for every origin.
    let lowered = format!("tech:{}", key_shaped().to_lowercase());
    for origin in ORIGINS {
        assert_eq!(
            check_stored(&lowered, origin),
            Err(Reason::WordShape),
            "{origin:?}"
        );
    }
    // A short word that mixes letters and digits: refused for the model above 6 characters, fine for a person.
    assert_eq!(
        check_stored("tech:ab12cd34ef56", Origin::Model),
        Err(Reason::WordShape)
    );
    assert_eq!(check_stored("tech:ab12cd34ef56", Origin::User), Ok(()));
    // A real repository name has such a word.
    let repo = "project:hamh-e2e-ntier-t40x91321-20261018";
    assert_eq!(check_stored(repo, Origin::Derived), Ok(()));
    assert_eq!(check_stored(repo, Origin::User), Ok(()));
    for ok in [
        "tech:s3",
        "tech:k8s",
        "tech:ec2",
        "tech:sha256",
        "tech:base64",
        "tech:route-53",
    ] {
        assert_eq!(check_stored(ok, Origin::Model), Ok(()), "{ok}");
    }
}

#[test]
fn outcome_tokens_are_forbidden_for_every_origin() {
    assert_eq!(OUTCOME_TOKENS, ["verified", "tested", "gated", "evaluated"]);
    for word in OUTCOME_TOKENS {
        for origin in ORIGINS {
            for tag in [
                format!("topic:{word}"),
                format!("topic:{word}-login"),
                format!("topic:login-{word}"),
                format!("tech:a-{word}-b"),
            ] {
                assert_eq!(
                    forbidden(&tag, origin),
                    Some(Reason::OutcomeToken),
                    "{tag} {origin:?}"
                );
                assert_eq!(
                    check_stored(&tag, origin),
                    Err(Reason::OutcomeToken),
                    "{tag} {origin:?}"
                );
            }
        }
    }
    for origin in ORIGINS {
        assert_eq!(forbidden("topic:unit-tests", origin), None);
        assert_eq!(check_stored("topic:unit-tests", origin), Ok(()));
        // A generic or status word is judged as the whole value, an outcome token as any hyphen-separated part.
        assert_eq!(check_stored("topic:test-plan", origin), Ok(()));
    }
}

#[test]
fn status_words_are_whole_value_only() {
    assert_eq!(STATUS_WORDS.len(), 22);
    for origin in ORIGINS {
        for tag in ["topic:blocked", "topic:done", "topic:in-progress"] {
            assert_eq!(
                forbidden(tag, origin),
                Some(Reason::StatusWord),
                "{tag} {origin:?}"
            );
            assert_eq!(
                check_stored(tag, origin),
                Err(Reason::StatusWord),
                "{tag} {origin:?}"
            );
        }
        assert_eq!(forbidden("topic:blocked-account", origin), None);
        assert_eq!(check_stored("topic:blocked-account", origin), Ok(()));
        assert_eq!(check_stored("topic:todo-app", origin), Ok(()));
    }
    for word in STATUS_WORDS {
        assert_eq!(
            forbidden(&format!("topic:{word}"), Origin::User),
            Some(Reason::StatusWord),
            "{word}"
        );
    }
}

#[test]
fn generic_words_are_for_model_tags_only() {
    for tag in ["topic:testing", "topic:bugfix", "tech:app"] {
        assert_eq!(
            forbidden(tag, Origin::Model),
            Some(Reason::Generic),
            "{tag}"
        );
        assert_eq!(
            check_stored(tag, Origin::Model),
            Err(Reason::Generic),
            "{tag}"
        );
        assert_eq!(forbidden(tag, Origin::User), None, "{tag}");
        assert_eq!(check_stored(tag, Origin::User), Ok(()), "{tag}");
        assert_eq!(check_stored(tag, Origin::Derived), Ok(()), "{tag}");
    }
    assert_eq!(GENERIC_WORDS.len(), 40);
    for word in GENERIC_WORDS {
        assert_eq!(
            forbidden(&format!("topic:{word}"), Origin::Model),
            Some(Reason::Generic),
            "{word}"
        );
        assert_eq!(
            forbidden(&format!("topic:{word}-x"), Origin::Model),
            None,
            "{word} inside a longer value is fine"
        );
    }
}

#[test]
fn derived_project_from_the_repository() {
    assert_eq!(project_tag_of("cc:calc"), Some("project:calc".to_string()));
    assert_eq!(project_tag_of("new cc:x"), Some("project:x".to_string()));
    assert_eq!(project_tag_of("s3-only"), None);
    assert_eq!(project_tag_of(""), None);
    assert_eq!(
        project_tag_of("gh:owner/Name"),
        Some("project:name".to_string())
    );
    assert_eq!(
        project_tag_of("gl:group/sub/proj"),
        Some("project:proj".to_string())
    );

    // A 100-character name is cut at a hyphen: at most 40 characters, no trailing hyphen.
    let name = ["abcdefghi"; 10].join("-");
    assert_eq!(name.len(), 99);
    let tag = project_tag_of(&format!("cc:{name}")).unwrap();
    assert_eq!(tag, "project:abcdefghi-abcdefghi-abcdefghi-abcdefghi");
    assert!(tag.len() - "project:".len() <= 40 && !tag.ends_with('-'));
    assert_eq!(check_stored(&tag, Origin::Derived), Ok(()));

    assert_eq!(
        project_tag_of("cc:Hamh_E2E.Python Calc"),
        Some("project:hamh-e2e-python-calc".to_string())
    );
    assert_eq!(
        project_tag_of("cc:___"),
        None,
        "a name that slugs to nothing"
    );
    assert_eq!(
        project_tag_of("cc:tested-things"),
        None,
        "a slug that holds an outcome token"
    );
    assert_eq!(
        project_tag_of("cc:abcdefghijklmnopqrstuvwxyz"),
        Some("project:abcdefghijklmnop".to_string()),
        "a word is cut at 16"
    );
    // The real fixture repository name keeps its mixed word.
    assert_eq!(
        project_tag_of("new cc:hamh-e2e-ntier-t40x91321-20261018"),
        Some("project:hamh-e2e-ntier-t40x91321-20261018".to_string())
    );
}

#[test]
fn escape_bidi_zero_width_url_never_pass() {
    let esc = '\u{1b}';
    let hostile = [
        format!("topic:a{esc}[31mred"),
        format!("topic:a{esc}]8;;x{esc}\\link"),
        format!("topic:a{esc}]52;c;QUJD\u{7}b"),
        "topic:a\u{200b}b".to_string(),
        "topic:a\u{202e}b".to_string(),
        "topic:a\u{e0041}b".to_string(),
        "topic:a://b".to_string(),
        "topic:x\u{0}y".to_string(),
        "topic:a\u{9b}31mb".to_string(),
    ];
    for tag in &hostile {
        for origin in ORIGINS {
            assert!(check_stored(tag, origin).is_err(), "{tag:?} {origin:?}");
        }
        assert!(normalize_typed(tag).is_err(), "{tag:?} typed");
    }
    assert_eq!(
        check_stored("topic:a\u{200b}b", Origin::Model),
        Err(Reason::InvisibleChars)
    );
    assert_eq!(
        check_stored(&format!("topic:a{esc}[31mred"), Origin::Model),
        Err(Reason::ControlChars)
    );
    assert_eq!(
        check_stored(&format!("tech:{}", key_shaped()), Origin::User),
        Err(Reason::CredentialShape)
    );
    assert_eq!(
        check_stored("topic:a://b", Origin::Model),
        Err(Reason::UrlShape)
    );
}

proptest! {
    #[test]
    fn an_accepted_tag_is_plain_ascii_in_the_grammar(s in ".{0,60}", origin in 0usize..3) {
        let origin = ORIGINS[origin];
        if check_stored(&s, origin).is_ok() {
            prop_assert_eq!(sanitize(&s), s.clone());
            prop_assert!(s.is_ascii());
            let (kind, value) = s.split_once(':').unwrap();
            prop_assert!(["project", "topic", "tech"].contains(&kind));
            prop_assert!(!value.contains(':'));
            prop_assert!(value.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
            prop_assert!(!value.starts_with('-') && !value.ends_with('-') && !value.contains("--"));
        }
    }

    #[test]
    fn a_typed_tag_that_normalises_is_accepted_by_the_stored_check(s in "[ -~]{0,50}") {
        if let Ok(tag) = normalize_typed(&s) {
            prop_assert_eq!(check_stored(&tag, Origin::User), Ok(()));
            // Normalising is idempotent.
            prop_assert_eq!(normalize_typed(&tag), Ok(tag.clone()));
        }
    }
}

#[test]
fn model_words_of_digits_are_at_most_six_long() {
    // A 12-digit account number, a phone number or a timestamp is an identifier, not a topic: the model may not write one.
    for tag in [
        "topic:123456789012",
        "tech:v-12345678",
        "topic:acct-1234567",
    ] {
        assert_eq!(
            check_stored(tag, Origin::Model),
            Err(Reason::WordShape),
            "{tag}"
        );
        assert_eq!(
            check_stored(tag, Origin::User),
            Ok(()),
            "{tag} is the person's own tag"
        );
    }
    // Versions, years, ports and short numbers are topics.
    for tag in [
        "topic:python-3-12",
        "tech:port-8080",
        "topic:year-2026",
        "tech:route-53",
        "topic:ticket-123456",
    ] {
        assert_eq!(check_stored(tag, Origin::Model), Ok(()), "{tag}");
    }
}
