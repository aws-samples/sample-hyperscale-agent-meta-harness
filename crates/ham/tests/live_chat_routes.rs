//! Live proof of the chat-client server routes against a
//! deployed stage. It does nothing in a normal test run (`#[ignore]`). Run it after the Lambda stack that carries the routes is deployed:
//!
//! ```bash
//! HAMH_LIVE_API=<apiEndpoint of the stage file> HAMH_LIVE_PROFILE=<profile of the caller> \
//!   [HAMH_LIVE_OTHER_PROFILE=<profile of a second principal; proves the 404 on every route>] \
//!   scripts/cargo-slot.sh cargo nextest run -p ham --test live_chat_routes --run-ignored only --no-capture
//! ```
//!
//! Every check prints one `LIVE PASS <id>: <measured value>` line and fails the test when it does not hold. It writes the backfill
//! (idempotent, the caller's own runs) and downloads one snapshot into a scratch directory; it starts no run and spends no tokens.
#![allow(clippy::unwrap_used)]

use ham::api::{Api, ApiError};
use ham::chat_routes::{SnapshotFetchError, fetch_snapshot};
use ham::https::HttpsPlane;
use ham::sigv4::Signer;
use ham_core::events_view::EventsQuery;
use ham_core::run_index::RunFilter;
use harness_protocol::envelope::Authority;
use harness_protocol::store::event_shard;
use std::sync::Arc;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

async fn api(endpoint: &str, profile: &str) -> Api<HttpsPlane> {
    let signer = Signer::from_profile(Some(profile), None).await.unwrap();
    Api::new(Arc::new(HttpsPlane::new(endpoint, signer).unwrap()))
}

fn pass(id: &str, value: impl std::fmt::Display) {
    println!("LIVE PASS {id}: {value}");
}

#[tokio::test]
#[ignore = "live: needs a deployed stage (HAMH_LIVE_API, HAMH_LIVE_PROFILE)"]
async fn the_chat_client_routes_work_on_the_stage() {
    let endpoint = env("HAMH_LIVE_API").expect("HAMH_LIVE_API: the apiEndpoint of the stage");
    let profile = env("HAMH_LIVE_PROFILE").expect("HAMH_LIVE_PROFILE: the caller's AWS profile");
    let me = api(&endpoint, &profile).await;
    let who = me.whoami().await.unwrap();
    pass("whoami", &who.principal_arn);

    // The list parses as rows; the backfill is idempotent; after it every run has a title (a run with no goal item is skipped).
    let report = me.backfill_run_index().await.unwrap();
    pass(
        "N45-backfill",
        format!(
            "indexed={} alreadyIndexed={} skipped={} scanned={}",
            report.indexed, report.already_indexed, report.skipped, report.scanned
        ),
    );
    let rows = me.run_rows_all(&RunFilter::default()).await.unwrap();
    let titled = rows.iter().filter(|r| r.title.is_some()).count();
    assert!(
        titled + report.skipped as usize >= rows.len(),
        "{titled} titled of {}",
        rows.len()
    );
    pass(
        "N45-rows",
        format!("{} rows, {titled} with a title", rows.len()),
    );
    if let Some(word) = rows
        .iter()
        .filter_map(|r| r.title.as_deref())
        .flat_map(str::split_whitespace)
        .find(|w| w.chars().count() >= 5)
    {
        let hits = me
            .run_rows_all(&RunFilter {
                q: Some(word.to_string()),
                ..RunFilter::default()
            })
            .await
            .unwrap();
        assert!(!hits.is_empty(), "q={word:?} found nothing");
        pass("N45-q", format!("q={word:?} found {} rows", hits.len()));
    }
    let again = me.backfill_run_index().await.unwrap();
    assert_eq!(
        again.indexed, 0,
        "a second backfill writes nothing: {again:?}"
    );
    pass("N45-backfill-idempotent", "second call indexed 0");

    let run = env("HAMH_LIVE_RUN")
        .or_else(|| rows.first().map(|r| r.summary.run_id.clone()))
        .expect("the caller has no run; set HAMH_LIVE_RUN");

    // The end of the direct agent's shard in one call, and the page before it.
    let shard = event_shard(Authority::Agent, Some("direct"));
    let tail = me
        .events_view(&run, &EventsQuery::tail(shard, 50))
        .await
        .unwrap();
    assert!(tail.events.len() <= 50);
    pass(
        "N44-tail",
        format!(
            "{} events, hasMore={}, answers={}",
            tail.events.len(),
            tail.has_more,
            tail.answers.len()
        ),
    );
    if let (true, Some(before)) = (tail.has_more, tail.next_before.as_deref()) {
        let older = me
            .events_view(&run, &EventsQuery::older(shard, before, 50))
            .await
            .unwrap();
        assert!(older.events.last().unwrap().event_id.as_str() < before);
        pass("N44-before", format!("{} older events", older.events.len()));
    }

    // The latest snapshot downloads and verifies, or the server says there is none yet.
    let dir = std::env::temp_dir().join(format!("hamh-live-chat-routes-{}", std::process::id()));
    match fetch_snapshot(&me, &run, &dir).await {
        Ok(got) => pass(
            "N47-snapshot",
            format!(
                "{} bytes, head {}, taken {}",
                got.size_bytes, got.head_sha, got.taken_at
            ),
        ),
        Err(SnapshotFetchError::Api(ApiError::Http { status: 404, error }))
            if error.code == "no-snapshot" =>
        {
            pass("N47-snapshot", "no snapshot saved yet (404 no-snapshot)");
        }
        Err(e) => panic!("snapshot: {e}"),
    }

    // A second principal gets a 404 on every route of this run.
    if let Some(other_profile) = env("HAMH_LIVE_OTHER_PROFILE") {
        let other = api(&endpoint, &other_profile).await;
        let denied = |r: Result<(), ApiError>| matches!(r, Err(ApiError::Http { status: 404, .. }));
        assert!(denied(
            other
                .events_view(&run, &EventsQuery::tail(shard, 5))
                .await
                .map(|_| ())
        ));
        assert!(denied(other.snapshot_download(&run).await.map(|_| ())));
        let theirs = other.run_rows_all(&RunFilter::default()).await.unwrap();
        assert!(theirs.iter().all(|r| r.summary.run_id != run));
        pass(
            "owner-only",
            "a second principal gets 404 on events and snapshot and never lists the run",
        );
    }
}
