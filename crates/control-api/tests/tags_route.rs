//! The tag edit route: `POST /v1/tags/edit` changes a run's two person-owned tag lists, `runTagsUser` (what the person
//! added) and `runTagsOff` (what the person removed), on the run's search item, for the authenticated caller only. One request holds at most
//! 40 edits and answers one result for each in request order. An edit is read (consistent), planned by `runtag::apply_edit`, and written
//! with a compare-and-set on `runTagsRev`; a lost race is read again once. The tests drive the real handler over the in-memory world of
//! `tests/chat_support`, whose tag store reads each `UpdateSpec` from its own text and panics on what DynamoDB refuses.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, SONNET, World, at, deps, get, post, request, run_item, ulid};
use control_api::handler::Store;
use ham_core::run_index::{RunIndexItem, RunRowsPage};
use ham_core::runtag::dynamo::{Answered, Av, UpdateSpec, auto_update, edit_update, expr_rejects};
use ham_core::runtag::grammar::Reason;
use ham_core::runtag::wire::{EditStatus, TagEditResponse, TagEditResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

const HAIKU: &str = "us.anthropic.claude-haiku-5-5";
const TAGGED_AT: &str = "2026-10-21T10:00:00Z";

fn s3_only() -> Value {
    json!({"mode": "s3-only"})
}

fn repo(name: &str) -> Value {
    json!({"mode": "repo", "repo": format!("cc:{name}")})
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

/// A complete search item for run `i`.
fn whole(id: &str, i: u64, repository: &str) -> RunIndexItem {
    RunIndexItem {
        run_id: id.into(),
        created_at: at(i),
        prompt_head: format!("Write the parser number {i}\nplease"),
        model: Some(SONNET.into()),
        repository: repository.into(),
        last_activity: at(i),
        ..RunIndexItem::default()
    }
}

/// Run `i` of `owner`, a finished run with no search item.
fn bare_run(w: &World, owner: &str, i: u64, target: Value) -> String {
    let id = ulid(i);
    w.add_run(run_item(&id, owner, "complete", &at(i), target));
    id
}

/// Run `i` of `owner` with a complete search item for `repository` (`s3-only` or `cc:<name>`) and the tag attributes `tune` leaves.
fn tagged_run(
    w: &World,
    owner: &str,
    i: u64,
    repository: &str,
    tune: impl FnOnce(RunIndexItem) -> RunIndexItem,
) -> String {
    let target = match repository.strip_prefix("cc:") {
        Some(name) => repo(name),
        None => s3_only(),
    };
    let id = bare_run(w, owner, i, target);
    w.add_index(owner, tune(whole(&id, i, repository)));
    id
}

fn item_of(w: &World, owner: &str, id: &str) -> RunIndexItem {
    w.indexed(owner)
        .into_iter()
        .find(|i| i.run_id == id)
        .unwrap_or_else(|| panic!("no search item for {id}"))
}

async fn edit_raw(w: &World, caller: &str, body: &str) -> (u16, Value, Vec<String>) {
    post(w, caller, "/v1/tags/edit", &[], body).await
}

async fn edit_logged(w: &World, caller: &str, edits: Value) -> (u16, Value, Vec<String>) {
    edit_raw(w, caller, &json!({"edits": edits}).to_string()).await
}

/// The results of a request that is answered 200.
async fn edit_ok(w: &World, caller: &str, edits: Value) -> Vec<TagEditResult> {
    let (status, body, logs) = edit_logged(w, caller, edits).await;
    assert_eq!(status, 200, "{body} {logs:?}");
    serde_json::from_value::<TagEditResponse>(body)
        .unwrap()
        .results
}

fn statuses(results: &[TagEditResult]) -> Vec<EditStatus> {
    results.iter().map(|r| r.status).collect()
}

async fn list(w: &World, caller: &str) -> Value {
    let (status, body) = get(w, caller, "/v1/runs", &[]).await;
    assert_eq!(status, 200, "{body}");
    body
}

fn row_of<'a>(body: &'a Value, run: &str) -> &'a Value {
    body["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runId"] == run)
        .unwrap_or_else(|| panic!("no row for {run}: {body}"))
}

fn row_tags(body: &Value, run: &str) -> Vec<String> {
    row_of(body, run)["tags"]
        .as_array()
        .unwrap_or_else(|| panic!("row {run} has no tags: {body}"))
        .iter()
        .map(|t| t.as_str().unwrap().to_string())
        .collect()
}

fn start_body(prompt: &str) -> String {
    json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": SONNET}).to_string()
}

async fn start(w: &World, key: &str, prompt: &str) -> String {
    let mut req = request(ME, "POST", "/v1/direct/runs", &[], &start_body(prompt));
    req.headers.insert("idempotency-key".into(), key.into());
    let (out, _) = control_api::handler::handle_logged(req, &deps(w)).await;
    assert_eq!(out.status, 201, "{}", out.body);
    serde_json::from_str::<Value>(&out.body).unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string()
}

