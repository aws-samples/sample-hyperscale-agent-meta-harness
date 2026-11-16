//! The records a person with only the zip reads say what the harness told the engine about the delivery, and what the zip holds
//! that a repository would have refused.
//!
//! * `PackageRequest.direct.deliveryNote` is the block the worker wrote before the first prompt of the session; EVIDENCE.md quotes it in a
//!   section of its own. A run that was told nothing has no such section.
//! * A run that delivers a zip alone says so in `PackageRequest.target` (as the bundle validator is told). The packager then reads the
//!   changes of the bundle it packages and puts the sentence the run card shows into the Detail row of EVIDENCE.md: "The zip holds a
//!   workflow file (...)". A pull-request run, and a zip without such a file, get no sentence.
//! * The changes are read from the commit the bundle validator judged from (`PackageRequest.judgedBaseSha`, the plan's
//!   `validate.baseSha`; absent is the empty tree), so EVIDENCE.md names the files the validator reported and the run card shows. For a
//!   follow-up of a run that was not delivered that commit is further back than the staged base (`baseSha`).
//! * The zip's copy of the plan's record, `evidence/direct.json`, holds the same sentence as the member `zipHolds`, after the members
//!   the plan wrote, which stay byte for byte. The object in the bucket is not changed, and a zip without such a file holds the record
//!   as the bucket holds it.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::git::Git;
use delivery::packager::{PackageRequest, Packager, render_evidence};
use delivery::validator::DeliveryTarget;
use harness_protocol::direct::{DirectEvidence, DirectOutcome, HeadSource, banned_claim_word};
use harness_protocol::store::s3;
use harness_protocol::store::s3::verified_bundle_key;
use std::io::{Cursor, Read};

const REPO: &str = "s3-only";
const NOTE: &str = "How this run delivers (written by the harness): this run delivers only a zip. A zip takes files under `.github/workflows/` and a root `buildspec.yml`, so add such a file when the task asks for it, and name each one you add in your last message.";
const HOLDS: &str = "The zip holds a workflow file (.github/workflows/ci.yml); it runs only if the code is pushed to a git host that runs such files.";

fn evidence(note: Option<&str>, detail: Option<&str>) -> DirectEvidence {
    DirectEvidence {
        outcome: DirectOutcome::ReportedDone,
        label: "Cline reported done".into(),
        detail: detail.map(str::to_string),
        stop_reason: Some("end_turn".into()),
        cline_message: Some("Added the workflow file .github/workflows/ci.yml.".into()),
        model_id: "us.anthropic.claude-sonnet-5-5".into(),
        engine_version: Some("3.0.71".into()),
        rules: vec![],
        usage: None,
        wall_s: Some(190),
        head_source: HeadSource::Final,
        final_model_id: None,
        model_switches: 0,
        test_report: None,
        review: None,
        flow: None,
        engine: None,
        delivery_note: note.map(str::to_string),
    }
}

fn stub(d: DirectEvidence, target: DeliveryTarget) -> PackageRequest {
    PackageRequest {
        run_id: RUN.into(),
        repo: REPO.into(),
        base_sha: None,
        judged_base_sha: None,
        head_sha: "a".repeat(40),
        objective: "Add a CI workflow".into(),
        target,
        pr_url: None,
        direct: d,
    }
}

#[test]
fn b850_evidence_quotes_what_the_engine_was_told_about_the_delivery() {
    let md = render_evidence(&stub(evidence(Some(NOTE), None), DeliveryTarget::Zip));
    assert!(
        md.contains("## What the harness told Cline about the delivery\n\n> How this run delivers (written by the harness): this run delivers only a zip."),
        "{md}"
    );
    assert!(
        md.contains(&format!("> {NOTE}\n")),
        "the block, word for word: {md}"
    );
    let at = |needle: &str| {
        md.find(needle)
            .unwrap_or_else(|| panic!("{needle} in {md}"))
    };
    assert!(
        at("## Objective") < at("## What the harness told Cline about the delivery")
            && at("## What the harness told Cline about the delivery")
                < at("## Cline's last message"),
        "after the objective, before the last message: {md}"
    );
    assert_eq!(banned_claim_word(&md), None, "{md}");
}

#[test]
fn b850_a_run_that_was_told_nothing_has_no_such_section() {
    let md = render_evidence(&stub(evidence(None, None), DeliveryTarget::Zip));
    assert!(!md.contains("about the delivery"), "{md}");
    assert!(!md.contains("How this run delivers"), "{md}");
}

