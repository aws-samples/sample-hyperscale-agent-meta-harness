//! The container check of `stage`. When the payload carries `containerSignals`, the repo-gateway reads the tree of the base
//! commit it bundles (never a working directory) and answers `containerCheck`: `{}` when no signal matched and `{"found": {"signal", "path"}}`
//! for the first one that did. Without the member there is no check and no member in the answer. A bad line is refused before anything is written.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use delivery::error::DeliveryError;
use delivery::gateway::Gateway;
use delivery::git::Git;
use ham_core::container_signals::DEFAULT_SIGNALS;
use serde_json::{Value, json};
use std::path::PathBuf;

const AT: u64 = 1_790_337_600;

struct World {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    remotes_dir: PathBuf,
    gw: Gateway<FakeCodeCommit, FakeAllowlist, MemBlobs, LocalRemotes>,
}

fn world() -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let remotes_dir = root.join("codecommit");
    let work = root.join("work");
    let home = root.join("home");
    for d in [&remotes_dir, &work, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let gw = Gateway {
        codecommit: FakeCodeCommit::new(&remotes_dir),
        allowlist: FakeAllowlist::with(&[&format!("cc:{FIXTURE}")]),
        blobs: MemBlobs::default(),
        remotes: LocalRemotes::new(&remotes_dir),
        git: Git::new("git", &home),
        stage: "live".into(),
        owner: "team-a".into(),
        bucket: BUCKET.into(),
        work_root: work,
        clock: || AT,
        repo_names: Default::default(),
    };
    World {
        _tmp: tmp,
        root,
        remotes_dir,
        gw,
    }
}

/// The repository as CodeCommit would hold it: `main` with one commit per file set; returns the work repository and the commits.
fn with_files(w: &World, commits: &[&[(&str, &str)]]) -> (PathBuf, Vec<String>) {
    let (work, shas) = work_repo(&w.root, "fixture-work", commits);
    publish(&work, &w.remotes_dir, FIXTURE);
    (work, shas)
}

fn payload(signals: Option<Value>) -> Value {
    let mut v = json!({"operation": "stage", "runId": RUN, "target": format!("cc:{FIXTURE}")});
    if let Some(s) = signals {
        v["containerSignals"] = s;
    }
    v
}

async fn stage(w: &World, v: Value) -> Result<Value, DeliveryError> {
    w.gw.handle(serde_json::from_value(v).unwrap()).await
}

#[tokio::test]
async fn b632_stage_reports_the_first_signal_of_the_list_and_the_path_it_found() {
    let w = world();
    with_files(
        &w,
        &[&[
            ("src/Main.java", "class Main {}"),
            ("docker-compose.yml", "services: {}"),
            (
                "services/api/pom.xml",
                "<artifactId>testcontainers-postgresql</artifactId>",
            ),
        ]],
    );
    let out = stage(&w, payload(Some(json!(["default"])))).await.unwrap();
    assert_eq!(
        out["containerCheck"],
        json!({"found": {"signal": DEFAULT_SIGNALS[0], "path": "services/api/pom.xml"}}),
        "{out}"
    );
    // The rest of the answer is the one it always was.
    assert_eq!(out["baseRef"], "refs/heads/main");
    assert_eq!(
        out["bundleKey"],
        harness_protocol::store::s3::repo_bundle_key(RUN)
    );
}

#[tokio::test]
async fn b632_stage_finds_a_compose_file_by_its_name() {
    let w = world();
    with_files(
        &w,
        &[&[
            ("README.md", "hello"),
            ("deploy/compose.override.yaml", "x: 1"),
        ]],
    );
    let out = stage(&w, payload(Some(json!(["default"])))).await.unwrap();
    assert_eq!(
        out["containerCheck"]["found"],
        json!({"signal": DEFAULT_SIGNALS[7], "path": "deploy/compose.override.yaml"}),
        "{out}"
    );
}

#[tokio::test]
async fn b632_stage_says_it_checked_and_found_nothing_for_a_repository_without_signals() {
    let w = world();
    with_files(
        &w,
        &[&[
            ("README.md", "We mention docker and testcontainers here"),
            ("package.json", "{\"dependencies\": {\"express\": \"4\"}}"),
            ("src/compose.rs", "fn main() {}"),
        ]],
    );
    let out = stage(&w, payload(Some(json!(["default"])))).await.unwrap();
    assert_eq!(out["containerCheck"], json!({}), "{out}");
}

