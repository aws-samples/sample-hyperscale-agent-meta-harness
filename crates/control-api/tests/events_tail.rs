//! `GET /v1/runs/{id}/events` from the end: `tail=N` returns the newest N events of one shard, `before=<eventId>` pages
//! backwards, and the user's answer text rides with the question it answers. Every read is owner-only; another principal's run is a
//! 404. The response is the `ham-core` superset of the old page, so an old client reads it unchanged.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{
    ME, OTHER, World, agent_item, answer_control, get, get_logged, message, question, run_item,
    ulid,
};
use ham_core::events_view::{EVENTS_BYTE_BUDGET, MAX_ANSWERS, MAX_TAIL};
use harness_protocol::api::v1::EventsPage;
use harness_protocol::direct_input::InstructOrigin;
use harness_protocol::envelope::Authority;
use harness_protocol::store::event_shard;
use serde_json::{Value, json};

const RUN: &str = "01K5ZQ4T8M3N6P7R9S0V1W2X3Y";

fn shard() -> String {
    event_shard(Authority::Agent, Some("direct")).to_string()
}

fn events_path() -> String {
    format!("/v1/runs/{RUN}/events")
}

/// A run of `ME` with `n` message events (ids `ulid(1)` to `ulid(n)`) in the direct agent's shard.
fn world_with(n: u64) -> World {
    let w = World::new();
    w.add_run(run_item(
        RUN,
        ME,
        "active",
        "2026-10-15T13:00:00Z",
        json!({"mode": "s3-only"}),
    ));
    w.add_events((1..=n).map(|i| {
        message(
            RUN,
            &ulid(i),
            "2026-10-15T13:01:00Z",
            &format!("message {i}"),
        )
    }));
    w
}

fn ids(body: &Value) -> Vec<String> {
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["eventId"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn the_last_fifty_events_come_in_one_call_oldest_first() {
    let w = world_with(120);
    let s = shard();
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "50")]).await;
    assert_eq!(status, 200, "{body}");
    let got = ids(&body);
    assert_eq!(got.len(), 50);
    assert_eq!(got, (71..=120).map(ulid).collect::<Vec<_>>());
    assert_eq!(body["hasMore"], true);
    assert_eq!(body["nextBefore"], ulid(71));
    assert_eq!(body["nextAfter"], ulid(120));
    // One read of the table gave it: the page asks for one more than it shows, to know there is more.
    assert_eq!(w.count("events_before"), 1);
}

#[tokio::test]
async fn before_pages_back_without_a_gap_or_a_repeat() {
    let w = world_with(120);
    let s = shard();
    let mut all: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut q = vec![("shard", s.as_str()), ("tail", "50")];
        if let Some(c) = &cursor {
            q.push(("before", c.as_str()));
        }
        let (status, body) = get(&w, ME, &events_path(), &q).await;
        assert_eq!(status, 200, "{body}");
        let mut page = ids(&body);
        page.extend(all);
        all = page;
        pages += 1;
        if body["hasMore"] == true {
            cursor = Some(body["nextBefore"].as_str().unwrap().to_string());
        } else {
            assert!(body.get("nextBefore").is_none(), "{body}");
            break;
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(all, (1..=120).map(ulid).collect::<Vec<_>>());
}

#[tokio::test]
async fn before_alone_asks_for_the_default_page() {
    let w = world_with(120);
    let s = shard();
    let b = ulid(100);
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("before", &b)]).await;
    assert_eq!(status, 200);
    assert_eq!(ids(&body), (50..=99).map(ulid).collect::<Vec<_>>());
    assert_eq!(body["hasMore"], true);
}

#[tokio::test]
async fn a_short_log_has_no_more_and_an_empty_shard_has_no_cursors() {
    let w = world_with(10);
    let s = shard();
    let (_, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "50")]).await;
    assert_eq!(ids(&body).len(), 10);
    assert!(body.get("hasMore").is_none(), "{body}");
    assert!(body.get("nextBefore").is_none());
    assert_eq!(body["nextAfter"], ulid(10));
    // Another shard of the run holds nothing.
    let (status, body) = get(&w, ME, &events_path(), &[("shard", "3"), ("tail", "50")]).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"events": []}));
}

#[tokio::test]
async fn the_biggest_tail_is_five_hundred_and_an_exact_fit_has_no_more() {
    let w = world_with(MAX_TAIL as u64);
    let s = shard();
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "500")]).await;
    assert_eq!(status, 200);
    assert_eq!(ids(&body).len(), MAX_TAIL);
    assert!(
        body.get("hasMore").is_none(),
        "an exact fit has nothing older: {body}"
    );
}

