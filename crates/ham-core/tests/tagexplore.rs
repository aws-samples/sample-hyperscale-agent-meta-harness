//! The overlay on the Runs screen draws what `ham_core::tagexplore` returns: the tags of the rows the client already holds, with
//! counts that narrow as tags are picked, a find line, the age of the newest run per tag, the note line, and the plan for removing
//! one tag from every held run. Everything here is pure, so the same numbers serve the screen and the live driver's oracle.

#![allow(clippy::unwrap_used)]

use ham_core::tagexplore::{
    Held, TagIndex, TagStat, View, age_text, batches, find_matches, note_text, remove_plan,
};
use harness_protocol::common::parse_utc_unix;
use proptest::prelude::*;
use std::time::{Duration, Instant};

const NOW: &str = "2026-10-20T12:00:00Z";

fn now_unix() -> i64 {
    parse_utc_unix(NOW).unwrap()
}

/// An RFC 3339 UTC time `secs` before [`NOW`] (days from civil, Hinnant's algorithm in reverse).
fn ago(secs: i64) -> String {
    rfc3339(now_unix() - secs)
}

fn rfc3339(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let rem = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

const MIN: i64 = 60;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

fn held(id: &str, secs_ago: i64, tags: &[&str]) -> Held {
    Held {
        run_id: id.to_string(),
        created_at: ago(secs_ago),
        tags: tags.iter().map(ToString::to_string).collect(),
    }
}

fn picks(p: &[&str]) -> Vec<String> {
    p.iter().map(ToString::to_string).collect()
}

fn summary(v: &View) -> Vec<(String, usize, String)> {
    v.rows
        .iter()
        .map(|r| (r.tag.clone(), r.runs, r.last.clone()))
        .collect()
}

fn tag_names(v: &View) -> Vec<&str> {
    v.rows.iter().map(|r| r.tag.as_str()).collect()
}

fn stat(tag: &str, runs: usize, last: &str) -> (String, usize, String) {
    (tag.to_string(), runs, last.to_string())
}

#[test]
fn the_test_clock_helper_round_trips() {
    for s in [0, 1, 59, DAY, 400 * DAY, 20_000 * DAY] {
        assert_eq!(parse_utc_unix(&rfc3339(s)), Some(s), "{s}");
    }
    assert_eq!(rfc3339(now_unix()), NOW);
    assert_eq!(ago(5 * MIN), "2026-10-20T11:55:00Z");
}

// ---- counts -------------------------------------------------------------------------------------------------------------------------

#[test]
fn counts_over_all_rows_with_no_picks() {
    let rows = [
        held(
            "r1",
            5 * MIN,
            &["project:billing", "tech:python", "topic:csv"],
        ),
        held("r2", 2 * DAY, &["tech:python", "topic:csv"]),
        held("r3", 3 * HOUR, &["tech:python", "tech:go"]),
        held("r4", 7 * DAY, &[]),
    ];
    let index = TagIndex::build(&rows);
    let v = index.view(&[], "", NOW);
    // runs descending, then the newest run first, then the tag text.
    assert_eq!(
        summary(&v),
        [
            stat("tech:python", 3, "5m"),
            stat("topic:csv", 2, "5m"),
            stat("project:billing", 1, "5m"),
            stat("tech:go", 1, "3h"),
        ]
    );
    assert_eq!(v.matched, 4);
    assert_eq!(v.held, 4);
    assert_eq!(v.tag_count, 4);
    assert_eq!(v.untagged, 1);
    assert_eq!(index.matching(&[]), ["r1", "r2", "r3", "r4"]);
}

#[test]
fn order_is_runs_then_recency_then_text() {
    let rows = [
        // two tags on one run: equal runs, equal recency, so the text decides (a before b).
        held("x1", HOUR, &["topic:b", "topic:a"]),
        // the same second as x1: still a tie on recency, so c follows a and b.
        held("x2", HOUR, &["topic:c"]),
        // an hour older: after the three above.
        held("x3", 2 * HOUR, &["topic:d", "topic:e"]),
        // fewer runs but newer, against more runs and older: the run count wins.
        held("y1", 30 * DAY, &["tech:many"]),
        held("y2", 30 * DAY, &["tech:many"]),
        held("y3", MIN, &["tech:new"]),
        // equal runs, the newer run first even though its text sorts later.
        held("z1", 10 * MIN, &["tech:p"]),
        held("z2", 20 * MIN, &["tech:o"]),
    ];
    let v = TagIndex::build(&rows).view(&[], "", NOW);
    assert_eq!(
        tag_names(&v),
        [
            "tech:many",
            "tech:new",
            "tech:p",
            "tech:o",
            "topic:a",
            "topic:b",
            "topic:c",
            "topic:d",
            "topic:e",
        ]
    );
    // a second build of the same rows in another order gives the same view.
    let mut shuffled = rows.to_vec();
    shuffled.reverse();
    assert_eq!(TagIndex::build(&shuffled).view(&[], "", NOW), v);
}

#[test]
fn picks_are_and_and_counts_are_faceted() {
    let rows = [
        held("a", 5 * MIN, &["x", "y"]),
        held("b", 2 * HOUR, &["x", "z"]),
        held("c", 3 * HOUR, &["y", "z"]),
        held("d", 4 * HOUR, &["x", "y", "z"]),
        held("e", DAY, &["w"]),
    ];
    let index = TagIndex::build(&rows);

    let none = index.view(&[], "", NOW);
    assert_eq!(
        summary(&none),
        [
            stat("x", 3, "5m"),
            stat("y", 3, "5m"),
            stat("z", 3, "2h"),
            stat("w", 1, "1d"),
        ]
    );

    let p = picks(&["x"]);
    let v = index.view(&p, "", NOW);
    // x is picked, so it leads; y and z count only inside the runs that hold x; w is in none of them and is hidden.
    assert_eq!(
        summary(&v),
        [stat("x", 3, "5m"), stat("y", 2, "5m"), stat("z", 2, "2h"),]
    );
    assert_eq!((v.matched, v.held, v.tag_count, v.untagged), (3, 5, 3, 0));
    assert_eq!(v.matched, index.matching(&p).len());
    assert_eq!(index.matching(&p), ["a", "b", "d"]);

    let p = picks(&["x", "y"]);
    let v = index.view(&p, "", NOW);
    assert_eq!(
        summary(&v),
        [stat("x", 2, "5m"), stat("y", 2, "5m"), stat("z", 1, "4h"),]
    );
    assert_eq!(index.matching(&p), ["a", "d"]);
    assert_eq!(v.matched, 2);

    // the order of the picks does not matter, and a pick named twice is one pick.
    assert_eq!(index.view(&picks(&["y", "x"]), "", NOW), v);
    assert_eq!(index.view(&picks(&["x", "y", "x"]), "", NOW), v);
    assert_eq!(index.matching(&picks(&["y", "x", "y"])), ["a", "d"]);
}

#[test]
fn a_picked_tag_leads_even_when_others_have_more_runs() {
    let rows = [
        held("a", HOUR, &["tech:big", "tech:small"]),
        held("b", HOUR, &["tech:big"]),
        held("c", HOUR, &["tech:big"]),
        held("d", HOUR, &["tech:small", "tech:big"]),
    ];
    let index = TagIndex::build(&rows);
    let v = index.view(&picks(&["tech:small"]), "", NOW);
    assert_eq!(tag_names(&v), ["tech:small", "tech:big"]);
    assert_eq!(v.rows[0].runs, 2);
    assert_eq!(v.rows[1].runs, 2);
}

#[test]
fn a_pick_that_matches_nothing_stays_visible_so_it_can_be_dropped() {
    let rows = [held("a", HOUR, &["x"]), held("b", HOUR, &["w"])];
    let index = TagIndex::build(&rows);
    // x and w never meet: no run matches, and the other tags are hidden, but both picks stay.
    let p = picks(&["x", "w"]);
    let v = index.view(&p, "", NOW);
    assert_eq!(v.matched, 0);
    assert!(index.matching(&p).is_empty());
    assert_eq!(summary(&v), [stat("w", 0, ""), stat("x", 0, "")]);
    assert_eq!(v.tag_count, 2);
    // a pick that no row holds at all (a tag removed everywhere while it was picked) is shown the same way.
    let p = picks(&["tech:gone"]);
    let v = index.view(&p, "", NOW);
    assert_eq!(v.matched, 0);
    assert_eq!(summary(&v), [stat("tech:gone", 0, "")]);
    assert!(index.matching(&p).is_empty());
}

#[test]
fn find_does_not_change_the_runs_or_the_counts() {
    let rows = [
        held("a", HOUR, &["topic:csv-export", "tech:python"]),
        held("b", 2 * HOUR, &["topic:csv-export", "tech:go"]),
        held("c", 3 * HOUR, &["tech:python"]),
    ];
    let index = TagIndex::build(&rows);
    let all = index.view(&[], "", NOW);
    let found = index.view(&[], "csv", NOW);
    assert_eq!(summary(&found), [stat("topic:csv-export", 2, "1h")]);
    assert_eq!(found.matched, all.matched);
    assert_eq!(found.held, all.held);
    assert_eq!(found.untagged, all.untagged);
    // the tag count is the count before the find narrowed the table, so the note does not jump while someone types.
    assert_eq!(found.tag_count, all.tag_count);
    let p = picks(&["tech:python"]);
    let faceted = index.view(&p, "csv", NOW);
    assert_eq!(
        summary(&faceted),
        [
            stat("tech:python", 2, "1h"),
            stat("topic:csv-export", 1, "1h")
        ]
    );
    assert_eq!(index.view(&p, "nomatch", NOW).matched, 2);
}

// ---- find ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn find_words_prefix_and_kind() {
    let rows = [
        held("1", HOUR, &["topic:csv-export"]),
        held("2", HOUR, &["topic:export-csv"]),
        held("3", HOUR, &["tech:csv"]),
        held("4", HOUR, &["project:billing-export"]),
        held("5", HOUR, &["project:shop-cart"]),
        held("6", HOUR, &["tech:python"]),
    ];
    let index = TagIndex::build(&rows);
    let names = |find: &str| {
        let mut n: Vec<String> = index
            .view(&[], find, NOW)
            .rows
            .into_iter()
            .map(|r| r.tag)
            .collect();
        n.sort();
        n
    };
    // a word is a substring, whatever the case.
    assert_eq!(
        names("csv"),
        ["tech:csv", "topic:csv-export", "topic:export-csv"]
    );
    assert_eq!(names("CSV"), names("csv"));
    assert_eq!(names("sv-ex"), ["topic:csv-export"]);
    // ^ means the tag starts so.
    assert_eq!(names("^topic:csv"), ["topic:csv-export"]);
    assert_eq!(names("^csv"), Vec::<String>::new());
    assert_eq!(names("^TECH:"), ["tech:csv", "tech:python"]);
    // a kind word lists that kind.
    assert_eq!(
        names("project:"),
        ["project:billing-export", "project:shop-cart"]
    );
    // two words must both occur, in any order.
    assert_eq!(
        names("csv export"),
        ["topic:csv-export", "topic:export-csv"]
    );
    assert_eq!(names("export csv"), names("csv export"));
    assert_eq!(names("csv python"), Vec::<String>::new());
    // blanks around and between words are nothing; a lone caret and an empty find hold nothing back.
    assert_eq!(names("   csv   "), names("csv"));
    assert_eq!(names("").len(), 6);
    assert_eq!(names("   ").len(), 6);
    assert_eq!(names("^").len(), 6);
    assert_eq!(names("^ csv"), names("csv"));
}

