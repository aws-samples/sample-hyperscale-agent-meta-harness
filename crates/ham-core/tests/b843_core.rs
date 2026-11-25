//! The rules that need no process: what line mode says about a saved copy and how often, what `ham cancel` says about the
//! work of a run it cleared, and how the plan of `setup.sh --update --dry-run` is read for an update that deploys nothing on the stage. The
//! last test holds the sentences the plan is read by to the setup of this repository.
#![allow(clippy::unwrap_used)]

use ham_core::saved_copy::{
    ClearedCopy, SavedCopy, WATCH_LINE_EVERY_S, WatchMemo, cleared_work, watch_line,
};
use ham_core::update::{PLAN_GUARD_NOTHING, PLAN_UPDATE_NOTHING, plan_deploys_nothing};

const RUN: &str = "01M7CEECZXAQ04R6F2GZ583CPR";
const HEAD: &str = "ad2645e679230000000000000000000000000000";

fn copy(at: &str, session: u32, count: u32, last: bool) -> SavedCopy {
    SavedCopy {
        at: at.to_string(),
        at_unix: harness_protocol::common::parse_utc_unix(at).unwrap(),
        count,
        head_sha: HEAD.to_string(),
        session,
        final_copy: last,
    }
}

#[test]
fn b843_the_line_of_a_saved_copy_names_its_time_and_the_command() {
    assert_eq!(
        watch_line(&copy("2026-11-14T07:04:04Z", 1, 3, false), RUN),
        format!(
            "saved: a copy of the work at 2026-11-14 07:04 UTC; ham pull {RUN} --snapshot downloads it"
        )
    );
    // The run id is text of the service: an escape sequence in it never reaches the terminal.
    let line = watch_line(&copy("2026-11-14T07:04:04Z", 1, 3, false), "R\u{1b}[2J");
    assert!(!line.contains('\u{1b}'), "{line:?}");
}

#[test]
fn b843_line_mode_says_a_newer_copy_at_most_once_a_minute() {
    assert_eq!(WATCH_LINE_EVERY_S, 60);
    let mut memo = WatchMemo::default();
    assert_eq!(memo.line(None, RUN, 0), None, "no copy, no line");
    let first = copy("2026-11-14T07:03:34Z", 1, 1, false);
    assert!(
        memo.line(Some(&first), RUN, 5)
            .unwrap()
            .contains("07:03 UTC")
    );
    assert_eq!(
        memo.line(Some(&first), RUN, 6),
        None,
        "the same copy is said once"
    );
    assert_eq!(memo.line(Some(&first), RUN, 500), None, "however long ago");
    // Newer copies within the minute are held back; the newest one is said when the minute is over.
    let mut memo = WatchMemo::default();
    assert!(memo.line(Some(&first), RUN, 5).is_some());
    let second = copy("2026-11-14T07:04:04Z", 1, 2, false);
    let third = copy("2026-11-14T07:04:19Z", 1, 3, false);
    assert_eq!(memo.line(Some(&second), RUN, 35), None);
    assert_eq!(
        memo.line(Some(&third), RUN, 64),
        None,
        "59 s after the last line"
    );
    assert!(
        memo.line(Some(&third), RUN, 65)
            .unwrap()
            .contains("07:04 UTC")
    );
    // An older copy that arrives late is never said.
    assert_eq!(memo.line(Some(&second), RUN, 300), None);
    // A copy of a later session is newer whatever its number.
    let renewed = copy("2026-11-14T07:14:29Z", 2, 1, false);
    assert!(memo.line(Some(&renewed), RUN, 400).is_some());
    // The copy of the end is the card's: line mode adds no line of its own for it.
    let end = copy("2026-11-14T07:24:41Z", 2, 5, true);
    assert_eq!(memo.line(Some(&end), RUN, 900), None);
}

