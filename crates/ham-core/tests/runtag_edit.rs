//! The person's edits and what a follow-up inherits. A removal always writes the off list, the off list wins on every read, and
//! an edit that would pass a cap is refused: nothing is ever evicted.
#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::runtag::edit::{TagState, apply_edit, inherit, parse_tag_words};
use ham_core::runtag::effective::effective;
use ham_core::runtag::grammar::{Reason, key_of};
use ham_core::runtag::{MAX_AUTO, MAX_OFF, MAX_USER};
use proptest::prelude::*;

fn v(tags: &[&str]) -> Vec<String> {
    tags.iter().map(|t| t.to_string()).collect()
}

fn state(auto: &[&str], user: &[&str], off: &[&str]) -> TagState {
    TagState {
        auto: v(auto),
        user: v(user),
        off: v(off),
    }
}

fn shown(repo: &str, s: &TagState) -> Vec<String> {
    effective(repo, &s.auto, &s.user, &s.off).tags
}

#[test]
fn add_and_remove_follow_the_precedence() {
    let start = state(
        &["tech:python", "topic:csv-export"],
        &[],
        &["topic:old-one"],
    );

    // Add puts the tag in user and takes it out of off.
    let s = apply_edit(&start, "s3-only", &v(&["topic:old-one"]), &[]).unwrap();
    assert_eq!(
        (s.user.clone(), s.off.clone()),
        (v(&["topic:old-one"]), vec![])
    );
    assert_eq!(
        s.auto, start.auto,
        "an edit never touches the automatic list"
    );

    // Remove of an automatic tag: the off list gets it, the automatic list keeps it, and it is not shown.
    let s = apply_edit(&start, "s3-only", &[], &v(&["tech:python"])).unwrap();
    assert_eq!(s.off, v(&["topic:old-one", "tech:python"]));
    assert_eq!(s.auto, start.auto);
    assert_eq!(shown("s3-only", &s), v(&["topic:csv-export"]));

    // Remove of the derived project: the off list gets it.
    let s = apply_edit(&start, "cc:calc", &[], &v(&["project:calc"])).unwrap();
    assert!(s.off.contains(&"project:calc".to_string()));
    assert!(!shown("cc:calc", &s).contains(&"project:calc".to_string()));
    assert!(shown("cc:calc", &start).contains(&"project:calc".to_string()));

    // Remove of a user tag: out of user, into off.
    let mine = state(&[], &["topic:mine", "topic:other-one"], &[]);
    let s = apply_edit(&mine, "s3-only", &[], &v(&["topic:mine"])).unwrap();
    assert_eq!(
        (s.user, s.off),
        (v(&["topic:other-one"]), v(&["topic:mine"]))
    );
}

#[test]
fn add_and_remove_of_one_tag_in_one_edit_is_invalid() {
    let start = TagState::default();
    let err = apply_edit(&start, "s3-only", &v(&["tech:go"]), &v(&["tech:go"])).unwrap_err();
    assert_eq!(err.reason, Reason::AddAndRemove);
    assert_eq!(err.tag.as_deref(), Some("tech:go"));
    // The same comparison key in another spelling is the same tag.
    let err = apply_edit(&start, "s3-only", &v(&["tech:s3"]), &v(&["Tech:AWS-S3"])).unwrap_err();
    assert_eq!(err.reason, Reason::AddAndRemove);
    assert!(
        err.message().contains("add and remove"),
        "{}",
        err.message()
    );
}

#[test]
fn a_replay_changes_nothing() {
    let start = state(&["tech:python"], &["topic:mine"], &[]);
    let edit = |s: &TagState| {
        apply_edit(
            s,
            "cc:calc",
            &v(&["tech:go"]),
            &v(&["tech:python", "topic:mine"]),
        )
        .unwrap()
    };
    let once = edit(&start);
    assert_ne!(once, start);
    assert_eq!(edit(&once), once, "the same edit twice is the same state");
    // An empty edit and an add of a tag already held (in another spelling) change nothing.
    assert_eq!(apply_edit(&start, "s3-only", &[], &[]).unwrap(), start);
    let held = state(&[], &["tech:aws-s3"], &[]);
    assert_eq!(
        apply_edit(&held, "s3-only", &v(&["tech:s3"]), &[]).unwrap(),
        held
    );
}

