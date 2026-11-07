//! The run index: the owner-keyed search item that sits outside the run-list index (GSI1), the `RunRow` the list
//! route returns (a `RunSummary` plus title, model, last activity and outcome), and the server-side filter. The types live in
//! `ham-core`, outside the worker image key; `control-api` writes and reads the item, `run-session` updates it at the end of a
//! run, and `ham` builds the query, so all three share these functions.
#![allow(clippy::unwrap_used)]

use ham_core::run_index::{
    BACKFILL_MAX_PAGES, BACKFILL_MAX_WRITES, BackfillReport, FILTER_FILL, FILTER_MAX_PAGES,
    MAX_QUERY_CHARS, MAX_QUERY_WORDS, PROMPT_HEAD_CHARS, RunFilter, RunIndexItem, RunRow,
    RunRowsPage, TITLE_CHARS, owner_pk, percent_encode, prompt_head, search_sk, target_text,
    title_of,
};
use ham_core::runname::{UNTITLED, accept_stored, seed_name};
use harness_protocol::api::v1::{ListRunsResponse, RunSummary};
use harness_protocol::goal::GoalStatus;
use harness_protocol::target::RunTarget;
use serde_json::json;
use std::collections::BTreeMap;

const ME: &str = "arn:aws:sts::700000000011:assumed-role/Admin/testuser";
const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

fn summary(id: &str, status: &str, target: serde_json::Value, created: &str) -> RunSummary {
    serde_json::from_value(json!({
        "runId": id, "status": status, "phase": "green", "target": target, "createdAt": created,
        "ownerPrincipalArn": ME
    }))
    .unwrap()
}

fn item() -> RunIndexItem {
    RunIndexItem {
        run_id: RUN.into(),
        created_at: "2026-10-15T13:00:00Z".into(),
        prompt_head: "Add percent_change(a, b) with tests\nand a README".into(),
        prompt_truncated: false,
        model: Some("global.anthropic.claude-sonnet-5-5".into()),
        repository: "cc:hamh-e2e-python-calc".into(),
        outcome: None,
        last_activity: "2026-10-15T13:00:00Z".into(),
        run_name: None,
        run_name_source: None,
        run_name_model: None,
        ..Default::default()
    }
}

fn row(
    id: &str,
    status: &str,
    target: serde_json::Value,
    created: &str,
    head: &str,
    model: &str,
) -> RunRow {
    RunRow::new(
        summary(id, status, target, created),
        Some(&RunIndexItem {
            run_id: id.into(),
            created_at: created.into(),
            prompt_head: head.into(),
            prompt_truncated: false,
            model: Some(model.into()),
            repository: "cc:hamh-e2e-python-calc".into(),
            outcome: Some("Cline reported done".into()),
            last_activity: created.into(),
            run_name: None,
            run_name_source: None,
            run_name_model: None,
            ..Default::default()
        }),
    )
}

#[test]
fn the_limits_are_the_documented_numbers() {
    // A filtered list stops after the first page that brings it to a screenful of matches, or after this many pages.
    assert_eq!((FILTER_FILL, FILTER_MAX_PAGES), (25, 10));
    // One backfill call writes at most this many items and reads at most this many pages of 100 runs.
    assert_eq!((BACKFILL_MAX_WRITES, BACKFILL_MAX_PAGES), (40, 5));
    assert_eq!((MAX_QUERY_CHARS, MAX_QUERY_WORDS), (200, 8));
}

#[test]
fn query_text_is_percent_encoded_the_way_sigv4_signs_it() {
    assert_eq!(percent_encode("calc tests"), "calc%20tests");
    assert_eq!(percent_encode("cc:x/y"), "cc%3Ax%2Fy");
    assert_eq!(percent_encode("A-z_0.9~"), "A-z_0.9~");
    assert_eq!(percent_encode("é"), "%C3%A9");
    assert_eq!(percent_encode("a+b&c=d"), "a%2Bb%26c%3Dd");
}

#[test]
fn the_key_is_the_hash_of_the_caller_and_the_time_then_the_run() {
    // sha256 of the ARN above, computed independently (python hashlib).
    assert_eq!(
        owner_pk(ME),
        "OWNER#1a1068ea38473b546739722ead49ce607134c11d2dbb3e9a820a6d4c0e3ec2c3"
    );
    assert_ne!(
        owner_pk(ME),
        owner_pk("arn:aws:sts::700000000011:assumed-role/Admin/someone")
    );
    assert_eq!(
        search_sk("2026-10-15T13:00:00Z", RUN),
        format!("SRCH#2026-10-15T13:00:00Z#{RUN}")
    );
}

#[test]
fn the_prompt_head_is_two_thousand_characters_not_bytes() {
    assert_eq!(PROMPT_HEAD_CHARS, 2000);
    assert_eq!(prompt_head(""), (String::new(), false));
    let exact = "é".repeat(2000);
    assert_eq!(prompt_head(&exact), (exact.clone(), false));
    let long = format!("{exact}tail");
    let (head, cut) = prompt_head(&long);
    assert_eq!(head.chars().count(), 2000);
    assert_eq!(head, exact);
    assert!(cut);
}

#[test]
fn a_title_is_the_first_line_made_safe() {
    assert_eq!(TITLE_CHARS, 80);
    assert_eq!(
        title_of("  Add percent_change  \nmore"),
        "Add percent_change"
    );
    assert_eq!(
        title_of("\n\n   \nSecond line wins\nthird"),
        "Second line wins"
    );
    assert_eq!(title_of(""), "");
    assert_eq!(title_of("   \n  "), "");
    assert_eq!(title_of("a  b\t\tc"), "a b c");
    let t = title_of("\u{1b}[31mred\u{7} text");
    assert!(t.chars().all(|c| !c.is_control()), "{t:?}");
    assert!(t.contains("red") && t.contains("text"));
    let long = "x".repeat(300);
    let t = title_of(&long);
    assert_eq!(t.chars().count(), TITLE_CHARS);
    assert!(t.ends_with('\u{2026}'));
    let wide = "é".repeat(TITLE_CHARS);
    assert_eq!(title_of(&wide), wide);
}

