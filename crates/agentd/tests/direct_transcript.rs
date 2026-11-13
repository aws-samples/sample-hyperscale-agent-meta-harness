//! The transcript module in a whole run: the standard module set against the fake
//! AWS and the fake ACP engine. The conversation lands in chunk objects the route's reader accepts, `turn-complete` carries the
//! summary, the capability event is posted once per session, a failing store changes nothing about the run, and the session secrets
//! reach no stored byte. The fake bucket honours `If-None-Match` and lists, as S3 does.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;
mod direct_transcript_support;

use agentd::direct::hooks::HookEnv;
use agentd::direct::transcript::settings::Settings;
use agentd::events::{EventRecorder, TurnControls};
use direct_support::{Answer, BUCKET, Direct, Opts, PROMPT, RUN};
use direct_transcript_support::{gunzip, read_chunk, seq_of};
use harness_protocol::direct::InstructAction;
use harness_protocol::envelope::{EventType, Payload};
use harness_protocol::runtime::{InvocationResponse, TurnOutcome};
use harness_protocol::transcript::CompletenessKind;
use serde_json::Value;
use std::time::Duration;

async fn run(opts: Opts) -> (Direct, InvocationResponse) {
    let d = Direct::start(opts).await;
    let (_, resp) = d.invoke().await;
    (d, resp)
}

/// The transcript objects of the run: `(seq, bytes)`, in key order.
fn objects(d: &Direct) -> Vec<(u64, Vec<u8>)> {
    let mut out: Vec<(u64, Vec<u8>)> = d
        .aws
        .keys()
        .iter()
        .filter(|k| k.contains("/transcript/"))
        .map(|k| {
            let key = k.strip_prefix(&format!("{BUCKET}/")).unwrap();
            (
                seq_of(key).unwrap_or_else(|| panic!("{key} is not a chunk name")),
                d.aws.get(key).unwrap(),
            )
        })
        .collect();
    out.sort_by_key(|o| o.0);
    // With HAMH_TRANSCRIPT_DUMP_DIR set the objects are also written there, for the reader of another crate.
    if let Some(dir) = std::env::var_os("HAMH_TRANSCRIPT_DUMP_DIR") {
        let test = std::thread::current()
            .name()
            .unwrap_or("test")
            .replace("::", "-");
        for (seq, bytes) in &out {
            let _ = std::fs::write(
                std::path::Path::new(&dir).join(format!("{test}-{seq:012}.ndjson.gz")),
                bytes,
            );
        }
    }
    out
}

fn records(d: &Direct) -> Vec<Value> {
    objects(d)
        .iter()
        .flat_map(|(seq, b)| {
            read_chunk(b, *seq)
                .unwrap_or_else(|e| panic!("object {seq}: {e}"))
                .1
        })
        .collect()
}

