//! The refresh of the session's credentials at the end of a run, in a whole run: the real driver in process, the fake
//! engine, a fake AWS. No AWS call is made.
//!
//! The end sequence takes the session's credentials once, before it commits, for the upload of the bundle and for the upload of
//! `summary.json`. Every other step that needs credentials asks the store when it needs them (the copy of the end, the final
//! flush, turn-complete). Before a refresh that failed at that one take left both uploads without credentials, although the
//! store answered the step after it: the run had no delivery, and the saved copy held the work. Now an upload whose last answer
//! was a failure that may be another answer a moment later asks the store once more.
//!
//! How the tests put the failure there. The credentials of the invocation end inside the refresh window, and so do the
//! credentials every refresh of the stand-in answers, unless a test says otherwise: each step that needs credentials then asks
//! the service, and one ask of the store is one post to the stand-in. Nothing but the steps of the run asks: the poll for events
//! and the heartbeat are an hour apart, and the copies while the engine works are off. The end sequence puts the git settings back
//! right before it takes the credentials; the file calls of that step are the test's own (`App::settings_write`), which make the
//! host's calls and tell the stand-in service at the first one what to answer from then on. So the first refresh the stand-in
//! answers after that is the end sequence's take, the next one is the copy of the end's, and the ones after it are the uploads'.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::checked_write::{FileCalls, Host};
use agentd::direct::git_settings::Repeat;
use agentd::time::{format_rfc3339, now_unix};
use common::{creds, git};
use direct_support::{AwsState, BUCKET, Direct, Opts, RUN, checkout_of_copy_bytes};
use harness_protocol::direct::s3 as keys;
use harness_protocol::envelope::EventType;
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

/// "No refresh was answered before the end sequence": the value of [`AtTheEnd::asked_before`] until the end sequence writes.
const NOT_YET: u32 = u32::MAX;

/// How the worker's log line of a further ask of the end sequence starts.
const ASKED_AGAIN: &str = "direct turn end credentials: asked again for the upload of ";

/// What the stand-in is told at the first file call of the end sequence.
type Told = Box<dyn Fn(&AwsState) + Send + Sync>;

/// The file calls of the end sequence's write of the git settings, made as the host makes them. At the first one the stand-in
/// service is told what to answer to the refreshes from then on.
struct AtTheEnd {
    aws: Arc<AwsState>,
    tell: Told,
    /// How many refreshes the stand-in had answered when the end sequence made its first file call.
    asked_before: AtomicU32,
}

impl std::fmt::Debug for AtTheEnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AtTheEnd").finish_non_exhaustive()
    }
}

impl FileCalls for AtTheEnd {
    fn create(&self, path: &Path, mode: u32) -> std::io::Result<std::fs::File> {
        let asked = u32::try_from(self.aws.refresh_statuses().len()).unwrap();
        if self
            .asked_before
            .compare_exchange(NOT_YET, asked, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            (self.tell)(&self.aws);
        }
        Host.create(path, mode)
    }
}

/// A whole run of `writes_files` in which the stand-in service is told `tell` when the end sequence begins to write. Returns the
/// run, its result, and the statuses the stand-in answered to the refreshes of the end sequence, in order.
async fn run_told_at_the_end(tell: Told) -> (Direct, InvocationResponse, Vec<u16>) {
    let mut d = Direct::start(Opts {
        scenario: "writes_files",
        // The copies while the engine works are off: the one copy of this run is the copy of the end.
        snapshot_every_s: Some(0),
        edit_cfg: Some(Box::new(|c| {
            // No poll for events and no heartbeat asks the store while the run works: the events go out in the final flush.
            c.direct.events_every = Duration::from_secs(3600);
            c.direct.heartbeat_every = Duration::from_secs(3600);
        })),
        ..Opts::default()
    })
    .await;
    // Inside the refresh window from the first use on, and each refresh answers credentials that are inside it again.
    let soon = format_rfc3339(now_unix() + 300);
    d.req.credentials = creds(&soon);
    d.aws.refresh_answers(creds(&soon), "next");
    let at_the_end = Arc::new(AtTheEnd {
        aws: d.aws.state.clone(),
        tell,
        asked_before: AtomicU32::new(NOT_YET),
    });
    assert!(
        d.app
            .settings_write
            .set((at_the_end.clone(), Repeat::default()))
            .is_ok()
    );
    let (_, resp) = d.invoke().await;
    let before = at_the_end.asked_before.load(Ordering::SeqCst);
    assert_ne!(
        before,
        NOT_YET,
        "TRIGGER PRESENT is not shown: the end sequence made no file call for the git settings; log: {:?}",
        d.log.lines()
    );
    let statuses = d.aws.state.refresh_statuses();
    let at_the_end: Vec<u16> = statuses[before as usize..].to_vec();
    assert!(
        before >= 1 && statuses[..before as usize].iter().all(|s| *s == 200),
        "TRIGGER PRESENT is not shown: the steps before the end sequence were to ask the service and to be answered: {statuses:?}"
    );
    (d, resp, at_the_end)
}