#[test]
fn the_derived_project_is_stored_nowhere() {
    // Adding back the derived project only clears its removal; it is not copied into the user list.
    let removed = state(&[], &[], &["project:calc"]);
    let s = apply_edit(&removed, "cc:calc", &v(&["project:calc"]), &[]).unwrap();
    assert_eq!((s.user.clone(), s.off.clone()), (vec![], vec![]));
    assert_eq!(shown("cc:calc", &s), v(&["project:calc"]));
    // A copy that an older writer left in the user list is cleaned up by the same edit.
    let legacy = state(&[], &["project:calc"], &[]);
    let s = apply_edit(&legacy, "cc:calc", &v(&["project:calc"]), &[]).unwrap();
    assert!(s.user.is_empty());
    // On a run with another repository, or none, the same tag is the person's own and is stored.
    let s = apply_edit(&TagState::default(), "s3-only", &v(&["project:calc"]), &[]).unwrap();
    assert_eq!(s.user, v(&["project:calc"]));
    let s = apply_edit(&TagState::default(), "cc:other", &v(&["project:calc"]), &[]).unwrap();
    assert_eq!(s.user, v(&["project:calc"]));
}

#[test]
fn typed_tags_are_normalised_and_bad_ones_are_invalid_with_the_rule_text() {
    let s = apply_edit(
        &TagState::default(),
        "s3-only",
        &v(&["Topic:CSV Export", "refactoring"]),
        &v(&["TECH:Python_3"]),
    )
    .unwrap();
    assert_eq!(s.user, v(&["topic:csv-export", "topic:refactoring"]));
    assert_eq!(s.off, v(&["tech:python-3"]));

    for (word, reason) in [
        ("foo:bar", Reason::UnknownKind),
        ("topic:verified-login", Reason::OutcomeToken),
        ("topic:blocked", Reason::StatusWord),
        ("日本語", Reason::NonAscii),
        ("topic:", Reason::EmptyValue),
        ("", Reason::RawShape),
    ] {
        let err = apply_edit(&TagState::default(), "s3-only", &v(&[word]), &[]).unwrap_err();
        assert_eq!(err.reason, reason, "{word:?}");
        assert!(
            err.message().starts_with(reason.message()),
            "{}",
            err.message()
        );
        // A removal is judged by the same rules: nothing outside the grammar can be written to the off list.
        let err = apply_edit(&TagState::default(), "s3-only", &[], &v(&[word])).unwrap_err();
        assert_eq!(err.reason, reason, "{word:?} removed");
    }
}

#[test]
fn caps_reject_and_never_evict() {
    let twelve: Vec<String> = (0..MAX_USER)
        .map(|i| format!("topic:mine-{i:02}"))
        .collect();
    let full_user = TagState {
        user: twelve.clone(),
        ..TagState::default()
    };
    let err = apply_edit(&full_user, "s3-only", &v(&["topic:one-more"]), &[]).unwrap_err();
    assert_eq!(err.reason, Reason::UserCap);
    assert_eq!(full_user.user, twelve, "the list is unchanged");
    // A tag already held, or a removal, still works at the cap.
    assert_eq!(
        apply_edit(&full_user, "s3-only", &v(&["topic:mine-03"]), &[]).unwrap(),
        full_user
    );
    let s = apply_edit(
        &full_user,
        "s3-only",
        &v(&["topic:one-more"]),
        &v(&["topic:mine-00"]),
    )
    .unwrap();
    assert_eq!(s.user.len(), MAX_USER);
    assert!(
        !s.user.contains(&"topic:mine-00".to_string())
            && s.user.contains(&"topic:one-more".to_string())
    );

    let thirty_two: Vec<String> = (0..MAX_OFF).map(|i| format!("tech:gone-{i:02}")).collect();
    let full_off = TagState {
        off: thirty_two.clone(),
        ..TagState::default()
    };
    let err = apply_edit(&full_off, "s3-only", &[], &v(&["tech:the-33rd"])).unwrap_err();
    assert_eq!(err.reason, Reason::OffCap);
    assert_eq!(full_off.off, thirty_two);
    // A removal of a tag already on the list, and an add that clears an entry, work at the cap.
    assert_eq!(
        apply_edit(&full_off, "s3-only", &[], &v(&["tech:gone-05"])).unwrap(),
        full_off
    );
    let s = apply_edit(&full_off, "s3-only", &v(&["tech:gone-05"]), &[]).unwrap();
    assert_eq!(s.off.len(), MAX_OFF - 1);
    // Moving a user tag to the off list at the off cap is refused whole.
    let both = TagState {
        user: v(&["topic:mine"]),
        off: thirty_two,
        ..TagState::default()
    };
    let err = apply_edit(&both, "s3-only", &[], &v(&["topic:mine"])).unwrap_err();
    assert_eq!(err.reason, Reason::OffCap);
}

#[test]
fn an_edit_holds_at_most_twelve_tags() {
    let many: Vec<String> = (0..13).map(|i| format!("topic:t-{i:02}")).collect();
    let err = apply_edit(&TagState::default(), "s3-only", &many, &[]).unwrap_err();
    assert_eq!(err.reason, Reason::TooMany);
    let (add, remove) = many.split_at(7);
    assert_eq!(
        apply_edit(&TagState::default(), "s3-only", add, remove)
            .unwrap_err()
            .reason,
        Reason::TooMany
    );
    assert!(apply_edit(&TagState::default(), "s3-only", &many[..12], &[]).is_ok());
}