#[test]
fn a_target_reads_as_the_list_shows_it() {
    let t: RunTarget = serde_json::from_value(json!({"mode": "s3-only"})).unwrap();
    assert_eq!(target_text(&t), "s3-only");
    let t: RunTarget =
        serde_json::from_value(json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"})).unwrap();
    assert_eq!(target_text(&t), "cc:hamh-e2e-python-calc");
    let t: RunTarget =
        serde_json::from_value(json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new"})).unwrap();
    assert_eq!(target_text(&t), "new cc:hamh-e2e-new");
}

#[test]
fn the_stored_item_never_carries_the_run_list_key() {
    let j = serde_json::to_value(item()).unwrap();
    // GSI1 is sparse on ownerPrincipalArn: an item with that attribute would enter the run list.
    assert!(j.get("ownerPrincipalArn").is_none(), "{j}");
    assert_eq!(j["runId"], RUN);
    assert_eq!(j["createdAt"], "2026-10-15T13:00:00Z");
    assert_eq!(
        j["promptHead"],
        "Add percent_change(a, b) with tests\nand a README"
    );
    assert_eq!(j["promptTruncated"], false);
    assert_eq!(j["repository"], "cc:hamh-e2e-python-calc");
    assert!(j.get("outcome").is_none());
    let back: RunIndexItem = serde_json::from_value(j).unwrap();
    assert_eq!(back, item());
}

#[test]
fn a_row_is_the_summary_plus_the_extras_and_old_clients_still_parse_it() {
    let r = row(
        RUN,
        "complete",
        json!({"mode": "s3-only"}),
        "2026-10-15T13:00:00Z",
        "Add percent_change\nmore",
        "global.anthropic.claude-sonnet-5-5",
    );
    assert_eq!(r.title.as_deref(), Some("Add percent_change"));
    assert_eq!(
        r.model.as_deref(),
        Some("global.anthropic.claude-sonnet-5-5")
    );
    assert_eq!(r.last_activity.as_deref(), Some("2026-10-15T13:00:00Z"));
    assert_eq!(r.outcome.as_deref(), Some("Cline reported done"));
    let j = serde_json::to_value(&r).unwrap();
    // Flat: the summary members sit beside the extras.
    assert_eq!(j["runId"], RUN);
    assert_eq!(j["status"], "complete");
    assert_eq!(j["title"], "Add percent_change");
    let page = RunRowsPage {
        runs: vec![r.clone()],
        next_token: Some("tok".into()),
        partial: false,
        unindexed: 0,
    };
    let text = serde_json::to_string(&page).unwrap();
    let old: ListRunsResponse = serde_json::from_str(&text).unwrap();
    assert_eq!(old.runs.len(), 1);
    assert_eq!(old.runs[0], r.summary);
    assert_eq!(old.next_token.as_deref(), Some("tok"));
    assert!(!text.contains("partial") && !text.contains("unindexed"));
    let back: RunRowsPage = serde_json::from_str(&text).unwrap();
    assert_eq!(back, page);
}

#[test]
fn a_page_counts_the_rows_that_predate_the_index() {
    let page = RunRowsPage {
        runs: vec![],
        next_token: None,
        partial: true,
        unindexed: 4,
    };
    assert_eq!(
        serde_json::to_value(&page).unwrap(),
        json!({"runs": [], "partial": true, "unindexed": 4})
    );
}

#[test]
fn an_item_a_run_ending_wrote_first_is_partial_until_the_start_fields_arrive() {
    // run-session can record the end of a run before the backfill has made the rest of the item; it holds only what it owns.
    let partial: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "outcome": "Cline reported done", "lastActivity": "2026-10-15T14:00:00Z"
    }))
    .unwrap();
    assert!(partial.is_partial());
    assert_eq!(partial.prompt_head, "");
    assert!(!item().is_partial());
    // A bare row from such an item has no title, and `q` still reaches the outcome word.
    let r = RunRow::new(
        summary(
            RUN,
            "complete",
            json!({"mode": "s3-only"}),
            "2026-10-15T13:00:00Z",
        ),
        Some(&partial),
    );
    assert_eq!(r.title, None);
    assert_eq!(r.outcome.as_deref(), Some("Cline reported done"));
    assert_eq!(r.last_activity.as_deref(), Some("2026-10-15T14:00:00Z"));
}

#[test]
fn a_run_without_an_index_item_is_a_bare_summary_row() {
    let s = summary(
        RUN,
        "active",
        json!({"mode": "s3-only"}),
        "2026-10-15T13:00:00Z",
    );
    let r = RunRow::new(s.clone(), None);
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        serde_json::to_value(&s).unwrap()
    );
    // And a server that predates the index sends a plain list that parses as rows.
    let old = serde_json::to_string(&ListRunsResponse {
        runs: vec![s.clone()],
        next_token: None,
    })
    .unwrap();
    let page: RunRowsPage = serde_json::from_str(&old).unwrap();
    assert_eq!(page.runs[0].summary, s);
    assert_eq!(page.runs[0].title, None);
}

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

#[test]
fn the_filter_takes_four_parameters_and_ignores_empty_ones() {
    let f = RunFilter::from_params(&params(&[])).unwrap();
    assert!(f.is_empty());
    let f = RunFilter::from_params(&params(&[
        ("q", "  Calc  Tests "),
        ("target", " python-calc "),
        ("status", "complete"),
        ("since", "2026-10-01T00:00:00Z"),
    ]))
    .unwrap();
    assert_eq!(f.q.as_deref(), Some("Calc Tests"));
    assert_eq!(f.target.as_deref(), Some("python-calc"));
    assert_eq!(f.status, Some(GoalStatus::Complete));
    assert_eq!(f.since.as_deref(), Some("2026-10-01T00:00:00Z"));
    assert!(!f.is_empty());
    // nextToken is not a filter; blank values are no filter.
    let f = RunFilter::from_params(&params(&[("nextToken", "x"), ("q", "  "), ("target", "")]))
        .unwrap();
    assert!(f.is_empty());
}

#[test]
fn the_filter_refuses_what_it_cannot_run() {
    let bad = |pairs: &[(&str, &str)]| {
        RunFilter::from_params(&params(pairs))
            .unwrap_err()
            .to_string()
    };
    let long = "a".repeat(MAX_QUERY_CHARS + 1);
    assert!(bad(&[("q", &long)]).contains("q"));
    let words = ["w"; MAX_QUERY_WORDS + 1].join(" ");
    assert!(bad(&[("q", &words)]).contains("words"));
    assert!(bad(&[("q", "a\u{1b}b")]).contains("q"));
    let m = bad(&[("status", "finished")]);
    assert!(
        m.contains("status") && m.contains("active") && m.contains("complete"),
        "{m}"
    );
    assert!(bad(&[("since", "yesterday")]).contains("since"));
    assert!(bad(&[("since", "2026-10-01")]).contains("since"));
    assert!(bad(&[("target", "a\u{0}b")]).contains("target"));
}