#[test]
fn find_matches_is_the_same_test_for_one_tag() {
    assert!(find_matches("topic:csv-export", ""));
    assert!(find_matches("topic:csv-export", "csv"));
    assert!(find_matches("topic:csv-export", "EXPORT csv"));
    assert!(find_matches("topic:csv-export", "^topic:csv"));
    assert!(find_matches("topic:csv-export", "^TOPIC: export"));
    assert!(!find_matches("topic:csv-export", "^csv"));
    assert!(!find_matches("topic:csv-export", "csv python"));
    assert!(!find_matches("topic:csv-export", "^topic:export"));
    // the Runs filter folds case the same way (a sharp s is "ss", a final sigma is a sigma).
    assert!(find_matches("topic:strasse", "STRAßE"));
    assert!(find_matches("topic:straße", "STRASSE"));
}

#[test]
fn a_picked_tag_the_find_would_hide_stays_at_the_top() {
    let rows = [
        held("a", HOUR, &["tech:python", "topic:csv-export"]),
        held("b", HOUR, &["tech:python", "tech:go"]),
    ];
    let index = TagIndex::build(&rows);
    let p = picks(&["tech:python"]);
    let v = index.view(&p, "csv", NOW);
    // python does not hold "csv", yet it is picked, so it stays first; go is hidden by the find and by the pick's facet.
    assert_eq!(tag_names(&v), ["tech:python", "topic:csv-export"]);
    let v = index.view(&p, "zzz", NOW);
    assert_eq!(tag_names(&v), ["tech:python"]);
    assert_eq!(v.matched, 2);
}

