//! The comparison key of a tag: two spellings of one idea have one key. The groups below were measured over 150 distinct tags
//! of 38 recorded runs; each is a true duplicate.
#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::runtag::grammar::{STOP_WORDS, SUFFIXES, VENDOR_PREFIXES, key_of};

fn same(a: &str, b: &str) {
    assert_eq!(key_of(a), key_of(b), "{a} and {b} should have one key");
}

fn differ(a: &str, b: &str) {
    assert_ne!(
        key_of(a),
        key_of(b),
        "{a} and {b} should keep their own keys"
    );
}

#[test]
fn comparison_key_joins_the_eight_measured_groups() {
    same("tech:aws-s3", "tech:s3");
    same("tech:aws-cdk", "tech:cdk");
    same("tech:aws-lambda", "tech:lambda");
    same("tech:aws-codepipeline", "tech:codepipeline");
    same("topic:api-key", "topic:api-keys");
    same("topic:integration-test", "topic:integration-testing");
    same("topic:unit-testing", "topic:unit-tests");
    same("topic:photo-rename", "topic:photo-renamer");
    same("topic:photo-renamer", "topic:photo-renaming");
    same("topic:photo-rename", "topic:photo-renaming");
}

#[test]
fn related_but_different_tags_keep_their_keys() {
    differ("tech:java", "tech:javascript");
    differ("topic:api", "topic:api-keys");
    differ("tech:spring", "tech:spring-boot");
    differ("topic:s3", "tech:s3");
    differ("tech:python", "tech:pytest");
}

#[test]
fn the_key_ignores_case_word_order_stop_words_and_repeats() {
    same("tech:AWS-S3", "tech:s3");
    same("topic:csv-export", "topic:export-csv");
    same("topic:rate-limiting-of-the-api", "topic:api-rate-limiting");
    same("topic:csv-csv-export", "topic:csv-export");
    same("topic:Export", "topic:export");
}

#[test]
fn the_vendor_prefixes_are_removed_for_tech_only() {
    same("tech:google-cloud", "tech:cloud");
    same("tech:azure-functions", "tech:functions");
    differ("topic:aws-billing", "topic:billing");
    // A tag that is only a vendor prefix keeps it, so it does not join every other vendor.
    differ("tech:aws", "tech:azure");
    differ("tech:aws", "tech:gcp");
}

#[test]
fn the_suffix_is_stripped_only_when_three_letters_remain() {
    same("topic:uses", "topic:use");
    same("topic:boxes", "topic:box");
    differ("topic:go", "topic:goe");
    // Short words stay whole.
    assert_eq!(key_of("tech:go"), "tech:go");
    assert_eq!(key_of("tech:s3"), "tech:s3");
}

#[test]
fn the_key_is_total_and_never_panics() {
    for junk in [
        "",
        ":",
        "nocolon",
        "::",
        "topic:",
        "topic:--",
        "日本語",
        "topic:日本語",
        "tech:\u{1b}[31m",
        &"x".repeat(5000),
    ] {
        let _ = key_of(junk);
    }
    assert_eq!(key_of("topic:"), key_of("topic:--"));
    assert_ne!(key_of("topic:x"), key_of("tech:x"));
}

#[test]
fn the_key_tables_are_frozen() {
    // Keys are recomputed on every read from the stored spellings: a change of any table splits a key that is already stored
    // (a removed `tech:aws-s3` would show again). Changing one is a migration, not an edit.
    assert_eq!(
        STOP_WORDS,
        ["and", "of", "the", "for", "to", "in", "a", "with"]
    );
    assert_eq!(VENDOR_PREFIXES, ["aws", "amazon", "google", "gcp", "azure"]);
    assert_eq!(SUFFIXES, ["ings", "ing", "ers", "er", "ed", "es", "s", "e"]);
}

#[test]
fn only_ascii_letters_and_digits_make_a_token() {
    // A character outside ASCII separates words, so the key does not depend on a Unicode table.
    same("tech:go\u{2013}lang", "tech:go-lang");
    same("topic:caf\u{e9}-bar", "topic:caf-bar");
    same("topic:ab\u{661}\u{662}", "topic:ab");
}