#[test]
fn the_filter_query_survives_its_own_text() {
    let f = RunFilter {
        q: Some("calc tests".into()),
        target: Some("cc:hamh-e2e-python-calc".into()),
        status: Some(GoalStatus::Complete),
        since: Some("2026-10-01T00:00:00Z".into()),
    };
    let text = f.to_query();
    assert!(!text.contains(' '), "{text}");
    assert!(text.contains("q=calc%20tests"), "{text}");
    let map: BTreeMap<String, String> = text
        .split('&')
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap();
            (
                k.to_string(),
                ham_core::run_index::percent_decode(v).unwrap(),
            )
        })
        .collect();
    assert_eq!(RunFilter::from_params(&map).unwrap(), f);
    assert_eq!(RunFilter::default().to_query(), "");
}

#[test]
fn q_matches_every_word_anywhere_in_the_row() {
    let a = row(
        RUN,
        "complete",
        json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}),
        "2026-10-15T13:00:00Z",
        "Add percent_change(a, b) with tests",
        "global.anthropic.claude-sonnet-5-5",
    );
    let f = |q: &str| RunFilter {
        q: Some(q.into()),
        ..RunFilter::default()
    };
    assert!(f("percent").matches(&a));
    assert!(f("PERCENT_change").matches(&a));
    assert!(f("sonnet calc").matches(&a));
    assert!(f(RUN).matches(&a));
    assert!(f("reported done").matches(&a));
    assert!(!f("percent kubernetes").matches(&a));
    assert!(!f("calcx").matches(&a));
    // A row with no index item matches only on what the summary holds.
    let bare = RunRow::new(a.summary.clone(), None);
    assert!(f("python").matches(&bare));
    assert!(!f("percent").matches(&bare));
}

#[test]
fn target_status_and_since_each_narrow_the_rows() {
    let a = row(
        RUN,
        "complete",
        json!({"mode": "repo", "repo": "cc:hamh-e2e-python-calc"}),
        "2026-10-15T13:00:00Z",
        "x",
        "m",
    );
    let g = |t: Option<&str>, s: Option<GoalStatus>, since: Option<&str>| RunFilter {
        q: None,
        target: t.map(Into::into),
        status: s,
        since: since.map(Into::into),
    };
    assert!(g(Some("PYTHON-calc"), None, None).matches(&a));
    assert!(!g(Some("textstats"), None, None).matches(&a));
    assert!(g(None, Some(GoalStatus::Complete), None).matches(&a));
    assert!(!g(None, Some(GoalStatus::Active), None).matches(&a));
    assert!(g(None, None, Some("2026-10-15T13:00:00Z")).matches(&a));
    assert!(g(None, None, Some("2026-10-15T12:59:59Z")).matches(&a));
    assert!(!g(None, None, Some("2026-10-15T13:00:01Z")).matches(&a));
    assert!(RunFilter::default().matches(&a));
}

#[test]
fn only_the_items_made_add_up_across_backfill_calls() {
    // A call that stops inside a page is followed by a call that reads the page again from its start, so the three read counts are per
    // call: the summed report keeps the last call's, and `indexed` is the only number that adds up.
    let first = BackfillReport {
        indexed: 40,
        already_indexed: 0,
        skipped: 2,
        scanned: 60,
        more: true,
        next_token: Some("t".into()),
    };
    let last = BackfillReport {
        indexed: 7,
        already_indexed: 61,
        skipped: 2,
        scanned: 70,
        more: false,
        next_token: None,
    };
    let mut total = BackfillReport::default();
    total.absorb(&first);
    total.absorb(&last);
    assert_eq!(total.indexed, 47);
    assert_eq!(
        (total.scanned, total.already_indexed, total.skipped),
        (70, 61, 2)
    );
    assert!(!total.more && total.next_token.is_none());
}

#[test]
fn a_backfill_report_names_what_it_did() {
    let r = BackfillReport {
        indexed: 40,
        already_indexed: 55,
        skipped: 1,
        scanned: 96,
        more: true,
        next_token: Some("tok".into()),
    };
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        json!({"indexed": 40, "alreadyIndexed": 55, "skipped": 1, "scanned": 96, "more": true, "nextToken": "tok"})
    );
    let done = BackfillReport {
        more: false,
        next_token: None,
        ..r
    };
    assert!(
        serde_json::to_value(&done)
            .unwrap()
            .get("nextToken")
            .is_none()
    );
}

// ---- run names --------------------------------------------------------------------------------------------------------
// `name-run` (run-session) stores the checked name of a run on its search item; the list builder shows that name, or the seed built
// from the stored prompt head when there is none. Display text only: the name is never an id, a key or an outcome.

/// A name the utility model wrote: seven words, nothing the checks of a stored name refuse.
const AI_NAME: &str = "Make uploads idempotent across retries now";
const HAIKU: &str = "us.anthropic.claude-haiku-5-5";
/// The prompt of corpus entry c01 and the seed that `tests/fixtures/runname/seeds.golden.json` holds for it.
const C01_PROMPT: &str = "Add a retry with exponential backoff to the S3 uploader in uploader.py";
const C01_SEED: &str = "Add a retry with exponential backoff";

/// The item of `item()` after `name-run` stored `name` on it.
fn named(name: &str, source: Option<&str>) -> RunIndexItem {
    RunIndexItem {
        run_name: Some(name.into()),
        run_name_source: source.map(Into::into),
        run_name_model: Some(HAIKU.into()),
        ..item()
    }
}

fn complete() -> RunSummary {
    summary(
        RUN,
        "complete",
        json!({"mode": "s3-only"}),
        "2026-10-15T13:00:00Z",
    )
}