#[test]
fn b850_the_section_names_the_engine_that_ran() {
    let mut d = evidence(Some(NOTE), None);
    d.engine = Some(harness_protocol::common::Engine::new("opencode").unwrap());
    d.label = "OpenCode reported done".into();
    let md = render_evidence(&stub(d, DeliveryTarget::Zip));
    assert!(
        md.contains("## What the harness told OpenCode about the delivery"),
        "{md}"
    );
}

// ---- the whole package: the zip of a zip-only run that holds a workflow file ------------------------------------------------------------

struct World {
    _tmp: tempfile::TempDir,
    head: String,
    base: String,
    p: Packager<MemBlobs, FakeArtifacts>,
}

/// A repository whose run added a workflow file (when `workflow`) beside its code, bundled as the validator leaves it.
fn world(workflow: bool) -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(
        &root,
        "work",
        &[&[("src/calc.py", "def add(a, b):\n    return a + b\n")]],
    );
    let mut files: Vec<(&str, Option<&str>)> = vec![(
        "src/calc.py",
        Some("def add(a, b):\n    return a + b\n\ndef pct(a, b):\n    return (b - a) / a * 100\n"),
    )];
    if workflow {
        files.push((
            ".github/workflows/ci.yml",
            Some("name: ci\non: [push]\njobs: {}\n"),
        ));
    }
    let head = commit(&work, &files, "pct and its workflow");
    let f = root.join("verified.bundle");
    verified_bundle(&work, RUN, &f);
    let p = Packager {
        blobs: MemBlobs::default(),
        artifacts: FakeArtifacts::default(),
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: root.join("scratch"),
    };
    p.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    World {
        _tmp: tmp,
        base: shas[0].clone(),
        head,
        p,
    }
}

fn req(w: &World, target: DeliveryTarget, detail: Option<&str>) -> PackageRequest {
    PackageRequest {
        run_id: RUN.into(),
        repo: REPO.into(),
        base_sha: Some(w.base.clone()),
        judged_base_sha: Some(w.base.clone()),
        head_sha: w.head.clone(),
        objective: "Add a CI workflow".into(),
        target,
        pr_url: None,
        direct: evidence(Some(NOTE), detail),
    }
}

/// EVIDENCE.md of the zip the packager stored.
async fn evidence_md(w: &World, r: &PackageRequest) -> String {
    w.p.package(r).await.unwrap();
    let key = s3::deliverable_key(RUN, REPO, &w.head[..7]);
    let zip = w.p.blobs.get(&key).expect("the zip is stored");
    let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
    for i in 0..a.len() {
        let mut f = a.by_index(i).unwrap();
        if f.name().unwrap().ends_with("/EVIDENCE.md") {
            let mut s = String::new();
            f.read_to_string(&mut s).unwrap();
            return s;
        }
    }
    panic!("the zip has no EVIDENCE.md");
}

#[tokio::test]
async fn b850_the_zip_of_a_zip_only_run_says_in_evidence_that_it_holds_a_workflow_file() {
    let w = world(true);
    let md = evidence_md(&w, &req(&w, DeliveryTarget::Zip, None)).await;
    println!(
        "B850 TRIGGER PRESENT: the run changed a workflow file: {}",
        md.contains("src/")
    );
    assert!(md.contains(&format!("| Detail | {HOLDS} |")), "{md}");
    assert!(md.contains(&format!("> {NOTE}\n")), "{md}");
}

#[tokio::test]
async fn b850_the_sentence_follows_the_detail_the_run_already_has() {
    let w = world(true);
    let md = evidence_md(
        &w,
        &req(&w, DeliveryTarget::Zip, Some("The run used 2 sessions.")),
    )
    .await;
    assert!(
        md.contains(&format!("| Detail | The run used 2 sessions. {HOLDS} |")),
        "{md}"
    );
    let md = evidence_md(
        &w,
        &req(&w, DeliveryTarget::Zip, Some("no pull request: none asked")),
    )
    .await;
    assert!(
        md.contains(&format!(
            "| Detail | no pull request: none asked. {HOLDS} |"
        )),
        "{md}"
    );
}

