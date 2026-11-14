//! The work in progress: the record agentd saves beside each snapshot bundle (`snapshot.json`), the header of a git
//! bundle, and the answer of the download route. The bundle header is the check that ties a downloaded file to the commit the route
//! named; `control-api` runs it on the first bytes of the stored bundle and `ham` runs it on the bytes it downloaded.
#![allow(clippy::unwrap_used)]

use ham_core::snapshot::{
    BundleError, HEADER_BYTES, MAX_RECORD_BYTES, SNAPSHOT_REF, SNAPSHOT_TRUST_NOTE,
    STALE_RECORD_AFTER_S, SnapshotDownload, SnapshotError, SnapshotRecord, bundle_head,
    verify_download,
};
use harness_protocol::goal::GoalStatus;
use serde_json::json;

const HEAD: &str = "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b";
const BASE: &str = "1111111111111111111111111111111111111111";
const PREREQ: &str = "2222222222222222222222222222222222222222";

fn record_json() -> serde_json::Value {
    json!({"schemaVersion": 1, "headSha": HEAD, "baseSha": BASE, "at": "2026-10-15T13:10:00Z", "count": 3})
}

fn bundle(refs: &[(&str, &str)]) -> Vec<u8> {
    let mut b = b"# v2 git bundle\n".to_vec();
    b.extend(format!("-{PREREQ} base commit subject\n").as_bytes());
    for (oid, name) in refs {
        b.extend(format!("{oid} {name}\n").as_bytes());
    }
    b.extend(b"\nPACK\x00\x00\x00\x02binary pack bytes follow");
    b
}

fn download(size: u64) -> SnapshotDownload {
    SnapshotDownload {
        url: "https://bucket.s3.us-east-1.amazonaws.com/runs/x/snapshot.bundle?X-Amz-Signature=deadbeef"
            .into(),
        expires_at: "2026-10-15T13:25:00Z".into(),
        head_sha: HEAD.into(),
        base_sha: Some(BASE.into()),
        taken_at: "2026-10-15T13:10:00Z".into(),
        count: Some(3),
        size_bytes: size,
        etag: "\"0cc175b9c0f1b6a831c399e269772661\"".into(),
        record_stale: false,
        run_status: GoalStatus::Active,
        final_copy: false,
        engine: None,
    }
}

#[test]
fn limits_are_the_documented_numbers() {
    assert_eq!(SNAPSHOT_REF, "refs/heads/snapshot");
    assert_eq!(HEADER_BYTES, 8192);
    assert_eq!(MAX_RECORD_BYTES, 64 * 1024);
    assert_eq!(STALE_RECORD_AFTER_S, 120);
    assert!(SNAPSHOT_TRUST_NOTE.contains("untrusted"));
}

#[test]
fn the_record_agentd_writes_parses() {
    let r = SnapshotRecord::parse(record_json().to_string().as_bytes()).unwrap();
    assert_eq!(r.head_sha, HEAD);
    assert_eq!(r.base_sha.as_deref(), Some(BASE));
    assert_eq!(r.at, "2026-10-15T13:10:00Z");
    assert_eq!(r.count, 3);
    // The base is optional: a run with no base commit has none.
    let mut j = record_json();
    j.as_object_mut().unwrap().remove("baseSha");
    assert_eq!(
        SnapshotRecord::parse(j.to_string().as_bytes())
            .unwrap()
            .base_sha,
        None
    );
}