#[test]
fn row_with_a_stored_name_shows_it_as_ai() {
    let r = RunRow::new(complete(), Some(&named(AI_NAME, Some("ai"))));
    assert_eq!(r.name.as_deref(), Some(AI_NAME));
    assert_eq!(r.name_source.as_deref(), Some("ai"));
    // The title is still the first line of the prompt: the name does not replace it.
    assert_eq!(
        r.title.as_deref(),
        Some("Add percent_change(a, b) with tests")
    );
    let j = serde_json::to_value(&r).unwrap();
    assert_eq!(j["name"], AI_NAME);
    assert_eq!(j["nameSource"], "ai");
    // The attributes of the item are not wire members: the model route is provenance, not display text.
    for k in ["runName", "runNameSource", "runNameModel"] {
        assert!(j.get(k).is_none(), "{k}: {j}");
    }
    assert!(!j.to_string().contains(HAIKU), "{j}");
}

#[test]
fn row_without_a_stored_name_shows_the_seed() {
    let it = RunIndexItem {
        prompt_head: C01_PROMPT.into(),
        ..item()
    };
    let r = RunRow::new(complete(), Some(&it));
    assert_eq!(r.name.as_deref(), Some(C01_SEED));
    assert_eq!(r.name_source.as_deref(), Some("seed"));
    // The one function the client uses for a run it has just started.
    assert_eq!(r.name.as_deref(), Some(seed_name(&it.prompt_head).as_str()));
    // A head with nothing to name still gets the constant, never an empty name.
    let blank = RunIndexItem {
        prompt_head: "  \n  ".into(),
        ..item()
    };
    let b = RunRow::new(complete(), Some(&blank));
    assert_eq!(b.name.as_deref(), Some(UNTITLED));
    assert_eq!(b.name_source.as_deref(), Some("seed"));
    assert_eq!(b.title, None);
}

#[test]
fn a_stored_name_that_fails_accept_stored_shows_the_seed() {
    // (the stored name, a word of it that must never reach the wire)
    let esc = '\u{1b}';
    let bad = [
        (format!("Fix the {esc}[31mlogin{esc}[0m bug today"), "login"),
        (
            "Read https://example.test/docs for setup first".to_string(),
            "example.test",
        ),
        (["Reorganize"; 7].join(" "), "Reorganize"),
        (
            "Add <b>percent</b> change function tests".to_string(),
            "<b>",
        ),
    ];
    for (name, word) in bad {
        assert!(
            !accept_stored(&name),
            "the test needs a name the check refuses: {name:?}"
        );
        let it = RunIndexItem {
            prompt_head: C01_PROMPT.into(),
            ..named(&name, Some("ai"))
        };
        let r = RunRow::new(complete(), Some(&it));
        assert_eq!(r.name.as_deref(), Some(C01_SEED), "{name:?}");
        assert_eq!(r.name_source.as_deref(), Some("seed"), "{name:?}");
        let wire = serde_json::to_string(&r).unwrap();
        assert!(!wire.contains(word), "{word} reached the wire: {wire}");
    }
}

#[test]
fn a_partial_item_with_a_name_shows_the_name_and_no_title() {
    // What `name-run` creates when it runs before the start has written the rest: the run id, the creation time and the three attributes.
    let it: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z",
        "runName": AI_NAME, "runNameSource": "ai", "runNameModel": HAIKU
    }))
    .unwrap();
    assert!(it.is_partial());
    let r = RunRow::new(complete(), Some(&it));
    assert_eq!(r.name.as_deref(), Some(AI_NAME));
    assert_eq!(r.name_source.as_deref(), Some("ai"));
    assert_eq!(r.title, None);
    assert!(!r.is_indexed(), "the backfill still has to complete it");
    assert_eq!(r.model, None);
}

#[test]
fn a_partial_item_without_a_name_has_no_name() {
    let it: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z",
        "outcome": "Cline reported done", "lastActivity": "2026-10-15T14:00:00Z"
    }))
    .unwrap();
    let r = RunRow::new(complete(), Some(&it));
    assert_eq!((r.name.as_deref(), r.name_source.as_deref()), (None, None));
    let j = serde_json::to_value(&r).unwrap();
    assert!(
        j.get("name").is_none() && j.get("nameSource").is_none(),
        "{j}"
    );
}

#[test]
fn a_row_with_no_item_serializes_as_the_bare_summary() {
    let s = summary(
        RUN,
        "active",
        json!({"mode": "s3-only"}),
        "2026-10-15T13:00:00Z",
    );
    let r = RunRow::new(s.clone(), None);
    assert_eq!(r.name, None);
    assert_eq!(r.name_source, None);
    // Byte for byte: an old backend and a run that predates the index send exactly the summary.
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        serde_json::to_string(&s).unwrap()
    );
}

#[test]
fn old_item_and_old_row_json_still_decode() {
    // An item written before run names has none of the three attributes.
    let old_item: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "promptHead": "x", "promptTruncated": false,
        "repository": "s3-only", "lastActivity": "2026-10-15T13:00:00Z"
    }))
    .unwrap();
    assert_eq!(
        (
            old_item.run_name,
            old_item.run_name_source,
            old_item.run_name_model
        ),
        (None, None, None)
    );
    // A row from a backend that predates run names has no name members.
    let old_row: RunRow = serde_json::from_value(json!({
        "runId": RUN, "status": "complete", "phase": "green", "target": {"mode": "s3-only"},
        "createdAt": "2026-10-15T13:00:00Z", "ownerPrincipalArn": ME, "title": "Add percent_change"
    }))
    .unwrap();
    assert_eq!(old_row.title.as_deref(), Some("Add percent_change"));
    assert_eq!(old_row.name, None);
    assert_eq!(old_row.name_source, None);
    // A row with the new members parses as the types an old client uses, and as itself.
    let new_row = RunRow::new(complete(), Some(&named(AI_NAME, Some("ai"))));
    let text = serde_json::to_string(&new_row).unwrap();
    let as_summary: RunSummary = serde_json::from_str(&text).unwrap();
    assert_eq!(as_summary, new_row.summary);
    let page = RunRowsPage {
        runs: vec![new_row.clone()],
        next_token: None,
        partial: false,
        unindexed: 0,
    };
    let page_text = serde_json::to_string(&page).unwrap();
    let old: ListRunsResponse = serde_json::from_str(&page_text).unwrap();
    assert_eq!(old.runs[0], new_row.summary);
    let back: RunRowsPage = serde_json::from_str(&page_text).unwrap();
    assert_eq!(back, page);
    assert_eq!(back.runs[0].name.as_deref(), Some(AI_NAME));
}