// ---- age ----------------------------------------------------------------------------------------------------------------------------

#[test]
fn last_is_the_age_of_the_newest_run_holding_the_tag() {
    let at = |secs: i64| age_text(NOW, &ago(secs));
    assert_eq!(at(59), "59s");
    assert_eq!(at(60), "1m");
    assert_eq!(at(59 * MIN), "59m");
    assert_eq!(at(60 * MIN), "1h");
    assert_eq!(at(23 * HOUR), "23h");
    assert_eq!(at(24 * HOUR), "1d");
    assert_eq!(at(13 * DAY), "13d");
    assert_eq!(at(14 * DAY), "2w");
    // the examples of the design.
    assert_eq!(at(5 * MIN), "5m");
    assert_eq!(at(3 * HOUR), "3h");
    assert_eq!(at(2 * DAY), "2d");
    assert_eq!(at(21 * DAY), "3w");
    assert_eq!(at(0), "0s");
    assert_eq!(at(3_599), "59m");
    assert_eq!(at(86_399), "23h");
    assert_eq!(at(14 * DAY + 6 * DAY + 23 * HOUR), "2w");
    assert_eq!(at(21 * DAY - 1), "2w");
    // weeks go to 99w; after that years, rounded, so five columns hold any age a timestamp can name.
    assert_eq!(at(699 * DAY), "99w");
    assert_eq!(at(700 * DAY), "2y");
    assert_eq!(at(365 * DAY), "52w");
    assert_eq!(at(10 * 365 * DAY), "10y");
    // a fraction of a second is nothing, a time in the future (a clock that runs ahead) is zero, a time that is not a time is a dash.
    assert_eq!(age_text(NOW, "2026-10-20T11:55:00.987Z"), "5m");
    assert_eq!(age_text(NOW, "2026-10-20T12:00:30Z"), "0s");
    assert_eq!(age_text(NOW, "yesterday"), "-");
    assert_eq!(age_text("", &ago(MIN)), "-");
    assert_eq!(age_text(NOW, ""), "-");

    // in the table it is the newest run that holds the tag, inside the runs that match.
    let rows = [
        held("old", 3 * DAY, &["tech:x", "tech:z"]),
        held("new", 5 * MIN, &["tech:z"]),
        held("mid", HOUR, &["tech:z", "tech:w"]),
    ];
    let index = TagIndex::build(&rows);
    let v = index.view(&[], "", NOW);
    assert_eq!(
        summary(&v),
        [
            stat("tech:z", 3, "5m"),
            stat("tech:w", 1, "1h"),
            stat("tech:x", 1, "3d"),
        ]
    );
    let v = index.view(&picks(&["tech:x"]), "", NOW);
    assert_eq!(
        summary(&v),
        [stat("tech:x", 1, "3d"), stat("tech:z", 1, "3d")]
    );
    // a clock that cannot be read leaves a dash in every cell and the order alone.
    let v = index.view(&[], "", "not a time");
    assert_eq!(
        v.rows.iter().map(|r| r.last.as_str()).collect::<Vec<_>>(),
        ["-", "-", "-"]
    );
    assert_eq!(tag_names(&v), ["tech:z", "tech:w", "tech:x"]);
}