#[tokio::test]
async fn bad_parameters_are_a_400_before_any_read() {
    let w = world_with(5);
    let s = shard();
    let b = ulid(3);
    let cases: Vec<Vec<(&str, &str)>> = vec![
        vec![("tail", "50")],
        vec![("shard", "32"), ("tail", "50")],
        vec![("shard", &s), ("tail", "0")],
        vec![("shard", &s), ("tail", "501")],
        vec![("shard", &s), ("tail", "ten")],
        vec![("shard", &s), ("before", "not-an-id")],
        vec![("shard", &s), ("after", &b), ("tail", "5")],
        vec![("shard", &s), ("after", &b), ("before", &b)],
    ];
    for q in cases {
        let (status, body) = get(&w, ME, &events_path(), &q).await;
        assert_eq!(status, 400, "{q:?}: {body}");
        assert_eq!(body["code"], "bad-request");
    }
    assert_eq!(w.count("events"), 0, "no table read for a bad request");
}

#[tokio::test]
async fn only_the_owner_reads_and_the_answer_is_a_404_not_a_refusal() {
    let w = world_with(5);
    let s = shard();
    let q = [("shard", s.as_str()), ("tail", "50")];
    let (status, body) = get(&w, OTHER, &events_path(), &q).await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["code"], "not-found");
    let b = ulid(3);
    let (status, _) = get(&w, OTHER, &events_path(), &[("shard", &s), ("before", &b)]).await;
    assert_eq!(status, 404);
    // A run that does not exist reads the same as a run that is not yours.
    let (status, body2) = get(&w, ME, "/v1/runs/01K5ZQ4T8M3N6P7R9S0V1W2X4Z/events", &q).await;
    assert_eq!(status, 404);
    assert_eq!(body2["code"], "not-found");
    assert!(
        body["message"].as_str().unwrap().starts_with("no run "),
        "{body}"
    );
    assert_eq!(
        w.count("events_before"),
        0,
        "nothing was read for a stranger"
    );
    let (status, _) = get(&w, ME, "/v1/runs/not-a-run/events", &q).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn a_plain_forward_read_is_the_old_page_byte_for_byte() {
    let w = world_with(5);
    let s = shard();
    let (out, _) = control_api::handler::handle_logged(
        chat_support::request(ME, "GET", &events_path(), &[("shard", &s)], ""),
        &chat_support::deps(&w),
    )
    .await;
    assert_eq!(out.status, 200);
    let page: EventsPage = serde_json::from_str(&out.body).unwrap();
    assert_eq!(page.events.len(), 5);
    assert_eq!(out.body, serde_json::to_string(&page).unwrap());
    // And a tail page parses as the old page too.
    let (out, _) = control_api::handler::handle_logged(
        chat_support::request(
            ME,
            "GET",
            &events_path(),
            &[("shard", &s), ("tail", "2")],
            "",
        ),
        &chat_support::deps(&w),
    )
    .await;
    let old: EventsPage = serde_json::from_str(&out.body).unwrap();
    assert_eq!(old.events.len(), 2);
    assert_eq!(old.next_after.as_deref(), Some(ulid(5).as_str()));
}

fn world_with_answers() -> World {
    let w = world_with(0);
    w.agents
        .lock()
        .unwrap()
        .push(agent_item(RUN, "m", "running"));
    // q1 answered by the user, q2 left open, a message between.
    w.add_events([
        message(RUN, &ulid(1), "2026-10-15T13:01:00Z", "working"),
        question(RUN, &ulid(2), "2026-10-15T13:02:00Z"),
        message(RUN, &ulid(3), "2026-10-15T13:03:00Z", "continuing"),
        question(RUN, &ulid(4), "2026-10-15T13:04:00Z"),
    ]);
    w.controls.lock().unwrap().push(answer_control(
        RUN,
        &ulid(2),
        Some("Poll the log file instead."),
        InstructOrigin::User,
    ));
    w
}

#[tokio::test]
async fn the_users_answer_text_rides_with_its_question() {
    let w = world_with_answers();
    let s = shard();
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "50")]).await;
    assert_eq!(status, 200, "{body}");
    let answers = body["answers"].as_array().unwrap();
    assert_eq!(answers.len(), 1, "{body}");
    assert_eq!(answers[0]["requestId"], ulid(2));
    assert_eq!(answers[0]["action"], "continue_with_prompt");
    assert_eq!(answers[0]["text"], "Poll the log file instead.");
    assert_eq!(answers[0]["createdAt"], "2026-10-15T13:42:00Z");
    // One Query of the run's controls answers for every question on the page.
    assert_eq!(w.count("list_controls"), 1);
    assert_eq!(w.count("get_control"), 0);
    // The same answers come with a forward read that holds the question.
    let (_, body) = get(&w, ME, &events_path(), &[("shard", &s)]).await;
    assert_eq!(body["answers"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_page_without_a_question_reads_no_control() {
    let w = world_with(30);
    let s = shard();
    let (_, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "50")]).await;
    assert!(body.get("answers").is_none(), "{body}");
    assert_eq!(w.count("list_controls"), 0);
}

