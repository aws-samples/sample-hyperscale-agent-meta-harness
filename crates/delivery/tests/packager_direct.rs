//! `packager` for a direct run: `PackageRequest.direct` carries what Cline
//! said about itself. The zip is the complete-name zip, and EVIDENCE.md shows the label, the model, the rules, the
//! wall time, the objective, Cline's last message and one sentence on what the harness did. It has no Gates,
//! Requirements, Evaluator or Task section, and none of its text, whatever the outcome, says the harness checked,
//! tested, gated or evaluated the work.
#![allow(clippy::unwrap_used)] // test assertions unwrap by design

mod common;

use common::*;
use delivery::git::Git;
use delivery::packager::{PackageRequest, Packager, render_evidence, top_dir};
use harness_protocol::common::Usage;
use harness_protocol::direct::{
    DirectEvidence, DirectOutcome, DirectRuleRef, HeadSource, banned_claim_word,
};
use harness_protocol::store::s3::verified_bundle_key;
use harness_protocol::store::{ArtifactKind, s3};
use std::io::{Cursor, Read};
use std::path::PathBuf;

const REPO: &str = "hamh-e2e-python-calc-20260925";
const PR: &str = "https://us-east-1.console.aws.amazon.com/codesuite/codecommit/repositories/hamh-e2e-python-calc-20260925/pull-requests/1/details";

struct World {
    _tmp: tempfile::TempDir,
    base: String,
    head: String,
    p: Packager<MemBlobs, FakeArtifacts>,
}

fn world() -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let (work, shas) = work_repo(
        &root,
        "work",
        &[&[
            ("src/calc.py", "def add(a, b):\n    return a + b\n"),
            ("tests/test_calc.py", "from src.calc import add\n"),
        ]],
    );
    let head = commit(
        &work,
        &[(
            "src/calc.py",
            Some(
                "def add(a, b):\n    return a + b\n\ndef pct(a, b):\n    return (b - a) / a * 100\n",
            ),
        )],
        "pct",
    );
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

fn evidence(outcome: DirectOutcome) -> DirectEvidence {
    DirectEvidence {
        outcome,
        label: outcome.label().to_string(),
        detail: None,
        stop_reason: Some("end_turn".into()),
        cline_message: Some("Added pct(a, b) in src/calc.py.\nRan the suite: 3 passed.".into()),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        engine_version: Some("3.0.71".into()),
        rules: vec![DirectRuleRef {
            name: "00-my-rules.md".into(),
            sha256: "ab".repeat(32),
            bytes: Some(2943),
        }],
        usage: None,
        wall_s: Some(3731),
        head_source: HeadSource::Final,
        final_model_id: None,
        model_switches: 0,
        test_report: None,
        review: None,
        flow: None,
        engine: None,
    }
}

fn req(w: &World, d: DirectEvidence) -> PackageRequest {
    PackageRequest {
        run_id: RUN.into(),
        repo: REPO.into(),
        base_sha: Some(w.base.clone()),
        head_sha: w.head.clone(),
        objective: "Add percent_change(a, b) with unit coverage".into(),
        pr_url: Some(PR.into()),
        direct: d,
    }
}

fn entries(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut a = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
    let mut out = Vec::new();
    for i in 0..a.len() {
        let mut f = a.by_index(i).unwrap();
        if f.is_dir() {
            continue;
        }
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        out.push((f.name().unwrap().to_string(), b));
    }
    out
}

