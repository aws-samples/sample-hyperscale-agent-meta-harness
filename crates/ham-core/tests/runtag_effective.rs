//! `effective`: the one function that says which tags a run shows. Derived project, then the person's tags, then the automatic
//! tags, minus the tags the person removed. The removal wins on every read.
#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::runtag::effective::effective;
use ham_core::runtag::{MAX_EFFECTIVE, grammar};
use proptest::prelude::*;

fn v(tags: &[&str]) -> Vec<String> {
    tags.iter().map(|t| t.to_string()).collect()
}

#[test]
fn effective_orders_derived_user_auto_and_subtracts_off() {
    let e = effective("cc:calc", &v(&["topic:alpha"]), &v(&["project:x"]), &[]);
    assert_eq!(e.tags, v(&["project:calc", "project:x", "topic:alpha"]));
    assert_eq!(e.dropped, 0);

    let e = effective(
        "cc:calc",
        &v(&["topic:alpha"]),
        &v(&["project:x"]),
        &v(&["topic:alpha"]),
    );
    assert_eq!(
        e.tags,
        v(&["project:calc", "project:x"]),
        "an auto tag on the off list is gone"
    );

    let e = effective(
        "cc:calc",
        &v(&["topic:alpha"]),
        &v(&["project:x"]),
        &v(&["project:calc"]),
    );
    assert_eq!(
        e.tags,
        v(&["project:x", "topic:alpha"]),
        "the derived project is gone when removed"
    );

    let e = effective(
        "s3-only",
        &v(&["tech:s3", "topic:alpha"]),
        &[],
        &v(&["tech:AWS-s3"]),
    );
    assert_eq!(
        e.tags,
        v(&["topic:alpha"]),
        "the off list is compared by key"
    );

    let e = effective(
        "cc:calc",
        &[],
        &v(&["project:x", "topic:mine"]),
        &v(&["project:x"]),
    );
    assert_eq!(
        e.tags,
        v(&["project:calc", "topic:mine"]),
        "a user tag on the off list is gone: the removal wins"
    );

    let e = effective("s3-only", &[], &[], &[]);
    assert!(
        e.tags.is_empty() && e.dropped == 0,
        "a run with no repository and no tags shows none"
    );
}

#[test]
fn effective_is_idempotent_and_dedupes_by_key_keeping_the_user_spelling() {
    let e = effective(
        "cc:calc",
        &v(&["tech:s3", "topic:csv-export"]),
        &v(&["tech:aws-s3", "topic:mine"]),
        &v(&["topic:gone"]),
    );
    assert_eq!(
        e.tags,
        v(&[
            "project:calc",
            "tech:aws-s3",
            "topic:mine",
            "topic:csv-export"
        ]),
        "one s3 tag, the user's spelling, first"
    );

    let again = effective("cc:calc", &[], &e.tags, &v(&["topic:gone"]));
    assert_eq!(again.tags, e.tags);
    assert_eq!(again.dropped, 0);
    let without_off = effective("cc:calc", &[], &e.tags, &[]);
    assert_eq!(without_off.tags, e.tags);
}

#[test]
fn stored_tags_that_fail_the_policy_are_not_shown_and_are_counted() {
    let esc = format!("topic:a{}[31mred", '\u{1b}');
    let auto = vec![
        esc,
        "topic:UPPER".to_string(),
        "topic:verified-x".to_string(),
        "topic:done".to_string(),
        "tech:python".to_string(),
    ];
    let e = effective("s3-only", &auto, &[], &[]);
    assert_eq!(e.tags, v(&["tech:python"]));
    assert_eq!(e.dropped, 4);

    // A generic word fails for an automatic tag and passes when the person typed it.
    let e = effective("s3-only", &v(&["topic:testing"]), &[], &[]);
    assert!(e.tags.is_empty() && e.dropped == 1);
    let e = effective("s3-only", &[], &v(&["topic:testing"]), &[]);
    assert_eq!(e.tags, v(&["topic:testing"]));
    // The model never wrote a project: a stored auto project tag is not shown.
    let e = effective("s3-only", &v(&["project:admin"]), &[], &[]);
    assert!(e.tags.is_empty() && e.dropped == 1);
    // A user tag with an outcome word is not shown either.
    let e = effective("s3-only", &[], &v(&["topic:tested-code"]), &[]);
    assert!(e.tags.is_empty() && e.dropped == 1);
}

#[test]
fn a_run_shows_at_most_nineteen_tags_and_the_rest_are_counted() {
    let user: Vec<String> = (0..30).map(|i| format!("topic:mine-{i:02}")).collect();
    let e = effective("cc:calc", &[], &user, &[]);
    assert_eq!(MAX_EFFECTIVE, 19);
    assert_eq!(e.tags.len(), 19);
    assert_eq!(e.tags[0], "project:calc");
    assert_eq!(e.tags[18], "topic:mine-17");
    assert_eq!(e.dropped, 12);
    // Applying it to its own output changes nothing.
    let again = effective("cc:calc", &[], &e.tags, &[]);
    assert_eq!(again.tags, e.tags);
    assert_eq!(again.dropped, 0);
}

#[test]
fn a_very_long_junk_list_is_cheap_and_shows_nothing() {
    let junk: Vec<String> = (0..5000)
        .map(|i| format!("{}{i}", "x".repeat(200)))
        .collect();
    let e = effective("s3-only", &junk, &junk, &junk);
    assert!(e.tags.is_empty());
    assert!(e.dropped >= 5000);
}

fn pool() -> Vec<&'static str> {
    vec![
        "topic:alpha",
        "topic:beta",
        "tech:s3",
        "tech:aws-s3",
        "tech:python",
        "topic:unit-tests",
        "topic:unit-testing",
        "project:billing",
        "project:calc",
        "topic:verified-x",
        "topic:UPPER",
        "topic:testing",
        "topic:done",
        "lang:x",
        "",
    ]
}

proptest! {
    #[test]
    fn effective_of_its_own_output_changes_nothing(
        auto in proptest::collection::vec(proptest::sample::select(pool()), 0..8),
        user in proptest::collection::vec(proptest::sample::select(pool()), 0..8),
        off in proptest::collection::vec(proptest::sample::select(pool()), 0..6),
        repo in proptest::sample::select(vec!["cc:calc", "new cc:billing", "s3-only", "cc:tested"]),
    ) {
        let (auto, user, off) = (v(&auto), v(&user), v(&off));
        let e = effective(repo, &auto, &user, &off);
        prop_assert!(e.tags.len() <= MAX_EFFECTIVE);
        // Every shown tag is a valid tag for the person's list and none is removed.
        for t in &e.tags {
            prop_assert!(grammar::check_stored(t, grammar::Origin::User).is_ok(), "{}", t);
            for o in &off {
                prop_assert_ne!(grammar::key_of(t), grammar::key_of(o));
            }
        }
        let again = effective(repo, &[], &e.tags, &off);
        prop_assert_eq!(&again.tags, &e.tags);
        prop_assert_eq!(again.dropped, 0);
    }
}