#[test]
fn a_record_that_is_not_what_agentd_writes_is_refused() {
    let parse = |j: serde_json::Value| SnapshotRecord::parse(j.to_string().as_bytes());
    assert!(matches!(
        SnapshotRecord::parse(b"not json"),
        Err(SnapshotError::Record(_))
    ));
    let mut j = record_json();
    j["schemaVersion"] = json!(2);
    assert!(matches!(parse(j), Err(SnapshotError::Record(m)) if m.contains("schemaVersion")));
    for bad in [
        "9D2F1A7C4B8E03D56A1F7C9E2B4D6A8C0E1F3A5B",
        // An abbreviation cannot be compared with the full ids a bundle header holds.
        "9d2f1a7",
        "9d2f1a7c4b8e03d56a1f7c9e2b4d6a8c0e1f3a5b0",
        "abc",
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        "",
        "9d2f1a7 refs/heads/x",
    ] {
        let mut j = record_json();
        j["headSha"] = json!(bad);
        assert!(
            matches!(parse(j), Err(SnapshotError::Record(m)) if m.contains("headSha")),
            "{bad}"
        );
    }
    let mut j = record_json();
    j["at"] = json!("yesterday");
    assert!(matches!(parse(j), Err(SnapshotError::Record(m)) if m.contains("at")));
    let mut j = record_json();
    j["baseSha"] = json!("not-an-oid");
    assert!(matches!(parse(j), Err(SnapshotError::Record(m)) if m.contains("baseSha")));
    // A count that is missing or not a whole number is refused, not read as 0.
    for bad in [
        json!(null),
        json!(-1),
        json!(1.5),
        json!("3"),
        json!(4_294_967_296_u64),
    ] {
        let mut j = record_json();
        j["count"] = bad.clone();
        assert!(
            matches!(parse(j), Err(SnapshotError::Record(m)) if m.contains("count")),
            "{bad}"
        );
    }
    let mut j = record_json();
    j.as_object_mut().unwrap().remove("count");
    assert!(matches!(parse(j), Err(SnapshotError::Record(m)) if m.contains("count")));
    let huge = vec![b' '; MAX_RECORD_BYTES + 1];
    assert!(matches!(
        SnapshotRecord::parse(&huge),
        Err(SnapshotError::Record(m)) if m.contains("bytes")
    ));
}

#[test]
fn the_bundle_header_names_the_commit_of_the_snapshot_ref() {
    let b = bundle(&[(HEAD, SNAPSHOT_REF)]);
    assert_eq!(bundle_head(&b).unwrap(), HEAD);
    // Other refs do not matter; the snapshot ref does.
    let b = bundle(&[(BASE, "refs/heads/main"), (HEAD, SNAPSHOT_REF)]);
    assert_eq!(bundle_head(&b).unwrap(), HEAD);
    // A v3 header carries capability lines.
    let mut v3 = b"# v3 git bundle\n@object-format=sha1\n".to_vec();
    v3.extend(format!("{HEAD} {SNAPSHOT_REF}\n\nPACK").as_bytes());
    assert_eq!(bundle_head(&v3).unwrap(), HEAD);
    // A prefix cut inside the pack still has the whole header.
    let b = bundle(&[(HEAD, SNAPSHOT_REF)]);
    assert_eq!(bundle_head(&b[..b.len() - 10]).unwrap(), HEAD);
}

#[test]
fn a_bundle_header_that_cannot_be_trusted_is_refused() {
    assert!(matches!(
        bundle_head(b"PK\x03\x04 a zip"),
        Err(BundleError::NotABundle)
    ));
    assert!(matches!(bundle_head(b""), Err(BundleError::NotABundle)));
    assert!(matches!(
        bundle_head(&bundle(&[(BASE, "refs/heads/main")])),
        Err(BundleError::NoSnapshotRef)
    ));
    // The header is cut before its blank line: the prefix says nothing certain.
    let b = bundle(&[(HEAD, SNAPSHOT_REF)]);
    let cut = &b[..b.iter().position(|c| *c == b'\n').unwrap() + 20];
    assert!(matches!(bundle_head(cut), Err(BundleError::Truncated)));
    // Two entries for the ref are ambiguous.
    assert!(matches!(
        bundle_head(&bundle(&[(HEAD, SNAPSHOT_REF), (BASE, SNAPSHOT_REF)])),
        Err(BundleError::Ambiguous)
    ));
    // The object id is a hex string of 40 or 64 digits.
    assert!(matches!(
        bundle_head(&bundle(&[("nothex", SNAPSHOT_REF)])),
        Err(BundleError::BadObjectId(_))
    ));
    assert!(matches!(
        bundle_head(b"# v9 git bundle\n\n"),
        Err(BundleError::NotABundle)
    ));
    // A ref name that only ends like the snapshot ref is another ref.
    assert!(matches!(
        bundle_head(&bundle(&[(HEAD, "refs/remotes/x/refs/heads/snapshot")])),
        Err(BundleError::NoSnapshotRef)
    ));
}

