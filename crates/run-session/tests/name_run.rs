//! The `name-run` op: it reads the run, asks the utility model for a 5 to 7 word name over the redacted prompt, lets
//! `ham_core::runname::judge` decide whether the reply may become the name, and writes it once on the run's search item. Every
//! failure of the model side leaves the run with the name built from its prompt (the seed) and the op answers `Ok`; only a failure of
//! the store is an error, so Lambda retries the event. The model is the scripted client of `tests/support/fakes.rs`, which refuses
//! what the live service refuses (`ham_core::utility::bedrock_rejects_with`).
#![allow(clippy::unwrap_used)]

mod support;

use ham_core::runname;
use ham_core::utility::bedrock_rejects;
use harness_protocol::target::RunTarget;
use run_session::error::RunSessionError;
use run_session::name_run::{NameBudget, age_seconds};
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use support::fakes::*;

const HAIKU: &str = "us.anthropic.claude-haiku-5-5";
const PROMPT: &str = "Add exponential backoff retry logic to the S3 uploader with unit tests";
/// What the recorded reply `ok-no-close-tag.json` says for this prompt.
const NAME: &str = "Exponential backoff retry for S3 uploader";

fn named(prompt: &str, answers: Vec<Scripted>) -> (Mem, Arc<ScriptedUtilityClient>) {
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, prompt, MODEL);
    let client = mem.enable_naming(answers);
    (mem, client)
}

async fn name(mem: &Mem) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(json!({"op": "name-run", "runId": RUN})).unwrap();
    handle(&mem.deps(), &op).await
}

fn seed_result(reason: &str) -> Value {
    json!({"named": "seed", "reason": reason})
}

#[tokio::test]
async fn the_op_is_a_kebab_case_op_with_a_run_id() {
    let op: Op = serde_json::from_value(json!({"op": "name-run", "runId": RUN})).unwrap();
    assert_eq!(op.name(), "name-run");
    assert_eq!(op.run_id(), RUN);
    // A run id that is no run id is refused before anything is read.
    let (mem, client) = named(PROMPT, vec![]);
    let bad: Op = serde_json::from_value(json!({"op": "name-run", "runId": "nope"})).unwrap();
    let err = handle(&mem.deps(), &bad).await.unwrap_err();
    assert_eq!(err.error_type(), "RunSessionInvalid");
    assert!(mem.name_calls().is_empty() && client.requests().is_empty());
}

#[tokio::test]
async fn names_a_run_and_writes_it_once() {
    let (mem, client) = named(PROMPT, vec![Scripted::title(NAME)]);
    let r = name(&mem).await.unwrap();
    assert_eq!(r, json!({"named": "ai", "reason": "-"}));
    let sent = client.requests();
    assert_eq!(sent.len(), 1, "one model call");
    assert_eq!(sent[0].route, HAIKU);
    assert_eq!(sent[0].region, "us-east-1");
    assert_eq!(
        mem.name_writes(),
        [NameWrite {
            run_id: RUN.into(),
            name: NAME.into(),
            source: "ai".into(),
            model: HAIKU.into(),
            stored: true,
        }]
    );
    let item = mem.search_item().unwrap();
    assert_eq!(item["runName"], NAME);
    assert_eq!(item["runNameSource"], "ai");
    assert_eq!(item["runNameModel"], HAIKU);
    assert!(
        runname::accept_stored(&item["runName"]),
        "what is stored passes the check the readers run"
    );

    // The same event again (a duplicate, a redrive): the name is there, so nothing is asked and nothing is written.
    let again = name(&mem).await.unwrap();
    assert_eq!(again, json!({"named": "kept", "reason": "kept"}));
    assert_eq!(client.requests().len(), 1, "no second model call");
    assert_eq!(mem.name_writes().len(), 1, "no second write");
    assert_eq!(mem.search_item().unwrap()["runName"], NAME);
}