#[test]
fn b843_cancel_names_the_snapshot_command_only_for_a_copy_the_service_knows() {
    assert_eq!(
        cleared_work(
            RUN,
            &ClearedCopy::Saved {
                at: "2026-11-12T00:47:40Z".to_string()
            }
        ),
        format!(
            "ham pull {RUN} --snapshot downloads the copy of its work saved at 2026-11-12 00:47 UTC."
        )
    );
    assert_eq!(
        cleared_work(RUN, &ClearedCopy::None),
        "The run saved no copy of its work, so there is nothing to download."
    );
    // The service could not be asked: the sentence promises no copy.
    assert_eq!(
        cleared_work(RUN, &ClearedCopy::Unknown),
        format!("If the run saved a copy of its work, ham pull {RUN} --snapshot downloads it.")
    );
}

// ---- the plan of setup ---------------------------------------------------------------------------------------------------------------

const DEPLOYS: &str = "\
== Plan
  stage:         direct
  source:        commit 2a49dc96c4adf8a45ec1459a91852029c0c2ee12 (clean clone)
  update:        the stage keeps its resources and gets the new revision; the runner is installed again for the build and removed afterwards
  image:         the worker image is reused
  duration:      about 30 minutes
  guard:         the update takes the stage's deploy lock, so the stage starts no new runs, and waits up to 60 minutes for the active runs to end (--wait-for-runs). No run is cancelled.
                 The build also refuses worker runtime changes while runs are active.
  active runs:   1 running run(s): 01M7CEBG372JG65RYY55770FJM. The update waits up to 60 minutes for them to end; new runs are refused meanwhile. No run is cancelled.

== Dry run
would install the runner
Nothing was created. Run again without --dry-run to do it.
";

fn nothing(update: &str, guard: &str) -> String {
    format!(
        "This update builds only the client: the stage already runs this commit, so nothing on the stage changes and active runs do not stop it.\n\n== Plan\n  stage:         direct\n  label:         the backend reports v5.19 and will report v5.20 (this release changes no file the stage runs, so nothing is deployed and only its version label is written)\n  update:        {update}\n  guard:         {guard}\n\n== Dry run\nNothing was created. Run again without --dry-run to do it.\n"
    )
}

const GUARD_WAIT: &str = "this update changes nothing on the stage, so it waits for no run (--wait-for-runs) and the stage keeps starting runs.";
const GUARD_PLAIN: &str =
    "this update deploys nothing, so active runs do not stop it and the stage keeps starting runs.";
const GUARD_DEPLOYS: &str = "the update takes the stage's deploy lock, so the stage starts no new runs, and waits up to 60 minutes for the active runs to end (--wait-for-runs). No run is cancelled.";

#[test]
fn b841_a_plan_that_deploys_nothing_is_read_from_its_update_and_guard_lines() {
    for update in [
        "nothing is deployed; the runner is installed again, builds only the client and is removed afterwards",
        "nothing is deployed; the runner is installed again, builds only the client and is kept (--keep-runner)",
        "nothing is deployed; only the client is built here, and no runner is installed",
        "nothing on the stage changes; no runner is installed",
    ] {
        for guard in [GUARD_WAIT, GUARD_PLAIN] {
            assert!(
                plan_deploys_nothing(&nothing(update, guard)),
                "{update} / {guard}"
            );
        }
    }
}

#[test]
fn b841_any_other_plan_decides_nothing() {
    assert!(!plan_deploys_nothing(DEPLOYS), "a plan that deploys");
    assert!(!plan_deploys_nothing(""), "no plan");
    assert!(!plan_deploys_nothing(
        "Nothing was created. Run again without --dry-run to do it.\n"
    ));
    // The two lines must agree.
    assert!(!plan_deploys_nothing(&nothing(
        "nothing is deployed; the runner is installed again, builds only the client and is removed afterwards",
        GUARD_DEPLOYS
    )));
    assert!(!plan_deploys_nothing(&nothing(
        "the stage keeps its resources and gets the new revision; no runner is installed",
        GUARD_WAIT
    )));
    // A guard that leaves the runs unchecked is not the guard of a plan that deploys nothing.
    assert!(!plan_deploys_nothing(&nothing(
        "nothing is deployed; only the client is built here, and no runner is installed",
        "active runs are not checked before the build; the build refuses only when the worker runtime changes"
    )));
    // One line of each kind: a plan with two is not one this client knows.
    let twice = nothing(
        "nothing on the stage changes; no runner is installed",
        GUARD_PLAIN,
    );
    assert!(!plan_deploys_nothing(&format!("{twice}{twice}")));
    // The words inside a sentence are no plan line.
    assert!(!plan_deploys_nothing(
        "The update: nothing is deployed; guard: this update deploys nothing, so active runs do not stop it.\n"
    ));
    // One line without the other decides nothing.
    assert!(!plan_deploys_nothing(
        "  update:        nothing is deployed; only the client is built here\n"
    ));
    assert!(!plan_deploys_nothing(&format!(
        "  guard:         {GUARD_PLAIN}\n"
    )));
}