#[test]
fn tag_words_parse() {
    let w = parse_tag_words("+project:x -tech:y topic:z").unwrap();
    assert_eq!(
        (w.add, w.remove),
        (v(&["project:x", "topic:z"]), v(&["tech:y"]))
    );
    // A bare word is a topic; words are normalised; a repeat is kept once.
    let w = parse_tag_words("csv +Tech:K8s -  ").unwrap_err();
    assert_eq!(w, Reason::NoTag);
    let w = parse_tag_words("csv +Tech:K8s +tech:k8s -Topic:Old").unwrap();
    assert_eq!(
        (w.add, w.remove),
        (v(&["topic:csv", "tech:k8s"]), v(&["topic:old"]))
    );
    // `-` alone, `+` alone: no tag after the sign.
    assert_eq!(parse_tag_words("-").unwrap_err(), Reason::NoTag);
    assert_eq!(parse_tag_words("+x +").unwrap_err(), Reason::NoTag);
    // More than 12 words is an error; 12 is the most.
    let twelve = (0..12)
        .map(|i| format!("+topic:t-{i:02}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(parse_tag_words(&twelve).unwrap().add.len(), 12);
    assert_eq!(
        parse_tag_words(&format!("{twelve} +topic:t-12")).unwrap_err(),
        Reason::TooMany
    );
    // A word outside the grammar names its rule.
    assert_eq!(
        parse_tag_words("+foo:bar").unwrap_err(),
        Reason::UnknownKind
    );
    assert_eq!(parse_tag_words("+日本語").unwrap_err(), Reason::NonAscii);
    assert_eq!(
        parse_tag_words("-verified-login").unwrap_err(),
        Reason::OutcomeToken
    );
    // Nothing typed is no edit.
    let w = parse_tag_words("  ").unwrap();
    assert!(w.add.is_empty() && w.remove.is_empty());
}

#[test]
fn inherit_copies_user_and_off_and_lists_the_parents_auto_tags() {
    let parent = state(
        &["tech:delta", "topic:epsilon-five"],
        &["topic:alpha-one", "topic:beta-two"],
        &["tech:gamma"],
    );
    let i = inherit(&parent);
    assert_eq!(i.user, v(&["topic:alpha-one", "topic:beta-two"]));
    assert_eq!(i.off, v(&["tech:gamma"]));
    assert_eq!(i.auto, v(&["tech:delta", "topic:epsilon-five"]));

    // The caps hold.
    let big = TagState {
        auto: (0..8).map(|i| format!("tech:auto-{i}")).collect(),
        user: (0..15).map(|i| format!("topic:user-{i:02}")).collect(),
        off: (0..40).map(|i| format!("tech:off-{i:02}")).collect(),
    };
    let i = inherit(&big);
    assert_eq!(
        (i.auto.len(), i.user.len(), i.off.len()),
        (MAX_AUTO, MAX_USER, MAX_OFF)
    );
    assert_eq!(i.user, big.user[..MAX_USER].to_vec());
    assert_eq!(inherit(&TagState::default()).user, Vec::<String>::new());
}

fn pool() -> Vec<&'static str> {
    vec![
        "topic:alpha",
        "topic:beta",
        "tech:s3",
        "tech:aws-s3",
        "tech:python",
        "project:calc",
        "project:billing",
        "topic:unit-tests",
        "topic:unit-testing",
        "topic:testing",
    ]
}

proptest! {
    #[test]
    fn a_removal_wins_and_an_add_shows(
        auto in proptest::collection::vec(proptest::sample::select(pool()), 0..5),
        user in proptest::collection::vec(proptest::sample::select(pool()), 0..5),
        off in proptest::collection::vec(proptest::sample::select(pool()), 0..5),
        add in proptest::collection::vec(proptest::sample::select(pool()), 0..4),
        remove in proptest::collection::vec(proptest::sample::select(pool()), 0..4),
    ) {
        let start = TagState { auto: v(&auto), user: v(&user), off: v(&off) };
        let (add, remove) = (v(&add), v(&remove));
        if let Ok(next) = apply_edit(&start, "cc:calc", &add, &remove) {
            let list = shown("cc:calc", &next);
            let keys: Vec<String> = list.iter().map(|t| key_of(t)).collect();
            for t in &remove {
                prop_assert!(!keys.contains(&key_of(t)), "removed {} is shown in {:?}", t, list);
            }
            for t in &add {
                prop_assert!(keys.contains(&key_of(t)), "added {} is missing in {:?}", t, list);
            }
            // A replay of the edit is a no-op.
            prop_assert_eq!(apply_edit(&next, "cc:calc", &add, &remove).unwrap(), next);
        }
    }
}