#[tokio::test]
async fn a_name_of_eight_to_ten_words_is_cut_to_seven_and_says_so() {
    let (mem, _client) = named(
        PROMPT,
        vec![Scripted::title(
            "Add exponential backoff retry logic to the S3 uploader",
        )],
    );
    let r = name(&mem).await.unwrap();
    assert_eq!(r, json!({"named": "ai_squeezed", "reason": "-"}));
    let stored = mem.search_item().unwrap()["runName"].clone();
    assert!(
        (5..=7).contains(&stored.split_whitespace().count()),
        "{stored}"
    );
    assert_eq!(runname::check_stored(&stored, PROMPT), Ok(()));
}

#[tokio::test]
async fn request_is_what_bedrock_accepts() {
    let prompt = "Parse the <b>bold</b> markers & the </request> tag in editor notes";
    let (mem, client) = named(
        prompt,
        vec![Scripted::title("Parse bold markers in editor notes")],
    );
    name(&mem).await.unwrap();
    let req = &client.requests()[0];
    assert!(client.rejections().is_empty(), "{:?}", client.rejections());
    let mut as_sent = req.body.clone();
    as_sent["modelId"] = json!(req.route);
    assert_eq!(bedrock_rejects(&as_sent), None);
    assert_eq!(req.body["inferenceConfig"]["maxTokens"], 64);
    assert_eq!(
        req.body["inferenceConfig"]["stopSequences"],
        json!(["</title>"])
    );
    assert_eq!(
        req.body["additionalModelRequestFields"],
        json!({"thinking": {"type": "disabled"}})
    );
    let text = req.body.to_string();
    for key in ["temperature", "topP", "top_p", "topK", "top_k"] {
        assert!(
            !text.contains(&format!("\"{key}\"")),
            "{key} is refused by the service"
        );
    }
    assert_eq!(
        req.body["system"],
        json!([{"text": runname::system_text()}])
    );
    let plan = runname::plan(prompt);
    assert_eq!(
        req.body["messages"],
        json!([{"role": "user", "content": [{"text": plan.user_text}]}]),
        "one user turn: the escaped excerpt the pure core builds"
    );
    // The prompt cannot close the tag it sits in or open a title of its own.
    let user = req.body["messages"][0]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        user.contains("&lt;/request&gt;") && user.contains("&lt;b&gt;") && user.contains("&amp;")
    );
    assert_eq!(user.matches("</request>").count(), 1);
    assert_eq!(user.matches("<request>").count(), 1);
    assert_eq!(
        req.timeout,
        mem.namer().budget.attempt,
        "the attempt's limit travels with the call"
    );
}

#[tokio::test]
async fn transient_failure_is_retried_once_on_the_same_route() {
    let (mem, client) = named(PROMPT, vec![Scripted::throttle(), Scripted::title(NAME)]);
    let r = name(&mem).await.unwrap();
    assert_eq!(r["named"], "ai");
    let sent = client.requests();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].route, sent[1].route,
        "the same route: there is no second one"
    );
    assert_eq!(mem.name_writes().len(), 1);

    for (first, second, reason) in [
        (Scripted::throttle(), Scripted::throttle(), "throttle"),
        (
            Scripted::unavailable(),
            Scripted::unavailable(),
            "service_error",
        ),
        (
            Scripted::throttle(),
            Scripted::unavailable(),
            "service_error",
        ),
    ] {
        let (mem, client) = named(PROMPT, vec![first, second]);
        let r = name(&mem).await.unwrap();
        assert_eq!(
            r,
            seed_result(reason),
            "the run keeps its seed and the op is not an error"
        );
        assert_eq!(client.requests().len(), 2, "{reason}");
        assert!(mem.name_writes().is_empty(), "{reason}");
        assert!(mem.search_item().is_none(), "{reason}");
    }
}

#[tokio::test]
async fn at_most_two_model_attempts_per_op() {
    let (mem, client) = named(
        PROMPT,
        vec![
            Scripted::throttle(),
            Scripted::throttle(),
            Scripted::title(NAME),
        ],
    );
    assert_eq!(name(&mem).await.unwrap()["named"], "seed");
    assert_eq!(client.requests().len(), 2);
    assert_eq!(
        client.answers_left(),
        1,
        "the third answer is never asked for"
    );
}