#[tokio::test]
async fn b850_a_zip_without_such_a_file_and_a_pull_request_run_get_no_sentence() {
    let plain = world(false);
    let md = evidence_md(&plain, &req(&plain, DeliveryTarget::Zip, None)).await;
    assert!(!md.contains("The zip holds"), "{md}");
    assert!(!md.contains("| Detail |"), "{md}");
    // The same tree as a pull-request run (it could not hold the file; the request says the target, and the packager reads no changes).
    let w = world(true);
    let md = evidence_md(&w, &req(&w, DeliveryTarget::PullRequest, None)).await;
    assert!(!md.contains("The zip holds"), "{md}");
}

#[test]
fn b850_a_request_without_the_member_is_a_pull_request_and_zip_is_the_word_the_plan_writes() {
    let mut v =
        serde_json::to_value(stub(evidence(None, None), DeliveryTarget::PullRequest)).unwrap();
    assert!(
        v.get("target").is_none(),
        "the default is off the wire: {v}"
    );
    let back: PackageRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(back.target, DeliveryTarget::PullRequest);
    v["target"] = serde_json::json!("zip");
    let zip: PackageRequest = serde_json::from_value(v).unwrap();
    assert_eq!(zip.target, DeliveryTarget::Zip);
}

// ---- the base the sentence is read from: the one the validator judged from -----------------------------------------------------------

/// Three commits of one line of work: the base run A started from, the head run A left (a run that was not delivered), and the head of
/// run B, which continues A. `first` is what the base holds, `a` what run A changed, `b` what run B changed.
struct Chain {
    _tmp: tempfile::TempDir,
    base: String,
    a_head: String,
    b_head: String,
    p: Packager<MemBlobs, FakeArtifacts>,
}

fn chain(first: &[(&str, &str)], a: &[(&str, Option<&str>)], b: &[(&str, Option<&str>)]) -> Chain {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(&root, "work", &[first]);
    let a_head = commit(&work, a, "run A");
    let b_head = commit(&work, b, "run B");
    let f = root.join("verified.bundle");
    verified_bundle(&work, RUN, &f);
    let p = Packager {
        blobs: MemBlobs::default(),
        artifacts: FakeArtifacts::default(),
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: root.join("scratch"),
    };
    p.blobs
        .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
    Chain {
        _tmp: tmp,
        base: shas[0].clone(),
        a_head,
        b_head,
        p,
    }
}

/// The package request of run B: staged on the head run A left, judged from `judged`.
fn follow_up_req(c: &Chain, judged: Option<&str>) -> PackageRequest {
    PackageRequest {
        run_id: RUN.into(),
        repo: REPO.into(),
        base_sha: Some(c.a_head.clone()),
        judged_base_sha: judged.map(str::to_string),
        head_sha: c.b_head.clone(),
        objective: "Go on with the work".into(),
        target: DeliveryTarget::Zip,
        pr_url: None,
        direct: evidence(Some(NOTE), None),
    }
}

/// An entry of the delivered zip, by its path under the top directory.
fn zip_entry(blobs: &MemBlobs, head: &str, rel: &str) -> Option<Vec<u8>> {
    let key = s3::deliverable_key(RUN, REPO, &head[..7]);
    let zip = blobs.get(&key).expect("the zip is stored");
    let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
    for i in 0..a.len() {
        let mut f = a.by_index(i).unwrap();
        if f.name().unwrap().ends_with(&format!("/{rel}")) {
            let mut body = Vec::new();
            f.read_to_end(&mut body).unwrap();
            return Some(body);
        }
    }
    None
}

async fn chain_md(c: &Chain, judged: Option<&str>) -> String {
    c.p.package(&follow_up_req(c, judged)).await.unwrap();
    String::from_utf8(zip_entry(&c.p.blobs, &c.b_head, "EVIDENCE.md").unwrap()).unwrap()
}

/// Run A (not delivered) added the workflow file; run B continues it and does not touch the file. The validator judges B from the base
/// A started from, reports the file, and the card of B names it. EVIDENCE.md of B's zip names it too.
#[tokio::test]
async fn b850_a_follow_up_of_a_run_that_was_not_delivered_names_the_file_the_card_names() {
    let calc = "def add(a, b):\n    return a + b\n";
    let c = chain(
        &[("src/calc.py", calc)],
        &[(
            ".github/workflows/ci.yml",
            Some("name: ci\non: [push]\njobs: {}\n"),
        )],
        &[(
            "src/calc.py",
            Some("def add(a, b):\n    return a + b\n\ndef sub(a, b):\n    return a - b\n"),
        )],
    );
    let from_staged = chain_md(&c, Some(&c.a_head)).await;
    println!(
        "B850 TRIGGER PRESENT: read from the staged base the changes of run B hold no workflow file: {}",
        !from_staged.contains("The zip holds")
    );
    assert!(
        !from_staged.contains("The zip holds"),
        "the trigger: {from_staged}"
    );
    let md = chain_md(&c, Some(&c.base)).await;
    assert!(
        md.contains(&format!("| Detail | {HOLDS} |")),
        "read from the base the validator judged from: {md}"
    );
}