// ---- grammar, per edit ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn edit_normalises_and_refuses_bad_grammar_per_edit() {
    let w = World::new();
    let ids: Vec<String> = (1..=6)
        .map(|i| tagged_run(&w, ME, i, "s3-only", |x| x))
        .collect();
    let r = edit_ok(
        &w,
        ME,
        json!([
            {"runId": ids[0], "add": ["Topic:CSV Export"]},
            {"runId": ids[1], "add": ["foo:bar"]},
            {"runId": ids[2], "add": ["topic:verified-login"]},
            {"runId": ids[3], "add": ["topic:blocked"]},
            {"runId": ids[4], "add": ["csv"]},
            {"runId": ids[5], "add": ["topic:ok-tag", "tech:python"], "remove": ["tech:java"]},
        ]),
    )
    .await;
    let (ok, invalid) = (EditStatus::Ok, EditStatus::Invalid);
    assert_eq!(statuses(&r), [ok, invalid, invalid, invalid, ok, ok]);
    assert_eq!(
        r.iter().map(|x| x.run_id.clone()).collect::<Vec<_>>(),
        ids,
        "one result for each edit, in request order"
    );
    assert_eq!(r[0].tags, ["topic:csv-export"], "typed text is normalised");
    assert_eq!(r[4].tags, ["topic:csv"], "a bare word is a topic");
    assert_eq!(r[5].tags, ["topic:ok-tag", "tech:python"]);
    // The message names the rule, then the tag it is about.
    let message = |n: usize| r[n].message.clone().unwrap();
    assert!(
        message(1).contains(Reason::UnknownKind.message()) && message(1).contains("foo:bar"),
        "{}",
        message(1)
    );
    assert!(
        message(2).contains(Reason::OutcomeToken.message()),
        "{}",
        message(2)
    );
    assert!(
        message(3).contains(Reason::StatusWord.message()),
        "{}",
        message(3)
    );
    assert!(r[0].message.is_none() && r[4].message.is_none());
    assert!(r[1].tags.is_empty(), "an invalid edit shows no tags");
    // What was stored: the normalised tag, the removal, the revision; the three invalid edits wrote nothing.
    let stored = item_of(&w, ME, &ids[0]);
    assert_eq!(stored.run_tags_user, Some(strs(&["topic:csv-export"])));
    assert_eq!(stored.run_tags_off, None);
    assert_eq!(stored.run_tags_rev, Some(1));
    let last = item_of(&w, ME, &ids[5]);
    assert_eq!(last.run_tags_off, Some(strs(&["tech:java"])));
    for n in [1, 2, 3] {
        let it = item_of(&w, ME, &ids[n]);
        assert_eq!(
            (it.run_tags_user, it.run_tags_off, it.run_tags_rev),
            (None, None, None),
            "edit {n} wrote nothing"
        );
    }
    assert_eq!(w.count("put_run_tags"), 3);
}

// ---- who may edit what ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn foreign_run_is_not_found_and_nothing_is_written() {
    let w = World::new();
    let mine = tagged_run(&w, ME, 1, "s3-only", |x| x);
    let theirs = tagged_run(&w, OTHER, 2, "s3-only", |x| RunIndexItem {
        run_tags_user: Some(strs(&["topic:secret"])),
        run_tags_rev: Some(4),
        ..x
    });
    let before = w.indexed(OTHER);
    let r = edit_ok(
        &w,
        ME,
        json!([
            {"runId": theirs, "add": ["topic:mine"]},
            {"runId": ulid(99), "add": ["topic:mine"]},
            {"runId": "not-a-run", "add": ["topic:mine"]},
            {"runId": ulid(98), "remove": ["topic:secret"]},
            {"runId": mine, "add": ["topic:x"]},
        ]),
    )
    .await;
    let nf = EditStatus::NotFound;
    assert_eq!(statuses(&r), [nf, nf, nf, nf, EditStatus::Ok]);
    assert!(
        r[..4]
            .iter()
            .all(|x| x.tags.is_empty() && x.message.is_none())
    );
    // A run that is not yours and a run that does not exist look the same: only the id differs.
    let blank = |x: &TagEditResult| {
        let mut v = serde_json::to_value(x).unwrap();
        v["runId"] = Value::Null;
        v
    };
    assert_eq!(blank(&r[0]), blank(&r[1]));
    assert_eq!(
        w.indexed(OTHER),
        before,
        "the other principal's item is as it was"
    );
    assert_eq!(
        (w.count("get_run_tag_state"), w.count("put_run_tags")),
        (1, 1),
        "only the caller's own run was read or written"
    );
    assert!(
        w.tag_writes.lock().unwrap().iter().all(|(o, _, _)| o == ME),
        "every write is for the caller's partition"
    );
    // An invalid tag on a run that is not yours is still not-found: the answer says nothing about the run.
    let r = edit_ok(
        &w,
        ME,
        json!([{"runId": theirs, "add": ["foo:bar"]}, {"runId": mine, "add": ["foo:bar"]}]),
    )
    .await;
    assert_eq!(statuses(&r), [EditStatus::NotFound, EditStatus::Invalid]);
    // The run's owner may edit it, and the other principal's run is not reachable from here.
    let r = edit_ok(
        &w,
        OTHER,
        json!([{"runId": theirs, "add": ["topic:own"]}, {"runId": mine, "add": ["topic:own"]}]),
    )
    .await;
    assert_eq!(statuses(&r), [EditStatus::Ok, EditStatus::NotFound]);
    assert_eq!(r[0].tags, ["topic:secret", "topic:own"]);
}

#[tokio::test]
async fn the_owner_is_the_caller_and_nothing_in_the_body_can_change_it() {
    let w = World::new();
    let mine = tagged_run(&w, ME, 1, "s3-only", |x| x);
    let body = json!({
        "edits": [{"runId": mine, "add": ["topic:x"], "owner": OTHER, "ownerPrincipalArn": OTHER}],
        "owner": OTHER,
        "ownerPrincipalArn": OTHER,
        "principal": OTHER,
    })
    .to_string();
    let (status, answer, _) = edit_raw(&w, ME, &body).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["results"][0]["status"], "ok");
    assert!(
        w.indexed(OTHER).is_empty(),
        "nothing lands in another partition"
    );
    assert_eq!(
        item_of(&w, ME, &mine).run_tags_user,
        Some(strs(&["topic:x"]))
    );
    assert!(w.tag_writes.lock().unwrap().iter().all(|(o, _, _)| o == ME));
    // A request that carries no caller is refused before any read.
    let mut anonymous = request(ME, "POST", "/v1/tags/edit", &[], &body);
    anonymous.caller_arn = None;
    let out = control_api::handler::handle(anonymous, &deps(&w)).await;
    assert_eq!(out.status, 403);
}

