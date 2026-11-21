//! The detail of `run.completed` reads as sentences, and the label the event carries is the label the rule gives for the event's
//! own detail (`harness-protocol`: `label == outcome.label_for(detail)`).
//!
//! Found live by row A6 on release 5.24: a run that the harness ended after five rounds without a changed file wrote the detail
//! "The harness ended the run: 5 rounds in a row changed no file.; no pull request: Cline left the repository unchanged". The label
//! rule ends the reason at a period followed by a space, found none and took the whole text, so the label a reader computes from the
//! event's detail was not the label the event carried.
//!
//! `finalize` joins the parts of the detail: a note about the test report, the plan's detail, how many sessions the run used, why there is
//! no pull request, and what the zip holds. After a part that ends a sentence the next one follows after one space and the harness's own
//! fragments begin with a capital letter; after any other part the joiner stays "; ". A stop by the harness leads the detail.
//!
//! The same join stood in two more texts (the review of the lane): the reason of a blocked run, which joins the validator's violations,
//! and the detail of a run whose session left no summary, which wraps the error agentd joined for the turn. Both follow the same rule.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::common::Engine;
use harness_protocol::direct::{
    DirectOutcome, HARNESS_STOP_PREFIX, MAX_DIRECT_DETAIL_CHARS, zip_holds,
};
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

const PROMPT: &str = "Add percent_change(a, b) with tests";
/// The detail agentd wrote in the live run: the goal loop's rule for rounds that changed no file.
const STALL: &str = "The harness ended the run: 5 rounds in a row changed no file.";
const COPY_NOTE: &str = "the test report could not be saved to the run's evidence";
const SKIPPED_NOTE: &str = "test report skipped: not enough time before the run deadline";
const WORKFLOW: &str = ".github/workflows/ci.yml";
const WORKFLOW_NOTICE: &str = "The zip holds a workflow file (.github/workflows/ci.yml); it runs only if the code is pushed to a git host that runs such files.";
const CONFLICT: &str = "delivery conflict: branch harness/01M4 was moved. No pull request was opened, because its list of changes would be wrong. The zip holds this run's code.";

/// The two engines: the engine, its profile name and the name its sentences use.
fn engines() -> [(Engine, &'static str, &'static str); 2] {
    [
        (Engine::CLINE, "cline", "Cline"),
        (Engine::OPENCODE, "opencode", "OpenCode"),
    ]
}

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

/// A repository run of the engine named `profile`.
fn world(engine: Engine, profile: &str) -> Mem {
    let mem = Mem::default();
    if profile == "cline" {
        mem.seed(repo_target(), PROMPT, MODEL);
    } else {
        mem.seed_with_members(repo_target(), PROMPT, MODEL, json!({"engine": profile}));
        mem.set_run_engine(engine);
    }
    mem.put_bundle();
    mem
}

fn packaged() -> Value {
    json!({"artifactId": "zip-0123456789abcdef0123", "sha256": "ab".repeat(32), "sizeBytes": 4096})
}

fn completed(mem: &Mem) -> Value {
    mem.events()
        .into_iter()
        .find(|(t, _)| t == "run.completed")
        .map(|(_, v)| v)
        .expect("run.completed")
}

/// The plan of a run of the engine whose session the harness ended with `detail` and that left the repository as it found it: the
/// decision `plan-delivery` makes from the session's own summary.
async fn unchanged_plan(mem: &Mem, engine: Engine, profile: &str, detail: &str) -> Value {
    let mut s = summary(DirectOutcome::StoppedOther, Some("m"), true);
    s.engine.name = profile.into();
    s.detail = Some(detail.into());
    s.label = DirectOutcome::StoppedOther.label_for_engine(engine, s.detail.as_deref());
    s.git.head_sha = Some(BASE.into());
    mem.put_summary(&s);
    let mut turn = turn_output(true);
    turn["lastCommitSha"] = json!(BASE);
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": {"turn": turn, "failure": {}}}),
    )
    .await
    .unwrap()
}

async fn finalize(mem: &Mem, plan: &Value, validated: Value, pr_failure: Value) -> Value {
    mem.clear_events();
    call(
        mem,
        json!({"op": "finalize", "runId": RUN, "plan": plan, "validated": validated,
            "packaged": packaged(), "prLink": {}, "prFailure": pr_failure}),
    )
    .await
    .unwrap();
    completed(mem)
}