fn kinds(recs: &[Value]) -> Vec<String> {
    recs.iter()
        .map(|r| r["kind"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_run_leaves_its_conversation_in_chunks_the_route_can_read() {
    let (d, resp) = run(Opts::default()).await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let objs = objects(&d);
    assert!(objs.len() >= 2, "{} objects", objs.len());
    assert_eq!(
        objs.iter().map(|o| o.0).collect::<Vec<_>>(),
        (0..objs.len() as u64).collect::<Vec<_>>()
    );
    let recs = records(&d);
    let k = kinds(&recs);
    for want in [
        "opaque",
        "session",
        "prompt",
        "tool_call",
        "agent_message_chunk",
        "usage_update",
        "prompt_result",
        "end",
    ] {
        assert!(
            k.contains(&want.to_string()),
            "{want} is missing from {k:?}"
        );
    }
    let ns: Vec<u64> = recs.iter().map(|r| r["n"].as_u64().unwrap()).collect();
    assert_eq!(
        ns,
        (0..ns.len() as u64).collect::<Vec<_>>(),
        "numbered from 0 without a gap"
    );
    let prompt = recs.iter().find(|r| r["kind"] == "prompt").unwrap();
    assert!(
        prompt["data"]["prompt"][0]["text"]
            .as_str()
            .unwrap()
            .contains("percent_change"),
        "{prompt}"
    );
    assert_eq!(
        prompt["data"]["prompt"][0]["text"].as_str().unwrap(),
        PROMPT.trim_end_matches('\n').to_string() + "\n",
        "typed as is"
    );
    let said = recs
        .iter()
        .find(|r| r["kind"] == "agent_message_chunk")
        .unwrap();
    assert_eq!(
        said["data"]["content"]["text"],
        "Added percent_change with tests."
    );
    let result = recs.iter().find(|r| r["kind"] == "prompt_result").unwrap();
    assert_eq!(result["data"]["result"]["stopReason"], "end_turn");
    // the last object says it is closed; the others do not
    let (last_seq, last) = objs.last().unwrap();
    assert_eq!(read_chunk(last, *last_seq).unwrap().0["closed"], true);
    assert!(
        objs[..objs.len() - 1].iter().all(|(s, b)| read_chunk(b, *s)
            .unwrap()
            .0
            .get("closed")
            .is_none())
    );
    // turn-complete carries the summary: the last stored number and no loss
    let complete = d.aws.complete();
    let t = complete
        .last()
        .unwrap()
        .transcript
        .clone()
        .expect("turn-complete carries the summary");
    assert_eq!(t.last_transcript_seq, Some(*last_seq));
    assert!(t.is_lossless(), "{t:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_capability_event_is_posted_once_per_session_by_the_agent() {
    let (d, _) = run(Opts::default()).await;
    let caps: Vec<_> = d
        .aws
        .envelopes()
        .into_iter()
        .filter(|e| e.event_type == EventType::SessionCapabilities)
        .collect();
    assert_eq!(caps.len(), 1);
    assert_eq!(
        caps[0].authority,
        harness_protocol::envelope::Authority::Agent
    );
    harness_protocol::envelope::validate(&caps[0]).unwrap();
    let harness_protocol::envelope::Payload::Inline { value } = &caps[0].payload else {
        panic!("inline")
    };
    assert!(serde_json::to_vec(value).unwrap().len() <= 4096);
    assert_eq!(value["v"], 1);
    assert_eq!(value["engine"], "Cline");
    assert_eq!(value["session"], 1);
    assert_eq!(value["sessionId"], "fake-session-0001");
    assert_eq!(value["agent"]["name"], "fake-acp");
    assert_eq!(
        value["harness"]["features"],
        serde_json::json!(["transcript.v1"])
    );
    assert_eq!(
        value["harness"]["controls"],
        serde_json::json!(["message.next", "message.now"])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_session_gets_its_own_capability_event_and_a_session_record() {
    // the question-and-fresh-session run of the golden streams: the user answers with a new session
    let d = Direct::start(Opts {
        scenario: "stuck_engine_stop_then_done",
        engine_env: vec![
            ("FAKE_ACP_NEW_SESSION_SCENARIO", "fresh_session_done".into()),
            ("FAKE_ACP_RESTART_SCENARIO", "fresh_session_done".into()),
        ],
        input_wait_s: Some(30),
        edit_cfg: Some(Box::new(|c: &mut agentd::config::Config| {
            c.direct.provider_retries = 3;
            c.direct.engine_stop_resumes = 1;
            c.direct.provider_retry_pause = Duration::from_millis(20);
            c.direct.failover_wait = Duration::from_millis(100);
            c.direct.failover_wait_cap = Duration::from_millis(200);
            c.direct.failover_probe = false;
            c.direct.input_margin = Duration::ZERO;
            c.direct.input_min_window = Duration::from_secs(1);
        })),
        ..Opts::default()
    })
    .await;
    d.aws.instruct_on(
        "needs-input",
        Answer::new(InstructAction::NewSessionWithPrompt).text("Poll the log file instead."),
    );
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let sessions: Vec<u64> = d
        .aws
        .envelopes()
        .into_iter()
        .filter(|e| e.event_type == EventType::SessionCapabilities)
        .map(|e| match e.payload {
            harness_protocol::envelope::Payload::Inline { value } => {
                value["session"].as_u64().unwrap()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(sessions, [1, 2]);
    let recs = records(&d);
    assert_eq!(
        recs.iter().filter(|r| r["kind"] == "session").count(),
        2,
        "{:?}",
        kinds(&recs)
    );
    // the record numbers run on across the two sessions
    let ns: Vec<u64> = recs.iter().map(|r| r["n"].as_u64().unwrap()).collect();
    assert_eq!(ns, (0..ns.len() as u64).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_secrets_reach_no_stored_byte() {
    let (d, resp) = run(Opts {
        scenario: "leaky",
        ..Opts::default()
    })
    .await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let mut all = String::new();
    for (seq, b) in objects(&d) {
        all.push_str(&String::from_utf8(gunzip(&b, 4 << 20).unwrap()).unwrap());
        read_chunk(&b, seq).unwrap();
    }
    for secret in [
        common::CANARY_SECRET,
        common::CANARY_TOKEN,
        &common::canary_key_id(),
    ] {
        assert!(!all.contains(secret), "{secret} is in a stored object");
    }
    assert!(
        all.contains("<redacted>"),
        "the message that carried them is there, redacted"
    );
    assert!(all.contains("the key is"), "{all}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bucket_that_refuses_every_transcript_write_does_not_change_the_run() {
    let d = Direct::start(Opts::default()).await;
    d.aws.fail_puts("/transcript/", u32::MAX);
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    assert_eq!(
        d.summary().outcome,
        harness_protocol::direct::DirectOutcome::ReportedDone
    );
    assert!(objects(&d).is_empty(), "nothing was stored");
    let t = d
        .aws
        .complete()
        .last()
        .unwrap()
        .transcript
        .clone()
        .expect("the summary is still delivered");
    assert_eq!(t.last_transcript_seq, None);
    let failed = t
        .entries
        .iter()
        .find(|e| e.kind == CompletenessKind::Failed)
        .expect("a failed entry");
    assert!(failed.count.unwrap() >= 5, "{t:?}");
    assert!(!t.is_lossless());
    // the rest of what a run leaves is there
    assert!(d.bundle_file().is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_module_registers_unless_it_is_switched_off() {
    let d = Direct::start(Opts::default()).await;
    let rec = EventRecorder::new(RUN, "direct");
    let (controls, _) = TurnControls::new();
    let env = HookEnv::new(&d.app, &d.req, &rec, &controls);
    let on = agentd::direct::transcript::hook_with_settings(&env, Settings::default());
    assert_eq!(on.map(|h| h.name()), Some("transcript"));
    let off = agentd::direct::transcript::hook_with_settings(
        &env,
        Settings {
            enabled: false,
            ..Settings::default()
        },
    );
    assert!(
        off.is_none(),
        "switched off: nothing registered, the run is as it was"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_the_engine_sends_outside_a_prompt_get_turn_id_null_and_are_counted() {
    // The engine gives up, then sends three more updates after the answer of prompt 1, while the run waits for the user's answer: a usage
    // figure, a word of text and an update of a tool call nobody knows (the last one is the mark: it makes agentd post a progress event, and
    // the pipe is read in order, so the plane has seen that event only once agentd has read the two lines before it). The user answers when
    // the plane has seen it, and prompt 2 finishes the work. The three late updates belong to no open prompt.
    let d = Direct::start(Opts {
        scenario: "updates_outside_a_prompt",
        // The engine writes its late updates 300 ms apart, as a loaded host spreads them: the order below must not depend on the speed
        // of the machine (it failed in 21 of 300 runs on a builder under 100 busy processes).
        engine_env: vec![("FAKE_ACP_LATE_GAP_MS", "300".to_string())],
        input_wait_s: Some(30),
        edit_cfg: Some(Box::new(|c: &mut agentd::config::Config| {
            c.direct.provider_retry_pause = Duration::from_millis(20);
            c.direct.input_margin = Duration::ZERO;
            c.direct.input_min_window = Duration::from_secs(1);
        })),
        ..Opts::default()
    })
    .await;
    // The answer waits for the mark, not for a pause: an answer that came before agentd had read the engine's late lines would open
    // prompt 2 first, and the lines would be read, and stored, as part of it (21 of 300 runs on a builder under 100 busy processes).
    d.aws.instruct_after_event(
        |e| {
            e.event_type == EventType::TurnProgress
                && matches!(&e.payload, Payload::Inline { value } if value["toolCallId"] == "late-sentinel")
        },
        "needs-input",
        Answer::new(InstructAction::ContinueWithPrompt).text("Add the function and fix the test."),
    );
    let (_, resp) = d.invoke().await;
    assert_eq!(resp.outcome, TurnOutcome::Completed, "{resp:?}");
    let recs = records(&d);
    let prompts: Vec<&Value> = recs.iter().filter(|r| r["kind"] == "prompt").collect();
    assert_eq!(prompts.len(), 2, "{:?}", kinds(&recs));
    let (first, second) = (
        prompts[0]["data"]["id"].clone(),
        prompts[1]["data"]["id"].clone(),
    );
    assert!(first.is_number() && second.is_number() && first != second);
    // every record names the prompt that was open when agentd wrote or read it
    let late: Vec<&Value> = recs
        .iter()
        .filter(|r| {
            r["kind"] == "usage_update" && r["data"]["usage"]["inputTokens"] == 410
                || r["data"]["content"]["text"] == "late word"
                || r["data"]["toolCallId"] == "late-sentinel"
        })
        .collect();
    assert_eq!(late.len(), 3, "{:?}", kinds(&recs));
    for r in &late {
        assert_eq!(r["turn_id"], Value::Null, "{r}");
    }
    for r in recs.iter().filter(|r| {
        !late.contains(r)
            && r["dir"] == "in"
            && matches!(
                r["kind"].as_str(),
                Some("tool_call" | "agent_message_chunk" | "prompt_result")
            )
    }) {
        assert!(r["turn_id"] == first || r["turn_id"] == second, "{r}");
    }
    assert_eq!(prompts[0]["turn_id"], first);
    assert_eq!(prompts[1]["turn_id"], second);
    let answers: Vec<&Value> = recs
        .iter()
        .filter(|r| r["kind"] == "prompt_result")
        .collect();
    assert_eq!(
        (answers[0]["turn_id"].clone(), answers[1]["turn_id"].clone()),
        (first, second)
    );
    let end = recs.last().unwrap();
    assert_eq!(end["kind"], "end");
    assert_eq!(end["data"]["unattributed"], 3, "{end}");
    let t = d.aws.complete().last().unwrap().transcript.clone().unwrap();
    assert!(t.is_lossless(), "stored, not lost: {t:?}");
}
