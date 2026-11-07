//! `POST /v1/direct/runs` with the `skills` member and the `continueFrom` member.
//!
//! Skills: the request passes the one validator (`harness_protocol::direct::check_skills`) that `ham` and agentd also call, so a bad skill is
//! refused 400 `invalid-skills` with the words of the validator, before a claim, a run or a blob exists.
//!
//! continueFrom: the source run must be the caller's and finished, and its verified bundle must name a head on `refs/heads/harness/<run>`
//! (the first bytes of the bundle are read, as the snapshot route does). The stored request carries the head control-api resolved. A run that
//! is not the caller's answers exactly like a run that does not exist.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, OTHER, SONNET, World, deps, request, run_item, ulid};
use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{Skill, SkillFile, check_skills};
use harness_protocol::store::s3;
use serde_json::{Value, json};

const SOURCE: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const OTHER_HEAD: &str = "3333333333333333333333333333333333333333";
const BASE: &str = "1111111111111111111111111111111111111111";
/// A `SKILL.md` that names its own folder: the one validator requires it.
fn entry_of(name: &str) -> String {
    format!("---\nname: {name}\ndescription: A demo skill.\n---\nDo the demo.\n")
}

fn skill(name: &str) -> Skill {
    Skill {
        name: name.into(),
        files: vec![
            SkillFile::new("SKILL.md", &entry_of(name)),
            SkillFile::new("scripts/run.py", "print('ok')\n"),
        ],
    }
}

fn body() -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    })
}

fn with_skills(skills: &[Skill]) -> Value {
    let mut b = body();
    b["skills"] = serde_json::to_value(skills).unwrap();
    b
}

fn with_continue(c: Value) -> Value {
    let mut b = body();
    b["continueFrom"] = c;
    b
}

async fn start(w: &World, key: &str, caller: &str, b: &Value) -> (u16, Value) {
    let mut r = request(caller, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let out = control_api::handler::handle(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
    )
}

fn code(v: &Value) -> &str {
    v["code"]
        .as_str()
        .unwrap_or_else(|| panic!("no error code in {v}"))
}

fn untouched(w: &World) {
    assert!(w.runs.lock().unwrap().is_empty(), "no run was created");
    assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert_eq!(w.count("blob_put"), 0, "no blob was written");
}

/// Like `untouched`, for a world that already holds the source run: nothing new was written.
fn nothing_new(w: &World) {
    assert_eq!(
        w.runs.lock().unwrap().len(),
        1,
        "only the source run exists"
    );
    assert!(w.idem.lock().unwrap().is_empty(), "no claim was written");
    assert!(w.started.lock().unwrap().is_empty(), "no execution started");
    assert_eq!(w.count("blob_put"), 0, "no blob was written");
}

fn stored_request(w: &World, run_id: &str) -> StartDirectRunRequest {
    let g = w.objects.lock().unwrap();
    serde_json::from_slice(&g[&s3::request_key(run_id)]).unwrap()
}

// ---- skills -----------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_request_with_skills_is_stored_with_them() {
    let w = World::new();
    let skills = vec![skill("lifecycle-tdd"), skill("lifecycle-check")];
    let (status, v) = start(&w, "k-1", ME, &with_skills(&skills)).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap().to_string();
    let stored = stored_request(&w, &id);
    assert_eq!(stored.skills, skills);
    assert_eq!(check_skills(&stored.skills), Ok(()));
}

#[tokio::test]
async fn b776_skills_over_the_old_caps_are_stored_whole() {
    // The old limits were 12 files a skill, 64 KiB a file and 2 MiB in all; the request as a whole is held to the platform's bound instead.
    let w = World::new();
    let mut big = skill("big-skill");
    for j in 0..40 {
        big.files.push(SkillFile::new(
            &format!("references/deep/a/b/c/d/r{j}.md"),
            &"x".repeat(100 * 1024),
        ));
    }
    let skills = vec![big, skill("lifecycle-tdd")];
    let (status, v) = start(&w, "k-1", ME, &with_skills(&skills)).await;
    assert_eq!(status, 201, "{v}");
    let stored = stored_request(&w, v["runId"].as_str().unwrap());
    assert_eq!(stored.skills, skills);
}

