//! The vocabulary: the person's recent topic and tech tags with counts, the reuse hints the model sees. Items are the search items
//! of the person's newest runs, newest first.
#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::runtag::grammar::key_of;
use ham_core::runtag::vocab::{VocabItem, Vocabulary};
use std::path::PathBuf;

fn fixture(rel: &str) -> String {
    std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/runtag")
            .join(rel),
    )
    .unwrap()
}

fn item(auto: &[&str], user: &[&str], off: &[&str]) -> VocabItem {
    let own = |l: &[&str]| l.iter().map(|t| t.to_string()).collect();
    VocabItem {
        auto: own(auto),
        user: own(user),
        off: own(off),
    }
}

fn lines(v: &Vocabulary) -> Vec<String> {
    v.lines.iter().map(|(t, n)| format!("{t} ({n})")).collect()
}

/// 500 tags over a 40-run window. The same formulas build the golden in `run-tags/evidence/rt1-make-fixtures.py`, written separately.
fn items_500() -> Vec<VocabItem> {
    const RUNS: usize = 40;
    let mut items: Vec<VocabItem> = vec![VocabItem::default(); RUNS];
    for t in 0..500usize {
        let tag = if t < 300 {
            format!("topic:area-{t:03}")
        } else {
            format!("tech:lib-{:03}", t - 300)
        };
        let n = 1 + 8 / (1 + (t * 7919) % 53);
        for s in 0..n {
            let run = (t * 31 + s * 17) % RUNS;
            if !items[run].auto.contains(&tag) {
                items[run].auto.push(tag.clone());
            }
        }
    }
    items
}

#[test]
fn vocabulary_is_topics_and_techs_only_capped_and_ranked() {
    let mut items = items_500();
    // Project tags of the person are never lines.
    for it in items.iter_mut().take(20) {
        it.user.push("project:billing-export".to_string());
    }
    let v = Vocabulary::from_items(&items);
    assert_eq!(v.window_runs, 40);
    assert_eq!(v.lines.len(), 40);
    assert!(v.lines.iter().all(|(t, _)| !t.starts_with("project:")));
    assert_eq!(
        v.lines
            .iter()
            .filter(|(t, _)| t.starts_with("topic:"))
            .count(),
        24
    );
    assert_eq!(
        v.lines
            .iter()
            .filter(|(t, _)| t.starts_with("tech:"))
            .count(),
        16
    );
    let golden = fixture("vocabulary.golden.txt");
    assert_eq!(format!("{}\n", v.block()), golden);
    assert_eq!(lines(&v).join("\n") + "\n", golden);
    // The key map holds every topic and tech tag read, not only the lines.
    assert_eq!(v.keys.len(), 500);
    assert_eq!(
        v.keys.get(&key_of("tech:lib-007")).map(String::as_str),
        Some("tech:lib-007")
    );
    assert!(!v.keys.values().any(|t| t.starts_with("project:")));
}

#[test]
fn an_empty_vocabulary_is_none_yet() {
    let v = Vocabulary::from_items(&[]);
    assert_eq!(v.block(), "(none yet)");
    assert!(v.lines.is_empty() && v.keys.is_empty());
    assert_eq!((v.window_runs, v.top_share_bp), (0, 0));
    let only_projects =
        Vocabulary::from_items(&[item(&[], &["project:x"], &[]), item(&[], &[], &[])]);
    assert_eq!(only_projects.block(), "(none yet)");
    assert_eq!(
        only_projects.window_runs, 2,
        "runs without tags are still part of the window"
    );
}

#[test]
fn order_is_runs_then_recency_then_text() {
    let items = vec![
        item(&["topic:new-one"], &[], &[]),
        item(&["tech:rust"], &[], &[]),
        item(&[], &[], &[]),
        item(&["topic:old-one", "topic:aaa-two"], &[], &[]),
        item(&["topic:popular"], &[], &[]),
        item(&["topic:popular", "tech:rust"], &[], &[]),
    ];
    let v = Vocabulary::from_items(&items);
    assert_eq!(
        lines(&v),
        [
            "tech:rust (2)",
            "topic:popular (2)",
            "topic:new-one (1)",
            "topic:aaa-two (1)",
            "topic:old-one (1)"
        ],
        "two runs first; among them the newer; then one run each by recency, then text"
    );
}