#[tokio::test]
async fn only_a_users_instruct_is_an_answer() {
    let w = world_with_answers();
    {
        let mut c = w.controls.lock().unwrap();
        c.clear();
        // The analyzer's answer and a cancel never come back as the user's words.
        c.push(answer_control(
            RUN,
            &ulid(2),
            Some("analyzer text"),
            InstructOrigin::Analyzer,
        ));
        c.push(harness_protocol::store::ControlItem {
            run_id: RUN.into(),
            agent_id: "direct".into(),
            control_id: ulid(4),
            control: harness_protocol::api::internal::PendingControl::Cancel,
            created_at: "2026-10-15T13:05:00Z".into(),
            delivered_at: None,
        });
    }
    let s = shard();
    let (_, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "50")]).await;
    assert!(body.get("answers").is_none(), "{body}");
}

#[tokio::test]
async fn an_event_that_only_looks_like_a_question_is_not_looked_up() {
    let w = world_with(0);
    // Not blocking, a request id that is not the event's own, and a question of another agent.
    let mut not_blocking = question(RUN, &ulid(1), "2026-10-15T13:01:00Z");
    if let harness_protocol::envelope::Payload::Inline { value } = &mut not_blocking.payload {
        value["blocking"] = json!(false);
    }
    let mut foreign_id = question(RUN, &ulid(2), "2026-10-15T13:02:00Z");
    if let harness_protocol::envelope::Payload::Inline { value } = &mut foreign_id.payload {
        value["requestId"] = json!(ulid(9));
    }
    w.add_events([not_blocking, foreign_id]);
    let s = shard();
    let (status, _) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "50")]).await;
    assert_eq!(status, 200);
    assert_eq!(w.count("list_controls"), 0);
}

#[tokio::test]
async fn the_answers_read_is_capped_and_a_cut_list_says_so() {
    let w = world_with(0);
    let total = MAX_ANSWERS as u64 + 10;
    w.add_events((1..=total).map(|i| question(RUN, &ulid(i), "2026-10-15T13:02:00Z")));
    w.controls.lock().unwrap().extend((1..=total).map(|i| {
        answer_control(
            RUN,
            &ulid(i),
            Some(&format!("answer {i}")),
            InstructOrigin::User,
        )
    }));
    let s = shard();
    let (status, body, _) =
        get_logged(&w, ME, &events_path(), &[("shard", &s), ("tail", "500")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["events"].as_array().unwrap().len() as u64, total);
    // An answer that is missing here is not an answer that was never given: the page says its list was cut.
    assert_eq!(body["answers"].as_array().unwrap().len(), MAX_ANSWERS);
    assert_eq!(body["answersTruncated"], true);
    assert_eq!(w.count("list_controls"), 1);
}

#[tokio::test]
async fn the_newest_questions_of_a_long_run_get_their_answers() {
    // 300 answered questions: the tail page shows the newest 10, and the read of controls is the window of those 10, not the first 200.
    let w = world_with(0);
    w.add_events((1..=300).map(|i| question(RUN, &ulid(i), "2026-10-15T13:02:00Z")));
    w.controls.lock().unwrap().extend((1..=300).map(|i| {
        answer_control(
            RUN,
            &ulid(i),
            Some(&format!("answer {i}")),
            InstructOrigin::User,
        )
    }));
    let s = shard();
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "10")]).await;
    assert_eq!(status, 200);
    let answers = body["answers"].as_array().unwrap();
    assert_eq!(answers.len(), 10, "{body}");
    assert_eq!(answers[0]["text"], "answer 291");
    assert_eq!(answers[9]["text"], "answer 300");
    assert!(body.get("answersTruncated").is_none(), "{body}");
    let calls = w.calls.lock().unwrap().clone();
    let read = calls
        .iter()
        .find(|c| c.starts_with("list_controls"))
        .unwrap();
    assert!(
        read.ends_with(&format!("{} {}", ulid(291), ulid(300))),
        "{read}"
    );
}

#[tokio::test]
async fn a_page_dense_in_quotes_stays_under_the_lambda_response_limit_after_the_second_escaping() {
    let w = world_with(0);
    // 60 events whose text is 100,000 quotation marks: 2 bytes each in the event's JSON, 4 once the body is a string in the proxy answer.
    let quotes = "\"".repeat(100_000);
    w.add_events((1..=60).map(|i| message(RUN, &ulid(i), "2026-10-15T13:01:00Z", &quotes)));
    let s = shard();
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "60")]).await;
    assert_eq!(status, 200);
    let proxy_answer = serde_json::to_string(&serde_json::to_string(&body).unwrap()).unwrap();
    assert!(proxy_answer.len() < 6_000_000, "{}", proxy_answer.len());
    assert_eq!(body["hasMore"], true);
    assert!(ids(&body).len() >= 5, "a page still carries events");
}