#[tokio::test]
async fn a_request_with_no_skills_stores_the_bytes_it_always_did() {
    let w = World::new();
    let (status, v) = start(&w, "k-1", ME, &body()).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap().to_string();
    // The lock is released in its own block: a guard must not live across the await below.
    let text = {
        let g = w.objects.lock().unwrap();
        String::from_utf8(g[&s3::request_key(&id)].clone()).unwrap()
    };
    assert!(
        !text.contains("skills") && !text.contains("continueFrom"),
        "{text}"
    );
    // An explicit empty list is the same request: the same stored bytes, and a retry with either form is one run.
    let mut explicit = body();
    explicit["skills"] = json!([]);
    let (status, again) = start(&w, "k-1", ME, &explicit).await;
    assert_eq!(status, 201, "{again}");
    assert_eq!(again["runId"], v["runId"]);
}

#[tokio::test]
async fn every_bad_skill_is_400_with_the_validators_words_and_nothing_is_written() {
    type Edit = Box<dyn Fn(&mut Vec<Skill>)>;
    let cases: Vec<(&str, Edit)> = vec![
        ("bad_name", Box::new(|s| s[0].name = "Bad Name".into())),
        ("duplicate_skill", Box::new(|s| s.push(s[0].clone()))),
        ("no_files", Box::new(|s| s[0].files.clear())),
        (
            "bad_path",
            Box::new(|s| s[0].files.push(SkillFile::new("../escape.md", "x"))),
        ),
        (
            "sha_mismatch",
            Box::new(|s| s[0].files[0].sha256 = "00".repeat(32)),
        ),
        ("size_mismatch", Box::new(|s| s[0].files[0].size += 1)),
        (
            "sha_mismatch",
            Box::new(|s| s[0].files[0].sha256 = s[0].files[0].sha256.to_ascii_uppercase()),
        ),
        (
            "path_conflict",
            Box::new(|s| s[0].files.push(SkillFile::new("SKILL.md/x", "x"))),
        ),
        (
            "bad_entry",
            Box::new(|s| s[0].files[0] = SkillFile::new("SKILL.md", "no frontmatter at all\n")),
        ),
        (
            "missing_entry",
            Box::new(|s| {
                s[0].files.remove(0);
            }),
        ),
        (
            "too_many_skills",
            Box::new(|s| {
                for i in 0..32 {
                    s.push(skill(&format!("extra-{i}")));
                }
            }),
        ),
    ];
    for (want, edit) in cases {
        let w = World::new();
        let mut skills = vec![skill("lifecycle-tdd")];
        edit(&mut skills);
        let expected = check_skills(&skills).unwrap_err();
        assert_eq!(expected.problem.code(), want, "the case itself");
        let (status, v) = start(&w, "k-1", ME, &with_skills(&skills)).await;
        assert_eq!((status, code(&v)), (400, "invalid-skills"), "{want}: {v}");
        assert_eq!(
            v["message"].as_str().unwrap(),
            expected.to_string(),
            "{want}"
        );
        untouched(&w);
    }
}

#[tokio::test]
async fn an_unknown_member_of_a_skill_is_400_bad_request() {
    let w = World::new();
    let mut b = with_skills(&[skill("demo")]);
    b["skills"][0]["files"][0]["mode"] = json!(493);
    let (status, v) = start(&w, "k-1", ME, &b).await;
    assert_eq!((status, code(&v)), (400, "bad-request"), "{v}");
    untouched(&w);
}

// ---- continueFrom: the shape and the target ---------------------------------------------------------------------------------

#[tokio::test]
async fn an_s3_only_follow_up_takes_no_base_ref() {
    let w = World::new();
    let mut b = with_continue(json!({"runId": SOURCE}));
    b["baseRef"] = json!("main");
    let (status, v) = start(&w, "k-1", ME, &b).await;
    assert_eq!((status, code(&v)), (400, "invalid-continue-from"), "{v}");
    assert!(v["message"].as_str().unwrap().contains("s3-only"), "{v}");
    untouched(&w);
}