/// A whole run in which the refreshes of the end sequence are answered with the statuses of `next`, in order, and every one after
/// them with credentials.
async fn run_with_refreshes_at_the_end(
    next: &'static [u16],
) -> (Direct, InvocationResponse, Vec<u16>) {
    let (d, resp, at_the_end) =
        run_told_at_the_end(Box::new(move |aws| aws.script_refreshes(next, None))).await;
    assert!(
        at_the_end.len() >= next.len() && at_the_end[..next.len()] == *next,
        "TRIGGER PRESENT is not shown: the refreshes of the end sequence were to be answered with {next:?} first: {at_the_end:?}"
    );
    assert!(
        at_the_end[next.len()..].iter().all(|s| *s == 200),
        "TRIGGER PRESENT is not shown: every later refresh was to be answered with credentials: {at_the_end:?}"
    );
    println!(
        "B989 TRIGGER PRESENT: the stand-in answered the refreshes of the end sequence with {at_the_end:?}"
    );
    (d, resp, at_the_end)
}

/// The label and the outcome word of the run's `turn.ended` event: what the run's row shows.
fn ended(d: &Direct) -> (String, String) {
    let events = d.aws.events();
    let (_, payload) = events
        .iter()
        .rev()
        .find(|(t, _)| *t == EventType::TurnEnded)
        .expect("a turn.ended event");
    (
        payload["label"].as_str().unwrap().to_string(),
        payload["directOutcome"].as_str().unwrap().to_string(),
    )
}

/// The worker's log lines of the further asks of the end sequence, without their start.
fn asked_again(d: &Direct) -> Vec<String> {
    d.log
        .lines()
        .into_iter()
        .filter_map(|l| l.strip_prefix(ASKED_AGAIN).map(str::to_string))
        .collect()
}

/// Root invariant 9: the copy of the end is in the bucket and holds the work of the engine. Returns the head the copy names.
fn the_saved_copy_holds_the_work(d: &Direct) -> String {
    let record: serde_json::Value = serde_json::from_slice(
        &d.aws
            .get(&keys::snapshot_json_key(RUN))
            .expect("the record of the saved copy"),
    )
    .unwrap();
    assert_eq!(record["final"], true, "the copy of the end: {record}");
    let copy = d
        .aws
        .get(&keys::snapshot_bundle_key(RUN))
        .expect("the bundle of the saved copy");
    let clone = checkout_of_copy_bytes(&d.aws, &d.dir, &copy, "b989-copy");
    assert!(
        git(&clone, &["show", "snapshot:src/calc.py"]).contains("def percent_change"),
        "the saved copy holds the engine's work"
    );
    assert!(
        git(&clone, &["show", "snapshot:tests/test_calc.py"]).contains("percent_change"),
        "the saved copy holds the engine's test"
    );
    record["headSha"].as_str().unwrap().to_string()
}

/// The run delivered: the bundle and `summary.json` are in the bucket, the result and the one turn-complete name the bundle and
/// carry the records of both objects, nothing is said of an upload, and the run's row keeps the engine's label.
fn delivered(d: &Direct, resp: &InvocationResponse) {
    let bundle_uri = keys::uri(BUCKET, &keys::bundle_key(RUN));
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_some(),
        "the bundle of the run is in the bucket; the turn's error: {:?}; log: {:?}",
        resp.error,
        d.log.lines()
    );
    assert_eq!(
        resp.bundle_uri.as_deref(),
        Some(bundle_uri.as_str()),
        "{resp:?}"
    );
    assert!(resp.last_commit_sha.is_some(), "{resp:?}");
    assert_eq!(resp.error, None, "{resp:?}");
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let s = d.summary();
    assert_eq!(s.git.bundle_uri.as_deref(), Some(bundle_uri.as_str()));
    assert_eq!(s.git.head_sha, resp.last_commit_sha);
    assert_eq!(s.error, None);
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1, "one turn-complete");
    assert_eq!(complete[0].bundle_uri.as_deref(), Some(bundle_uri.as_str()));
    assert_eq!(complete[0].error, None);
    let pins = complete[0]
        .output_pins
        .clone()
        .expect("the records of the stored objects");
    assert!(pins.bundle.is_some() && pins.summary.is_some(), "{pins:?}");
    assert_eq!(
        ended(d),
        (
            "Cline reported done".to_string(),
            "reported_done".to_string()
        )
    );
}

