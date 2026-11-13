//! `inputWaitS` through `start-session`: the answer window an interactive `ham` asks for travels from
//! the stored request into the invocation body, and a request that asks for none gives the same bytes as before the member
//! existed. The order of a deploy depends on both: an agentd of an older image refuses a body with a member it does not know, so
//! the member must be absent unless the client asked for a window.
#![allow(clippy::unwrap_used)]

mod support;

use harness_protocol::direct::{DirectInvocationRequest, validate_direct_invocation};
use run_session::error::RunSessionError;
use run_session::ops::{Op, handle};
use serde_json::{Value, json};
use support::fakes::*;

/// The longest window the protocol allows (`harness_protocol::direct::MAX_INPUT_WAIT_S`): the longest run, 30 days, pinned here so a change of
/// the limit is a decision, not a side effect.
const MAX_WAIT: u64 = 2_592_000;

#[test]
fn the_protocol_maximum_is_the_one_these_tests_pin() {
    assert_eq!(
        u64::from(harness_protocol::direct::MAX_INPUT_WAIT_S),
        MAX_WAIT
    );
}

/// The body of `start-session` for the request `seed` makes, as run-session sent it before `inputWaitS` existed, with the
/// per-run random nonce replaced by `<nonce>` (`tests/golden/invocation_without_input_wait.json`).
const GOLDEN: &str = include_str!("golden/invocation_without_input_wait.json");

async fn call(mem: &Mem, v: Value) -> Result<Value, RunSessionError> {
    let op: Op = serde_json::from_value(v).unwrap();
    handle(&mem.deps(), &op).await
}

async fn start(mem: &Mem) -> Result<Value, RunSessionError> {
    call(
        mem,
        json!({"op": "start-session", "runId": RUN, "taskToken": "t", "staged": staged()}),
    )
    .await
}

/// `text` with the value of `"<member>":<open> ... <close>` replaced by `"<member>"`: the model rows come from the catalog and are not
/// what these tests are about.
fn hide(text: &str, member: &str, open: char, close: char) -> String {
    let key = format!("\"{member}\":{open}");
    let start = text
        .find(&key)
        .unwrap_or_else(|| panic!("no {member} in {text}"));
    let mut depth = 0;
    let mut end = start;
    for (i, c) in text[start + key.len() - 1..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                end = start + key.len() - 1 + i;
                break;
            }
        }
    }
    format!(
        "{}\"{member}\":\"<{member}>\"{}",
        &text[..start],
        &text[end + 1..]
    )
}

/// The text of the n-th invocation body, byte for byte as the microVM got it, with the random nonce and the catalog rows hidden.
fn sent_text(mem: &Mem, n: usize) -> String {
    let text = String::from_utf8(mem.invocation_bytes(n)).unwrap();
    let nonce = mem.invocations()[n]["nonce"].as_str().unwrap().to_string();
    assert!(!nonce.is_empty());
    let text = text.replace(&nonce, "<nonce>");
    let text = hide(&hide(&text, "failover", '[', ']'), "model", '{', '}');
    hide_string(&hide_string(&text, "packagesToken"), "AccessKeyId")
}

/// `text` with the string value of `"<member>":"..."` replaced by `<member>`: the stand-in package token and access key ID are no secrets,
/// but the secret scan reads a key-shaped value in a committed file as one.
fn hide_string(text: &str, member: &str) -> String {
    let key = format!("\"{member}\":\"");
    let start = text
        .find(&key)
        .unwrap_or_else(|| panic!("no {member} in {text}"))
        + key.len();
    let end = start + text[start..].find('"').unwrap();
    format!("{}<{member}>{}", &text[..start], &text[end..])
}

fn seeded(extra: Value) -> Mem {
    let mem = Mem::default();
    mem.seed_with_members(
        repo_target(),
        "Add percent_change(a, b) with tests",
        MODEL,
        extra,
    );
    mem
}

#[tokio::test]
async fn a_request_without_input_wait_s_gives_the_invocation_bytes_of_before() {
    let mem = seeded(json!({}));
    start(&mem).await.unwrap();
    assert_eq!(sent_text(&mem, 0), GOLDEN.trim_end());
    assert!(
        !sent_text(&mem, 0).contains("inputWaitS"),
        "no member for an agentd that predates it"
    );
}

#[tokio::test]
async fn a_zero_or_null_input_wait_s_in_the_stored_request_is_left_out_of_the_invocation() {
    for value in [json!(0), json!(null)] {
        let mem = seeded(json!({ "inputWaitS": value }));
        start(&mem).await.unwrap();
        let text = sent_text(&mem, 0);
        assert!(
            !text.contains("inputWaitS"),
            "inputWaitS {value} reached the microVM: {text}"
        );
    }
}

#[tokio::test]
async fn the_input_wait_of_the_stored_request_is_copied_into_the_invocation() {
    for s in [1u64, 60, 1800, MAX_WAIT] {
        let mem = seeded(json!({ "inputWaitS": s }));
        start(&mem).await.unwrap();
        let sent = mem.invocations();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["inputWaitS"], json!(s), "{}", sent[0]);
        // The body is otherwise what a run without a window gets, apart from the digest of the request object it names.
        let mut got: Value = serde_json::from_str(&sent_text(&mem, 0)).unwrap();
        let got = got.as_object_mut().unwrap();
        got.remove("inputWaitS");
        got.remove("request");
        let mut want: Value = serde_json::from_str(GOLDEN).unwrap();
        let want = want.as_object_mut().unwrap();
        want.remove("request");
        assert_eq!(got, want, "inputWaitS {s}");
        let req: DirectInvocationRequest = serde_json::from_value(sent[0].clone()).unwrap();
        validate_direct_invocation(&req).unwrap();
        assert_eq!(req.input_wait_s, Some(u32::try_from(s).unwrap()));
    }
}

#[tokio::test]
async fn the_request_object_digest_in_the_invocation_names_the_stored_object_with_the_window() {
    let mem = seeded(json!({ "inputWaitS": 1800 }));
    start(&mem).await.unwrap();
    let stored = mem.blob(&format!("runs/{RUN}/input/request.json")).unwrap();
    assert!(
        String::from_utf8(stored.clone())
            .unwrap()
            .contains("\"inputWaitS\":1800")
    );
    assert_eq!(mem.invocations()[0]["request"]["sha256"], hex(&stored));
}

#[tokio::test]
async fn an_input_wait_over_the_maximum_ends_the_start_as_invalid_and_invokes_nothing() {
    for s in [MAX_WAIT + 1, 3_000_000] {
        let mem = seeded(json!({ "inputWaitS": s }));
        let e = start(&mem).await.unwrap_err();
        assert_eq!(e.error_type(), "RunSessionInvalid", "{s}: {e}");
        assert!(e.to_string().starts_with("invocation:"), "{e}");
        assert!(mem.invocations().is_empty(), "inputWaitS {s} was invoked");
        assert!(
            mem.turn().is_none_or(|t| t.invoked_at.is_none()),
            "the turn must not be marked invoked"
        );
    }
}

#[tokio::test]
async fn prepare_reads_a_stored_request_that_carries_the_window() {
    let mem = seeded(json!({ "inputWaitS": 1800 }));
    let out = call(&mem, json!({"op": "prepare", "runId": RUN}))
        .await
        .unwrap();
    assert_eq!(out["model"]["id"], MODEL);
}