#[tokio::test]
async fn long_answers_are_cut_at_their_byte_budget_and_the_cut_is_said() {
    let w = world_with(0);
    // 60 answers of 32,000 bytes (the longest a person may send): 1.9 MB, over the 1 MB budget of answers.
    let long = "a".repeat(32_000);
    w.add_events((1..=60).map(|i| question(RUN, &ulid(i), "2026-10-15T13:02:00Z")));
    w.controls
        .lock()
        .unwrap()
        .extend((1..=60).map(|i| answer_control(RUN, &ulid(i), Some(&long), InstructOrigin::User)));
    let s = shard();
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "100")]).await;
    assert_eq!(status, 200);
    let n = body["answers"].as_array().unwrap().len();
    assert!(n > 20 && n < 60, "{n}");
    assert_eq!(body["answersTruncated"], true);
    // Events and answers together stay under what a Lambda may return.
    assert!(serde_json::to_string(&body).unwrap().len() < 5_600_000);
}

#[tokio::test]
async fn a_page_over_the_byte_budget_is_cut_at_a_whole_event_and_says_there_is_more() {
    let w = world_with(0);
    // 60 events of about 100 KB each: 6 MB, over the budget of 4.5 MB.
    let big = "x".repeat(100_000);
    w.add_events((1..=60).map(|i| message(RUN, &ulid(i), "2026-10-15T13:01:00Z", &big)));
    let s = shard();
    let (status, body) = get(&w, ME, &events_path(), &[("shard", &s), ("tail", "60")]).await;
    assert_eq!(status, 200);
    let got = ids(&body);
    assert!(got.len() < 60 && got.len() > 30, "{}", got.len());
    // The newest events stay; the oldest are left for the next page, and the cursor starts at the oldest one returned.
    assert_eq!(got.last().unwrap(), &ulid(60));
    assert_eq!(body["hasMore"], true);
    assert_eq!(body["nextBefore"], got[0]);
    assert!(serde_json::to_string(&body).unwrap().len() <= EVENTS_BYTE_BUDGET + 4096);
    // Paging back from the cursor reaches the rest without a gap.
    let (_, older) = get(
        &w,
        ME,
        &events_path(),
        &[("shard", &s), ("tail", "60"), ("before", &got[0])],
    )
    .await;
    let mut all = ids(&older);
    all.extend(got);
    assert_eq!(all, (1..=60).map(ulid).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_forward_page_over_the_byte_budget_is_cut_at_the_end_and_the_cursor_follows() {
    let w = world_with(0);
    let big = "x".repeat(100_000);
    w.add_events((1..=60).map(|i| message(RUN, &ulid(i), "2026-10-15T13:01:00Z", &big)));
    let s = shard();
    let (_, body) = get(&w, ME, &events_path(), &[("shard", &s)]).await;
    let got = ids(&body);
    assert!(got.len() < 60);
    assert_eq!(got[0], ulid(1));
    assert_eq!(body["nextAfter"], got.last().unwrap().as_str());
    let (_, next) = get(
        &w,
        ME,
        &events_path(),
        &[("shard", &s), ("after", got.last().unwrap())],
    )
    .await;
    assert_eq!(ids(&next)[0], ulid(got.len() as u64 + 1));
}

#[tokio::test]
async fn a_failed_answer_read_never_hides_the_events() {
    let w = world_with_answers();
    w.control_read_down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let s = shard();
    let (status, body, logs) =
        get_logged(&w, ME, &events_path(), &[("shard", &s), ("tail", "50")]).await;
    assert_eq!(status, 200);
    assert_eq!(body["events"].as_array().unwrap().len(), 4);
    assert!(body.get("answers").is_none());
    assert!(logs.iter().any(|l| l.contains("answer")), "{logs:?}");
    // The failure text is the store's, not the user's words.
    assert!(logs.iter().all(|l| !l.contains("Poll the log")), "{logs:?}");
}

#[tokio::test]
async fn the_access_line_names_the_path_and_never_the_cursor_or_the_answers() {
    let s = shard();
    let b = ulid(4);
    let req = chat_support::request(
        ME,
        "GET",
        &events_path(),
        &[("shard", &s), ("tail", "50"), ("before", &b)],
        "",
    );
    let line = control_api::lambda::access_line(&req, 200);
    assert!(!line.contains(&b), "{line}");
    assert!(!line.contains("before"), "{line}");
    assert!(line.contains("/events"), "{line}");
}