/// What every `run.completed` must hold: the label is the one the rule gives for the event's own detail, the detail has no sentence end
/// followed by a semicolon, and it fits the limit of a detail.
fn assert_reads_as_sentences(engine: Engine, outcome: DirectOutcome, card: &Value, what: &str) {
    let detail = card.get("detail").and_then(Value::as_str);
    let label = card["label"].as_str().unwrap();
    assert_eq!(
        label,
        outcome.label_for_engine(engine, detail),
        "the label of the event is the label of its own detail ({what}): {card}"
    );
    if let Some(d) = detail {
        // The notice of what the zip holds marks a path it cut with three dots (`zip_holds`). That mark is no sentence end, so it is
        // left out of the search for a doubled one.
        let sentences = d.replace("...", "\u{2026}");
        for broken in [".;", "!;", "?;", ".. ", "; ."] {
            assert!(
                !sentences.contains(broken),
                "{broken:?} in the detail ({what}): {d}"
            );
        }
        assert!(
            d.chars().count() <= MAX_DIRECT_DETAIL_CHARS,
            "the detail is {} characters ({what})",
            d.chars().count()
        );
        assert!(!d.is_empty() && d.trim() == d, "{what}: {d:?}");
    }
}

// ---- T1: the measured defect ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn b875_a_run_the_harness_ended_without_a_changed_file_carries_the_label_of_its_own_detail() {
    for (engine, profile, name) in engines() {
        let mem = world(engine, profile);
        let p = unchanged_plan(&mem, engine, profile, STALL).await;
        let label = format!(
            "{name} stopped: the harness ended the run (5 rounds in a row changed no file)"
        );
        assert_eq!(
            p["label"],
            json!(label),
            "TRIGGER PRESENT: the plan's label names the rule: {p}"
        );
        assert_eq!(
            p["noPrReason"],
            json!(format!("{name} left the repository unchanged")),
            "TRIGGER PRESENT: the run opens no pull request: {p}"
        );
        let card = finalize(&mem, &p, json!({"pass": true}), json!({})).await;
        assert_eq!(card["label"], json!(label), "{card}");
        assert_reads_as_sentences(engine, DirectOutcome::StoppedOther, &card, name);
        assert_eq!(
            card["detail"],
            json!(format!(
                "{STALL} No pull request: {name} left the repository unchanged"
            )),
            "{card}"
        );
    }
}

#[tokio::test]
async fn b875_the_sentence_about_the_sessions_is_followed_by_a_sentence() {
    for (engine, profile, name) in engines() {
        let mem = world(engine, profile);
        let mut p = unchanged_plan(&mem, engine, profile, STALL).await;
        p["sessions"] = json!(3);
        let card = finalize(&mem, &p, json!({"pass": true}), json!({})).await;
        assert_reads_as_sentences(engine, DirectOutcome::StoppedOther, &card, name);
        assert_eq!(
            card["detail"],
            json!(format!(
                "{STALL} The run used 3 sessions. No pull request: {name} left the repository unchanged"
            )),
            "{card}"
        );
    }
}

// ---- T2: every combination of the parts ---------------------------------------------------------------------------------------------