/// The run ended as it did when the take of the end sequence failed: no bundle and no `summary.json` in the bucket,
/// a turn-complete that names no bundle, the turn's error with the sentence of the refresh, and the engine's label on the run's row.
fn no_delivery_as_before(d: &Direct, resp: &InvocationResponse, refresh_said: &str) {
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_none(),
        "the bundle of the run"
    );
    assert!(
        d.aws.get(&keys::summary_key(RUN)).is_none(),
        "summary.json of the run"
    );
    assert_eq!(resp.bundle_uri, None, "{resp:?}");
    assert_eq!(resp.last_commit_sha, None, "{resp:?}");
    let error = format!("upload: credential refresh failed: {refresh_said}");
    assert_eq!(resp.error.as_deref(), Some(error.as_str()), "{resp:?}");
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1, "one turn-complete");
    assert_eq!(complete[0].bundle_uri, None);
    assert_eq!(complete[0].output_pins, None);
    assert_eq!(complete[0].error.as_deref(), Some(error.as_str()));
    assert_eq!(
        ended(d),
        (
            "Cline reported done".to_string(),
            "reported_done".to_string()
        )
    );
    let not_uploaded =
        format!("direct turn summary not uploaded: credential refresh failed: {refresh_said}");
    assert!(d.log.lines().contains(&not_uploaded), "{:?}", d.log.lines());
}

/// Done-when 2: the service answers one refresh at the end of a run with a temporary error and the next ones with credentials.
/// Before the run had no delivery (the record of that is the first commit of this file). Now the upload of the bundle asks
/// the store once more, the run delivers, and the saved copy holds the work as before.
#[tokio::test(flavor = "multi_thread")]
async fn b989_a_refresh_that_fails_once_at_the_end_of_a_run_is_asked_again_and_the_run_delivers() {
    let (d, resp, _) = run_with_refreshes_at_the_end(&[503]).await;
    let copied_head = the_saved_copy_holds_the_work(&d);
    delivered(&d, &resp);
    assert_eq!(
        resp.last_commit_sha.as_deref(),
        Some(copied_head.as_str()),
        "the copy of the end and the delivery are of one head"
    );
    // One further ask, by the first upload; the upload of the summary used its answer.
    assert_eq!(asked_again(&d), ["the bundle: answered"]);
}

/// The way of a real session: the copy of the end asks the store itself, between the take and the uploads, and the credentials
/// its refresh brings are good for an hour. The upload of the bundle then asks the store, which answers from what it holds: the
/// run delivers and the uploads post nothing to the service.
#[tokio::test(flavor = "multi_thread")]
async fn b989_the_uploads_take_what_the_copy_of_the_end_brought_and_post_no_refresh_of_their_own() {
    let (d, resp, at_the_end) = run_told_at_the_end(Box::new(|aws| {
        aws.script_refreshes(&[503], None);
        aws.refresh_answers(creds(&format_rfc3339(now_unix() + 3600)), "next");
    }))
    .await;
    assert_eq!(
        at_the_end,
        [503, 200],
        "the take of the end sequence, then the copy of the end's own ask, and no post after it"
    );
    println!(
        "B989 TRIGGER PRESENT: the stand-in answered the take with 503 and the copy of the end's ask with credentials for an hour"
    );
    the_saved_copy_holds_the_work(&d);
    delivered(&d, &resp);
    assert_eq!(asked_again(&d), ["the bundle: answered"]);
}