// ---- limits ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn limits_are_400() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| x);
    let too_many_edits: Vec<Value> = (1..=41)
        .map(|i| json!({"runId": ulid(i), "add": ["topic:x"]}))
        .collect();
    let seven: Vec<String> = (1..=7).map(|n| format!("topic:a{n}")).collect();
    let six: Vec<String> = (1..=6).map(|n| format!("topic:r{n}")).collect();
    let cases: Vec<(&str, String, &str)> = vec![
        (
            "41 edits",
            json!({"edits": too_many_edits}).to_string(),
            "40",
        ),
        (
            "13 tags in one edit",
            json!({"edits": [{"runId": id, "add": seven, "remove": six}]}).to_string(),
            "12",
        ),
        ("not json", "{".to_string(), ""),
        ("no edits member", "{}".to_string(), ""),
        ("edits is a string", json!({"edits": "x"}).to_string(), ""),
        (
            "run id is a number",
            json!({"edits": [{"runId": 7}]}).to_string(),
            "",
        ),
        (
            "add is a string",
            json!({"edits": [{"runId": id, "add": "topic:x"}]}).to_string(),
            "",
        ),
        (
            "a run twice in one batch",
            json!({"edits": [{"runId": id, "add": ["topic:x"]}, {"runId": id, "add": ["topic:y"]}]})
                .to_string(),
            "two edits",
        ),
        (
            "a run id with a slash",
            json!({"edits": [{"runId": "../x", "add": ["topic:x"]}]}).to_string(),
            "run id",
        ),
        (
            "an empty run id",
            json!({"edits": [{"runId": "", "add": ["topic:x"]}]}).to_string(),
            "run id",
        ),
        ("an empty body", String::new(), ""),
    ];
    for (what, body, word) in cases {
        let (status, answer, _) = edit_raw(&w, ME, &body).await;
        assert_eq!(status, 400, "{what}: {answer}");
        assert_eq!(answer["code"], "bad-request", "{what}: {answer}");
        let message = answer["message"].as_str().unwrap();
        assert!(
            !message.is_empty() && message.contains(word),
            "{what}: {message}"
        );
    }
    assert_eq!(
        (w.count("get_run_tag_state"), w.count("put_run_tags")),
        (0, 0),
        "a request outside the limits reads and writes nothing"
    );
    // The limits themselves are allowed: 40 edits, 12 tags in one edit, no edits at all.
    let forty: Vec<Value> = (1..=40)
        .map(|i| json!({"runId": ulid(1000 + i), "add": ["topic:x"]}))
        .collect();
    let r = edit_ok(&w, ME, json!(forty)).await;
    assert_eq!(r.len(), 40);
    assert!(r.iter().all(|x| x.status == EditStatus::NotFound));
    let twelve: Vec<String> = (1..=12).map(|n| format!("topic:t{n:02}")).collect();
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": twelve}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert_eq!(r[0].tags.len(), 12);
    let (status, answer, _) = edit_raw(&w, ME, r#"{"edits":[]}"#).await;
    assert_eq!((status, answer), (200, json!({"results": []})));
}

// ---- removal, additions and the person's words ---------------------------------------------------------------------------------

#[tokio::test]
async fn remove_always_writes_the_off_list_and_wins() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "cc:calc", |x| RunIndexItem {
        run_tags: Some(strs(&["topic:a", "tech:b"])),
        run_tags_user: Some(strs(&["topic:u"])),
        ..x
    });
    assert_eq!(
        row_tags(&list(&w, ME).await, &id),
        ["project:calc", "topic:u", "topic:a", "tech:b"],
        "the derived project, the typed tag, the automatic tags"
    );
    // A derived tag, an automatic tag and a typed tag each go, and the key of each is on the removal list.
    let steps: [(&str, Vec<&str>, Vec<&str>); 3] = [
        (
            "project:calc",
            vec!["topic:u", "topic:a", "tech:b"],
            vec!["project:calc"],
        ),
        (
            "topic:a",
            vec!["topic:u", "tech:b"],
            vec!["project:calc", "topic:a"],
        ),
        (
            "topic:u",
            vec!["tech:b"],
            vec!["project:calc", "topic:a", "topic:u"],
        ),
    ];
    for (n, (victim, shown, off)) in steps.into_iter().enumerate() {
        let r = edit_ok(&w, ME, json!([{"runId": id, "remove": [victim]}])).await;
        assert_eq!(r[0].status, EditStatus::Ok, "removing {victim}");
        assert_eq!(r[0].tags, shown, "removing {victim}");
        let stored = item_of(&w, ME, &id);
        assert_eq!(stored.run_tags_off, Some(strs(&off)), "removing {victim}");
        assert_eq!(stored.run_tags_rev, Some(n as u32 + 1));
        assert_eq!(row_tags(&list(&w, ME).await, &id), shown, "the list agrees");
    }
    let stored = item_of(&w, ME, &id);
    assert_eq!(
        stored.run_tags_user, None,
        "an empty typed list is not stored"
    );
    assert_eq!(
        stored.run_tags,
        Some(strs(&["topic:a", "tech:b"])),
        "the automatic list is never touched by an edit"
    );
    // Adding a removed tag back clears its removal. The derived project stays derived; a typed tag goes on the typed list.
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["project:calc"]}])).await;
    assert_eq!(r[0].tags, ["project:calc", "tech:b"]);
    let stored = item_of(&w, ME, &id);
    assert_eq!(stored.run_tags_off, Some(strs(&["topic:a", "topic:u"])));
    assert_eq!(
        stored.run_tags_user, None,
        "the repository gives that tag; no copy is kept"
    );
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["topic:a"]}])).await;
    assert_eq!(r[0].tags, ["project:calc", "topic:a", "tech:b"]);
    let stored = item_of(&w, ME, &id);
    assert_eq!(stored.run_tags_off, Some(strs(&["topic:u"])));
    assert_eq!(stored.run_tags_user, Some(strs(&["topic:a"])));
    assert_eq!(stored.run_tags_rev, Some(5));
    // One edit cannot both add and remove a tag.
    let r = edit_ok(
        &w,
        ME,
        json!([{"runId": id, "add": ["topic:z"], "remove": ["Topic:Z"]}]),
    )
    .await;
    assert_eq!(r[0].status, EditStatus::Invalid);
    assert!(
        r[0].message
            .clone()
            .unwrap()
            .contains(Reason::AddAndRemove.message())
    );
    assert_eq!(item_of(&w, ME, &id).run_tags_rev, Some(5));
}