/// The details a plan can carry for `outcome`: none, a text with no sentence end, sentences that end with a period, a question mark or
/// an exclamation mark, and a text as long as a detail may be. A `stopped_other` run also carries the harness's own stops.
fn plan_details(outcome: DirectOutcome) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = vec![
        None,
        Some("no output for 900 s".into()),
        Some("The route failed twice.".into()),
        Some("Ran on us.anthropic.claude-opus-5-5 after 2 switches. Every route was down for the last 40 s.".into()),
        Some("Which database should it use?".into()),
        Some("It gave up!".into()),
        Some("word ".repeat(200).trim_end().to_string()),
        // The error of a turn that left no summary, as `plan-delivery` joins it: sentences inside the brackets.
        Some(format!(
            "the session wrote no summary ({STALL} summary.json was not uploaded: AccessDenied)"
        )),
    ];
    if outcome == DirectOutcome::StoppedOther {
        let long_first = format!("{HARNESS_STOP_PREFIX} {}", "round ".repeat(165));
        let long_tail = format!("{STALL} {}", "Its words went on. ".repeat(50));
        out.extend(
            [
                // One sentence that ends with a period: the live run.
                STALL.to_string(),
                // Two sentences and no period at the end: a refusal of the run's final files.
                format!(
                    "{HARNESS_STOP_PREFIX} the session sent no record of the files it saved at its end. They were not used"
                ),
                // The engine's quoted words after the rule.
                format!(
                    "{HARNESS_STOP_PREFIX} Cline ended 3 rounds in a row with the same last line. Its words: \"BLOCKED: which database?\""
                ),
                // One sentence with no period, with a question mark and with an exclamation mark: none is written today, and the label
                // must not depend on that.
                format!("{HARNESS_STOP_PREFIX} the goal loop did not restart"),
                format!("{HARNESS_STOP_PREFIX} did the goal loop restart?"),
                format!("{HARNESS_STOP_PREFIX} the goal loop did not restart!"),
                // A snapshot delivery: the sentence about the copy follows the stop.
                format!(
                    "{HARNESS_STOP_PREFIX} you stopped it at 2026-10-13 18:55 UTC while its session was not answering. No further session was started. The work is the last saved snapshot."
                ),
                // As long as a detail may be: a first sentence that fills it, and a short first sentence with a long tail.
                long_first.chars().take(MAX_DIRECT_DETAIL_CHARS).collect::<String>().trim_end().to_string(),
                long_tail.chars().take(MAX_DIRECT_DETAIL_CHARS).collect::<String>().trim_end().to_string(),
            ]
            .map(Some),
        );
    }
    out
}

/// The validator's answer that gives the longest notice of what a zip holds, and that notice: several workflow files and a build
/// specification, each named by a path longer than the 120 characters the notice shows of it.
fn longest_zip_answer() -> (Value, String) {
    let deep = "deep/".repeat(30);
    let workflow = format!(".github/workflows/{deep}ci.yml");
    let buildspec = format!("{deep}buildspec.yml");
    let notice = zip_holds(
        &[workflow.clone(), WORKFLOW.to_string()],
        std::slice::from_ref(&buildspec),
    )
    .unwrap();
    let found = json!({"pass": true, "violations": [], "findings": [
        {"rule": "workflow-file", "path": workflow, "detail": "reported"},
        {"rule": "workflow-file", "path": WORKFLOW, "detail": "reported"},
        {"rule": "buildspec", "path": buildspec, "detail": "reported"}]});
    (found, notice)
}

