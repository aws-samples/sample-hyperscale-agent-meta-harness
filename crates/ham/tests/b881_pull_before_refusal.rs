//! `ham pull <runId> --before-refusal <notice id>` downloads the copy a notice of a storage refusal names.
//!
//! The route answers `?beforeRefusal=<id>` with that one copy, at the version the notice names, and says in its answer which notice
//! it is about (`beforeRefusal`). A copy that holds what the run changed comes with the repository the run started from, as the
//! newest copy does: the request always says `parts=1`, and `ham` keeps both files. What this file pins:
//!
//! * the request, the checks of a snapshot download, the file names and the text of the command, word for word;
//! * an answer that is not about that notice (a stage that predates the selector answers with the run's newest copy) is refused
//!   with one sentence, and nothing is written: no file, and no download directory.
#![allow(clippy::unwrap_used)]

mod support;

use ham::api::{Api, Retry};
use ham::chat_routes::{SnapshotFetchError, before_refusal_not_known, fetch_before_refusal};
use ham::http::{ControlPlane, Response, TransportError};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const RUN: &str = "01M4ZHFBV5SDX7J92E6YRH4QTW";
/// The id of the notice: an event id.
const NOTICE: &str = "01M5A2R7K30000000000000007";
const OTHER_NOTICE: &str = "01M5A2R7K30000000000000008";
const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";

/// Answers the download route with one JSON object and serves the copy and the base by the key in the URL, with S3's range rules.
struct Plane {
    answer: Value,
    copy: Vec<u8>,
    base: Vec<u8>,
    calls: Mutex<Vec<String>>,
    fetches: Mutex<Vec<String>>,
}

impl ControlPlane for Plane {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.calls.lock().unwrap().push(format!("{method} {path}"));
        Ok(Response {
            status: 200,
            headers: BTreeMap::new(),
            body: serde_json::to_vec(&self.answer).unwrap(),
        })
    }
    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        let is_base = url.contains("input/repo.bundle");
        self.fetches
            .lock()
            .unwrap()
            .push(if is_base { "base" } else { "copy" }.to_string());
        let object = if is_base { &self.base } else { &self.copy };
        let size = object.len() as u64;
        let (s, e) = range.unwrap_or((0, size - 1));
        let e = e.min(size - 1);
        Ok(Response {
            status: 206,
            headers: BTreeMap::from([(
                "content-range".to_string(),
                format!("bytes {s}-{e}/{size}"),
            )]),
            body: object[usize::try_from(s).unwrap()..=usize::try_from(e).unwrap()].to_vec(),
        })
    }
}

/// A copy that holds what the run changed: its header names the commit of the base it needs.
fn thin_copy() -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n-{BASE} base\n{HEAD} refs/heads/snapshot\n\nPACK").into_bytes();
    b.extend((0..3_000).map(|i| (i % 251) as u8));
    b
}

/// A copy that holds the whole history.
fn whole_copy() -> Vec<u8> {
    let mut b = format!("# v2 git bundle\n{HEAD} refs/heads/snapshot\n\nPACK").into_bytes();
    b.extend((0..2_000).map(|i| (i % 239) as u8));
    b
}

fn whole_base() -> Vec<u8> {
    let mut b =
        format!("# v2 git bundle\n{BASE} HEAD\n{BASE} refs/heads/main\n\nPACK").into_bytes();
    b.extend((0..9_000).map(|i| (i % 241) as u8));
    b
}

/// The route's answer for the copy the notice names: copy 2 of the run, saved at 00:37:12 UTC, of a run that has ended blocked.
fn answer(copy: &[u8], base: Option<&[u8]>, before_refusal: Option<&str>) -> Value {
    let mut v = json!({
        "url": "https://bucket.s3.us-east-1.amazonaws.com/runs/x/agents/direct/turns/1/snapshot.bundle?versionId=version-of-copy-2&X-Amz-Signature=deadbeef&X-Amz-Security-Token=SECRETTOKEN",
        "expiresAt": "2099-10-15T13:25:00Z", "headSha": HEAD, "takenAt": "2026-11-21T00:37:12Z",
        "count": 2, "sizeBytes": copy.len(), "etag": "\"abc\"", "runStatus": "blocked", "engine": "cline",
    });
    if let Some(base) = base {
        v["base"] = json!({
            "url": "https://bucket.s3.us-east-1.amazonaws.com/runs/x/input/repo.bundle?X-Amz-Signature=cafecafe&X-Amz-Security-Token=SECRETTOKEN",
            "sizeBytes": base.len(), "etag": "\"def\""
        });
    }
    if let Some(id) = before_refusal {
        v["beforeRefusal"] = json!(id);
    }
    v
}