#[tokio::test]
async fn caps_reject_and_never_evict() {
    let w = World::new();
    let a = tagged_run(&w, ME, 1, "s3-only", |x| x);
    let twelve: Vec<String> = (1..=12).map(|n| format!("topic:t{n:02}")).collect();
    let r = edit_ok(&w, ME, json!([{"runId": a, "add": twelve}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    let full = item_of(&w, ME, &a);
    assert_eq!(full.run_tags_user.as_ref().map(Vec::len), Some(12));
    let r = edit_ok(&w, ME, json!([{"runId": a, "add": ["topic:t13"]}])).await;
    assert_eq!(r[0].status, EditStatus::Invalid);
    assert!(
        r[0].message
            .clone()
            .unwrap()
            .contains(Reason::UserCap.message())
    );
    assert_eq!(
        item_of(&w, ME, &a),
        full,
        "the 13th tag evicted nothing and wrote nothing"
    );
    // A replay at the cap is a replay, not a refusal.
    let again: Vec<String> = (1..=12).map(|n| format!("topic:t{n:02}")).collect();
    let r = edit_ok(&w, ME, json!([{"runId": a, "add": again}])).await;
    assert_eq!(r[0].status, EditStatus::Unchanged, "{r:?}");
    assert_eq!(item_of(&w, ME, &a), full);

    let off32: Vec<String> = (1..=32).map(|n| format!("topic:o{n:02}")).collect();
    let b = tagged_run(&w, ME, 2, "s3-only", |x| RunIndexItem {
        run_tags_off: Some(off32.clone()),
        run_tags_rev: Some(3),
        ..x
    });
    let stored = item_of(&w, ME, &b);
    let r = edit_ok(&w, ME, json!([{"runId": b, "remove": ["topic:o33"]}])).await;
    assert_eq!(r[0].status, EditStatus::Invalid);
    assert!(
        r[0].message
            .clone()
            .unwrap()
            .contains(Reason::OffCap.message())
    );
    assert_eq!(
        item_of(&w, ME, &b),
        stored,
        "the 33rd removal evicted nothing"
    );
    let r = edit_ok(&w, ME, json!([{"runId": b, "remove": ["topic:o01"]}])).await;
    assert_eq!(
        r[0].status,
        EditStatus::Unchanged,
        "a tag that is already removed"
    );
    // Adding a removed tag back shrinks the list, which is allowed at the cap.
    let r = edit_ok(&w, ME, json!([{"runId": b, "add": ["topic:o01"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert_eq!(
        item_of(&w, ME, &b).run_tags_off.as_ref().map(Vec::len),
        Some(31)
    );
}

#[tokio::test]
async fn an_over_cap_or_odd_stored_list_does_not_block_a_shrinking_edit() {
    // Lists an older writer left: 40 removals and a typed entry that today's grammar would refuse. An edit validates what it is given,
    // never what is already stored, and only a list that would GROW past its cap is refused.
    let w = World::new();
    let off40: Vec<String> = (1..=40).map(|n| format!("topic:o{n:02}")).collect();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| RunIndexItem {
        run_tags_user: Some(strs(&["TOPIC:Weird"])),
        run_tags_off: Some(off40.clone()),
        run_tags_rev: Some(2),
        ..x
    });
    let r = edit_ok(&w, ME, json!([{"runId": id, "remove": ["topic:o01"]}])).await;
    assert_eq!(
        r[0].status,
        EditStatus::Unchanged,
        "an already-removed tag at 40 entries"
    );
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["topic:o02"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok, "{r:?}");
    let stored = item_of(&w, ME, &id);
    assert_eq!(stored.run_tags_off.as_ref().map(Vec::len), Some(39));
    assert_eq!(
        stored.run_tags_user,
        Some(strs(&["TOPIC:Weird", "topic:o02"])),
        "the entry it did not touch is carried as it was"
    );
    let r = edit_ok(&w, ME, json!([{"runId": id, "remove": ["topic:o41"]}])).await;
    assert_eq!(
        r[0].status,
        EditStatus::Invalid,
        "growing past the cap is still refused"
    );
}

#[tokio::test]
async fn replay_is_unchanged_and_the_revision_moves_once() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| x);
    let edits = json!([{"runId": id, "add": ["topic:x"], "remove": ["tech:y"]}]);
    let first = edit_ok(&w, ME, edits.clone()).await;
    assert_eq!(first[0].status, EditStatus::Ok);
    assert_eq!(item_of(&w, ME, &id).run_tags_rev, Some(1));
    let second = edit_ok(&w, ME, edits.clone()).await;
    assert_eq!(second[0].status, EditStatus::Unchanged);
    assert_eq!(
        second[0].tags, first[0].tags,
        "an unchanged edit still answers the tags"
    );
    assert_eq!(
        item_of(&w, ME, &id).run_tags_rev,
        Some(1),
        "no write, no revision"
    );
    assert_eq!(w.count("put_run_tags"), 1);
    // An edit with nothing in it changes nothing either.
    let r = edit_ok(&w, ME, json!([{"runId": id}])).await;
    assert_eq!(r[0].status, EditStatus::Unchanged);
    assert_eq!(w.count("put_run_tags"), 1);
    // A different edit moves it once more.
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["topic:z"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert_eq!(item_of(&w, ME, &id).run_tags_rev, Some(2));
}

// ---- the item ---------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn creates_the_item_when_missing_and_the_backfill_completes_it() {
    let w = World::new();
    let run = start(&w, "k-tags", "Add percent_change\nwith tests").await;
    let created = w.runs.lock().unwrap().get(&run).unwrap().created_at.clone();
    // The start's write has not landed: the run has no search item.
    w.index.lock().unwrap().clear();
    let r = edit_ok(&w, ME, json!([{"runId": run, "add": ["project:billing"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert_eq!(r[0].tags, ["project:billing"]);
    let items = w.indexed(ME);
    assert_eq!(items.len(), 1, "one item, at the key the start uses");
    let made = &items[0];
    assert_eq!(
        (made.run_id.as_str(), made.created_at.as_str()),
        (run.as_str(), created.as_str())
    );
    assert_eq!(made.run_tags_user, Some(strs(&["project:billing"])));
    assert_eq!(made.run_tags_rev, Some(1));
    assert!(
        made.prompt_head.is_empty(),
        "the edit writes no start field"
    );
    // The list shows the tag on a row that still lacks its title, and the page says the run predates the index.
    let body = list(&w, ME).await;
    assert_eq!(row_tags(&body, &run), ["project:billing"]);
    assert!(row_of(&body, &run).get("title").is_none());
    assert_eq!(body["unindexed"], 1);
    // The backfill completes the item and keeps the tags.
    let (status, report, _) = post(&w, ME, "/v1/runs/index", &[], "").await;
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["indexed"], 1);
    let done = item_of(&w, ME, &run);
    assert_eq!(done.prompt_head, "Add percent_change\nwith tests");
    assert_eq!(done.run_tags_user, Some(strs(&["project:billing"])));
    assert_eq!(done.run_tags_rev, Some(1));
    // So does the start, when its write comes late (a retry of the same request reaches the same key).
    w.index.lock().unwrap().clear();
    edit_ok(&w, ME, json!([{"runId": run, "add": ["project:billing"]}])).await;
    let again = start(&w, "k-tags", "Add percent_change\nwith tests").await;
    assert_eq!(again, run);
    let done = item_of(&w, ME, &run);
    assert_eq!(w.indexed(ME).len(), 1);
    assert_eq!(done.prompt_head, "Add percent_change\nwith tests");
    assert_eq!(done.run_tags_user, Some(strs(&["project:billing"])));
    let body = list(&w, ME).await;
    assert_eq!(row_of(&body, &run)["title"], "Add percent_change");
    assert_eq!(row_tags(&body, &run), ["project:billing"]);
}

#[tokio::test]
async fn removing_the_derived_project_of_a_run_with_no_item_sticks() {
    // The repository gives the derived project; with no item yet the edit reads it from the run's target, and the removal it writes is
    // the one the list matches once the item is complete.
    let w = World::new();
    let id = bare_run(&w, ME, 1, repo("calc"));
    let r = edit_ok(&w, ME, json!([{"runId": id, "remove": ["project:calc"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert!(r[0].tags.is_empty());
    assert_eq!(
        item_of(&w, ME, &id).run_tags_off,
        Some(strs(&["project:calc"]))
    );
    // The backfill's item names the repository `cc:calc`; the removal still hides the tag.
    let item = w.indexed(ME).remove(0);
    w.add_index(
        ME,
        RunIndexItem {
            prompt_head: "Write it".into(),
            repository: "cc:calc".into(),
            ..item
        },
    );
    assert!(row_tags(&list(&w, ME).await, &id).is_empty());
}

#[tokio::test]
async fn title_model_and_name_survive_an_edit() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| RunIndexItem {
        run_name: Some("Monthly CSV export with manifest".into()),
        run_name_source: Some("ai".into()),
        run_name_model: Some(HAIKU.into()),
        outcome: Some("Cline reported done".into()),
        ..x
    });
    let before = list(&w, ME).await;
    let r = edit_ok(
        &w,
        ME,
        json!([{"runId": id, "add": ["project:billing", "topic:csv"]}]),
    )
    .await;
    let after = list(&w, ME).await;
    for member in [
        "title",
        "model",
        "name",
        "nameSource",
        "lastActivity",
        "outcome",
        "status",
        "target",
        "createdAt",
    ] {
        assert_eq!(
            row_of(&before, &id).get(member),
            row_of(&after, &id).get(member),
            "{member}"
        );
        assert!(
            row_of(&after, &id).get(member).is_some(),
            "{member} is on the row"
        );
    }
    assert_eq!(row_tags(&after, &id), r[0].tags);
    assert_eq!(r[0].tags, ["project:billing", "topic:csv"]);
    assert_eq!(row_tags(&before, &id), Vec::<String>::new());
    // Neither the old client nor the new one trips on the row.
    let page: RunRowsPage = serde_json::from_value(after).unwrap();
    assert_eq!(page.runs[0].tags.as_deref(), Some(&r[0].tags[..]));
    // What the edit stored stays off the wire.
    let text = list(&w, ME).await.to_string();
    for needle in ["runTags", "rt-1", HAIKU] {
        assert!(!text.contains(needle), "{needle} reached the wire: {text}");
    }
}

// ---- races --------------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_lost_race_costs_one_retry() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| RunIndexItem {
        run_tags_user: Some(strs(&["topic:a"])),
        run_tags_rev: Some(1),
        ..x
    });
    // Another session of the same person adds topic:z between this edit's read and its write.
    w.tag_gap.lock().unwrap().push_back(edit_update(
        &id,
        &at(1),
        &strs(&["topic:a", "topic:z"]),
        &[],
        Some(1),
    ));
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["topic:b"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert_eq!(
        r[0].tags,
        ["topic:a", "topic:z", "topic:b"],
        "both edits survive"
    );
    assert_eq!(
        (w.count("get_run_tag_state"), w.count("put_run_tags")),
        (2, 2)
    );
    let stored = item_of(&w, ME, &id);
    assert_eq!(
        stored.run_tags_user,
        Some(strs(&["topic:a", "topic:z", "topic:b"]))
    );
    assert_eq!(stored.run_tags_rev, Some(3));

    // A second bump in the gap of the retry: conflict, nothing of this edit written.
    let id2 = tagged_run(&w, ME, 2, "s3-only", |x| RunIndexItem {
        run_tags_user: Some(strs(&["topic:a"])),
        run_tags_rev: Some(1),
        ..x
    });
    {
        let mut gap = w.tag_gap.lock().unwrap();
        gap.push_back(edit_update(
            &id2,
            &at(2),
            &strs(&["topic:a", "topic:z"]),
            &[],
            Some(1),
        ));
        gap.push_back(edit_update(
            &id2,
            &at(2),
            &strs(&["topic:a", "topic:z", "topic:y"]),
            &[],
            Some(2),
        ));
    }
    let (reads, writes) = (w.count("get_run_tag_state"), w.count("put_run_tags"));
    let r = edit_ok(&w, ME, json!([{"runId": id2, "add": ["topic:b"]}])).await;
    assert_eq!(r[0].status, EditStatus::Conflict);
    assert!(r[0].tags.is_empty());
    assert_eq!(
        (
            w.count("get_run_tag_state") - reads,
            w.count("put_run_tags") - writes
        ),
        (2, 2),
        "one re-read and one more write, then it stops"
    );
    let stored = item_of(&w, ME, &id2);
    assert_eq!(
        stored.run_tags_user,
        Some(strs(&["topic:a", "topic:z", "topic:y"])),
        "only the other writer's lists: nothing of this edit"
    );
    assert_eq!(stored.run_tags_rev, Some(3));
}

#[tokio::test]
async fn an_auto_write_between_read_and_write_loses_nothing() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| x);
    // The tag branch stores its list after this edit read an item with no revision and before the edit writes.
    w.tag_gap.lock().unwrap().push_back(auto_update(
        &id,
        &at(1),
        &strs(&["topic:auto-one"]),
        TAGGED_AT,
        Some(Answered {
            route: HAIKU,
            version: "rt-1",
        }),
    ));
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["project:x"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert_eq!(r[0].tags, ["project:x", "topic:auto-one"]);
    assert_eq!(
        (w.count("get_run_tag_state"), w.count("put_run_tags")),
        (2, 2)
    );
    let stored = item_of(&w, ME, &id);
    assert_eq!(
        stored.run_tags,
        Some(strs(&["topic:auto-one"])),
        "the automatic list is intact"
    );
    assert_eq!(stored.run_tags_user, Some(strs(&["project:x"])));
    assert_eq!(stored.run_tags_at.as_deref(), Some(TAGGED_AT));
    assert_eq!(stored.run_tags_model.as_deref(), Some(HAIKU));
    assert_eq!(stored.run_tags_rev, Some(2));
}

#[tokio::test]
async fn a_removal_made_before_the_automatic_tags_arrive_still_hides_them() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| x);
    w.tag_gap.lock().unwrap().push_back(auto_update(
        &id,
        &at(1),
        &strs(&["topic:csv-export", "tech:python"]),
        TAGGED_AT,
        Some(Answered {
            route: HAIKU,
            version: "rt-1",
        }),
    ));
    let r = edit_ok(
        &w,
        ME,
        json!([{"runId": id, "remove": ["topic:csv-export"]}]),
    )
    .await;
    assert_eq!(r[0].status, EditStatus::Ok);
    assert_eq!(r[0].tags, ["tech:python"]);
    let stored = item_of(&w, ME, &id);
    assert_eq!(
        stored.run_tags,
        Some(strs(&["topic:csv-export", "tech:python"]))
    );
    assert_eq!(stored.run_tags_off, Some(strs(&["topic:csv-export"])));
    assert_eq!(row_tags(&list(&w, ME).await, &id), ["tech:python"]);
}

#[tokio::test]
async fn an_acknowledgement_lost_after_the_write_is_unchanged_on_the_next_read() {
    // The write landed and its answer did not: the retry of the SDK meets its own revision bump and reports a lost race.
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| x);
    w.tag_ack_lost_as_conflict.store(true, Ordering::SeqCst);
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["topic:x"]}])).await;
    assert_eq!(
        r[0].status,
        EditStatus::Unchanged,
        "the edit is already there"
    );
    assert_eq!(r[0].tags, ["topic:x"]);
    assert_eq!(w.count("put_run_tags"), 1, "nothing is written twice");
    assert_eq!(item_of(&w, ME, &id).run_tags_user, Some(strs(&["topic:x"])));
}

// ---- failures ---------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_run_whose_stored_tags_cannot_be_read_is_refused_not_overwritten() {
    // The attributes an edit replaces or compares: when one is there and cannot be read as written (a number where a list belongs, a
    // list with a stray entry, a revision that is no number), the edit would overwrite what nobody can see. It is refused and says so.
    let w = World::new();
    let good = tagged_run(&w, ME, 1, "s3-only", |x| x);
    let mut bad = Vec::new();
    for (n, attribute) in ["runTagsUser", "runTagsOff", "runTagsRev"]
        .into_iter()
        .enumerate()
    {
        let id = tagged_run(&w, ME, 2 + n as u64, "s3-only", |x| RunIndexItem {
            run_tags_user: Some(strs(&["topic:kept"])),
            ..x
        });
        w.tag_unreadable
            .lock()
            .unwrap()
            .insert(id.clone(), vec![attribute.to_string()]);
        bad.push((id, attribute));
    }
    let before: Vec<RunIndexItem> = bad.iter().map(|(id, _)| item_of(&w, ME, id)).collect();
    let mut edits = vec![json!({"runId": good, "add": ["topic:x"]})];
    edits.extend(
        bad.iter()
            .map(|(id, _)| json!({"runId": id, "add": ["topic:x"]})),
    );
    let (status, body, logs) = edit_logged(&w, ME, Value::Array(edits)).await;
    assert_eq!(status, 200, "{body}");
    let r = serde_json::from_value::<TagEditResponse>(body)
        .unwrap()
        .results;
    let invalid = EditStatus::Invalid;
    assert_eq!(statuses(&r), [EditStatus::Ok, invalid, invalid, invalid]);
    for x in &r[1..] {
        let message = x.message.clone().unwrap();
        assert!(message.contains("cannot be read"), "{message}");
        assert!(
            !message.contains("runTags"),
            "no attribute name for the person: {message}"
        );
    }
    for ((id, _), item) in bad.iter().zip(&before) {
        assert_eq!(&item_of(&w, ME, id), item, "nothing was overwritten");
    }
    assert_eq!(
        w.count("put_run_tags"),
        1,
        "only the readable run was written"
    );
    // The log says which attribute of which run, and holds no tag.
    for (id, attribute) in &bad {
        let line = logs
            .iter()
            .find(|l| l.contains(attribute) && l.contains(id.as_str()))
            .unwrap_or_else(|| panic!("{attribute}: {logs:?}"));
        assert!(!line.contains("topic:"), "{line}");
    }
}

#[tokio::test]
async fn after_a_lost_race_the_retry_may_find_the_edit_unchanged_or_invalid() {
    let w = World::new();
    // Another session made the very edit this one is making.
    let a = tagged_run(&w, ME, 1, "s3-only", |x| x);
    w.tag_gap
        .lock()
        .unwrap()
        .push_back(edit_update(&a, &at(1), &strs(&["topic:x"]), &[], None));
    let r = edit_ok(&w, ME, json!([{"runId": a, "add": ["topic:x"]}])).await;
    assert_eq!(r[0].status, EditStatus::Unchanged);
    assert_eq!(r[0].tags, ["topic:x"]);
    assert_eq!(
        (w.count("get_run_tag_state"), w.count("put_run_tags")),
        (2, 1)
    );
    // Another session filled the typed list to its cap: this edit is now one tag too many, which is `invalid`, not `conflict`.
    let eleven: Vec<String> = (1..=11).map(|n| format!("topic:t{n:02}")).collect();
    let twelve: Vec<String> = (1..=12).map(|n| format!("topic:t{n:02}")).collect();
    let b = tagged_run(&w, ME, 2, "s3-only", |x| RunIndexItem {
        run_tags_user: Some(eleven.clone()),
        run_tags_rev: Some(1),
        ..x
    });
    w.tag_gap
        .lock()
        .unwrap()
        .push_back(edit_update(&b, &at(2), &twelve, &[], Some(1)));
    let r = edit_ok(&w, ME, json!([{"runId": b, "add": ["topic:t13"]}])).await;
    assert_eq!(r[0].status, EditStatus::Invalid, "{r:?}");
    assert!(
        r[0].message
            .clone()
            .unwrap()
            .contains(Reason::UserCap.message())
    );
    assert_eq!(item_of(&w, ME, &b).run_tags_user, Some(twelve));
}

#[tokio::test]
async fn a_store_failure_stops_the_batch_after_its_group_and_the_replay_finishes_it() {
    let w = World::new();
    let ids: Vec<String> = (1..=20)
        .map(|i| tagged_run(&w, ME, i, "s3-only", |x| x))
        .collect();
    *w.tag_write_fail_run.lock().unwrap() = Some(ids[0].clone());
    let edits = Value::Array(
        ids.iter()
            .map(|id| json!({"runId": id, "add": ["topic:x"]}))
            .collect(),
    );
    let (status, body, logs) = edit_logged(&w, ME, edits.clone()).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["code"], "unavailable");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("retry"), "{message}");
    assert!(
        !message.contains("tag write"),
        "the store's reason stays out of the body: {message}"
    );
    assert!(logs.iter().any(|l| l.contains("tag write")), "{logs:?}");
    // Eight at a time: the group that met the failure finished, and no later group was started.
    assert_eq!(w.count("get_run_tag_state"), 8);
    assert_eq!(w.count("put_run_tags"), 8);
    // The edits that did land stay; the replay of the same batch finishes the job.
    *w.tag_write_fail_run.lock().unwrap() = None;
    let r = edit_ok(&w, ME, edits).await;
    let mut want = vec![EditStatus::Ok];
    want.extend([EditStatus::Unchanged; 7]);
    want.extend([EditStatus::Ok; 12]);
    assert_eq!(statuses(&r), want);
    for id in &ids {
        assert_eq!(item_of(&w, ME, id).run_tags_user, Some(strs(&["topic:x"])));
    }
}