#[tokio::test]
async fn b875_the_label_is_the_label_of_the_detail_for_every_combination_of_its_parts() {
    let notes = [None, Some(COPY_NOTE), Some(SKIPPED_NOTE)];
    // What the validator found for a zip, with the notice `finalize` makes of it: nothing, a workflow file the zip holds, and the
    // longest notice there is.
    let (longest_found, longest_notice) = longest_zip_answer();
    assert!(
        longest_notice.chars().count() > 450 && longest_notice.matches("...").count() == 2,
        "TRIGGER PRESENT: two sentences, each with a path cut at 120 characters: {longest_notice}"
    );
    let validated = [
        (json!({"pass": true, "violations": []}), None),
        (
            json!({"pass": true, "violations": [], "findings": [
                {"rule": "workflow-file", "path": WORKFLOW, "detail": "reported"}]}),
            Some(WORKFLOW_NOTICE.to_string()),
        ),
        (longest_found, Some(longest_notice)),
    ];
    let mut combinations = 0_u32;
    let mut harness_stops = 0_u32;
    let mut stops_that_gave_way = 0_u32;
    for (engine, profile, name) in engines() {
        // One plan of the real `plan-delivery`; each combination changes the members `finalize` joins.
        let base = unchanged_plan(&world(engine, profile), engine, profile, STALL).await;
        for outcome in DirectOutcome::ALL {
            for detail in plan_details(outcome) {
                for note in notes {
                    for sessions in [1_u32, 3] {
                        // No pull request by design, a pull request that could not be opened, and neither (a zip run, or a pull request).
                        for pull_request in 0..3_u8 {
                            for (found, notice) in &validated {
                                let mut p = base.clone();
                                p["outcome"] = json!(outcome);
                                p["label"] =
                                    json!(outcome.label_for_engine(engine, detail.as_deref()));
                                p["detail"] = json!(detail);
                                p["reportNote"] = json!(note);
                                p["sessions"] = json!(sessions);
                                p["slice"] = json!(sessions);
                                p["noPrReason"] = match pull_request {
                                    0 => json!(format!("{name} left the repository unchanged")),
                                    _ => Value::Null,
                                };
                                let failure = match pull_request {
                                    1 => json!({"Error": "Refused", "Cause": CONFLICT}),
                                    _ => json!({}),
                                };
                                let what = format!(
                                    "{name}, {}, detail {detail:?}, note {note:?}, {sessions} session(s), pull request case {pull_request}, {found}",
                                    outcome.as_str()
                                );
                                let mem = world(engine, profile);
                                let card = finalize(&mem, &p, found.clone(), failure).await;
                                assert_eq!(
                                    card["label"], p["label"],
                                    "the label is the plan's ({what}): {card}"
                                );
                                assert_reads_as_sentences(engine, outcome, &card, &what);
                                let nothing = detail.is_none()
                                    && note.is_none()
                                    && sessions == 1
                                    && pull_request == 2
                                    && found.get("findings").is_none();
                                let said = card.get("detail").and_then(Value::as_str);
                                assert_eq!(said.is_none(), nothing, "{what}: {card}");
                                if let Some(n) = notice {
                                    assert!(
                                        said.is_some_and(|d| d.ends_with(n.as_str())),
                                        "the notice of what the zip holds is whole ({what}): {card}"
                                    );
                                    // A stop by the harness that is too long to stand beside the notice is cut, and the label above
                                    // still reads its reason from what is left.
                                    if outcome == DirectOutcome::StoppedOther
                                        && detail.as_deref().is_some_and(|d| {
                                            d.starts_with(HARNESS_STOP_PREFIX)
                                                && d.chars().count() + n.chars().count() + 2
                                                    > MAX_DIRECT_DETAIL_CHARS
                                        })
                                    {
                                        stops_that_gave_way += 1;
                                    }
                                }
                                combinations += 1;
                                if card["label"]
                                    .as_str()
                                    .unwrap()
                                    .contains("the harness ended the run")
                                {
                                    harness_stops += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // TRIGGER PRESENT: the table holds runs of every outcome, runs the harness ended whose label carries a reason, and stops that
    // gave way to a notice.
    assert!(combinations > 5_000, "{combinations}");
    assert!(harness_stops > 500, "{harness_stops}");
    assert!(stops_that_gave_way > 100, "{stops_that_gave_way}");
}

// ---- the words: each join, as a person reads it ---------------------------------------------------------------------------------------

/// The detail of a Cline run whose plan carries these members.
async fn detail_of(
    outcome: DirectOutcome,
    detail: Option<&str>,
    note: Option<&str>,
    sessions: u32,
    no_pr: bool,
    failure: Value,
    validated: Value,
) -> String {
    let mem = world(Engine::CLINE, "cline");
    let mut p = unchanged_plan(&mem, Engine::CLINE, "cline", STALL).await;
    p["outcome"] = json!(outcome);
    p["label"] = json!(outcome.label_for(detail));
    p["detail"] = json!(detail);
    p["reportNote"] = json!(note);
    p["sessions"] = json!(sessions);
    p["slice"] = json!(sessions);
    if !no_pr {
        p["noPrReason"] = Value::Null;
    }
    let card = finalize(&mem, &p, validated, failure).await;
    assert_reads_as_sentences(Engine::CLINE, outcome, &card, "words");
    card["detail"].as_str().unwrap().to_string()
}

fn plain() -> Value {
    json!({"pass": true, "violations": []})
}

fn with_workflow() -> Value {
    json!({"pass": true, "violations": [], "findings": [{"rule": "workflow-file", "path": WORKFLOW, "detail": "reported"}]})
}

#[tokio::test]
async fn b875_after_a_part_with_no_sentence_end_the_joiner_and_the_fragments_stay_as_they_were() {
    let silence = DirectOutcome::StoppedSilence;
    assert_eq!(
        detail_of(
            silence,
            Some("no output for 900 s"),
            None,
            1,
            true,
            json!({}),
            plain()
        )
        .await,
        "no output for 900 s; no pull request: Cline left the repository unchanged"
    );
    assert_eq!(
        detail_of(
            silence,
            Some("no output for 900 s"),
            Some(COPY_NOTE),
            3,
            false,
            json!({"Error": "Refused", "Cause": "push refused"}),
            plain()
        )
        .await,
        "the test report could not be saved to the run's evidence; no output for 900 s; The run used 3 sessions. Pull request not opened: push refused"
    );
    // A fragment that is the whole detail stays as it was.
    assert_eq!(
        detail_of(
            DirectOutcome::ReportedDone,
            None,
            None,
            1,
            true,
            json!({}),
            plain()
        )
        .await,
        "no pull request: Cline left the repository unchanged"
    );
    assert_eq!(
        detail_of(
            DirectOutcome::ReportedDone,
            None,
            None,
            1,
            false,
            json!({"Error": "Refused", "Cause": "push refused"}),
            plain()
        )
        .await,
        "pull request not opened: push refused"
    );
    // The notice of what the zip holds follows as it did.
    assert_eq!(
        detail_of(
            silence,
            Some("no output for 900 s"),
            None,
            1,
            true,
            json!({}),
            with_workflow()
        )
        .await,
        format!(
            "no output for 900 s; no pull request: Cline left the repository unchanged. {WORKFLOW_NOTICE}"
        )
    );
}

#[tokio::test]
async fn b875_after_a_sentence_the_next_part_follows_after_one_space_and_begins_with_a_capital() {
    let done = DirectOutcome::ReportedDone;
    assert_eq!(
        detail_of(
            done,
            Some("The route failed twice."),
            None,
            1,
            true,
            json!({}),
            plain()
        )
        .await,
        "The route failed twice. No pull request: Cline left the repository unchanged"
    );
    assert_eq!(
        detail_of(
            done,
            Some("Which database should it use?"),
            None,
            1,
            false,
            json!({"Error": "Refused", "Cause": CONFLICT}),
            plain()
        )
        .await,
        format!("Which database should it use? Pull request not opened: {CONFLICT}")
    );
    assert_eq!(
        detail_of(done, None, None, 3, true, json!({}), with_workflow()).await,
        format!(
            "The run used 3 sessions. No pull request: Cline left the repository unchanged. {WORKFLOW_NOTICE}"
        )
    );
}

#[tokio::test]
async fn b875_a_stop_by_the_harness_leads_the_detail_and_the_note_about_the_test_report_follows_it()
{
    let other = DirectOutcome::StoppedOther;
    // TRIGGER PRESENT: with any other detail the note stands first, as it did.
    assert_eq!(
        detail_of(
            other,
            Some("exit status 3"),
            Some(COPY_NOTE),
            1,
            false,
            json!({}),
            plain()
        )
        .await,
        "the test report could not be saved to the run's evidence; exit status 3"
    );
    assert_eq!(
        detail_of(
            other,
            Some(STALL),
            Some(COPY_NOTE),
            1,
            true,
            json!({}),
            plain()
        )
        .await,
        format!(
            "{STALL} The test report could not be saved to the run's evidence; no pull request: Cline left the repository unchanged"
        )
    );
    assert_eq!(
        detail_of(
            other,
            Some(STALL),
            Some(SKIPPED_NOTE),
            1,
            false,
            json!({}),
            plain()
        )
        .await,
        format!("{STALL} Test report skipped: not enough time before the run deadline")
    );
    // A stop whose only sentence has no period gets one when something follows it, and stays as it is when nothing does.
    let bare = format!("{HARNESS_STOP_PREFIX} the goal loop did not restart");
    assert_eq!(
        detail_of(other, Some(&bare), None, 1, true, json!({}), plain()).await,
        format!("{bare}. No pull request: Cline left the repository unchanged")
    );
    assert_eq!(
        detail_of(other, Some(&bare), None, 1, false, json!({}), plain()).await,
        bare
    );
    // A stop as long as a detail may be gives way: the note and the notice are the harness's own words and are kept whole.
    let long = format!("{STALL} {}", "Its words went on. ".repeat(60));
    let long: String = long.chars().take(MAX_DIRECT_DETAIL_CHARS).collect();
    let d = detail_of(
        other,
        Some(long.trim_end()),
        Some(COPY_NOTE),
        3,
        true,
        json!({}),
        with_workflow(),
    )
    .await;
    assert!(d.starts_with(STALL), "{d}");
    assert!(d.to_lowercase().contains(COPY_NOTE), "{d}");
    assert!(d.ends_with(WORKFLOW_NOTICE), "{d}");
    assert!(d.chars().count() > MAX_DIRECT_DETAIL_CHARS - 40, "{d}");
}

#[tokio::test]
async fn b875_the_longest_note_and_the_longest_notice_leave_a_stop_its_reason() {
    let (found, notice) = longest_zip_answer();
    // The longest note about the test report: the report did not match the harness's record.
    let note = "the test report did not match the harness's record of it and was not used";
    // A stop whose only sentence fills a detail. The label cuts its reason at 120 characters.
    let stop = format!("{HARNESS_STOP_PREFIX} {}", "round ".repeat(165));
    let stop: String = stop.chars().take(MAX_DIRECT_DETAIL_CHARS).collect();
    // `detail_of` checks the label of the event against the detail it returns.
    let d = detail_of(
        DirectOutcome::StoppedOther,
        Some(stop.trim_end()),
        Some(note),
        3,
        false,
        json!({"Error": "Refused", "Cause": CONFLICT}),
        found,
    )
    .await;
    assert!(
        d.starts_with(&format!("{HARNESS_STOP_PREFIX} round round")),
        "{d}"
    );
    assert!(d.ends_with(&notice), "{d}");
    // The stop is closed with a period, the note follows it as a sentence, and what is left of the stop is more than three times the
    // 120 characters the label reads of it.
    let kept = d
        .find(". The test report did not match the harness's record of it and was not used")
        .unwrap_or_else(|| panic!("the note follows the stop: {d}"));
    assert!(kept > 400, "{kept} characters of the stop are left: {d}");
    assert!(d.chars().count() > MAX_DIRECT_DETAIL_CHARS - 3, "{d}");
}

// ---- the reason of a blocked run: the validator's violations ------------------------------------------------------------------------

/// The words the validator writes for a delivered copy of a stored third-party file (`delivery::third_party`): two sentences.
fn copy_violation(path: &str) -> Value {
    json!({"rule": "third-party-copy", "path": path, "detail": format!(
        "{path} equals lib/{path} of https://example.org/lib: the bytes are the same. A delivered copy of third-party text is not allowed."
    )})
}

/// The reason of a run whose bundle validation failed with `violations`: the answer of `mark-blocked`, which the `goal.blocked` event
/// carries too.
async fn blocked_reason(violations: Value) -> String {
    let mem = world(Engine::CLINE, "cline");
    let state =
        json!({"runId": RUN, "validated": {"result": {"pass": false, "violations": violations}}});
    let out = call(
        &mem,
        json!({"op": "mark-blocked", "runId": RUN, "state": state}),
    )
    .await
    .unwrap();
    let reason = out["reason"].as_str().unwrap().to_string();
    let event = mem
        .events()
        .into_iter()
        .find(|(t, _)| t == "goal.blocked")
        .map(|(_, v)| v)
        .expect("goal.blocked");
    assert_eq!(event["reason"], json!(reason));
    reason
}

#[tokio::test]
async fn b875_the_violations_of_a_blocked_run_are_joined_as_sentences() {
    let reason = blocked_reason(json!([copy_violation("a.c"), copy_violation("b.c")])).await;
    for broken in [".;", "!;", "?;"] {
        assert!(
            !reason.contains(broken),
            "{broken:?} in the reason: {reason}"
        );
    }
    assert_eq!(
        reason,
        "bundle validation failed: \
         third-party-copy a.c: a.c equals lib/a.c of https://example.org/lib: the bytes are the same. A delivered copy of third-party text is not allowed. \
         third-party-copy b.c: b.c equals lib/b.c of https://example.org/lib: the bytes are the same. A delivered copy of third-party text is not allowed."
    );
    // A violation that ends a sentence between two that do not: one space after it, a semicolon after the others.
    let reason = blocked_reason(json!([
        {"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"},
        copy_violation("a.c"),
        {"rule": "path-glob", "path": "x/y", "detail": "x/y is outside the allowed paths"},
        {"rule": "bundle", "detail": "is there a source bundle?"},
        {"rule": "file-size", "path": "big.bin", "detail": "big.bin is 3 GB"}
    ]))
    .await;
    assert_eq!(
        reason,
        "bundle validation failed: bundle-size: the bundle is 3 bytes; the limit is 2 bytes; \
         third-party-copy a.c: a.c equals lib/a.c of https://example.org/lib: the bytes are the same. A delivered copy of third-party text is not allowed. \
         path-glob x/y: x/y is outside the allowed paths; bundle: is there a source bundle? file-size big.bin: big.bin is 3 GB"
    );
}

#[tokio::test]
async fn b875_violations_without_a_sentence_end_keep_the_semicolon_between_them() {
    // TRIGGER PRESENT: none of these details ends a sentence, so the reason is word for word what it was.
    assert_eq!(
        blocked_reason(json!([
            {"rule": "path-glob", "path": "x/y", "detail": "x/y is outside the allowed paths"},
            {"rule": "bundle-size", "detail": "the bundle is 3 bytes; the limit is 2 bytes"},
            {"rule": "file-size", "path": "big.bin", "detail": "big.bin is 3 GB"}
        ]))
        .await,
        "bundle validation failed: path-glob x/y: x/y is outside the allowed paths; bundle-size: the bundle is 3 bytes; the limit is 2 bytes; file-size big.bin: big.bin is 3 GB"
    );
    assert_eq!(
        blocked_reason(json!([
            {"rule": "path-glob", "path": "x/y", "detail": "x/y is outside the allowed paths"}
        ]))
        .await,
        "bundle validation failed: path-glob x/y: x/y is outside the allowed paths"
    );
}

// ---- a turn that left no summary: the error agentd joined ---------------------------------------------------------------------------

/// The error of a turn whose summary was not stored, as agentd joins it (`compose_error`): the stop's detail, the upload that failed and
/// what the end of the session could not do, with "; " between them.
const TURN_ERROR: &str = "The harness ended the run: 5 rounds in a row changed no file.; summary.json was not uploaded: AccessDenied; the checkout was not tidied";

/// The plan of a run whose session left a turn-complete with `error` and a bundle, and no `summary.json`.
async fn plan_without_summary(mem: &Mem, error: Option<&str>) -> Value {
    let mut turn = turn_output(true);
    if let Some(e) = error {
        turn["error"] = json!(e);
    }
    call(
        mem,
        json!({"op": "plan-delivery", "runId": RUN, "staged": staged(), "session": {"turn": turn, "failure": {}}}),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn b875_the_error_of_a_turn_without_a_summary_reads_as_sentences() {
    for (engine, profile, name) in engines() {
        let mem = world(engine, profile);
        let p = plan_without_summary(&mem, Some(TURN_ERROR)).await;
        assert_eq!(
            p["outcome"],
            json!("stopped_session_lost"),
            "TRIGGER PRESENT: the session left no summary: {p}"
        );
        assert_eq!(
            p["deliver"],
            json!(true),
            "TRIGGER PRESENT: the run delivers the session's bundle: {p}"
        );
        assert_eq!(
            p["detail"],
            json!(format!(
                "the session wrote no summary ({STALL} summary.json was not uploaded: AccessDenied; the checkout was not tidied)"
            )),
            "{p}"
        );
        let card = finalize(&mem, &p, json!({"pass": true}), json!({})).await;
        assert_eq!(card["label"], p["label"], "{card}");
        assert_eq!(
            card["label"],
            json!(format!("{name} stopped: the session was lost")),
            "{card}"
        );
        assert_reads_as_sentences(engine, DirectOutcome::StoppedSessionLost, &card, name);
        assert!(
            card["detail"]
                .as_str()
                .unwrap()
                .starts_with(p["detail"].as_str().unwrap()),
            "{card}"
        );
    }
}

#[tokio::test]
async fn b875_a_turn_error_without_a_sentence_end_keeps_its_words() {
    // TRIGGER PRESENT: no part of these errors ends a sentence, so the detail is word for word what it was.
    let mem = world(Engine::CLINE, "cline");
    let p = plan_without_summary(
        &mem,
        Some("exit status 3; summary.json was not uploaded: AccessDenied"),
    )
    .await;
    assert_eq!(
        p["detail"],
        json!(
            "the session wrote no summary (exit status 3; summary.json was not uploaded: AccessDenied)"
        ),
        "{p}"
    );
    // A turn with no error names how the engine ended its turn.
    let mem = world(Engine::CLINE, "cline");
    let p = plan_without_summary(&mem, None).await;
    assert_eq!(
        p["detail"],
        json!("the session wrote no summary (end_turn)"),
        "{p}"
    );
    // A question and an exclamation end a sentence too.
    let mem = world(Engine::CLINE, "cline");
    let p = plan_without_summary(
        &mem,
        Some("Which database?; It gave up!; the checkout was not tidied"),
    )
    .await;
    assert_eq!(
        p["detail"],
        json!(
            "the session wrote no summary (Which database? It gave up! the checkout was not tidied)"
        ),
        "{p}"
    );
}