#[test]
fn a_run_whose_time_cannot_be_read_is_the_oldest() {
    let mut odd = held("odd", 0, &["tech:a"]);
    odd.created_at = "sometime".to_string();
    let rows = [odd, held("ok", 9 * DAY, &["tech:b"])];
    let v = TagIndex::build(&rows).view(&[], "", NOW);
    assert_eq!(
        summary(&v),
        [stat("tech:b", 1, "9d"), stat("tech:a", 1, "-")]
    );
}

// ---- remove -------------------------------------------------------------------------------------------------------------------------

#[test]
fn remove_plan_names_the_runs_holding_the_tag_and_batches_by_forty() {
    let rows: Vec<Held> = (0..120)
        .map(|i| {
            let tags: &[&str] = if i % 4 == 3 {
                &["tech:other"]
            } else if i % 4 == 2 {
                &["topic:csv", "tech:other"]
            } else {
                &["topic:csv-export", "topic:csv"]
            };
            held(&format!("run{i:03}"), i64::from(i) * MIN, tags)
        })
        .collect();
    let plan = remove_plan("topic:csv", &rows);
    // 90 of 120 rows hold the exact tag (topic:csv-export is another tag), in row order.
    assert_eq!(plan.len(), 90);
    assert_eq!(plan[0], "run000");
    assert_eq!(plan[1], "run001");
    assert_eq!(plan[2], "run002");
    assert_eq!(plan[3], "run004");
    assert!(plan.windows(2).all(|w| w[0] < w[1]));
    assert!(remove_plan("tech:nobody", &rows).is_empty());
    assert!(remove_plan("topic:csv", &[]).is_empty());
    // 85 rows holding a tag give three batches of 40, 40 and 5, in order.
    let ids: Vec<String> = (0..85).map(|i| format!("r{i:02}")).collect();
    let b = batches(&ids, 40);
    assert_eq!(b.iter().map(Vec::len).collect::<Vec<_>>(), [40, 40, 5]);
    assert_eq!(b.concat(), ids);
    assert_eq!(b[2][0], "r80");
}

#[test]
fn batches_edge_cases() {
    let ids: Vec<String> = (0..5).map(|i| i.to_string()).collect();
    assert!(batches(&[], 40).is_empty());
    assert_eq!(batches(&ids, 5).len(), 1);
    assert_eq!(batches(&ids, 6).len(), 1);
    assert_eq!(batches(&ids, 1).len(), 5);
    assert_eq!(
        batches(&ids, 2).iter().map(Vec::len).collect::<Vec<_>>(),
        [2, 2, 1]
    );
    // a batch size of zero would never end; it is one at a time.
    assert_eq!(batches(&ids, 0).len(), 5);
    let eighty: Vec<String> = (0..80).map(|i| i.to_string()).collect();
    assert_eq!(batches(&eighty, 40).len(), 2);
}

#[test]
fn a_run_listed_twice_is_removed_once() {
    let rows = [
        held("a", HOUR, &["tech:x"]),
        held("a", MIN, &["tech:x"]),
        held("b", HOUR, &["tech:x"]),
    ];
    assert_eq!(remove_plan("tech:x", &rows), ["a", "b"]);
}

// ---- the note -----------------------------------------------------------------------------------------------------------------------

fn lots(n: usize, tagged: usize) -> Vec<Held> {
    (0..n)
        .map(|i| {
            let tags: &[&str] = if i < tagged { &["tech:go"] } else { &[] };
            held(&format!("r{i:04}"), i64::try_from(i).unwrap() * MIN, tags)
        })
        .collect()
}

#[test]
fn note_text_matches_the_runs_summary_style() {
    // 212 runs, 41 distinct tags (40 topics and one project), 30 runs with none.
    let mut rows: Vec<Held> = (0..182)
        .map(|i| {
            let a = format!("topic:t{:02}", i % 40);
            let tags = if i == 0 {
                vec!["project:billing-export".to_string(), a]
            } else {
                vec![a]
            };
            Held {
                run_id: format!("r{i:04}"),
                created_at: ago(i64::from(i) * MIN),
                tags,
            }
        })
        .collect();
    rows.extend((182..212).map(|i| held(&format!("r{i:04}"), i64::from(i) * MIN, &[])));
    let index = TagIndex::build(&rows);
    let v = index.view(&[], "", NOW);
    assert_eq!((v.held, v.untagged, v.tag_count), (212, 30, 41));
    assert_eq!(
        note_text(&v, &[]),
        "212 runs \u{b7} 41 tags \u{b7} 30 have no tags"
    );

    let p = picks(&["project:billing-export"]);
    let v = index.view(&p, "", NOW);
    assert_eq!((v.matched, v.tag_count), (1, 2));
    assert_eq!(
        note_text(&v, &p),
        "1 of 212 runs match +project:billing-export \u{b7} 2 tags"
    );
}