#[test]
fn an_unknown_name_source_does_not_fail_decoding() {
    // `user` is reserved for a rename feature that does not exist yet; a newer writer must not make an older reader drop the item.
    let it: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "promptHead": "Add percent_change",
        "lastActivity": "2026-10-15T13:00:00Z", "runName": AI_NAME, "runNameSource": "user"
    }))
    .unwrap();
    assert_eq!(it.run_name_source.as_deref(), Some("user"));
    let r = RunRow::new(complete(), Some(&it));
    assert_eq!(r.name.as_deref(), Some(AI_NAME));
    assert_eq!(r.name_source.as_deref(), Some("user"));
    assert_ne!(
        r.name_source.as_deref(),
        Some("ai"),
        "only `ai` draws plain"
    );
    // An item edited by hand with a name and no source: the name shows, and nothing claims it is `ai`.
    let r = RunRow::new(complete(), Some(&named(AI_NAME, None)));
    assert_eq!(r.name.as_deref(), Some(AI_NAME));
    assert_eq!(r.name_source, None);
}

#[test]
fn a_source_that_is_not_a_plain_word_is_not_carried() {
    let esc = '\u{1b}';
    for bad in [
        format!("ai{esc}[2J"),
        "a b".to_string(),
        "x".repeat(40),
        String::new(),
        "AI-ish!".to_string(),
        "9ai".to_string(),
        "-ai".to_string(),
    ] {
        let r = RunRow::new(complete(), Some(&named(AI_NAME, Some(&bad))));
        assert_eq!(r.name.as_deref(), Some(AI_NAME), "{bad:?}");
        assert_eq!(r.name_source, None, "{bad:?}");
    }
}

#[test]
fn plain_source_words_are_carried() {
    let longest = "x".repeat(16);
    for ok in ["ai", "user", "seed", "ai-v2", longest.as_str()] {
        let r = RunRow::new(complete(), Some(&named(AI_NAME, Some(ok))));
        assert_eq!(r.name_source.as_deref(), Some(ok), "{ok}");
    }
}

#[test]
fn q_finds_a_word_that_is_only_in_the_stored_name() {
    let f = |q: &str| RunFilter {
        q: Some(q.into()),
        ..RunFilter::default()
    };
    let with = RunRow::new(complete(), Some(&named(AI_NAME, Some("ai"))));
    let without = RunRow::new(complete(), Some(&item()));
    assert!(f("idempotent").matches(&with));
    assert!(f("IDEMPOTENT retries").matches(&with));
    assert!(!f("idempotent").matches(&without));
    // A word of the prompt head finds the run with or without a stored name.
    assert!(f("readme").matches(&with) && f("readme").matches(&without));
    // A race item has a name and no head: the name alone makes the run findable.
    let race: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "runName": AI_NAME, "runNameSource": "ai"
    }))
    .unwrap();
    assert!(f("idempotent").matches(&RunRow::new(complete(), Some(&race))));
}

#[test]
fn at_start_leaves_the_name_attributes_none() {
    let it = RunIndexItem::at_start(
        RUN,
        "2026-10-15T13:00:00Z",
        "Add percent_change",
        Some("m"),
        &RunTarget::S3Only,
    );
    assert_eq!(
        (
            it.run_name.as_deref(),
            it.run_name_source.as_deref(),
            it.run_name_model.as_deref()
        ),
        (None, None, None)
    );
    let j = serde_json::to_value(&it).unwrap();
    for k in ["runName", "runNameSource", "runNameModel"] {
        assert!(j.get(k).is_none(), "an unset attribute is left out: {j}");
    }
    // Set, the members are the DynamoDB attribute names (`name` itself is a reserved word).
    let j = serde_json::to_value(named(AI_NAME, Some("ai"))).unwrap();
    assert_eq!(j["runName"], AI_NAME);
    assert_eq!(j["runNameSource"], "ai");
    assert_eq!(j["runNameModel"], HAIKU);
    assert!(j.get("name").is_none(), "{j}");
    let back: RunIndexItem = serde_json::from_value(j).unwrap();
    assert_eq!(back, named(AI_NAME, Some("ai")));
}

#[test]
fn rows_that_differ_only_in_name_are_different_rows() {
    let with = RunRow::new(complete(), Some(&named(AI_NAME, Some("ai"))));
    let seeded = RunRow::new(complete(), Some(&item()));
    assert_ne!(with, seeded);
    assert_eq!(
        with,
        RunRow::new(complete(), Some(&named(AI_NAME, Some("ai"))))
    );
    let other_source = RunRow::new(complete(), Some(&named(AI_NAME, Some("user"))));
    assert_ne!(with, other_source);
    // Same source, different name.
    let other_name = RunRow::new(
        complete(),
        Some(&named("Rework the uploader retry logic today", Some("ai"))),
    );
    assert_ne!(with, other_name);
}

// ---- run tags ----------------------------------------------------------------------------------------------
// Eight optional attributes on the search item hold the tags of a run: the automatic list, the person's added and removed lists and the
// bookkeeping of the writers. The list row carries `tags`, the effective list (`runtag::effective`), on every row that has an item. A
// bad tag attribute never costs a row its title, model or name. A tag is display and search text.

const BILLING: &str = "cc:billing-export";

/// An item whose three tag lists are as given; an empty slice leaves the attribute unset.
fn tagged(repository: &str, auto: &[&str], user: &[&str], off: &[&str]) -> RunIndexItem {
    let list = |l: &[&str]| (!l.is_empty()).then(|| l.iter().map(|t| (*t).to_string()).collect());
    RunIndexItem {
        repository: repository.into(),
        run_tags: list(auto),
        run_tags_user: list(user),
        run_tags_off: list(off),
        ..item()
    }
}

fn tags_of(row: &RunRow) -> Vec<&str> {
    row.tags
        .as_ref()
        .map(|t| t.iter().map(String::as_str).collect())
        .unwrap_or_default()
}