/// A refresh that is refused stays refused however often it is asked: it keeps its one ask, and the run ends as before, although
/// the stand-in would have answered a second ask.
#[tokio::test(flavor = "multi_thread")]
async fn b989_a_refresh_that_is_refused_at_the_end_of_a_run_keeps_its_one_ask() {
    let (d, resp, _) = run_with_refreshes_at_the_end(&[403]).await;
    the_saved_copy_holds_the_work(&d);
    no_delivery_as_before(&d, &resp, "creds refresh returned 403");
    assert_eq!(asked_again(&d), Vec::<String>::new());
}

/// A temporary answer that the service gave after it had replaced the nonce is followed by a refusal (the nonce the worker holds
/// is not current any more). The asks of the uploads end at the refusal: the upload of the summary asks nothing.
#[tokio::test(flavor = "multi_thread")]
async fn b989_a_refusal_after_a_temporary_answer_ends_the_asks_of_the_uploads() {
    // The take, the copy of the end's ask, the further ask of the bundle's upload.
    let (d, resp, _) = run_with_refreshes_at_the_end(&[502, 200, 403]).await;
    the_saved_copy_holds_the_work(&d);
    no_delivery_as_before(&d, &resp, "creds refresh returned 403");
    assert_eq!(
        asked_again(&d),
        ["the bundle: credential refresh failed: creds refresh returned 403"]
    );
}

/// Every ask of the uploads fails: the run ends as, with the sentence of the last ask, and the saved copy is there.
/// Each of the two uploads asked once more and no further.
#[tokio::test(flavor = "multi_thread")]
async fn b989_a_refresh_that_fails_at_every_ask_of_the_uploads_ends_the_run_as_before() {
    // The take, the copy of the end's ask, the further ask of the bundle's upload, the further ask of the summary's upload.
    let (d, resp, _) = run_with_refreshes_at_the_end(&[503, 200, 503, 503]).await;
    the_saved_copy_holds_the_work(&d);
    no_delivery_as_before(&d, &resp, "creds refresh returned 503");
    assert_eq!(
        asked_again(&d),
        [
            "the bundle: credential refresh failed: creds refresh returned 503",
            "summary.json: credential refresh failed: creds refresh returned 503"
        ]
    );
}

/// The ask of the bundle's upload fails and the ask of the summary's upload is answered: `summary.json` is stored and names the
/// upload that was not made; the run has no bundle, and the saved copy holds the work.
#[tokio::test(flavor = "multi_thread")]
async fn b989_a_refresh_that_comes_back_for_the_summary_stores_it_with_the_bundles_error() {
    // The take, the copy of the end's ask, the further ask of the bundle's upload.
    let (d, resp, _) = run_with_refreshes_at_the_end(&[503, 200, 503]).await;
    the_saved_copy_holds_the_work(&d);
    assert_eq!(
        asked_again(&d),
        [
            "the bundle: credential refresh failed: creds refresh returned 503",
            "summary.json: answered"
        ]
    );
    let error = "upload: credential refresh failed: creds refresh returned 503";
    assert!(
        d.aws.get(&keys::bundle_key(RUN)).is_none(),
        "the bundle of the run"
    );
    assert_eq!(resp.bundle_uri, None, "{resp:?}");
    assert_eq!(resp.error.as_deref(), Some(error), "{resp:?}");
    let s = d.summary();
    assert_eq!(s.git.bundle_uri, None);
    assert_eq!(s.error.as_deref(), Some(error));
    let complete = d.aws.complete();
    assert_eq!(complete.len(), 1, "one turn-complete");
    assert_eq!(complete[0].bundle_uri, None);
    let pins = complete[0]
        .output_pins
        .clone()
        .expect("the record of the stored summary");
    assert!(pins.bundle.is_none() && pins.summary.is_some(), "{pins:?}");
    assert_eq!(
        ended(&d),
        (
            "Cline reported done".to_string(),
            "reported_done".to_string()
        )
    );
}

/// An end sequence whose take is answered asks nothing again.
#[tokio::test(flavor = "multi_thread")]
async fn b989_an_end_whose_take_is_answered_asks_no_further_time() {
    let (d, resp, _) = run_with_refreshes_at_the_end(&[]).await;
    the_saved_copy_holds_the_work(&d);
    delivered(&d, &resp);
    assert_eq!(asked_again(&d), Vec::<String>::new());
    assert!(
        !d.log.lines().iter().any(|l| l.contains("end credentials")),
        "{:?}",
        d.log.lines()
    );
}