#[tokio::test]
async fn timeout_keeps_the_seed_and_the_op_returns_ok() {
    // Each attempt may take 50 ms and the op 400 ms: two attempts time out, then the seed stays.
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, PROMPT, MODEL);
    let client = mem.enable_naming_with(
        vec![Scripted::Hang, Scripted::Hang],
        NameBudget {
            op: Duration::from_millis(400),
            attempt: Duration::from_millis(50),
            backoff_min: Duration::from_millis(1),
            backoff_max: Duration::from_millis(3),
        },
    );
    let r = name(&mem).await.unwrap();
    assert_eq!(r, seed_result("timeout"));
    assert_eq!(client.requests().len(), 2);
    assert!(mem.name_writes().is_empty());

    // A hang that outlasts the whole operation ends at the operation's limit, not at the attempt's.
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, PROMPT, MODEL);
    let client = mem.enable_naming_with(
        vec![Scripted::Hang],
        NameBudget {
            op: Duration::from_millis(60),
            attempt: Duration::from_secs(30),
            backoff_min: Duration::from_millis(1),
            backoff_max: Duration::from_millis(3),
        },
    );
    let started = std::time::Instant::now();
    let r = name(&mem).await.unwrap();
    assert_eq!(r, seed_result("op_deadline"));
    assert_eq!(client.requests().len(), 1);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(mem.name_writes().is_empty());
}

#[tokio::test]
async fn config_error_is_not_retried() {
    for answer in [Scripted::denied(), Scripted::validation()] {
        let (mem, client) = named(PROMPT, vec![answer.clone(), Scripted::title(NAME)]);
        let r = name(&mem).await.unwrap();
        assert_eq!(r, seed_result("config_error"), "{answer:?}");
        assert_eq!(client.requests().len(), 1, "a refusal repeats: {answer:?}");
        assert!(mem.name_writes().is_empty());
    }
}

#[tokio::test]
async fn max_tokens_content_filtered_reasoning_only_untagged_give_no_write() {
    let cases = [
        (
            Scripted::recorded("max-tokens-reasoning-only"),
            "stop_max_tokens",
        ),
        (
            Scripted::recorded("content-filtered"),
            "stop_content_filtered",
        ),
        (Scripted::recorded("untagged-lines"), "stop_end_turn"),
        (
            Scripted::Text(format!("Here is a name for the run:\n{NAME}")),
            "untagged",
        ),
        (Scripted::recorded("no-output"), "no_text"),
        (Scripted::title("NONE"), "none"),
    ];
    for (answer, reason) in cases {
        let (mem, client) = named(PROMPT, vec![answer.clone(), Scripted::title(NAME)]);
        let r = name(&mem).await.unwrap();
        assert_eq!(r, seed_result(reason), "{answer:?}");
        assert_eq!(
            client.requests().len(),
            1,
            "{reason}: a refusal repeats, so there is no second attempt"
        );
        assert!(mem.name_writes().is_empty(), "{reason}");
        assert!(mem.search_item().is_none(), "{reason}");
    }
}

#[tokio::test]
async fn a_reply_the_judge_refuses_never_reaches_the_item() {
    // An escape byte, a link and an outcome word the prompt does not hold: refused whole, never cleaned.
    let esc = "<title>Exponential \u{1b}[31mbackoff retry for S3 uploader";
    for (text, reason) in [
        (esc, "control_chars"),
        (
            "<title>Exponential backoff retry for https://x.example/s3 uploader",
            "markup_or_link",
        ),
        (
            "<title>Verified exponential backoff retry for S3 uploader",
            "outcome_word",
        ),
    ] {
        let (mem, client) = named(PROMPT, vec![Scripted::Text(text.into())]);
        let r = name(&mem).await.unwrap();
        assert_eq!(r, seed_result(reason), "{text:?}");
        assert_eq!(client.requests().len(), 1);
        assert!(mem.search_item().is_none(), "{reason}");
    }
}