#[test]
fn row_with_an_item_carries_effective_tags_even_when_empty() {
    // No tag attributes and no repository: the row still says "this run has an item and no tags".
    let r = RunRow::new(complete(), Some(&tagged("s3-only", &[], &[], &[])));
    assert_eq!(r.tags, Some(vec![]));
    let j = serde_json::to_value(&r).unwrap();
    assert_eq!(j["tags"], json!([]), "an empty list is a member, not a gap");
    // The repository gives the derived project.
    let r = RunRow::new(complete(), Some(&tagged("cc:calc", &[], &[], &[])));
    assert_eq!(tags_of(&r), ["project:calc"]);
    // Derived first, then the person's tags, then the automatic ones, minus what the person removed.
    let r = RunRow::new(
        complete(),
        Some(&tagged(
            "cc:calc",
            &["topic:aaa"],
            &["topic:uuu"],
            &["topic:aaa"],
        )),
    );
    assert_eq!(tags_of(&r), ["project:calc", "topic:uuu"]);
    let r = RunRow::new(
        complete(),
        Some(&tagged(
            "cc:calc",
            &["topic:aaa", "tech:s3"],
            &["topic:uuu"],
            &[],
        )),
    );
    assert_eq!(
        tags_of(&r),
        ["project:calc", "topic:uuu", "topic:aaa", "tech:s3"]
    );
    // The person's removal also reaches the derived project, and it wins over a tag typed earlier.
    let r = RunRow::new(
        complete(),
        Some(&tagged(
            "cc:calc",
            &["topic:aaa"],
            &["topic:uuu"],
            &["project:calc", "topic:uuu"],
        )),
    );
    assert_eq!(tags_of(&r), ["topic:aaa"]);
}

#[test]
fn row_without_an_item_is_the_bare_summary() {
    let s = complete();
    let r = RunRow::new(s.clone(), None);
    assert_eq!(r.tags, None);
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        serde_json::to_string(&s).unwrap(),
        "byte for byte the summary's JSON: no tags member"
    );
}

#[test]
fn the_repository_of_the_item_is_preferred_and_the_target_fills_in_when_it_is_empty() {
    let repo_run = summary(
        RUN,
        "complete",
        json!({"mode": "repo", "repo": "cc:from-target"}),
        "2026-10-15T13:00:00Z",
    );
    // An item that run-session wrote first has no repository yet: the run's own target gives the project.
    let early = tagged("", &[], &[], &[]);
    let r = RunRow::new(repo_run.clone(), Some(&early));
    assert_eq!(tags_of(&r), ["project:from-target"]);
    assert_eq!(early.tag_repository(&repo_run.target), "cc:from-target");
    // The start writes `repository = target_text(target)`: the same string, so the item wins when both are there.
    let whole = tagged("cc:from-item", &[], &[], &[]);
    assert_eq!(whole.tag_repository(&repo_run.target), "cc:from-item");
    let r = RunRow::new(repo_run, Some(&whole));
    assert_eq!(tags_of(&r), ["project:from-item"]);
    // No repository anywhere: s3-only gives no project.
    let s3 = complete();
    assert_eq!(
        tagged("", &[], &[], &[]).tag_repository(&s3.target),
        "s3-only"
    );
}

#[test]
fn old_item_and_old_row_json_still_decode_with_tags() {
    // An item written before the tags existed.
    let old: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "promptHead": "Add percent_change",
        "promptTruncated": false, "repository": "s3-only", "lastActivity": "2026-10-15T13:00:00Z"
    }))
    .unwrap();
    assert_eq!(
        (
            &old.run_tags,
            &old.run_tags_user,
            &old.run_tags_off,
            &old.run_tags_at,
            &old.run_tags_model,
            &old.run_tags_ver,
            &old.run_tags_inh,
            &old.run_tags_rev
        ),
        (&None, &None, &None, &None, &None, &None, &None, &None)
    );
    // A new row parses as the old client reads it, as a summary and as a list page.
    let r = RunRow::new(complete(), Some(&tagged("cc:calc", &["topic:a"], &[], &[])));
    let text = serde_json::to_string(&r).unwrap();
    assert!(
        text.contains(r#""tags":["project:calc","topic:a"]"#),
        "{text}"
    );
    let summary_only: RunSummary = serde_json::from_str(&text).unwrap();
    assert_eq!(summary_only, r.summary);
    let page = RunRowsPage {
        runs: vec![r.clone()],
        next_token: None,
        partial: false,
        unindexed: 0,
    };
    let page_text = serde_json::to_string(&page).unwrap();
    let old_client: ListRunsResponse = serde_json::from_str(&page_text).unwrap();
    assert_eq!(old_client.runs, vec![r.summary.clone()]);
    // And back: the tags survive the wire, an empty list too.
    let back: RunRowsPage = serde_json::from_str(&page_text).unwrap();
    assert_eq!(back, page);
    let empty = RunRow::new(complete(), Some(&tagged("s3-only", &[], &[], &[])));
    let back: RunRow = serde_json::from_str(&serde_json::to_string(&empty).unwrap()).unwrap();
    assert_eq!(back.tags, Some(vec![]), "[] is not the same as no member");
}