#[tokio::test]
async fn an_unreadable_table_is_a_503_that_names_nothing() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| x);
    w.tag_read_down.store(true, Ordering::SeqCst);
    let (status, body, logs) =
        edit_logged(&w, ME, json!([{"runId": id, "add": ["topic:x"]}])).await;
    assert_eq!(status, 503, "{body}");
    assert!(!body.to_string().contains("tag read"));
    assert!(logs.iter().any(|l| l.contains("tag read")));
    assert_eq!(w.count("put_run_tags"), 0);
    w.tag_read_down.store(false, Ordering::SeqCst);
    let r = edit_ok(&w, ME, json!([{"runId": id, "add": ["topic:x"]}])).await;
    assert_eq!(r[0].status, EditStatus::Ok);
}

#[tokio::test]
async fn a_batch_runs_eight_edits_at_a_time() {
    let w = World::new();
    let ids: Vec<String> = (1..=40)
        .map(|i| tagged_run(&w, ME, i, "s3-only", |x| x))
        .collect();
    let edits = Value::Array(
        ids.iter()
            .map(|id| json!({"runId": id, "add": ["topic:x"]}))
            .collect(),
    );
    let r = edit_ok(&w, ME, edits).await;
    assert!(r.iter().all(|x| x.status == EditStatus::Ok));
    assert_eq!(
        r.iter().map(|x| x.run_id.clone()).collect::<Vec<_>>(),
        ids,
        "the order of the request, whatever finished first"
    );
    assert_eq!(w.tag_reading_max.load(Ordering::SeqCst), 8);
}