#[tokio::test]
async fn a_misshapen_continue_from_is_400() {
    for c in [
        json!({"runId": "short"}),
        json!({"runId": SOURCE, "headSha": "abc"}),
        json!({"runId": SOURCE, "headSha": HEAD.to_uppercase()}),
    ] {
        let w = World::new();
        let (status, v) = start(&w, "k-1", ME, &with_continue(c.clone())).await;
        assert_eq!(
            (status, code(&v)),
            (400, "invalid-continue-from"),
            "{c}: {v}"
        );
        untouched(&w);
    }
}

// ---- continueFrom: the source run -------------------------------------------------------------------------------------------

fn bundle(head: &str, run: &str) -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base subject\n{head} refs/heads/harness/{run}\n\nPACK")
            .into_bytes();
    b.extend(std::iter::repeat_n(7u8, 3000));
    b
}

/// A finished run of `ME` whose verified bundle names `HEAD`.
fn world_with_source(status: &str) -> World {
    let w = World::new();
    w.add_run(run_item(
        SOURCE,
        ME,
        status,
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.put_object(&s3::verified_bundle_key(SOURCE), bundle(HEAD, SOURCE));
    w
}

#[tokio::test]
async fn a_follow_up_stores_the_head_control_api_resolved() {
    let w = world_with_source("complete");
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap().to_string();
    let stored = stored_request(&w, &id);
    let c = stored.continue_from.expect("continueFrom is stored");
    assert_eq!(c.run_id, SOURCE);
    assert_eq!(
        c.head_sha.as_deref(),
        Some(HEAD),
        "the resolved head, though the client sent none"
    );
    assert!(w.count("read_prefix") >= 1, "the bundle header was read");
    assert_eq!(w.started.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_head_the_client_names_must_be_the_delivered_head() {
    let w = world_with_source("complete");
    let ok = json!({"runId": SOURCE, "headSha": HEAD});
    let (status, v) = start(&w, "k-1", ME, &with_continue(ok)).await;
    assert_eq!(status, 201, "{v}");

    let w = world_with_source("complete");
    let wrong = json!({"runId": SOURCE, "headSha": OTHER_HEAD});
    let (status, v) = start(&w, "k-2", ME, &with_continue(wrong)).await;
    assert_eq!((status, code(&v)), (409, "head-mismatch"), "{v}");
    assert!(
        v["message"].as_str().unwrap().contains(&HEAD[..12]),
        "names the delivered head: {v}"
    );
    nothing_new(&w);
}

#[tokio::test]
async fn a_retry_of_a_follow_up_is_one_run() {
    let w = world_with_source("complete");
    let b = with_continue(json!({"runId": SOURCE}));
    let (_, a) = start(&w, "k-1", ME, &b).await;
    let (status, again) = start(&w, "k-1", ME, &b).await;
    assert_eq!(status, 201, "{again}");
    assert_eq!(a["runId"], again["runId"]);
    // The client that named the head it resolved meets the same claim only if it sends the same request; a different request is 422.
    let named = with_continue(json!({"runId": SOURCE, "headSha": HEAD}));
    let (status, v) = start(&w, "k-1", ME, &named).await;
    assert_eq!(status, 201, "the resolved request is the same one: {v}");
    assert_eq!(v["runId"], a["runId"]);
}

#[tokio::test]
async fn another_principals_run_and_a_missing_run_answer_alike() {
    let w = world_with_source("complete");
    let (s1, other) = start(&w, "k-1", OTHER, &with_continue(json!({"runId": SOURCE}))).await;
    let missing = ulid(77);
    let (s2, gone) = start(&w, "k-2", ME, &with_continue(json!({"runId": missing}))).await;
    assert_eq!((s1, code(&other)), (404, "not-found"), "{other}");
    assert_eq!((s2, code(&gone)), (404, "not-found"), "{gone}");
    assert!(
        !other.to_string().contains("complete") && !other.to_string().contains(ME),
        "{other}"
    );
    assert!(
        w.runs.lock().unwrap().len() == 1,
        "only the source run exists"
    );
    assert_eq!(w.count("claim_idempotency"), 0);
}

#[tokio::test]
async fn a_run_that_is_still_active_cannot_be_continued() {
    let w = world_with_source("active");
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!((status, code(&v)), (409, "run-active"), "{v}");
    assert_eq!(w.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_run_with_no_verified_bundle_delivered_no_head() {
    let w = World::new();
    w.add_run(run_item(
        SOURCE,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!((status, code(&v)), (409, "no-delivered-head"), "{v}");
    assert_eq!(w.runs.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_bundle_that_names_no_head_for_the_run_is_422() {
    for bytes in [
        // The branch of another run.
        bundle(HEAD, &ulid(5)),
        // Not a bundle.
        b"hello".to_vec(),
        // A header the bytes read cut off.
        b"# v2 git bundle\n".to_vec(),
        // Two lines for the branch.
        format!("# v2 git bundle\n{HEAD} refs/heads/harness/{SOURCE}\n{OTHER_HEAD} refs/heads/harness/{SOURCE}\n\nPACK").into_bytes(),
    ] {
        let w = world_with_source("complete");
        w.put_object(&s3::verified_bundle_key(SOURCE), bytes);
        let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
        assert_eq!((status, code(&v)), (422, "delivered-head-unreadable"), "{v}");
        assert!(!v.to_string().contains("PACK"), "no bytes of the object in the answer: {v}");
        assert_eq!(w.runs.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn a_blob_outage_is_503_and_nothing_is_written() {
    let w = world_with_source("complete");
    w.objects_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!(status, 503, "{v}");
    assert_eq!(w.runs.lock().unwrap().len(), 1);
    assert_eq!(w.count("claim_idempotency"), 0);
}

// ---- continueFrom: a role that cannot list the bucket ------------------------------------------------------------------------

async fn start_logged(w: &World, key: &str, b: &Value) -> (u16, Value, Vec<String>) {
    let mut r = request(ME, "POST", "/v1/direct/runs", &[], &b.to_string());
    r.headers.insert("idempotency-key".into(), key.into());
    let (out, logs) = control_api::handler::handle_logged(r, &deps(w)).await;
    (
        out.status,
        serde_json::from_str(&out.body).unwrap_or(Value::Null),
        logs,
    )
}

/// The run that failed before it committed anything is the common case of a follow-up. The control-api role has no `s3:ListBucket` on the
/// bucket, so S3 answers a GET of the absent `verified.bundle` with 403, which the adapter reports as unavailable (503, "retry"): the user
/// must get the documented 409 no-delivered-head instead. The key is asked by a listing the role is granted for exactly that key.
#[tokio::test]
async fn a_source_run_with_no_bundle_is_409_though_a_get_of_the_absent_key_is_403() {
    let w = World::new();
    w.no_list_permission
        .store(true, std::sync::atomic::Ordering::SeqCst);
    w.add_run(run_item(
        SOURCE,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!((status, code(&v)), (409, "no-delivered-head"), "{v}");
    assert_eq!(
        w.count(&format!("exists {}", s3::verified_bundle_key(SOURCE))),
        1,
        "asked once, by key"
    );
    assert_eq!(w.count("read_prefix"), 0, "an absent key is never fetched");
    nothing_new(&w);
}

#[tokio::test]
async fn a_present_bundle_is_listed_then_read_once() {
    let w = world_with_source("complete");
    w.no_list_permission
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!(status, 201, "{v}");
    assert_eq!(w.count("exists "), 1);
    assert_eq!(w.count("read_prefix"), 1);
}

#[tokio::test]
async fn a_listing_that_fails_is_503_and_nothing_is_written() {
    let w = world_with_source("complete");
    w.objects_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!(status, 503, "{v}");
    nothing_new(&w);
}

// ---- continueFrom: what the 422 says -------------------------------------------------------------------------------------------

/// The answer says that the bundle cannot be read as a head and nothing of why; the cause (and the ref it is about) goes to the log only.
#[tokio::test]
async fn the_422_body_carries_no_cause_and_the_log_names_the_right_ref() {
    let wanted = format!("refs/heads/harness/{SOURCE}");
    let duplicate =
        format!("# v2 git bundle\n{HEAD} {wanted}\n{OTHER_HEAD} {wanted}\n\nPACK").into_bytes();
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("duplicate ref", duplicate, "more than once"),
        ("other run's ref", bundle(HEAD, &ulid(5)), "does not name"),
        ("not a bundle", b"hello".to_vec(), "not a git bundle"),
        ("cut header", b"# v2 git bundle\n".to_vec(), "does not end"),
    ];
    for (what, bytes, cause) in cases {
        let w = world_with_source("complete");
        w.put_object(&s3::verified_bundle_key(SOURCE), bytes);
        let (status, v, logs) =
            start_logged(&w, "k-1", &with_continue(json!({"runId": SOURCE}))).await;
        assert_eq!(
            (status, code(&v)),
            (422, "delivered-head-unreadable"),
            "{what}: {v}"
        );
        let message = v["message"].as_str().unwrap();
        assert_eq!(
            message,
            format!("the delivered bundle of run {SOURCE} cannot be read as a head"),
            "{what}"
        );
        assert!(
            !message.contains(cause),
            "{what}: the cause is not in the body: {message}"
        );
        let log = logs.join("\n");
        assert!(
            log.contains(SOURCE) && log.contains(cause),
            "{what}: the log has the cause: {log}"
        );
        if what == "duplicate ref" {
            assert!(
                log.contains(&wanted),
                "names the ref that was asked for: {log}"
            );
            assert!(
                !log.contains("refs/heads/snapshot"),
                "not the snapshot ref: {log}"
            );
        }
        assert!(
            !log.contains("PACK"),
            "{what}: no bytes of the object in the log"
        );
    }
}

// ---- continueFrom on a repository run --------------------------------------------------------------------------

const REPO: &str = "cc:hamh-e2e-python-calc";

fn repo_body(base_ref: Option<&str>) -> Value {
    let mut b = with_continue(json!({"runId": SOURCE}));
    b["target"] = json!({"mode": "repo", "repo": REPO});
    if let Some(r) = base_ref {
        b["baseRef"] = json!(r);
    }
    b
}

/// A finished run of `ME` on `target` whose verified bundle names `HEAD`.
fn world_with_source_on(target: Value) -> World {
    let w = World::new();
    w.add_run(run_item(
        SOURCE,
        ME,
        "complete",
        "2026-10-15T13:00:00Z",
        target,
    ));
    w.put_object(&s3::verified_bundle_key(SOURCE), bundle(HEAD, SOURCE));
    w
}

fn repo_source() -> World {
    world_with_source_on(json!({"mode": "repo", "repo": REPO}))
}

#[tokio::test]
async fn a_repository_follow_up_stores_the_repository_the_head_and_the_source() {
    let w = repo_source();
    let (status, v) = start(&w, "k-1", ME, &repo_body(None)).await;
    assert_eq!(status, 201, "{v}");
    let id = v["runId"].as_str().unwrap().to_string();
    let stored = stored_request(&w, &id);
    assert_eq!(
        serde_json::to_value(&stored.target).unwrap(),
        json!({"mode": "repo", "repo": REPO})
    );
    let c = stored.continue_from.expect("continueFrom is stored");
    assert_eq!(
        (c.run_id.as_str(), c.head_sha.as_deref()),
        (SOURCE, Some(HEAD))
    );
    // The run item names the source run and the repository, like any follow-up.
    let run = w.runs.lock().unwrap()[id.as_str()].clone();
    assert_eq!(run.continue_from.as_deref(), Some(SOURCE));
    assert_eq!(w.started.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_repository_follow_up_may_follow_a_run_that_made_its_repository() {
    let w = world_with_source_on(json!({"mode": "new-repo", "repo": REPO}));
    let (status, v) = start(&w, "k-1", ME, &repo_body(None)).await;
    assert_eq!(status, 201, "{v}");
}

#[tokio::test]
async fn a_repository_follow_up_names_no_base_ref_but_the_branch_of_its_source() {
    for ok in [
        format!("harness/{SOURCE}"),
        format!("refs/heads/harness/{SOURCE}"),
    ] {
        let w = repo_source();
        let (status, v) = start(&w, "k-1", ME, &repo_body(Some(&ok))).await;
        assert_eq!(status, 201, "{ok}: {v}");
    }
    for wrong in [
        "main",
        "harness/01K5ZQ4T8M3N6P7R9S0V1W2X5A",
        "refs/heads/main",
    ] {
        let w = repo_source();
        let (status, v) = start(&w, "k-1", ME, &repo_body(Some(wrong))).await;
        assert_eq!(
            (status, code(&v)),
            (400, "invalid-continue-from"),
            "{wrong}: {v}"
        );
        let m = v["message"].as_str().unwrap();
        assert!(
            m.contains("baseRef") && m.contains(&format!("harness/{SOURCE}")),
            "{wrong}: {m}"
        );
        // Refused on the request alone: the source run was not read.
        assert_eq!(w.runs.lock().unwrap().len(), 1);
        assert!(w.idem.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn a_follow_up_is_never_a_new_repository_or_a_repository_other_than_codecommit() {
    for (what, target) in [
        (
            "a new repository",
            json!({"mode": "new-repo", "repo": "cc:hamh-e2e-new-20261017"}),
        ),
        (
            "a GitHub repository",
            json!({"mode": "repo", "repo": "gh:acme/widgets"}),
        ),
        (
            "a GitLab repository",
            json!({"mode": "repo", "repo": "gl:acme/widgets"}),
        ),
    ] {
        let w = repo_source();
        let mut b = with_continue(json!({"runId": SOURCE}));
        b["target"] = target;
        let (status, v) = start(&w, "k-1", ME, &b).await;
        assert_eq!(
            (status, code(&v)),
            (400, "invalid-continue-from"),
            "{what}: {v}"
        );
        assert_eq!(w.runs.lock().unwrap().len(), 1, "{what}");
        assert!(w.idem.lock().unwrap().is_empty(), "{what}");
    }
}

#[tokio::test]
async fn a_repository_follow_up_continues_a_run_of_the_same_repository_only() {
    for (what, source) in [
        (
            "another repository",
            json!({"mode": "repo", "repo": "cc:hamh-e2e-calc-two"}),
        ),
        ("an s3-only run", json!({"mode": "s3-only"})),
    ] {
        let w = world_with_source_on(source);
        let (status, v) = start(&w, "k-1", ME, &repo_body(None)).await;
        assert_eq!(
            (status, code(&v)),
            (400, "invalid-continue-from"),
            "{what}: {v}"
        );
        let m = v["message"].as_str().unwrap();
        assert!(
            m.contains("hamh-e2e-python-calc") && m.contains("same repository"),
            "{what}: {m}"
        );
        nothing_new(&w);
    }
}

#[tokio::test]
async fn another_principals_repository_run_answers_like_a_missing_one() {
    let w = repo_source();
    let (status, v) = start(&w, "k-1", OTHER, &repo_body(None)).await;
    assert_eq!((status, code(&v)), (404, "not-found"), "{v}");
    assert!(!v.to_string().contains(REPO), "{v}");
}

#[tokio::test]
async fn an_s3_only_follow_up_of_a_repository_run_is_still_allowed() {
    let w = repo_source();
    let (status, v) = start(&w, "k-1", ME, &with_continue(json!({"runId": SOURCE}))).await;
    assert_eq!(status, 201, "{v}");
}

#[tokio::test]
async fn a_retry_of_a_repository_follow_up_is_one_run() {
    let w = repo_source();
    let b = repo_body(Some(&format!("harness/{SOURCE}")));
    let (_, a) = start(&w, "k-1", ME, &b).await;
    let (status, again) = start(&w, "k-1", ME, &b).await;
    assert_eq!(status, 201, "{again}");
    assert_eq!(a["runId"], again["runId"]);
    // The source run and the follow-up: a retry made no second run (Step Functions keeps one execution per name).
    assert_eq!(w.runs.lock().unwrap().len(), 2);
}