/// The other way round: the base already held the workflow file, run A changed it and run B put the earlier content back. From the
/// judged base nothing changed, the validator reports nothing and the card names nothing; EVIDENCE.md names nothing either.
#[tokio::test]
async fn b850_a_file_that_is_as_the_judged_base_had_it_is_not_named() {
    let workflow = "name: ci\non: [push]\njobs: {}\n";
    let c = chain(
        &[
            ("src/calc.py", "def add(a, b):\n    return a + b\n"),
            (".github/workflows/ci.yml", workflow),
        ],
        &[(
            ".github/workflows/ci.yml",
            Some("name: changed\non: [push]\njobs: {}\n"),
        )],
        &[
            (".github/workflows/ci.yml", Some(workflow)),
            ("src/calc.py", Some("def add(a, b):\n    return b + a\n")),
        ],
    );
    let from_staged = chain_md(&c, Some(&c.a_head)).await;
    println!(
        "B850 TRIGGER PRESENT: read from the staged base run B changed the workflow file: {}",
        from_staged.contains("The zip holds")
    );
    assert!(from_staged.contains(HOLDS), "the trigger: {from_staged}");
    let md = chain_md(&c, Some(&c.base)).await;
    assert!(!md.contains("The zip holds"), "{md}");
}

/// No judged base is the empty tree, as the validator reads a request without a base: every file of the head counts.
#[tokio::test]
async fn b850_without_a_judged_base_every_file_of_the_head_counts() {
    let workflow = "name: ci\non: [push]\njobs: {}\n";
    let c = chain(
        &[
            ("src/calc.py", "def add(a, b):\n    return a + b\n"),
            (".github/workflows/ci.yml", workflow),
        ],
        &[("src/calc.py", Some("def add(a, b):\n    return b + a\n"))],
        &[("src/calc.py", Some("def add(a, b):\n    return a + b\n"))],
    );
    let md = chain_md(&c, None).await;
    assert!(md.contains(&format!("| Detail | {HOLDS} |")), "{md}");
}

#[tokio::test]
async fn b850_a_judged_base_that_is_no_commit_id_is_refused() {
    let w = world(true);
    let mut r = req(&w, DeliveryTarget::Zip, None);
    r.judged_base_sha = Some("main".into());
    assert!(w.p.package(&r).await.is_err());
}

#[test]
fn b850_the_judged_base_is_off_the_wire_when_there_is_none() {
    let mut v = serde_json::to_value(stub(evidence(None, None), DeliveryTarget::Zip)).unwrap();
    assert!(v.get("judgedBaseSha").is_none(), "{v}");
    let back: PackageRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(back.judged_base_sha, None);
    v["judgedBaseSha"] = serde_json::json!("b".repeat(40));
    let named: PackageRequest = serde_json::from_value(v).unwrap();
    assert_eq!(named.judged_base_sha, Some("b".repeat(40)));
}

// ---- the plan's record in the zip -----------------------------------------------------------------------------------------------------

/// `evidence/direct.json` as `plan-delivery` writes it: pretty JSON of its own members and the worker's summary.
fn plan_record() -> Vec<u8> {
    serde_json::to_vec_pretty(&serde_json::json!({
        "schemaVersion": 1, "runId": RUN, "outcome": "reported_done", "label": "Cline reported done", "detail": null,
        "headSource": "final", "plannedAt": "2026-11-16T08:00:00Z", "plannedAtMs": 1_794_816_000_000u64, "failure": null,
        "summary": {"deliveryNote": NOTE, "clineMessage": "Added the workflow file .github/workflows/ci.yml."},
        "slice": 1
    }))
    .unwrap()
}

fn record_key() -> String {
    format!("runs/{RUN}/evidence/direct.json")
}