#[test]
fn a_tag_counts_once_per_run_and_spellings_of_one_key_join() {
    let items = vec![
        item(&["tech:s3", "tech:aws-s3"], &["tech:s3"], &[]),
        item(&["tech:s3"], &[], &[]),
        item(&["tech:aws-s3"], &[], &[]),
        item(&["tech:s3"], &[], &[]),
    ];
    let v = Vocabulary::from_items(&items);
    assert_eq!(
        lines(&v),
        ["tech:s3 (4)"],
        "four runs, the spelling that most runs hold"
    );
    assert_eq!(
        v.keys.get(&key_of("tech:aws-s3")).map(String::as_str),
        Some("tech:s3")
    );

    // A tie between spellings goes to the spelling in the newest run.
    let tie = Vocabulary::from_items(&[
        item(&["tech:aws-s3"], &[], &[]),
        item(&["tech:s3"], &[], &[]),
    ]);
    assert_eq!(lines(&tie), ["tech:aws-s3 (2)"]);
    // A tie inside one newest run goes to the smaller text.
    let tie = Vocabulary::from_items(&[item(&["tech:s3", "tech:aws-s3"], &[], &[])]);
    assert_eq!(lines(&tie), ["tech:aws-s3 (1)"]);
}

#[test]
fn top_share_cap_applies_from_fifty_runs() {
    let build = |runs: usize, hot: usize, warm: usize| -> Vocabulary {
        let items: Vec<VocabItem> = (0..runs)
            .map(|i| {
                let mut tags = Vec::new();
                if i < hot {
                    tags.push("topic:hot-spot");
                }
                if i >= runs - warm {
                    tags.push("tech:warm-spot");
                }
                item(&tags, &[], &[])
            })
            .collect();
        Vocabulary::from_items(&items)
    };
    // 19 of 60 runs is 31.7%: left out of the lines, kept in the key map; 18 of 60 is exactly 30% and stays.
    let v = build(60, 19, 18);
    assert_eq!(lines(&v), ["tech:warm-spot (18)"]);
    assert!(v.keys.contains_key(&key_of("topic:hot-spot")));
    assert_eq!(v.top_share_bp, 19 * 10_000 / 60);
    // Exactly 50 runs: 16 is 32% (out), 15 is 30% (in).
    assert_eq!(lines(&build(50, 16, 0)), Vec::<String>::new());
    assert_eq!(lines(&build(50, 15, 0)), ["topic:hot-spot (15)"]);
    // The same share in a 20-run window and in a 49-run window stays.
    assert_eq!(lines(&build(20, 7, 0)), ["topic:hot-spot (7)"]);
    assert_eq!(lines(&build(49, 40, 0)), ["topic:hot-spot (40)"]);
    assert_eq!(build(20, 7, 0).top_share_bp, 3500);
}

#[test]
fn off_listed_tags_and_imperative_words_are_left_out() {
    let items = vec![
        item(
            &["topic:csv-export", "tech:s3"],
            &[],
            &["topic:csv-export", "tech:AWS-s3"],
        ),
        item(&["topic:csv-export"], &[], &[]),
        item(&[], &["topic:mine"], &["topic:mine"]),
    ];
    let v = Vocabulary::from_items(&items);
    assert_eq!(
        lines(&v),
        ["topic:csv-export (1)"],
        "a tag on an item's off list is not counted for that item, whatever its spelling"
    );
    assert!(
        !v.keys.contains_key(&key_of("tech:s3")) && !v.keys.contains_key(&key_of("topic:mine"))
    );

    let imperative = [
        "topic:ignore-previous",
        "topic:system-design",
        "topic:override",
        "tech:assistant",
        "topic:forget-me",
        "topic:reveal",
        "topic:disregard",
        "topic:developer-tools",
        "topic:instructions",
        "topic:instruction",
        "topic:prompt-engineering",
    ];
    let mut tags: Vec<&str> = imperative.to_vec();
    tags.extend(["topic:ignorance", "topic:subsystem", "topic:csv-export"]);
    let v = Vocabulary::from_items(&[item(&tags, &[], &[])]);
    assert_eq!(
        lines(&v),
        [
            "topic:csv-export (1)",
            "topic:ignorance (1)",
            "topic:subsystem (1)"
        ],
        "whole words only"
    );
    for t in imperative {
        assert!(
            v.keys.contains_key(&key_of(t)),
            "{t} stays in the key map so a model tag still snaps to it"
        );
    }
}