#[tokio::test]
async fn a_direct_run_packages_the_complete_name_zip_and_an_evidence_zip() {
    let w = world();
    let out =
        w.p.package(&req(&w, evidence(DirectOutcome::ReportedDone)))
            .await
            .unwrap();
    let key = s3::deliverable_key(RUN, REPO, &w.head[..7]);
    let zip = w.p.blobs.get(&key).expect("the complete-name zip");
    assert!(
        w.p.blobs
            .get(&s3::partial_deliverable_key(RUN, REPO, &w.head[..7]))
            .is_none(),
        "a direct run never writes the partial name"
    );
    assert!(out.artifact_id.starts_with("zip-"), "{}", out.artifact_id);
    assert_eq!(out.pr_url.as_deref(), Some(PR));
    let kinds: Vec<ArtifactKind> =
        w.p.artifacts
            .items
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.kind)
            .collect();
    assert!(kinds.contains(&ArtifactKind::Zip), "{kinds:?}");
    assert!(!kinds.contains(&ArtifactKind::Partial), "{kinds:?}");
    let top = top_dir(REPO, RUN, &w.head) + "/";
    let rel: Vec<String> = entries(&zip)
        .into_iter()
        .map(|(n, _)| n[top.len()..].to_string())
        .collect();
    for want in ["repo.bundle", "EVIDENCE.md", "MANIFEST.sha256"] {
        assert!(rel.iter().any(|r| r == want), "{want} in {rel:?}");
    }
    assert!(
        !rel.iter()
            .any(|r| r.starts_with("evidence/gates/") || r == "evidence/evaluator/verdict.json"),
        "no gate report or evaluator verdict file in a direct run's zip: {rel:?}"
    );
}

#[tokio::test]
async fn the_run_evidence_objects_are_still_zipped() {
    let w = world();
    w.p.blobs.insert(
        &format!("runs/{RUN}/evidence/direct.json"),
        b"{\"schemaVersion\":1}".to_vec(),
    );
    w.p.package(&req(&w, evidence(DirectOutcome::ReportedDone)))
        .await
        .unwrap();
    let zip =
        w.p.blobs
            .get(&s3::deliverable_key(RUN, REPO, &w.head[..7]))
            .unwrap();
    let top = top_dir(REPO, RUN, &w.head) + "/";
    let rel: Vec<String> = entries(&zip)
        .into_iter()
        .map(|(n, _)| n[top.len()..].to_string())
        .collect();
    assert!(rel.iter().any(|r| r == "evidence/direct.json"), "{rel:?}");
}

#[test]
fn evidence_md_names_the_model_the_run_ended_on_after_its_switches() {
    let md_of = |final_model: Option<&str>, n: u32| {
        let mut d = evidence(DirectOutcome::ReportedDone);
        d.final_model_id = final_model.map(str::to_string);
        d.model_switches = n;
        render_evidence(&PackageRequest {
            head_sha: "a".repeat(40),
            base_sha: Some("b".repeat(40)),
            ..req_stub(d)
        })
    };
    // The Model row stays the model the user chose.
    let two = md_of(Some("us.anthropic.claude-sonnet-5-5"), 2);
    assert!(
        two.contains("| Model | us.anthropic.claude-opus-5-5 |"),
        "{two}"
    );
    assert!(
        two.contains("| Ran on | us.anthropic.claude-sonnet-5-5 after 2 switches |"),
        "{two}"
    );
    let one = md_of(Some("global.anthropic.claude-opus-5-5"), 1);
    assert!(
        one.contains("| Ran on | global.anthropic.claude-opus-5-5 after 1 switch |"),
        "{one}"
    );
    // A run that never switched says nothing more than before.
    let none = md_of(None, 0);
    assert!(!none.contains("Ran on"), "{none}");
    // A cell that carries a pipe or a line break cannot break the table.
    let odd = md_of(Some("pipe|model\nrow"), 3);
    assert!(!odd.contains("pipe|model"), "{odd}");
}

/// The EVIDENCE.md of a run that reported `usage`, as the packager renders it.
fn md_with_usage(usage: Option<Usage>) -> String {
    let mut d = evidence(DirectOutcome::ReportedDone);
    d.usage = usage;
    render_evidence(&req_stub(d))
}

fn tokens_row(md: &str) -> Option<&str> {
    md.lines().find(|l| l.starts_with("| Tokens |"))
}