/// The sentences above are the plan lines of the setup of this repository (`scripts/setup/full.sh`, `full_plan`): each plan of an update that
/// deploys nothing begins its `update:` line with one of [`PLAN_UPDATE_NOTHING`] and its `guard:` line with one of [`PLAN_GUARD_NOTHING`],
/// and no plan of an update that deploys does. A release that words them anew makes an earlier client wait as before (it reads no decision
/// from a plan it does not know); this test makes such a rewording visible in the change that makes it.
#[test]
fn b841_the_sentences_the_plan_is_read_by_are_the_ones_setup_prints() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/setup/full.sh");
    let text = std::fs::read_to_string(path).unwrap();
    let hows: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("how=\""))
        .collect();
    assert_eq!(
        hows.len(),
        7,
        "the update line of the plan has seven forms: {hows:?}"
    );
    let nothing = hows
        .iter()
        .filter(|h| PLAN_UPDATE_NOTHING.iter().any(|p| h.starts_with(p)))
        .count();
    assert_eq!(nothing, 4, "{hows:?}");
    for how in &hows {
        assert!(
            PLAN_UPDATE_NOTHING.iter().any(|p| how.starts_with(p))
                || how.starts_with("the stage keeps its resources and gets the new revision;"),
            "an update line this client does not know: {how}"
        );
    }
    assert!(
        text.contains("say \"  update:        $how\""),
        "the plan prints the line as `update:`"
    );
    let lines: Vec<&str> = text.lines().collect();
    let guards: Vec<(usize, &str)> = lines
        .iter()
        .enumerate()
        .filter_map(|(n, l)| {
            l.trim()
                .strip_prefix("say \"  guard:")
                .map(|g| (n, g.trim()))
        })
        .collect();
    assert_eq!(guards.len(), 5, "{guards:?}");
    for prefix in PLAN_GUARD_NOTHING {
        let with: Vec<&(usize, &str)> = guards
            .iter()
            .filter(|(_, g)| g.starts_with(prefix))
            .collect();
        assert_eq!(
            with.len(),
            1,
            "{prefix:?} begins exactly one guard line: {guards:?}"
        );
        // The guard of a plan that deploys nothing is printed only where the update builds the client alone.
        let condition = lines[with[0].0 - 1];
        assert!(
            condition.contains("SETUP_CLIENT_ONLY:-0}\" = 1"),
            "{prefix:?} follows {condition:?}"
        );
    }
}

/// With a run of the caller active, `--no-wait` answers only after setup's plan, a preflight of the AWS account. The help of the option
/// promises no answer at once: it names the plan and about how long it takes.
#[test]
fn b841_the_help_of_no_wait_names_the_plan_that_comes_first() {
    let text = ham_core::help::command_text("update").unwrap();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let options = flat.split_once("Options:").unwrap().1;
    let no_wait = options
        .split_once("--no-wait ")
        .unwrap()
        .1
        .split_once(" --wait <minutes>")
        .unwrap()
        .0;
    assert!(!no_wait.contains("at once"), "{no_wait}");
    for part in ["asks setup for its plan", "about 20 seconds"] {
        assert!(no_wait.contains(part), "{part:?} missing from: {no_wait}");
    }
}