fn scripted(answer: Value, copy: Vec<u8>) -> Api<Plane> {
    Api::new(Arc::new(Plane {
        answer,
        copy,
        base: whole_base(),
        calls: Mutex::new(Vec::new()),
        fetches: Mutex::new(Vec::new()),
    }))
    .with_retry(Retry {
        attempts: 3,
        base: Duration::from_millis(5),
    })
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn b881_pull_before_refusal_downloads_checks_and_names_the_file() {
    // The download directory does not exist yet: the command makes it once the stage has answered about the notice.
    let dir = support::scratch("b881-before-refusal").join("downloads");
    let api = scripted(
        answer(&thin_copy(), Some(&whole_base()), Some(NOTICE)),
        thin_copy(),
    );
    let f = fetch_before_refusal(&api, RUN, NOTICE, &dir).await.unwrap();
    println!(
        "B881 TRIGGER PRESENT: the stage answered about the notice {NOTICE} with a copy that needs the run's base"
    );
    // The request names the notice and says that this client takes a copy in two parts.
    assert_eq!(
        api.plane().calls.lock().unwrap().clone(),
        vec![format!(
            "POST /v1/runs/{RUN}/snapshot/download-url?parts=1&beforeRefusal={NOTICE}"
        )]
    );
    // Both files are on disk under their names, each the bytes the store holds, and no part file is left.
    let copy_path = dir.join(format!("{RUN}-before-refusal-{NOTICE}.bundle"));
    let base_path = dir.join(format!("{RUN}-base.bundle"));
    assert_eq!(
        files_in(&dir),
        vec![
            format!("{RUN}-base.bundle"),
            format!("{RUN}-before-refusal-{NOTICE}.bundle")
        ]
    );
    assert_eq!(std::fs::read(&copy_path).unwrap(), thin_copy());
    assert_eq!(std::fs::read(&base_path).unwrap(), whole_base());
    assert_eq!(f.path, copy_path);
    assert_eq!(f.before_refusal.as_deref(), Some(NOTICE));
    assert_eq!(f.head_sha, HEAD);
    assert!(!f.asked_read_back && f.read_back_from.is_none() && !f.needs_base);

    // The text, word for word: what the copy is, what it cannot promise, the two files and the line that puts them together.
    let text = ham::snapshot_cmd::text(RUN, &f);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        format!("pulled the copy from before a storage refusal of run {RUN}")
    );
    assert_eq!(lines[1], format!("File      {}", copy_path.display()));
    assert_eq!(
        lines[2],
        format!(
            "Needs     {} (the repository the run started from)",
            base_path.display()
        )
    );
    assert!(
        lines.contains(
            &"Saved     copy 2, 2026-11-21 00:37 UTC: the last copy saved before ham first saw the session's storage refuse a write"
        ),
        "{text}"
    );
    assert!(
        lines.contains(
            &"Note      Changes after that time are not in it. A refusal that ham did not see can have emptied files in it too."
        ),
        "{text}"
    );
    let look = format!(
        "Look      git clone {base} <dir> && git -C <dir> fetch {copy} snapshot && git -C <dir> checkout -B ham-snapshot FETCH_HEAD",
        base = base_path.display(),
        copy = copy_path.display()
    );
    assert!(lines.contains(&look.as_str()), "{text}");
    assert!(
        text.contains(ham_core::snapshot::SNAPSHOT_TRUST_NOTE),
        "{text}"
    );
    // It is an earlier copy: nothing calls it the run's work, the work as the engine left it or the newest copy, and no link is
    // printed.
    assert!(!text.contains("this copy is its work"), "{text}");
    assert!(!text.contains("as Cline left it"), "{text}");
    assert!(!text.contains("pulled the snapshot"), "{text}");
    assert!(
        !text.contains("X-Amz-") && !text.contains("SECRETTOKEN"),
        "{text}"
    );
    // `--json`: the copy as a snapshot, the base, and the notice it was asked for by.
    let v = ham::snapshot_cmd::json_of(RUN, &f);
    assert_eq!(v["snapshot"]["beforeRefusal"], NOTICE);
    assert_eq!(v["snapshot"]["headSha"], HEAD);
    assert_eq!(v["snapshot"]["count"], 2);
    assert_eq!(
        v["snapshot"]["base"]["path"],
        base_path.display().to_string()
    );

    // A copy that clones alone: one file and the one clone line.
    let dir = support::scratch("b881-before-refusal-whole").join("downloads");
    let api = scripted(answer(&whole_copy(), None, Some(NOTICE)), whole_copy());
    let f = fetch_before_refusal(&api, RUN, NOTICE, &dir).await.unwrap();
    assert_eq!(
        files_in(&dir),
        vec![format!("{RUN}-before-refusal-{NOTICE}.bundle")]
    );
    let text = ham::snapshot_cmd::text(RUN, &f);
    assert!(
        text.contains(&format!(
            "Look      git clone -b snapshot {} <dir>",
            f.path.display()
        )),
        "{text}"
    );
    assert!(!text.contains("Needs     "), "{text}");
    // A second pull of the same notice gives the same file under the same name.
    let again = fetch_before_refusal(&api, RUN, NOTICE, &dir).await.unwrap();
    assert_eq!(again.path, f.path);
    assert_eq!(
        files_in(&dir),
        vec![format!("{RUN}-before-refusal-{NOTICE}.bundle")]
    );
}