/// The row used to print `input 22, output 5718` for a run whose card showed 316198 tokens: the cache tokens, 273073 read and 37385
/// written, were left out and nothing said so. The row now labels the four kinds of token and the total, and the total is the sum the run
/// card prints (the four kinds added).
#[test]
fn the_tokens_row_labels_the_four_kinds_of_token_and_their_total() {
    let md = md_with_usage(Some(Usage {
        input_tokens: 22,
        output_tokens: 5_718,
        cache_read_input_tokens: 273_073,
        cache_write_input_tokens: 37_385,
    }));
    assert_eq!(
        tokens_row(&md),
        Some(
            "| Tokens | total 316198 = input 22 (not cached) + output 5718 + cache read 273073 + cache write 37385 |"
        ),
        "{md}"
    );
    // The figures of the T3 run: the card showed 3979137 tokens.
    let md = md_with_usage(Some(Usage {
        input_tokens: 104,
        output_tokens: 111_304,
        cache_read_input_tokens: 3_494_052,
        cache_write_input_tokens: 373_677,
    }));
    assert_eq!(
        tokens_row(&md),
        Some(
            "| Tokens | total 3979137 = input 104 (not cached) + output 111304 + cache read 3494052 + cache write 373677 |"
        ),
        "{md}"
    );
}

#[test]
fn the_total_of_the_tokens_row_is_the_sum_of_its_four_figures() {
    for (i, o, r, w) in [
        (0, 0, 0, 0),
        (1, 2, 3, 4),
        (22, 5_718, 0, 0),
        (0, 0, 273_073, 37_385),
        (1_000_000, 128_000, 9_999_999, 1),
    ] {
        let md = md_with_usage(Some(Usage {
            input_tokens: i,
            output_tokens: o,
            cache_read_input_tokens: r,
            cache_write_input_tokens: w,
        }));
        let row = tokens_row(&md).unwrap();
        let figures: Vec<u64> = row
            .split(|c: char| !c.is_ascii_digit())
            .filter_map(|t| t.parse().ok())
            .collect();
        assert_eq!(figures, vec![i + o + r + w, i, o, r, w], "{row}");
    }
}

#[test]
fn a_total_that_would_overflow_stops_at_the_largest_number() {
    let md = md_with_usage(Some(Usage {
        input_tokens: u64::MAX,
        output_tokens: 5,
        cache_read_input_tokens: u64::MAX,
        cache_write_input_tokens: 1,
    }));
    let row = tokens_row(&md).unwrap();
    assert!(
        row.starts_with("| Tokens | total 18446744073709551615 = "),
        "{row}"
    );
}

#[test]
fn a_run_that_reported_no_usage_has_no_tokens_row() {
    let md = md_with_usage(None);
    assert_eq!(tokens_row(&md), None, "{md}");
}

#[test]
fn the_tokens_row_claims_nothing_the_harness_did_not_do() {
    let md = md_with_usage(Some(Usage {
        input_tokens: 22,
        output_tokens: 5_718,
        cache_read_input_tokens: 273_073,
        cache_write_input_tokens: 37_385,
    }));
    assert_eq!(banned_claim_word(&md), None, "{md}");
}

#[test]
fn evidence_md_shows_the_label_the_model_the_rules_and_what_the_harness_did() {
    let w_head = "a".repeat(40);
    let r = PackageRequest {
        head_sha: w_head,
        base_sha: Some("b".repeat(40)),
        ..req_stub(evidence(DirectOutcome::ReportedDone))
    };
    let md = render_evidence(&r);
    assert!(md.contains("| Outcome | Cline reported done |"), "{md}");
    assert!(
        md.contains("| Model | us.anthropic.claude-opus-5-5 |"),
        "{md}"
    );
    assert!(md.contains("00-my-rules.md (abababababab)"), "{md}");
    assert!(md.contains("| Wall time | 1h 2m 11s |"), "{md}");
    assert!(md.contains("## Objective"), "{md}");
    assert!(
        md.contains("Add percent_change(a, b) with unit coverage"),
        "{md}"
    );
    assert!(md.contains("## Cline's last message"), "{md}");
    assert!(
        md.contains("> Added pct(a, b) in src/calc.py.\n> Ran the suite: 3 passed."),
        "{md}"
    );
    assert!(md.contains("## What the harness did"), "{md}");
    assert!(
        md.contains(
            "The harness hosted the session, committed what Cline left in the tree, scanned the commits for secrets and packaged this zip. It ran no test and made no judgment on the work."
        ),
        "{md}"
    );
    for gone in [
        "## Gates",
        "## Requirements",
        "## Evaluator verdicts",
        "## Task gate reports",
        "Required gates green",
        "Final gates",
    ] {
        assert!(
            !md.contains(gone),
            "{gone} in a direct run's EVIDENCE.md:\n{md}"
        );
    }
}