#[test]
fn note_text_for_the_design_examples() {
    // 12 of 212 runs hold project:billing-export, which co-occurs with 9 other tags: the drawing of the design.
    let mut rows: Vec<Held> = (0..12)
        .map(|i| {
            let mut tags = vec!["project:billing-export".to_string()];
            tags.push(format!("tech:t{}", i % 9));
            held_owned(&format!("b{i:02}"), i64::from(i) * HOUR, tags)
        })
        .collect();
    rows.extend((0..200).map(|i| held(&format!("o{i:03}"), i64::from(i) * MIN, &["tech:go"])));
    let index = TagIndex::build(&rows);
    let p = picks(&["project:billing-export"]);
    let v = index.view(&p, "", NOW);
    assert_eq!((v.matched, v.held, v.tag_count), (12, 212, 10));
    assert_eq!(
        note_text(&v, &p),
        "12 of 212 runs match +project:billing-export \u{b7} 10 tags"
    );
    // two picks are listed in the order given; one pick and no match.
    let two = picks(&["project:billing-export", "tech:t1"]);
    let v = index.view(&two, "", NOW);
    assert_eq!(v.matched, 2);
    assert_eq!(
        note_text(&v, &two),
        "2 of 212 runs match +project:billing-export +tech:t1 \u{b7} 2 tags"
    );
    let none = picks(&["tech:go", "tech:t1"]);
    let v = index.view(&none, "", NOW);
    assert_eq!(v.matched, 0);
    assert_eq!(
        note_text(&v, &none),
        "no run matches +tech:go +tech:t1 (212 runs)"
    );
}

fn held_owned(id: &str, secs_ago: i64, tags: Vec<String>) -> Held {
    Held {
        run_id: id.to_string(),
        created_at: ago(secs_ago),
        tags,
    }
}

#[test]
fn note_text_for_one_pick_and_no_match() {
    let rows = lots(212, 10);
    let index = TagIndex::build(&rows);
    let p = picks(&["tech:rust"]);
    let v = index.view(&p, "", NOW);
    assert_eq!(note_text(&v, &p), "no run matches +tech:rust (212 runs)");
    let p = picks(&["tech:go"]);
    let v = index.view(&p, "", NOW);
    assert_eq!(
        note_text(&v, &p),
        "10 of 212 runs match +tech:go \u{b7} 1 tag"
    );
}

#[test]
fn note_text_singular_empty_and_untrusted() {
    let one = TagIndex::build(&[held("only", MIN, &["tech:go"])]);
    let v = one.view(&[], "", NOW);
    assert_eq!(note_text(&v, &[]), "1 run \u{b7} 1 tag");
    let p = picks(&["tech:rust"]);
    assert_eq!(
        note_text(&one.view(&p, "", NOW), &p),
        "no run matches +tech:rust (1 run)"
    );
    // one run without tags, no tags anywhere: the empty state of the overlay.
    let bare = TagIndex::build(&[held("only", MIN, &[])]);
    assert_eq!(
        note_text(&bare.view(&[], "", NOW), &[]),
        "1 run \u{b7} 0 tags \u{b7} 1 has no tags"
    );
    let bare = TagIndex::build(&lots(212, 0));
    assert_eq!(
        note_text(&bare.view(&[], "", NOW), &[]),
        "212 runs \u{b7} 0 tags \u{b7} 212 have no tags"
    );
    // nothing held at all.
    let empty = TagIndex::build(&[]);
    let v = empty.view(&[], "", NOW);
    assert_eq!((v.held, v.matched, v.tag_count, v.untagged), (0, 0, 0, 0));
    assert!(v.rows.is_empty());
    assert_eq!(note_text(&v, &[]), "0 runs \u{b7} 0 tags");
    // a pick with an escape byte or a line break in it is cleaned before the note shows it.
    let hostile = vec!["tech:\u{1b}[31mred\u{1b}[0m\nx".to_string()];
    let v = one.view(&hostile, "", NOW);
    let note = note_text(&v, &hostile);
    // it is not a tag the table can show, so it is not a pick: the note is the note without picks.
    assert_eq!(note, "1 run \u{b7} 1 tag");
}

// ---- what a tag may be --------------------------------------------------------------------------------------------------------------

#[test]
fn a_run_listed_twice_counts_once_and_a_tag_twice_on_a_run_counts_once() {
    let rows = [
        held("a", HOUR, &["tech:x", "tech:x", "tech:y"]),
        held("a", MIN, &["tech:z"]),
        held("b", 2 * HOUR, &["tech:x"]),
    ];
    let index = TagIndex::build(&rows);
    let v = index.view(&[], "", NOW);
    // the first listing of a run is the one that counts.
    assert_eq!(
        summary(&v),
        [stat("tech:x", 2, "1h"), stat("tech:y", 1, "1h"),]
    );
    assert_eq!(v.held, 2);
    assert_eq!(index.matching(&[]), ["a", "b"]);
    assert_eq!(index.matching(&picks(&["tech:x"])), ["a", "b"]);
}