#[tokio::test]
async fn setting_off_short_prompt_missing_run_make_no_call() {
    // The setting is off (the Lambda has no UTILITY_MODEL): no read, no call.
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, PROMPT, MODEL);
    let r = name(&mem).await.unwrap();
    assert_eq!(r, json!({"named": "off", "reason": "disabled"}));
    assert!(mem.name_calls().is_empty(), "{:?}", mem.name_calls());

    // A prompt of three words: the seed stays and the model is not asked.
    let (mem, client) = named("fix the bug", vec![]);
    let r = name(&mem).await.unwrap();
    assert_eq!(r, seed_result("short_prompt"));
    assert!(client.requests().is_empty());
    assert!(mem.name_writes().is_empty());

    // No run item: nothing to name and nothing written.
    let mem = Mem::default();
    let client = mem.enable_naming(vec![]);
    let r = name(&mem).await.unwrap();
    assert_eq!(r, json!({"named": "skipped", "reason": "no_run"}));
    assert!(client.requests().is_empty() && mem.name_writes().is_empty());

    // A run without its root goal has no prompt to name.
    let (mem, client) = named(PROMPT, vec![]);
    mem.0.goals.lock().unwrap().remove(RUN);
    let r = name(&mem).await.unwrap();
    assert_eq!(r, json!({"named": "skipped", "reason": "no_run"}));
    assert!(client.requests().is_empty() && mem.name_writes().is_empty());
}

#[tokio::test]
async fn store_failure_surfaces_as_an_error_and_model_failure_never_does() {
    // The write fails after a good answer: an error, so Lambda retries the event.
    let (mem, _client) = named(PROMPT, vec![Scripted::title(NAME)]);
    mem.fail(
        "put_run_name",
        RunSessionError::Unavailable("DynamoDB is down".into()),
    );
    let err = name(&mem).await.unwrap_err();
    assert_eq!(err.error_type(), "RunSessionUnavailable");
    assert!(err.to_string().contains("DynamoDB is down"));

    // The read of the stored name fails: an error before any model call.
    let (mem, client) = named(PROMPT, vec![Scripted::title(NAME)]);
    mem.fail(
        "run_name",
        RunSessionError::Unavailable("DynamoDB is down".into()),
    );
    assert_eq!(
        name(&mem).await.unwrap_err().error_type(),
        "RunSessionUnavailable"
    );
    assert!(client.requests().is_empty());

    // Every failure of the model side is an answer, not an error.
    for answers in [
        vec![Scripted::Http(500, Some("InternalServerException")); 2],
        vec![Scripted::denied()],
        vec![Scripted::recorded("content-filtered")],
    ] {
        let (mem, _client) = named(PROMPT, answers);
        assert!(name(&mem).await.is_ok());
    }
}

#[tokio::test]
async fn a_lost_answer_of_the_write_is_followed_by_a_retry_that_finds_the_name() {
    let (mem, client) = named(
        PROMPT,
        vec![
            Scripted::title(NAME),
            Scripted::title("Another name for the same run"),
        ],
    );
    *mem.0.lose_name_response.lock().unwrap() = true;
    let err = name(&mem).await.unwrap_err();
    assert_eq!(err.error_type(), "RunSessionUnavailable");
    assert_eq!(
        mem.search_item().unwrap()["runName"],
        NAME,
        "the write happened"
    );

    // Lambda runs the event again: the name is there, so no second model call and no second write.
    let r = name(&mem).await.unwrap();
    assert_eq!(r, json!({"named": "kept", "reason": "kept"}));
    assert_eq!(client.requests().len(), 1);
    assert_eq!(mem.name_writes().len(), 1);
    assert_eq!(mem.search_item().unwrap()["runName"], NAME);
}

#[tokio::test]
async fn losing_the_race_for_the_name_keeps_the_winners() {
    let (mem, client) = named(PROMPT, vec![Scripted::title(NAME)]);
    // Another invocation of the same event wrote its name between this one's read and write.
    *mem.0.name_race.lock().unwrap() = Some("Retry S3 uploads with exponential backoff".into());
    let r = name(&mem).await.unwrap();
    assert_eq!(r, json!({"named": "kept", "reason": "kept"}));
    assert_eq!(client.requests().len(), 1);
    assert_eq!(
        mem.search_item().unwrap()["runName"],
        "Retry S3 uploads with exponential backoff"
    );
    assert_eq!(
        mem.name_writes().iter().filter(|w| w.stored).count(),
        0,
        "this call stored nothing"
    );
}