// ---- routes -------------------------------------------------------------------------------------------------------------------

async fn send(w: &World, method: &str, path: &str, body: &str) -> control_api::handler::V1Response {
    control_api::handler::handle(request(ME, method, path, &[], body), &deps(w)).await
}

#[tokio::test]
async fn route_table() {
    let w = World::new();
    let id = tagged_run(&w, ME, 1, "s3-only", |x| x);
    for method in ["GET", "PUT", "DELETE", "PATCH"] {
        let out = send(&w, method, "/v1/tags/edit", "").await;
        assert_eq!(out.status, 405, "{method}: {}", out.body);
        assert!(out.body.contains("method-not-allowed"));
    }
    for (method, path) in [
        ("POST", "/v1/tags"),
        ("GET", "/v1/tags"),
        ("POST", "/v1/tags/edit/x"),
        ("POST", "/v1/tag/edit"),
        ("POST", "/v1/runs/tags/edit"),
    ] {
        let out = send(&w, method, path, "{}").await;
        assert_eq!(out.status, 404, "{method} {path}: {}", out.body);
    }
    // A trailing slash is the same route.
    let out = send(&w, "POST", "/v1/tags/edit/", r#"{"edits":[]}"#).await;
    assert_eq!(out.status, 200, "{}", out.body);
    // The answer holds the results and nothing of the stored attributes.
    let body = json!({"edits": [{"runId": id, "add": ["topic:x"]}]}).to_string();
    let out = send(&w, "POST", "/v1/tags/edit", &body).await;
    assert_eq!(out.status, 200);
    assert!(!out.body.contains("runTags"), "{}", out.body);
    // A list page costs what it cost: the Runs page and the index read, no tag read or write.
    let w2 = World::new();
    for i in 1..=3 {
        tagged_run(&w2, ME, i, "s3-only", |x| x);
    }
    let (status, _) = get(&w2, ME, "/v1/runs", &[]).await;
    assert_eq!(status, 200);
    assert_eq!(
        w2.calls.lock().unwrap().len(),
        2,
        "{:?}",
        w2.calls.lock().unwrap()
    );
    assert_eq!(
        (w2.count("list_runs"), w2.count("run_index_between")),
        (1, 1)
    );
    assert_eq!(
        (w2.count("get_run_tag_state"), w2.count("put_run_tags")),
        (0, 0)
    );
}

// ---- the fake is as strict as the service ---------------------------------------------------------------------------------------

fn spec(expression: &str, names: &[(&str, &str)], values: Vec<(&str, Av)>) -> UpdateSpec {
    UpdateSpec {
        key_sk_prefix_note: String::new(),
        expression: expression.into(),
        condition: None,
        names: names
            .iter()
            .map(|(a, n)| ((*a).to_string(), (*n).to_string()))
            .collect::<BTreeMap<_, _>>(),
        values: values
            .into_iter()
            .map(|(a, v)| (a.to_string(), v))
            .collect(),
        item_bytes: None,
    }
}

async fn send_to_the_fake(bad: &UpdateSpec) {
    let w = World::new();
    let run = run_item(&ulid(1), ME, "complete", &at(1), s3_only());
    let store = &w;
    let _ = store.put_run_tags(ME, &run, bad).await;
}

#[tokio::test]
#[should_panic(expected = "DynamoDB would refuse")]
async fn fakes_enforce_dynamodb_rules_a_path_set_and_removed() {
    send_to_the_fake(&spec(
        "SET #u = :v REMOVE #u",
        &[("#u", "runTagsUser")],
        vec![(":v", Av::L(strs(&["topic:x"])))],
    ))
    .await;
}

#[tokio::test]
#[should_panic(expected = "DynamoDB would refuse")]
async fn fakes_enforce_dynamodb_rules_an_empty_set() {
    send_to_the_fake(&spec(
        "SET #u = :v",
        &[("#u", "runTagsUser")],
        vec![(":v", Av::Ss(vec![]))],
    ))
    .await;
}

#[tokio::test]
#[should_panic(expected = "DynamoDB would refuse")]
async fn fakes_enforce_dynamodb_rules_a_reserved_word_as_a_bare_name() {
    send_to_the_fake(&spec(
        "SET status = :v",
        &[],
        vec![(":v", Av::S("x".into()))],
    ))
    .await;
}

#[tokio::test]
async fn every_expression_the_route_sends_passes_the_dynamodb_rules() {
    let w = World::new();
    let ids: Vec<String> = (1..=4)
        .map(|i| tagged_run(&w, ME, i, "cc:calc", |x| x))
        .collect();
    let fresh = bare_run(&w, ME, 9, s3_only());
    edit_ok(
        &w,
        ME,
        json!([
            {"runId": ids[0], "add": ["topic:a"]},
            {"runId": ids[1], "remove": ["project:calc"]},
            {"runId": ids[2], "add": ["topic:a", "tech:b"], "remove": ["topic:c"]},
            {"runId": ids[3], "remove": ["topic:a"]},
            {"runId": fresh, "add": ["project:z"]},
        ]),
    )
    .await;
    let writes = w.tag_writes.lock().unwrap().clone();
    assert_eq!(writes.len(), 5);
    for (_, _, spec) in &writes {
        assert_eq!(expr_rejects(spec), None, "{spec:?}");
        // The route sends what the library builds, and it names only the attributes of the edit.
        let mut attributes: Vec<&str> = spec.names.values().map(String::as_str).collect();
        attributes.sort_unstable();
        assert_eq!(
            attributes,
            [
                "createdAt",
                "runId",
                "runTagsOff",
                "runTagsRev",
                "runTagsUser"
            ],
            "{spec:?}"
        );
        assert_eq!(
            spec.condition.as_deref(),
            Some("attribute_not_exists(#runTagsRev) OR #runTagsRev = :r")
        );
    }
}