#[test]
fn a_tag_that_is_not_text_a_person_can_read_is_never_counted_or_drawn() {
    let key = format!("topic:{}{}", "AKIA", "IOSFODNN7EXAMPLE");
    let bad: Vec<String> = vec![
        "tech:\u{1b}[31mred".to_string(),
        "tech:\u{1b}]8;;http://x\u{7}link\u{1b}]8;;\u{7}".to_string(),
        "tech:\u{1b}]52;c;AAAA\u{7}".to_string(),
        "tech:x\u{202e}evil".to_string(),
        "tech:zero\u{200b}width".to_string(),
        "tech:tag\u{e0041}char".to_string(),
        "tech:two words".to_string(),
        "tech:tab\tbed".to_string(),
        "tech:line\nbreak".to_string(),
        "tech:nul\0".to_string(),
        "tech:c1\u{9b}31m".to_string(),
        "tech:del\u{7f}".to_string(),
        "   ".to_string(),
        String::new(),
        key,
        // look-alikes, blank letters, wide and stacked characters, private use and emoji: not printable ASCII.
        "project:billing-expor\u{442}".to_string(),
        "tech:\u{3164}".to_string(),
        "tech:\u{2800}".to_string(),
        "tech:\u{ff58}".to_string(),
        "topic:e\u{301}\u{301}\u{301}\u{301}\u{301}\u{301}".to_string(),
        "tech:\u{e000}".to_string(),
        "tech:\u{1f600}".to_string(),
        "topic:\u{65e5}\u{672c}\u{8a9e}".to_string(),
        // longer than any tag the grammar allows.
        format!("topic:{}", "a".repeat(ham_core::tagexplore::MAX_TAG_LEN)),
    ];
    let mut good_tags: Vec<String> = bad.clone();
    good_tags.push("tech:fine".to_string());
    let rows = [
        Held {
            run_id: "a".into(),
            created_at: ago(HOUR),
            tags: good_tags,
        },
        Held {
            run_id: "b".into(),
            created_at: ago(HOUR),
            tags: bad.clone(),
        },
    ];
    let index = TagIndex::build(&rows);
    let v = index.view(&[], "", NOW);
    assert_eq!(summary(&v), [stat("tech:fine", 1, "1h")], "{v:?}");
    // run b held only tags that cannot be shown: it is a run without tags. The drop is counted.
    assert_eq!((v.held, v.untagged, v.tag_count), (2, 1, 1));
    assert_eq!(index.dropped(), 2 * bad.len());
    // none can be picked into a match either, and none is drawn as a pick.
    // none of them can be picked: a pick of that text is ignored, so it neither narrows the runs nor adds a row.
    let plain = index.view(&[], "", NOW);
    for b in &bad {
        let p = vec![b.clone()];
        assert_eq!(index.matching(&p), ["a", "b"], "{b:?}");
        assert_eq!(index.view(&p, "", NOW), plain, "{b:?}");
    }
    // real tags a person has: kinds, digits, long repository slugs and the shapes of the design all stay.
    let real = [
        "project:hamh-e2e-ntier-t40x91321-20261018",
        "project:billing-export",
        "tech:s3",
        "tech:k8s",
        "tech:ec2",
        "tech:sha256",
        "topic:unit-tests",
        "tech:a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
        "tech:c++",
        "tech:c#",
        "topic:Upper-Case",
        "topic:a.b_c/d",
    ];
    let rows = [Held {
        run_id: "r".into(),
        created_at: ago(MIN),
        tags: real.iter().map(ToString::to_string).collect(),
    }];
    let v = TagIndex::build(&rows).view(&[], "", NOW);
    let mut names: Vec<&str> = tag_names(&v);
    names.sort_unstable();
    let mut want = real.to_vec();
    want.sort_unstable();
    assert_eq!(names, want);
}

#[test]
fn a_pick_that_is_not_readable_text_is_ignored_by_the_table_the_runs_and_the_note() {
    let rows = [
        held("a", HOUR, &["project:billing", "tech:go"]),
        held("b", MIN, &["tech:go"]),
    ];
    let index = TagIndex::build(&rows);
    let plain = index.view(&[], "", NOW);
    // a pick no row could show must not filter the runs behind the person's back, nor forge the note.
    for bad in [
        "project:billing \u{b7} 999 tags",
        "a +b",
        "tech:go\u{200b}",
        "tech:\u{1b}[31m",
        "tech:\u{442}",
        "",
        "   ",
    ] {
        let p = vec![bad.to_string()];
        assert_eq!(index.view(&p, "", NOW), plain, "{bad:?}");
        assert_eq!(index.matching(&p), ["a", "b"], "{bad:?}");
        assert_eq!(note_text(&plain, &p), note_text(&plain, &[]), "{bad:?}");
    }
    // next to a real pick it changes nothing either, and the note names the real pick once.
    let real = picks(&["project:billing"]);
    let mixed = picks(&["project:billing", "x y", "project:billing"]);
    let v = index.view(&mixed, "", NOW);
    assert_eq!(v, index.view(&real, "", NOW));
    assert_eq!(index.matching(&mixed), ["a"]);
    assert_eq!(note_text(&v, &mixed), note_text(&v, &real));
    assert_eq!(
        note_text(&v, &mixed),
        "1 of 2 runs match +project:billing \u{b7} 2 tags"
    );
    // an absent pick named twice is one row and one name.
    let twice = picks(&["tech:gone", "tech:gone"]);
    let v = index.view(&twice, "", NOW);
    assert_eq!(summary(&v), [stat("tech:gone", 0, "")]);
    assert_eq!(note_text(&v, &twice), "no run matches +tech:gone (2 runs)");
}