#[tokio::test]
async fn b850_the_plan_record_in_the_zip_says_what_the_zip_holds() {
    let w = world(true);
    let stored = plan_record();
    w.p.blobs.insert(&record_key(), stored.clone());
    w.p.package(&req(&w, DeliveryTarget::Zip, None))
        .await
        .unwrap();
    let copy =
        zip_entry(&w.p.blobs, &w.head, "evidence/direct.json").expect("the zip holds the record");
    let parsed: serde_json::Value = serde_json::from_slice(&copy).unwrap();
    println!(
        "B850 TRIGGER PRESENT: the record the plan wrote has no such member: {}",
        serde_json::from_slice::<serde_json::Value>(&stored)
            .unwrap()
            .get("zipHolds")
            .is_none()
    );
    assert_eq!(parsed["zipHolds"], serde_json::json!(HOLDS), "{parsed}");
    // Everything the plan wrote is still there, byte for byte up to the brace that closes the record.
    let text = String::from_utf8(stored.clone()).unwrap();
    let before_close = text.trim_end().strip_suffix('}').unwrap().trim_end();
    assert!(
        String::from_utf8(copy.clone())
            .unwrap()
            .starts_with(before_close),
        "the members of the plan are unchanged"
    );
    let mut without = parsed.clone();
    without.as_object_mut().unwrap().remove("zipHolds");
    assert_eq!(
        without,
        serde_json::from_slice::<serde_json::Value>(&stored).unwrap()
    );
    assert_eq!(parsed["summary"]["deliveryNote"], serde_json::json!(NOTE));
    // The object in the bucket is the plan's own record and is not rewritten.
    assert_eq!(w.p.blobs.get(&record_key()).unwrap(), stored);
    // The evidence archive holds the same copy as the zip.
    let ev =
        w.p.blobs
            .get(&s3::evidence_archive_key(RUN, REPO, &w.head[..7]))
            .expect("the evidence archive is stored");
    let mut a = zip::ZipArchive::new(Cursor::new(ev)).unwrap();
    let mut in_archive = Vec::new();
    for i in 0..a.len() {
        let mut f = a.by_index(i).unwrap();
        if f.name().unwrap().ends_with("/evidence/direct.json") {
            f.read_to_end(&mut in_archive).unwrap();
        }
    }
    assert_eq!(in_archive, copy);
}

#[tokio::test]
async fn b850_a_record_on_one_line_gets_the_member_too() {
    let w = world(true);
    w.p.blobs.insert(
        &record_key(),
        br#"{"schemaVersion":1,"runId":"x"}"#.to_vec(),
    );
    w.p.package(&req(&w, DeliveryTarget::Zip, None))
        .await
        .unwrap();
    let copy = zip_entry(&w.p.blobs, &w.head, "evidence/direct.json").unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&copy).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({"schemaVersion": 1, "runId": "x", "zipHolds": HOLDS})
    );
}

#[tokio::test]
async fn b850_a_zip_with_no_such_file_and_a_pull_request_run_hold_the_record_as_the_bucket_holds_it()
 {
    let stored = plan_record();
    let plain = world(false);
    plain.p.blobs.insert(&record_key(), stored.clone());
    plain
        .p
        .package(&req(&plain, DeliveryTarget::Zip, None))
        .await
        .unwrap();
    assert_eq!(
        zip_entry(&plain.p.blobs, &plain.head, "evidence/direct.json").unwrap(),
        stored
    );
    let w = world(true);
    w.p.blobs.insert(&record_key(), stored.clone());
    w.p.package(&req(&w, DeliveryTarget::PullRequest, None))
        .await
        .unwrap();
    assert_eq!(
        zip_entry(&w.p.blobs, &w.head, "evidence/direct.json").unwrap(),
        stored
    );
}

#[tokio::test]
async fn b850_a_record_that_is_no_json_object_is_copied_as_it_is() {
    for odd in [&b"not json"[..], &b"[1, 2]"[..], &b""[..]] {
        let w = world(true);
        w.p.blobs.insert(&record_key(), odd.to_vec());
        w.p.package(&req(&w, DeliveryTarget::Zip, None))
            .await
            .unwrap();
        assert_eq!(
            zip_entry(&w.p.blobs, &w.head, "evidence/direct.json").unwrap(),
            odd,
            "{:?}",
            String::from_utf8_lossy(odd)
        );
    }
}