#[test]
fn a_wrong_typed_tag_attribute_keeps_the_title_model_and_name() {
    // Each of these made the whole item undecodable before: the row lost its title, model and name along with the tags.
    let bad_lists = [
        json!("topic:a"),
        json!(5),
        json!({"a": 1}),
        json!(true),
        json!(["topic:a", 5]),
        json!([5]),
        json!([["topic:a"]]),
    ];
    for bad in bad_lists {
        let it: RunIndexItem = serde_json::from_value(json!({
            "runId": RUN, "createdAt": "2026-10-15T13:00:00Z",
            "promptHead": "Add percent_change(a, b) with tests", "promptTruncated": false,
            "model": "global.anthropic.claude-sonnet-5-5", "repository": "s3-only",
            "lastActivity": "2026-10-15T13:00:00Z",
            "runName": AI_NAME, "runNameSource": "ai",
            "runTags": bad, "runTagsUser": ["project:billing"]
        }))
        .unwrap_or_else(|e| panic!("{bad}: the item must still decode: {e}"));
        assert_eq!(it.run_tags, None, "{bad}");
        assert_eq!(it.run_tags_user, Some(vec!["project:billing".to_string()]));
        let r = RunRow::new(complete(), Some(&it));
        assert_eq!(r.name.as_deref(), Some(AI_NAME), "{bad}");
        assert_eq!(
            r.model.as_deref(),
            Some("global.anthropic.claude-sonnet-5-5")
        );
        assert_eq!(
            r.title.as_deref(),
            Some("Add percent_change(a, b) with tests")
        );
        // The tags fall back to the other lists.
        assert_eq!(tags_of(&r), ["project:billing"], "{bad}");
    }
    // The scalar attributes are as lenient: a string where a number belongs, a number where text belongs, a number past u32.
    let scalar = json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "promptHead": "x", "runName": AI_NAME,
        "runTagsAt": 5, "runTagsModel": ["a"], "runTagsVer": {"v": 1}, "runTagsInh": false,
        "runTagsRev": "two"
    });
    let it: RunIndexItem = serde_json::from_value(scalar).unwrap();
    assert_eq!(
        (
            &it.run_tags_at,
            &it.run_tags_model,
            &it.run_tags_ver,
            &it.run_tags_inh,
            &it.run_tags_rev
        ),
        (&None, &None, &None, &None, &None)
    );
    assert_eq!(it.run_name.as_deref(), Some(AI_NAME));
    for big in [json!(4_294_967_296_u64), json!(-1), json!(1.5)] {
        let it: RunIndexItem = serde_json::from_value(
            json!({"runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "runTagsRev": big}),
        )
        .unwrap();
        assert_eq!(it.run_tags_rev, None, "{big}");
    }
    // A null attribute is no attribute.
    let it: RunIndexItem = serde_json::from_value(
        json!({"runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "runTags": null, "runTagsRev": null}),
    )
    .unwrap();
    assert_eq!((it.run_tags, it.run_tags_rev), (None, None));
    // Well-formed values decode as they are written.
    let ok: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "runTags": ["topic:a", "tech:b"],
        "runTagsUser": [], "runTagsOff": ["tech:b"], "runTagsAt": "2026-10-21T10:00:00Z",
        "runTagsModel": HAIKU, "runTagsVer": "rt-1", "runTagsInh": RUN, "runTagsRev": 7
    }))
    .unwrap();
    assert_eq!(ok.run_tags, Some(vec!["topic:a".into(), "tech:b".into()]));
    assert_eq!(ok.run_tags_user, Some(vec![]));
    assert_eq!(ok.run_tags_off, Some(vec!["tech:b".into()]));
    assert_eq!(ok.run_tags_at.as_deref(), Some("2026-10-21T10:00:00Z"));
    assert_eq!(ok.run_tags_model.as_deref(), Some(HAIKU));
    assert_eq!(ok.run_tags_ver.as_deref(), Some("rt-1"));
    assert_eq!(ok.run_tags_inh.as_deref(), Some(RUN));
    assert_eq!(ok.run_tags_rev, Some(7));
}