#[test]
fn a_huge_find_line_is_cut_and_cheap() {
    // 1,000 distinct tags, each with the letter a: a find of 500,000 words "a" would be 500 million comparisons if it were not cut.
    let rows: Vec<Held> = (0..1_000)
        .map(|i| {
            held(
                &format!("r{i:04}"),
                i64::from(i) * MIN,
                &[&format!("topic:a{i:04}")],
            )
        })
        .collect();
    let index = TagIndex::build(&rows);
    let many = "a ".repeat(500_000);
    let t = Instant::now();
    let v = index.view(&[], &many, NOW);
    let took = t.elapsed();
    assert_eq!(v, index.view(&[], "a", NOW));
    assert_eq!(v.rows.len(), 1_000);
    assert!(
        took < Duration::from_millis(500),
        "a 1 MB find line took {took:?}"
    );
    let long = "z".repeat(1_000_000);
    assert!(index.view(&[], &long, NOW).rows.is_empty());
    assert!(!find_matches("topic:alpha", &long));
}

#[test]
fn ages_at_the_edges_of_what_a_timestamp_can_say() {
    // years 0000 and 9999 are as far apart as two timestamps can be: the age fits in five columns and nothing overflows.
    assert_eq!(
        age_text("9999-12-31T23:59:59Z", "0000-01-01T00:00:00Z"),
        "9999y"
    );
    assert_eq!(
        age_text("0000-01-01T00:00:00Z", "9999-12-31T23:59:59Z"),
        "0s"
    );
    assert_eq!(age_text(NOW, "1800-01-01T00:00:00Z"), "227y");
    assert_eq!(
        age_text("1970-01-01T00:00:00Z", "1970-01-01T00:00:00Z"),
        "0s"
    );
}

// ---- size ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn ten_thousand_rows_build_and_view_fast() {
    // 10,000 rows of 4 tags drawn from 600 tags, one run a minute, newest first.
    let tags_for = |i: usize| -> Vec<String> {
        (0..4)
            .map(|k| {
                let n = (i * 7 + k * 131) % 600;
                let kind = ["topic", "tech", "project"][n % 3];
                format!("{kind}:tag-{n:03}")
            })
            .collect()
    };
    let rows: Vec<Held> = (0..10_000usize)
        .map(|i| {
            held_owned(
                &format!("run{i:05}"),
                i64::try_from(i).unwrap() * MIN,
                tags_for(i),
            )
        })
        .collect();

    let t = Instant::now();
    let index = TagIndex::build(&rows);
    let build = t.elapsed();

    let t = Instant::now();
    let all = index.view(&[], "", NOW);
    let view_all = t.elapsed();

    let p = picks(&["topic:tag-000"]);
    let t = Instant::now();
    let faceted = index.view(&p, "tag-1 4", NOW);
    let view_faceted = t.elapsed();

    let t = Instant::now();
    let matched = index.matching(&picks(&["topic:tag-000", "tech:tag-001"]));
    let matching = t.elapsed();

    println!(
        "RT1B-TIMING rows=10000 tags=600 build={build:?} view(no picks)={view_all:?} view(1 pick + find)={view_faceted:?} matching(2 picks)={matching:?}"
    );
    assert_eq!(all.held, 10_000);
    assert_eq!(all.matched, 10_000);
    assert_eq!(all.tag_count, 600);
    assert_eq!(all.untagged, 0);
    assert_eq!(all.rows.iter().map(|r| r.runs).sum::<usize>(), 40_000);
    assert!(faceted.matched > 0 && faceted.matched < 10_000);
    assert!(matched.len() <= faceted.matched);
    assert!(build < Duration::from_millis(200), "build took {build:?}");
    assert!(
        view_all < Duration::from_millis(50),
        "view took {view_all:?}"
    );
    assert!(
        view_faceted < Duration::from_millis(50),
        "faceted view took {view_faceted:?}"
    );
}

// ---- a second, plain implementation as the oracle -----------------------------------------------------------------------------------

const ALPHABET: [&str; 8] = [
    "topic:a",
    "topic:b",
    "topic:csv",
    "tech:x",
    "tech:python",
    "tech:go",
    "project:p",
    "project:billing",
];