#[tokio::test]
async fn b632_stage_without_the_member_checks_nothing_and_adds_nothing() {
    let w = world();
    with_files(&w, &[&[("docker-compose.yml", "services: {}")]]);
    let out = stage(&w, payload(None)).await.unwrap();
    assert!(out.get("containerCheck").is_none(), "{out}");
}

#[tokio::test]
async fn b632_stage_uses_the_list_it_is_given_and_an_empty_list_finds_nothing() {
    let w = world();
    with_files(
        &w,
        &[&[
            ("docker-compose.yml", "services: {}"),
            ("Makefile", "test:\n\tdocker run x\n"),
        ]],
    );
    let custom = stage(&w, payload(Some(json!(["Makefile contains DOCKER"]))))
        .await
        .unwrap();
    assert_eq!(
        custom["containerCheck"]["found"],
        json!({"signal": "Makefile contains DOCKER", "path": "Makefile"})
    );
    let empty = stage(&w, payload(Some(json!([])))).await.unwrap();
    assert_eq!(empty["containerCheck"], json!({}), "{empty}");
    let mixed = stage(
        &w,
        payload(Some(json!(["default", "Makefile contains docker"]))),
    )
    .await
    .unwrap();
    assert_eq!(
        mixed["containerCheck"]["found"]["path"],
        "docker-compose.yml"
    );
}

#[tokio::test]
async fn b632_stage_reads_the_tree_of_the_base_ref_it_bundles() {
    let w = world();
    let (work, shas) = with_files(
        &w,
        &[
            &[("README.md", "one")],
            &[("docker-compose.yml", "services: {}")],
        ],
    );
    git(&work, &["branch", "dev", &shas[0]]);
    git(
        &work,
        &[
            "push",
            "-q",
            w.remotes_dir
                .join(format!("{FIXTURE}.git"))
                .to_str()
                .unwrap(),
            "dev",
        ],
    );
    let main = stage(&w, payload(Some(json!(["default"])))).await.unwrap();
    assert_eq!(
        main["containerCheck"]["found"]["path"],
        "docker-compose.yml"
    );
    let mut dev = payload(Some(json!(["default"])));
    dev["baseRef"] = json!("dev");
    let dev = stage(&w, dev).await.unwrap();
    assert_eq!(dev["containerCheck"], json!({}), "{dev}");
}

#[tokio::test]
async fn b632_stage_of_an_empty_base_reports_nothing_found() {
    let w = world();
    let out = stage(
        &w,
        json!({"operation": "stage", "runId": RUN, "target": {"mode": "s3-only"}, "containerSignals": ["default"]}),
    )
    .await
    .unwrap();
    assert_eq!(out["containerCheck"], json!({}), "{out}");
}

#[tokio::test]
async fn b632_stage_reads_every_candidate_file_however_many_there_are() {
    let w = world();
    let mut owned: Vec<(String, String)> = (0..70)
        .map(|i| {
            (
                format!("p{i:02}/package.json"),
                "{\"name\": \"x\"}".to_string(),
            )
        })
        .collect();
    owned.push((
        "p70/package.json".to_string(),
        "{\"devDependencies\": {\"testcontainers\": \"10\"}}".to_string(),
    ));
    let files: Vec<(&str, &str)> = owned
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    with_files(&w, &[&files]);
    let out = stage(&w, payload(Some(json!(["default"])))).await.unwrap();
    assert_eq!(
        out["containerCheck"]["found"]["path"], "p70/package.json",
        "{out}"
    );
}

#[tokio::test]
async fn b632_stage_refuses_a_bad_line_before_it_writes_anything() {
    let w = world();
    with_files(&w, &[&[("README.md", "x")]]);
    let r = stage(&w, payload(Some(json!(["pom.xml contains"])))).await;
    assert!(
        matches!(&r, Err(DeliveryError::Invalid(m)) if m.contains("pom.xml contains")),
        "{r:?}"
    );
    assert!(w.gw.blobs.keys().is_empty(), "nothing was written");
}