#[test]
fn what_a_worker_wrote_in_a_bad_header_line_is_not_echoed_in_the_error() {
    // The error is logged; the worker chose these bytes.
    let e = bundle_head(&bundle(&[("SECRET-TOKEN-TEXT-0123456789", SNAPSHOT_REF)])).unwrap_err();
    assert!(matches!(e, BundleError::BadObjectId(_)));
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains("SECRET"), "{shown}");
    let e = bundle_head(b"# v2 git bundle\nPASSWORD=hunter2\n\n").unwrap_err();
    let shown = format!("{e} {e:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
}

#[test]
fn a_download_is_checked_against_what_the_route_said() {
    let bytes = bundle(&[(HEAD, SNAPSHOT_REF)]);
    let size = bytes.len() as u64;
    verify_download(&bytes, &download(size)).unwrap();
    // The size is the stored object's.
    assert!(matches!(
        verify_download(&bytes, &download(size + 1)),
        Err(SnapshotError::Size { expected, actual }) if expected == size + 1 && actual == size
    ));
    // The commit is the route's.
    let other = bundle(&[(BASE, SNAPSHOT_REF)]);
    let osize = other.len() as u64;
    assert!(matches!(
        verify_download(&other, &download(osize)),
        Err(SnapshotError::Head { expected, actual }) if expected == HEAD && actual == BASE
    ));
    assert!(matches!(
        verify_download(b"garbage garbage garbage", &download(23)),
        Err(SnapshotError::Bundle(BundleError::NotABundle))
    ));
}

#[test]
fn the_download_answer_hides_its_url_from_debug_and_keeps_it_on_the_wire() {
    let d = download(1234);
    let shown = format!("{d:?}");
    assert!(!shown.contains("X-Amz-Signature"), "{shown}");
    assert!(!shown.contains("deadbeef"), "{shown}");
    assert!(!shown.contains("amazonaws.com/runs"), "{shown}");
    assert!(shown.contains(HEAD));
    let j = serde_json::to_value(&d).unwrap();
    assert_eq!(j["url"], d.url);
    assert_eq!(j["expiresAt"], "2026-10-15T13:25:00Z");
    assert_eq!(j["headSha"], HEAD);
    assert_eq!(j["baseSha"], BASE);
    assert_eq!(j["takenAt"], "2026-10-15T13:10:00Z");
    assert_eq!(j["count"], 3);
    assert_eq!(j["sizeBytes"], 1234);
    assert_eq!(j["etag"], d.etag);
    assert!(j.get("recordStale").is_none());
    assert_eq!(j["runStatus"], "active");
    let back: SnapshotDownload = serde_json::from_value(j).unwrap();
    assert_eq!(back, d);
    // A run without a base leaves the member out.
    let mut d2 = download(1);
    d2.base_sha = None;
    assert!(serde_json::to_value(&d2).unwrap().get("baseSha").is_none());
    // A record that lags its bundle: the commit comes from the bundle, the count is unknown, and the answer says so.
    d2.count = None;
    d2.record_stale = true;
    let j2 = serde_json::to_value(&d2).unwrap();
    assert!(j2.get("count").is_none());
    assert_eq!(j2["recordStale"], true);
    let back2: SnapshotDownload = serde_json::from_value(j2).unwrap();
    assert_eq!(back2, d2);
}

#[test]
fn a_repeated_ref_that_was_asked_for_is_named_by_its_own_name() {
    use ham_core::snapshot::bundle_head_of;
    let wanted = "refs/heads/harness/01K5ZQ4T8M3N6P7R9S0V1W2X3Y";
    let id1 = "1".repeat(40);
    let id2 = "2".repeat(40);
    let bundle = format!("# v2 git bundle\n{id1} {wanted}\n{id2} {wanted}\n\nPACK");
    let e = bundle_head_of(bundle.as_bytes(), wanted).unwrap_err();
    let text = e.to_string();
    assert!(
        text.contains(wanted) && text.contains("more than once"),
        "{text}"
    );
    assert!(!text.contains("refs/heads/snapshot"), "{text}");
    // The snapshot's own function keeps its word for its own ref.
    let snap = format!("# v2 git bundle\n{id1} {SNAPSHOT_REF}\n{id2} {SNAPSHOT_REF}\n\nPACK");
    assert_eq!(bundle_head(snap.as_bytes()), Err(BundleError::Ambiguous));
    assert!(BundleError::Ambiguous.to_string().contains(SNAPSHOT_REF));
}