fn req_stub(d: DirectEvidence) -> PackageRequest {
    PackageRequest {
        run_id: RUN.into(),
        repo: REPO.into(),
        base_sha: None,
        head_sha: "c".repeat(40),
        objective: "Add percent_change(a, b) with unit coverage".into(),
        pr_url: Some(PR.into()),
        direct: d,
    }
}

#[test]
fn no_outcome_makes_evidence_md_claim_the_harness_checked_anything() {
    for outcome in DirectOutcome::ALL {
        let mut d = evidence(outcome);
        d.detail = Some("no output for 900 s".into());
        d.head_source = if outcome == DirectOutcome::StoppedSessionLost {
            HeadSource::Snapshot
        } else {
            HeadSource::Final
        };
        let md = render_evidence(&req_stub(d));
        assert!(md.contains(outcome.label()), "{outcome:?}: {md}");
        assert_eq!(
            banned_claim_word(&md),
            None,
            "{outcome:?} EVIDENCE.md says the harness checked something:\n{md}"
        );
    }
}

#[test]
fn only_reported_done_says_reported_done() {
    for outcome in DirectOutcome::ALL {
        let md = render_evidence(&req_stub(evidence(outcome)));
        assert_eq!(
            md.contains("Cline reported done"),
            outcome == DirectOutcome::ReportedDone,
            "{outcome:?}"
        );
    }
}

#[test]
fn a_snapshot_head_says_so_and_a_missing_message_is_stated() {
    let mut d = evidence(DirectOutcome::StoppedSessionLost);
    d.head_source = HeadSource::Snapshot;
    d.cline_message = None;
    d.detail = Some("no heartbeat for 600 s".into());
    let md = render_evidence(&req_stub(d));
    assert!(md.contains("| Delivered from | last snapshot |"), "{md}");
    assert!(md.contains("| Detail | no heartbeat for 600 s |"), "{md}");
    assert!(md.contains("Cline left no final message."), "{md}");
    let section = md.split("## Cline's last message").nth(1).unwrap();
    let section = section.split("## What the harness did").next().unwrap();
    assert!(
        !section.contains('>'),
        "no quote block without a message: {section}"
    );
}

#[test]
fn a_message_cannot_break_out_of_its_quote_or_the_tables() {
    let mut d = evidence(DirectOutcome::ReportedDone);
    d.cline_message = Some("line one\n\n## Gates\n| a | b |\nlast".into());
    d.detail = Some("pipe | and\nnewline".into());
    let md = render_evidence(&req_stub(d));
    assert!(
        md.contains("> line one\n>\n> ## Gates\n> | a | b |\n> last"),
        "{md}"
    );
    assert!(md.contains("| Detail | pipe \\| and newline |"), "{md}");
    assert!(!md.lines().any(|l| l == "## Gates"), "{md}");
}

#[test]
fn a_full_length_message_is_kept_whole() {
    let mut d = evidence(DirectOutcome::ReportedDone);
    let text: String = "x".repeat(8_000);
    d.cline_message = Some(text.clone());
    let md = render_evidence(&req_stub(d));
    assert!(md.contains(&format!("> {text}")), "message cut");
}

#[test]
fn the_wire_form_is_camel_case() {
    let r = req_stub(evidence(DirectOutcome::ReportedDone));
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(v["direct"]["outcome"], "reported_done");
    assert_eq!(v["direct"]["headSource"], "final");
    assert!(v.get("baseSha").is_none_or(|b| b.is_null()), "{v}");
    let back: PackageRequest = serde_json::from_value(serde_json::to_value(&r).unwrap()).unwrap();
    assert_eq!(back, r);
    let _unused: Option<PathBuf> = None;
}