#[tokio::test]
async fn the_op_touches_only_the_search_item() {
    let (mem, _client) = named(PROMPT, vec![Scripted::title(NAME)]);
    let run_before = serde_json::to_value(mem.run()).unwrap();
    let goal_before = serde_json::to_value(mem.goal()).unwrap();
    let blobs_before: Vec<String> = mem.0.blobs.lock().unwrap().keys().cloned().collect();
    name(&mem).await.unwrap();
    // The reads, the model call, then the one write.
    assert_eq!(
        mem.name_calls(),
        [
            "get_run",
            "get_root_goal",
            "run_name",
            "converse",
            "put_run_name"
        ]
    );
    assert_eq!(
        serde_json::to_value(mem.run()).unwrap(),
        run_before,
        "the Run item is not written"
    );
    assert_eq!(
        serde_json::to_value(mem.goal()).unwrap(),
        goal_before,
        "the Goal item is not written"
    );
    assert!(
        mem.agent().is_none() && mem.turn().is_none(),
        "no Agent or Turn item"
    );
    assert!(mem.events().is_empty(), "no event");
    assert!(
        mem.index_touches().is_empty(),
        "the end-of-run writer is not called"
    );
    assert!(mem.invocations().is_empty() && mem.stops().is_empty());
    assert!(
        mem.0.minted.lock().unwrap().is_empty(),
        "no worker session credential is minted for a name"
    );
    let blobs_after: Vec<String> = mem.0.blobs.lock().unwrap().keys().cloned().collect();
    assert_eq!(
        blobs_after, blobs_before,
        "nothing is written to the bucket"
    );
    let item = mem.search_item().unwrap();
    let mut keys: Vec<&str> = item.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "createdAt",
            "runId",
            "runName",
            "runNameModel",
            "runNameSource"
        ],
        "the three name attributes and the two key fields of a new item, nothing else"
    );
}

#[tokio::test]
async fn log_has_no_prompt_name_or_reply() {
    let prompt = "Build zebracanary parser module with unit tests and docs";
    let (mem, _client) = named(
        prompt,
        vec![Scripted::title(
            "Build quokkacanary parser module with tests",
        )],
    );
    name(&mem).await.unwrap();
    let logs = mem.name_logs();
    assert_eq!(logs.len(), 1, "one line per op: {logs:?}");
    let line = &logs[0];
    for secret in ["zebracanary", "quokkacanary", "unit tests", "parser module"] {
        assert!(!line.contains(secret), "{secret} is in the log: {line}");
    }
    assert!(
        !has_hex_run(line, 16),
        "no hash of the prompt or the name: {line}"
    );
    let v: Value = serde_json::from_str(line).unwrap();
    let plan = runname::plan(prompt);
    let redacted = runname::redacted_prompt(prompt);
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "ageS",
            "attempts",
            "callMs",
            "excerptChars",
            "function",
            "inputTokens",
            "nameCols",
            "nameWords",
            "op",
            "outcome",
            "outputTokens",
            "promptChars",
            "promptVersion",
            "promptWords",
            "purpose",
            "reason",
            "route",
            "runId",
            "stopReason"
        ]
    );
    assert_eq!(v["function"], "run-session");
    assert_eq!(v["op"], "name-run");
    assert_eq!(v["purpose"], "run-name");
    assert_eq!(v["promptVersion"], runname::PROMPT_VERSION);
    assert_eq!(v["runId"], RUN);
    assert_eq!(v["route"], HAIKU);
    assert_eq!(v["outcome"], "ai");
    assert_eq!(v["reason"], "-");
    assert_eq!(v["stopReason"], "stop_sequence");
    assert_eq!(v["inputTokens"], 454);
    assert_eq!(v["outputTokens"], 22);
    assert_eq!(v["attempts"], 1);
    assert!(v["callMs"].is_u64() && v["ageS"].is_u64(), "{v}");
    assert_eq!(v["promptChars"], prompt.chars().count());
    assert_eq!(v["excerptChars"], plan.user_text.chars().count());
    assert_eq!(v["promptWords"], runname::word_count(&redacted));
    assert_eq!(v["nameWords"], 6);
    assert_eq!(v["nameCols"], 43);
}