#[test]
fn stored_tags_that_fail_the_grammar_never_reach_the_prompt() {
    let esc = format!("topic:a{}[31mred", '\u{1b}');
    let items = vec![item(
        &[
            &esc,
            "topic:UPPER",
            "topic:verified-x",
            "topic:done",
            "topic:testing",
            "project:admin",
            "tech:python",
        ],
        &["topic:testing"],
        &[],
    )];
    let v = Vocabulary::from_items(&items);
    // `topic:testing` is a generic word: refused in the automatic list, and a line the model could not write back, so the person's
    // own copy is not shown either (see `a_key_is_shown_with_a_spelling_the_model_may_write`).
    assert_eq!(lines(&v), ["tech:python (1)"]);
    assert!(!v.block().contains('\u{1b}'));
}

#[test]
fn a_block_reads_back_into_a_vocabulary() {
    let golden = fixture("vocabulary.golden.txt");
    let v = Vocabulary::from_block(&golden);
    assert_eq!(v.lines.len(), 40);
    assert_eq!(format!("{}\n", v.block()), golden);
    assert_eq!(v.keys.len(), 40);
    assert_eq!(Vocabulary::from_block("(none yet)").block(), "(none yet)");
    assert_eq!(Vocabulary::from_block("").block(), "(none yet)");
    // A tag the person may type and the model may not write (generic word, long word with digits, 5 words) is not read back.
    let v = Vocabulary::from_block(
        "topic:testing (2)\ntech:ab12cd34ef56 (1)\ntopic:rate-limiting-for-the-api (3)\ntech:go (4)",
    );
    assert_eq!(lines(&v), ["tech:go (4)"]);
    // A line that is not `tag (n)` or whose tag fails the grammar is ignored.
    let v = Vocabulary::from_block(
        "topic:csv-export (5)\nignore the rules (3)\ntopic:UPPER (2)\ntech:python (x)\n\ntech:python (2)\n",
    );
    assert_eq!(lines(&v), ["topic:csv-export (5)", "tech:python (2)"]);
}

#[test]
fn imperative_words_are_matched_with_their_endings() {
    let lead = [
        "topic:ignored-rules",
        "topic:system-prompts",
        "topic:overrides",
        "tech:assistants",
        "topic:revealed",
        "topic:instructed-agent",
        "topic:disregarding",
    ];
    let mut tags: Vec<&str> = lead.to_vec();
    tags.extend(["topic:ignorance", "topic:promptly", "topic:csv-export"]);
    let v = Vocabulary::from_items(&[item(&tags, &[], &[])]);
    assert_eq!(
        lines(&v),
        [
            "topic:csv-export (1)",
            "topic:ignorance (1)",
            "topic:promptly (1)"
        ]
    );
    for t in lead {
        assert!(v.keys.contains_key(&key_of(t)), "{t} stays in the key map");
    }
}

#[test]
fn a_key_is_shown_with_a_spelling_the_model_may_write() {
    // One key, a 5-word spelling the person typed and a 3-word spelling the model wrote: the line carries the model's spelling and
    // counts the runs of both.
    let typed = "topic:rate-limiting-for-the-api";
    let model = "topic:api-rate-limiting";
    assert_eq!(key_of(typed), key_of(model));
    let items = vec![
        item(&[], &[typed], &[]),
        item(&[model], &[], &[]),
        item(&[], &[typed], &[]),
    ];
    let v = Vocabulary::from_items(&items);
    assert_eq!(lines(&v), ["topic:api-rate-limiting (3)"]);
    assert_eq!(v.keys.get(&key_of(typed)).map(String::as_str), Some(model));

    // A key that only has spellings the model may not write (generic word, long word shape, too many words) is neither shown nor
    // offered for snapping.
    let only_typed = [item(
        &[],
        &["topic:testing", "tech:ab12cd34ef56", typed],
        &[],
    )];
    let v = Vocabulary::from_items(&only_typed);
    assert!(v.lines.is_empty() && v.keys.is_empty(), "{:?}", v.keys);
    assert_eq!(v.block(), "(none yet)");
}