#[tokio::test]
async fn b881_an_answer_that_is_not_about_that_notice_is_refused_and_nothing_is_written() {
    let sentence = format!(
        "this stage predates copies from before a storage refusal; ham pull {RUN} --snapshot downloads the newest copy"
    );
    assert_eq!(before_refusal_not_known(RUN), sentence);
    // A stage that predates the selector ignores it and answers with the run's newest copy; an answer about another notice; and an
    // answer that is files read back from a session.
    let mut read_back = answer(&whole_copy(), None, Some(NOTICE));
    read_back["recovered"] = json!(true);
    read_back["session"] = json!(1);
    for (what, reply) in [
        ("an older stage", answer(&whole_copy(), None, None)),
        (
            "another notice",
            answer(&whole_copy(), None, Some(OTHER_NOTICE)),
        ),
        ("files read back", read_back),
    ] {
        let root = support::scratch("b881-refused");
        let dir = root.join("downloads");
        let api = scripted(reply, whole_copy());
        let e = fetch_before_refusal(&api, RUN, NOTICE, &dir)
            .await
            .unwrap_err();
        println!(
            "B881 TRIGGER PRESENT: the stage answered 200 with a copy that is not about the notice ({what})"
        );
        assert!(matches!(e, SnapshotFetchError::Fetch(_)), "{what}: {e:?}");
        assert_eq!(e.to_string(), format!("download: {sentence}"), "{what}");
        // Nothing is written: no file, no part file, and no download directory; and no byte of the copy was fetched.
        assert!(!dir.exists(), "{what}: the download directory was made");
        assert!(files_in(&root).is_empty(), "{what}: {:?}", files_in(&root));
        assert!(
            api.plane().fetches.lock().unwrap().is_empty(),
            "{what}: the copy was fetched"
        );
    }
    // A value that is no id of a notice is refused before the stage is asked.
    let root = support::scratch("b881-bad-id");
    let api = scripted(answer(&whole_copy(), None, Some(NOTICE)), whole_copy());
    for bad in ["", "7", "../x", "01M5A2R7K3000000000000000U"] {
        let e = fetch_before_refusal(&api, RUN, bad, &root.join("downloads"))
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("is not the id of a notice"),
            "{bad:?}: {e}"
        );
    }
    assert!(api.plane().calls.lock().unwrap().is_empty());
    assert!(!root.join("downloads").exists());
}