#[test]
fn a_damaged_removal_list_keeps_the_removals_it_can_read() {
    // The person's removal wins on every read. A removal list that is partly unreadable must not forget the removals that are readable,
    // or a tag the person removed would come back. (A list that is not a list of text at all cannot be read; the adapter counts it.)
    let off = |v: serde_json::Value| -> Option<Vec<String>> {
        serde_json::from_value::<RunIndexItem>(
            json!({"runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "runTagsOff": v}),
        )
        .unwrap()
        .run_tags_off
    };
    assert_eq!(
        off(json!(["tech:b", 5, "topic:a", null, ["x"]])),
        Some(vec!["tech:b".to_string(), "topic:a".to_string()])
    );
    assert_eq!(off(json!("tech:b")), Some(vec!["tech:b".to_string()]));
    assert_eq!(off(json!([5])), Some(vec![]));
    assert_eq!(off(json!({"tech:b": 1})), None);
    assert_eq!(off(json!(7)), None);
    // The other two lists are all or nothing: dropping an automatic or typed tag only hides it.
    let user = |v: serde_json::Value| -> Option<Vec<String>> {
        serde_json::from_value::<RunIndexItem>(
            json!({"runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "runTagsUser": v}),
        )
        .unwrap()
        .run_tags_user
    };
    assert_eq!(user(json!(["topic:a", 5])), None);
    assert_eq!(user(json!("topic:a")), None);
    // The removal still reaches the row when the rest of the removal list was damaged.
    let it: RunIndexItem = serde_json::from_value(json!({
        "runId": RUN, "createdAt": "2026-10-15T13:00:00Z", "promptHead": "x", "repository": "s3-only",
        "runTags": ["topic:csv-export", "tech:python"], "runTagsOff": ["tech:python", 5]
    }))
    .unwrap();
    assert_eq!(
        tags_of(&RunRow::new(complete(), Some(&it))),
        ["topic:csv-export"]
    );
}

#[test]
fn a_stored_tag_that_fails_the_policy_is_not_shown() {
    let esc = format!("topic:{}x", '\u{1b}');
    let r = RunRow::new(
        complete(),
        Some(&tagged(
            "s3-only",
            &[
                esc.as_str(),
                "topic:verified-x",
                "topic:UPPER",
                "topic:done",
                "topic:testing",
                "topic:fine-one",
            ],
            &["topic:a b", "tech:ok"],
            &[],
        )),
    );
    // `testing` is a generic word: refused for an automatic tag, and the typed list holds one tag that is not text.
    assert_eq!(tags_of(&r), ["tech:ok", "topic:fine-one"]);
    let wire = serde_json::to_string(&r).unwrap();
    assert!(
        !wire.contains('\u{1b}') && !wire.contains("\\u001b"),
        "{wire}"
    );
    assert!(
        !wire.contains("verified") && !wire.contains("UPPER"),
        "{wire}"
    );
}

#[test]
fn q_finds_a_word_that_is_only_in_a_tag() {
    let f = |q: &str| RunFilter {
        q: Some(q.into()),
        ..RunFilter::default()
    };
    let with = RunRow::new(
        complete(),
        Some(&tagged("s3-only", &["topic:csv-export"], &[], &[])),
    );
    let without = RunRow::new(complete(), Some(&tagged("s3-only", &[], &[], &[])));
    assert!(f("csv").matches(&with));
    assert!(f("CSV-Export").matches(&with));
    assert!(
        f("topic:csv percent").matches(&with),
        "every word, any field"
    );
    assert!(!f("csv").matches(&without));
    assert!(!f("csv kubernetes").matches(&with));
}

#[test]
fn q_finds_the_derived_project() {
    let f = |q: &str| RunFilter {
        q: Some(q.into()),
        ..RunFilter::default()
    };
    // The run's target is s3-only, so the word is nowhere else in the row.
    let r = RunRow::new(complete(), Some(&tagged(BILLING, &[], &[], &[])));
    assert!(f("billing").matches(&r));
    assert!(f("project:billing-export").matches(&r));
    let slug = RunRow::new(
        complete(),
        Some(&tagged("cc:Ham_E2E.Python Calc", &[], &[], &[])),
    );
    assert!(
        f("e2e-python-calc").matches(&slug),
        "the slug is what is searched"
    );
    assert!(!f("billing").matches(&slug));
    // A run with no item has no derived project to search.
    let bare = RunRow::new(complete(), None);
    assert!(!f("billing").matches(&bare));
}

#[test]
fn q_does_not_find_a_tag_the_person_removed() {
    let f = |q: &str| RunFilter {
        q: Some(q.into()),
        ..RunFilter::default()
    };
    let r = RunRow::new(
        complete(),
        Some(&tagged(
            BILLING,
            &["topic:csv-export"],
            &[],
            &["topic:csv-export", "project:billing-export"],
        )),
    );
    assert!(r.tags.as_ref().unwrap().is_empty());
    assert!(!f("csv").matches(&r));
    assert!(!f("billing").matches(&r));
}

#[test]
fn at_start_leaves_the_tag_attributes_none() {
    let target: RunTarget = serde_json::from_value(json!({"mode": "s3-only"})).unwrap();
    let it = RunIndexItem::at_start(RUN, "2026-10-15T13:00:00Z", "Add x", None, &target);
    assert_eq!(
        (
            it.run_tags,
            it.run_tags_user,
            it.run_tags_off,
            it.run_tags_at,
            it.run_tags_model,
            it.run_tags_ver,
            it.run_tags_inh,
            it.run_tags_rev
        ),
        (None, None, None, None, None, None, None, None)
    );
}

#[test]
fn the_eight_attributes_have_the_designed_names_and_are_left_out_when_unset() {
    let names = [
        "runTags",
        "runTagsUser",
        "runTagsOff",
        "runTagsAt",
        "runTagsModel",
        "runTagsVer",
        "runTagsInh",
        "runTagsRev",
    ];
    let j = serde_json::to_value(item()).unwrap();
    for k in names {
        assert!(j.get(k).is_none(), "{k} is written only when set: {j}");
    }
    let all = RunIndexItem {
        run_tags: Some(vec!["topic:a".into()]),
        run_tags_user: Some(vec![]),
        run_tags_off: Some(vec!["tech:b".into()]),
        run_tags_at: Some("2026-10-21T10:00:00Z".into()),
        run_tags_model: Some(HAIKU.into()),
        run_tags_ver: Some("rt-1".into()),
        run_tags_inh: Some(RUN.into()),
        run_tags_rev: Some(3),
        ..item()
    };
    let j = serde_json::to_value(&all).unwrap();
    assert_eq!(j["runTags"], json!(["topic:a"]));
    assert_eq!(j["runTagsUser"], json!([]), "an empty list is kept");
    assert_eq!(j["runTagsOff"], json!(["tech:b"]));
    assert_eq!(j["runTagsAt"], "2026-10-21T10:00:00Z");
    assert_eq!(j["runTagsModel"], HAIKU);
    assert_eq!(j["runTagsVer"], "rt-1");
    assert_eq!(j["runTagsInh"], RUN);
    assert_eq!(j["runTagsRev"], 3);
    let back: RunIndexItem = serde_json::from_value(j).unwrap();
    assert_eq!(back, all);
}

#[test]
fn the_bookkeeping_attributes_never_reach_the_wire() {
    let it = RunIndexItem {
        run_tags_at: Some("2026-10-21T10:00:00Z".into()),
        run_tags_model: Some(HAIKU.into()),
        run_tags_ver: Some("rt-1".into()),
        run_tags_inh: Some("01K5ZQ4T8M3N6P7R9S0V1W2X3Z".into()),
        run_tags_rev: Some(9),
        ..tagged("cc:calc", &["topic:a"], &["tech:b"], &["tech:c"])
    };
    let wire = serde_json::to_string(&RunRow::new(complete(), Some(&it))).unwrap();
    for k in [
        "runTags",
        HAIKU,
        "rt-1",
        "2026-10-21T10:00:00Z",
        "01K5ZQ4T8M3N6P7R9S0V1W2X3Z",
        "tech:c",
    ] {
        assert!(!wire.contains(k), "{k} reached the wire: {wire}");
    }
    assert!(
        wire.contains(r#""tags":["project:calc","tech:b","topic:a"]"#),
        "{wire}"
    );
}

#[test]
fn rows_that_differ_only_in_tags_are_different_rows() {
    let a = RunRow::new(complete(), Some(&tagged("s3-only", &["topic:a"], &[], &[])));
    let b = RunRow::new(complete(), Some(&tagged("s3-only", &["topic:b"], &[], &[])));
    let none = RunRow::new(complete(), Some(&tagged("s3-only", &[], &[], &[])));
    assert_ne!(a, b);
    assert_ne!(a, none);
    assert_eq!(
        a,
        RunRow::new(complete(), Some(&tagged("s3-only", &["topic:a"], &[], &[])))
    );
    // `Some([])` and no member are different too.
    assert_ne!(none, RunRow::new(complete(), None));
}

#[test]
fn a_huge_tag_list_shows_at_most_nineteen_tags_and_a_removal_at_its_end_still_wins() {
    // A hand-edited item of 400 KB can hold tens of thousands of entries. The row stays small and the work stays linear; the removal list is
    // never cut, because dropping a removal would show a tag again.
    let many: Vec<String> = (0..20_000).map(|n| format!("topic:t{n}")).collect();
    let mut off: Vec<String> = (0..20_000).map(|n| format!("tech:off{n}")).collect();
    off.push("topic:csv-export".into());
    let it = RunIndexItem {
        run_tags: Some(
            std::iter::once("topic:csv-export".to_string())
                .chain(many)
                .collect(),
        ),
        run_tags_off: Some(off),
        ..tagged("cc:calc", &[], &[], &[])
    };
    let r = RunRow::new(complete(), Some(&it));
    let tags = r.tags.unwrap();
    assert!(tags.len() <= 19, "{}", tags.len());
    assert_eq!(tags[0], "project:calc");
    assert!(!tags.iter().any(|t| t == "topic:csv-export"));
}