fn oracle(rows: &[Held], picks: &[String], find: &str) -> (View, Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    let rows: Vec<&Held> = rows.iter().filter(|r| seen.insert(&r.run_id)).collect();
    let has = |r: &Held, t: &str| r.tags.iter().any(|x| x == t);
    let matched: Vec<&Held> = rows
        .iter()
        .copied()
        .filter(|r| picks.iter().all(|p| has(r, p)))
        .collect();
    let mut all_tags: Vec<String> = rows.iter().flat_map(|r| r.tags.clone()).collect();
    all_tags.extend(picks.iter().cloned());
    all_tags.sort();
    all_tags.dedup();
    let words: Vec<String> = find.split_whitespace().map(str::to_lowercase).collect();
    let find_ok = |t: &str| {
        let t = t.to_lowercase();
        words.iter().all(|w| match w.strip_prefix('^') {
            Some(rest) => t.starts_with(rest),
            None => t.contains(w.as_str()),
        })
    };
    let mut rows_out: Vec<(bool, usize, i64, String)> = Vec::new();
    let mut tag_count = 0;
    for t in &all_tags {
        let holders: Vec<&Held> = matched.iter().copied().filter(|r| has(r, t)).collect();
        let picked = picks.contains(t);
        if holders.is_empty() && !picked {
            continue;
        }
        tag_count += 1;
        if !picked && !find_ok(t) {
            continue;
        }
        let newest = holders
            .iter()
            .map(|r| parse_utc_unix(&r.created_at).unwrap())
            .max();
        rows_out.push((picked, holders.len(), newest.unwrap_or(i64::MIN), t.clone()));
    }
    rows_out.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(b.2.cmp(&a.2))
            .then(a.3.cmp(&b.3))
    });
    let rows_view: Vec<TagStat> = rows_out
        .iter()
        .map(|(_, runs, newest, tag)| TagStat {
            tag: tag.clone(),
            runs: *runs,
            last: if *runs == 0 {
                String::new()
            } else {
                age_text(NOW, &rfc3339(*newest))
            },
        })
        .collect();
    (
        View {
            rows: rows_view,
            matched: matched.len(),
            held: rows.len(),
            tag_count,
            untagged: rows.iter().filter(|r| r.tags.is_empty()).count(),
        },
        matched.iter().map(|r| r.run_id.clone()).collect(),
    )
}

/// Any characters at all, controls and escapes included.
fn any_text(max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..max).prop_map(|v| v.into_iter().collect())
}

fn arb_rows() -> impl Strategy<Value = Vec<Held>> {
    proptest::collection::vec(
        (
            0u8..14,
            0i64..3_000_000,
            proptest::collection::vec(0usize..ALPHABET.len(), 0..6),
        ),
        0..30,
    )
    .prop_map(|rows| {
        rows.into_iter()
            .map(|(id, secs, tags)| Held {
                run_id: format!("r{id}"),
                created_at: ago(secs),
                tags: tags.into_iter().map(|t| ALPHABET[t].to_string()).collect(),
            })
            .collect()
    })
}

fn arb_picks() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(
        prop_oneof![
            (0usize..ALPHABET.len()).prop_map(|i| ALPHABET[i].to_string()),
            Just("tech:none".to_string()),
        ],
        0..4,
    )
}

fn arb_find() -> impl Strategy<Value = String> {
    proptest::sample::select(vec![
        "", "  ", "topic", "^topic", "a", "tech:", "^tech", "x p", "TOPIC", "^", "^proj", "csv go",
        "p", "zzz",
    ])
    .prop_map(str::to_string)
}

proptest! {
    #[test]
    fn the_view_equals_a_plain_recount(rows in arb_rows(), p in arb_picks(), find in arb_find()) {
        let index = TagIndex::build(&rows);
        let (want, want_ids) = oracle(&rows, &p, &find);
        let got = index.view(&p, &find, NOW);
        prop_assert_eq!(&got, &want);
        prop_assert_eq!(index.matching(&p), want_ids);
        prop_assert_eq!(got.matched, index.matching(&p).len());
        if find.trim().is_empty() {
            prop_assert_eq!(got.rows.len(), got.tag_count);
        } else {
            prop_assert!(got.rows.len() <= got.tag_count);
        }
        // counts never exceed the runs that match, and a picked tag holds all of them (unless nothing matches).
        for r in &got.rows {
            prop_assert!(r.runs <= got.matched);
            if p.contains(&r.tag) {
                prop_assert_eq!(r.runs, got.matched);
            }
        }
    }

    #[test]
    fn removal_batches_cover_every_id_once_in_order(n in 0usize..300, size in 0usize..60) {
        let ids: Vec<String> = (0..n).map(|i| format!("id{i}")).collect();
        let b = batches(&ids, size);
        prop_assert_eq!(b.concat(), ids.clone());
        prop_assert!(b.iter().all(|x| !x.is_empty() && x.len() <= size.max(1)));
        let full = b.len().saturating_sub(1);
        prop_assert!(b.iter().take(full).all(|x| x.len() == size.max(1)));
    }

    #[test]
    fn age_text_never_panics_and_is_short(secs in -1_000_000i64..60_000_000_000) {
        let t = age_text(NOW, &rfc3339(now_unix() - secs));
        prop_assert!(!t.is_empty() && t.len() <= 5, "{t}");
        let (digits, unit) = t.split_at(t.len() - 1);
        prop_assert!(digits.chars().all(|c| c.is_ascii_digit()), "{t}");
        prop_assert!("smhdwy".contains(unit), "{t}");
    }

    #[test]
    fn no_text_makes_find_or_build_panic(text in any_text(40), find in any_text(20)) {
        let _ = find_matches(&text, &find);
        let rows = [Held { run_id: "r".into(), created_at: text.clone(), tags: vec![text.clone()] }];
        let index = TagIndex::build(&rows);
        let v = index.view(std::slice::from_ref(&text), &find, &text);
        let _ = note_text(&v, &[text]);
    }
}