#[tokio::test]
async fn the_log_of_a_failure_and_of_a_skip_names_the_reason_and_nothing_of_the_text() {
    let prompt = "Build zebracanary parser module with unit tests and docs";
    let (mem, _client) = named(prompt, vec![Scripted::throttle(), Scripted::throttle()]);
    name(&mem).await.unwrap();
    let line = mem.name_logs().pop().unwrap();
    assert!(
        !line.contains("zebracanary") && !has_hex_run(&line, 16),
        "{line}"
    );
    let v: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["outcome"], "seed");
    assert_eq!(v["reason"], "throttle");
    assert_eq!(v["attempts"], 2);
    assert!(
        v.get("nameWords").is_none()
            && v.get("nameCols").is_none()
            && v.get("stopReason").is_none(),
        "{v}"
    );

    // Off: the line says so and carries no run data but the id.
    let mem = Mem::default();
    mem.seed(RunTarget::S3Only, prompt, MODEL);
    name(&mem).await.unwrap();
    let v: Value = serde_json::from_str(&mem.name_logs().pop().unwrap()).unwrap();
    assert_eq!(v["outcome"], "off");
    assert_eq!(v["reason"], "disabled");
    assert_eq!(v["runId"], RUN);
    assert!(
        v.get("route").is_none() && v.get("promptChars").is_none(),
        "{v}"
    );
}

#[test]
fn age_is_whole_seconds_from_the_run_to_now() {
    let ms = |ts: &str| harness_protocol::common::parse_utc_unix(ts).unwrap() as u64 * 1000;
    assert_eq!(
        age_seconds("2026-10-13T18:00:00Z", ms("2026-10-13T18:00:12Z") + 900),
        Some(12)
    );
    assert_eq!(
        age_seconds("2026-10-13T18:00:00Z", ms("2026-10-13T18:00:00Z")),
        Some(0)
    );
    // A clock behind the run's creation is not a negative age.
    assert_eq!(
        age_seconds("2026-10-13T18:00:10Z", ms("2026-10-13T18:00:00Z")),
        Some(0)
    );
    assert_eq!(age_seconds("not a time", 1), None);
}

/// Whether `s` holds a run of at least `n` hex digits (a digest, whatever its length).
fn has_hex_run(s: &str, n: usize) -> bool {
    let mut run = 0;
    for c in s.chars() {
        if c.is_ascii_hexdigit() {
            run += 1;
            if run >= n {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// A name is display text. Delivery (the pull request, the zip, the branch, the clone folder) never reads it: the only code in
/// `run-session` and `delivery` that names the DynamoDB attribute is the adapter and the op that writes it.
#[test]
fn delivery_code_never_reads_the_name() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&manifest.join("src"), &mut files);
    rust_files(&manifest.join("../delivery/src"), &mut files);
    assert!(files.len() > 10, "{} files found", files.len());
    let mut attribute: Vec<String> = Vec::new();
    let mut accessor: Vec<String> = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        let rel = f
            .strip_prefix(manifest)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if text.contains("runName") {
            attribute.push(rel.clone());
        }
        if text.contains("run_name") {
            accessor.push(rel);
        }
    }
    attribute.sort();
    accessor.sort();
    for f in &attribute {
        assert!(
            f == "src/aws.rs" || f == "src/name_run.rs",
            "{f} names the attribute `runName`"
        );
    }
    assert!(
        attribute.iter().any(|f| f == "src/aws.rs"),
        "the adapter names it: {attribute:?}"
    );
    for f in &accessor {
        assert!(
            ["src/aws.rs", "src/name_run.rs", "src/ports.rs"].contains(&f.as_str()),
            "{f} reads or writes the run name through the store"
        );
    }
}
