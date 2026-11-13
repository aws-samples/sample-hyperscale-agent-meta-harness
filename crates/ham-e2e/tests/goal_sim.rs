//! The goal-loop emulation of the fake ACP engine, driven in process through a recording
//! `SimIo`: how a scenario's `goal.json` is read, how the first prompt claims the goal, how the engine queues its own follow-up rounds, how it
//! answers a completion call, a client prompt during a round and a prompt on a dead loop, and what the observer writes. The same emulation
//! runs inside the `fake-acp-engine` binary (`goal_sim_engine.rs`); here every branch is reached without a process.
#![allow(clippy::unwrap_used)]

use ham_e2e::goal_sim::{Flow, GoalScript, GoalSim, Recv, SimIo};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

/// A recording stand-in for the engine's stdio: what is emitted, what is logged, and the input lines handed out when a round waits.
#[derive(Default)]
struct Mock {
    sent: Vec<Value>,
    logs: Vec<String>,
    input: VecDeque<Recv>,
}

impl SimIo for Mock {
    fn emit(&mut self, msg: Value) {
        self.sent.push(msg);
    }

    fn log(&mut self, line: &str) {
        self.logs.push(line.to_string());
    }

    fn recv(&mut self, wait: Duration) -> Recv {
        self.input.pop_front().unwrap_or_else(|| {
            std::thread::sleep(wait.min(Duration::from_millis(5)));
            Recv::Timeout
        })
    }
}

impl Mock {
    fn said(&self, needle: &str) -> bool {
        self.logs.iter().any(|l| l.contains(needle))
    }

    fn updates(&self, kind: &str) -> Vec<&Value> {
        self.sent
            .iter()
            .filter(|m| {
                m.pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some(kind)
            })
            .collect()
    }
}

fn scratch(label: &str) -> PathBuf {
    let base = std::env::var_os("TMPDIR")
        .map_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")), PathBuf::from);
    let d = base.join(format!("goal-sim-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Rig {
    sim: GoalSim,
    data: PathBuf,
}

impl Rig {
    /// A sim for `script`; `plugin` places the goal plugin's folder (and the observer's) in the configuration directory, `pending` writes
    /// the pending goal agentd would have written.
    fn new(label: &str, script: &Value, plugin: bool, pending: bool) -> Rig {
        let d = scratch(label);
        let (data, config) = (d.join("data"), d.join("config"));
        std::fs::create_dir_all(&data).unwrap();
        if plugin {
            for p in ["goal", "harness-rounds"] {
                std::fs::create_dir_all(config.join("plugins").join(p)).unwrap();
                std::fs::write(
                    config.join("plugins").join(p).join("index.js"),
                    "export default {};\n",
                )
                .unwrap();
            }
        }
        let s = Rig {
            sim: GoalSim::new(
                GoalScript::parse(&script.to_string()).unwrap(),
                data.clone(),
                config,
            ),
            data,
        };
        if pending {
            s.write_state(&json!({"version": 1, "pendingGoal": {"goal": "make the checks pass", "createdAt": "2026-10-16T00:00:00Z"}, "activeGoals": {}}));
        }
        s
    }

    fn state_path(&self) -> PathBuf {
        self.data.join("plugins/goal/goals.json")
    }

    fn write_state(&self, v: &Value) {
        std::fs::create_dir_all(self.state_path().parent().unwrap()).unwrap();
        std::fs::write(self.state_path(), v.to_string()).unwrap();
    }

    fn state(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.state_path()).unwrap()).unwrap()
    }

    fn rounds(&self) -> Vec<Value> {
        std::fs::read_to_string(self.data.join("plugins/harness-rounds/rounds.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// The first prompt, answered with `out` (the replayed transcript's messages), as the binary does it.
    fn first_prompt(&mut self, io: &mut Mock, out: &[Value]) {
        self.sim.begin_first_run("s1", io);
        self.sim.end_first_run("s1", out, io);
    }
}

fn first_answer(tools: usize, text: &str, stop: &str) -> Vec<Value> {
    let mut out = vec![];
    for i in 0..tools {
        out.push(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "tool_call", "toolCallId": format!("t{i}")}}}));
    }
    out.push(json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s1", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}}));
    out.push(json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": stop}}));
    out
}

fn two_rounds() -> Value {
    json!({"rounds": [
        {"text": "Running the checks.", "tools": [{"title": "run_commands: pytest", "kind": "execute"}], "iterations": 3},
        {"text": "Goal marked complete.", "complete": "all done", "iterations": 2}
    ]})
}

#[test]
fn a_script_is_read_with_every_member_and_the_defaults_for_the_rest() {
    let full = json!({"transcripts": "writes_files", "gap_ms": 5, "claim": false, "observer": false, "repeat_last": true,
        "queued": {"text": "Added the line.", "status": "failed"},
        "rounds": [{"text": "one", "tools": [{"title": "run_commands: pytest", "kind": "execute"}, {}], "edit": {"path": "x.py", "text": "y\n"},
            "status": "aborted", "iterations": 4, "delay_ms": 7, "hold_ms": 9, "complete": "done", "refuse": true, "error": "boom"}]});
    let g = GoalScript::parse(&full.to_string()).unwrap();
    assert_eq!(g.transcripts.as_deref(), Some("writes_files"));
    assert_eq!(
        (g.gap_ms, g.claim, g.observer, g.repeat_last),
        (5, false, false, true)
    );
    assert_eq!(
        (g.queued.text.as_str(), g.queued.status.as_str()),
        ("Added the line.", "failed")
    );
    let r = &g.rounds[0];
    assert_eq!(
        r.tools,
        [
            ("run_commands: pytest".to_string(), "execute".to_string()),
            ("tool".to_string(), "execute".to_string())
        ]
    );
    assert_eq!(r.edit, Some(("x.py".to_string(), "y\n".to_string())));
    assert_eq!(
        (r.status.as_str(), r.iterations, r.delay_ms, r.hold_ms),
        ("aborted", 4, 7, 9)
    );
    assert_eq!(
        (r.complete.as_deref(), r.refuse, r.error.as_deref()),
        (Some("done"), true, Some("boom"))
    );

    let bare = GoalScript::parse(r#"{"rounds": [{}]}"#).unwrap();
    assert_eq!(
        (bare.gap_ms, bare.claim, bare.observer, bare.repeat_last),
        (20, true, true, false)
    );
    assert_eq!(bare.transcripts, None);
    assert_eq!(
        (bare.queued.text.as_str(), bare.queued.status.as_str()),
        ("Done with your message.", "completed")
    );
    assert_eq!(bare.rounds[0].status, "completed");
    assert!(
        bare.rounds[0].tools.is_empty()
            && bare.rounds[0].edit.is_none()
            && bare.rounds[0].complete.is_none()
    );
}

#[test]
fn a_script_that_is_not_json_or_has_no_rounds_is_refused_and_a_directory_without_one_has_none() {
    assert!(GoalScript::parse("{").unwrap_err().contains("goal.json"));
    assert!(GoalScript::parse("{}").unwrap_err().contains("no rounds"));
    let d = scratch("load");
    assert!(GoalScript::load(&d).unwrap().is_none());
    std::fs::write(d.join("goal.json"), r#"{"rounds": []}"#).unwrap();
    assert!(GoalScript::load(&d).unwrap().is_some());
    std::fs::write(d.join("goal.json"), "{").unwrap();
    assert!(GoalScript::load(&d).is_err());
}

#[test]
fn the_first_prompt_claims_the_goal_writes_the_observers_lines_and_queues_the_next_round() {
    let mut r = Rig::new("claim", &two_rounds(), true, true);
    let mut io = Mock::default();
    assert!(!r.sim.engaged() && r.sim.next_due().is_none());
    r.sim.begin_first_run("s1", &mut io);
    let s = r.state();
    assert!(s.get("pendingGoal").is_none(), "{s}");
    assert_eq!(s["activeGoals"]["s1"]["sessionId"], "s1");
    assert_eq!(s["activeGoals"]["s1"]["goal"], "make the checks pass");
    assert!(
        io.said("goal claimed for s1: make the checks pass"),
        "{:?}",
        io.logs
    );
    assert_eq!(r.rounds().len(), 1);
    assert_eq!(r.rounds()[0]["ev"], "start");

    r.sim
        .end_first_run("s1", &first_answer(2, "Starting.", "end_turn"), &mut io);
    assert!(r.sim.engaged() && r.sim.next_due().is_some());
    let rounds = r.rounds();
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[1]["ev"], "end");
    assert_eq!(rounds[1]["status"], "completed");
    assert_eq!(
        rounds[1]["iterations"], 3,
        "two tool calls plus the model's last call"
    );
    assert_eq!(rounds[1]["text"], "Starting.");
    assert_eq!(r.state()["activeGoals"]["s1"]["awaitingVerification"], true);
}

#[test]
fn without_the_plugin_folder_or_a_claim_or_a_pending_goal_the_loop_never_engages() {
    // no plugin folder: the run is a plain run
    let mut r = Rig::new("noplugin", &two_rounds(), false, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "ok", "end_turn"));
    assert!(!r.sim.engaged() && r.sim.next_due().is_none());
    assert!(r.rounds().is_empty() && io.logs.is_empty());

    // the plugin never loads in this scenario
    let mut script = two_rounds();
    script["claim"] = json!(false);
    let mut r = Rig::new("noclaim", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "ok", "end_turn"));
    assert!(io.said("goal not claimed"), "{:?}", io.logs);
    assert!(!r.sim.engaged() && r.sim.next_due().is_none());
    assert!(
        r.state().get("pendingGoal").is_some(),
        "an unclaimed goal stays pending"
    );

    // a pending goal that is already complete is not claimed
    let mut r = Rig::new("done", &two_rounds(), true, false);
    r.write_state(&json!({"version": 1, "pendingGoal": {"goal": "g", "completedAt": "2026-10-16T00:00:00Z"}, "activeGoals": {}}));
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "ok", "end_turn"));
    assert!(!r.sim.engaged() && r.sim.next_due().is_none());

    // no state file at all
    let mut r = Rig::new("nostate", &two_rounds(), true, false);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "ok", "end_turn"));
    assert!(!r.sim.engaged());
}

#[test]
fn a_first_run_that_did_not_complete_queues_nothing_and_says_the_loop_stopped() {
    let mut r = Rig::new("aborted", &two_rounds(), true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "", "cancelled"));
    assert!(r.sim.next_due().is_none());
    assert!(io.said("the loop stopped"), "{:?}", io.logs);
    assert_eq!(r.rounds()[1]["status"], "aborted");
    // a first answer without a stop reason or text still ends the run as completed
    let mut r = Rig::new("bare", &two_rounds(), true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &[]);
    assert_eq!(r.rounds()[1]["status"], "completed");
    assert_eq!(r.rounds()[1]["text"], "");
}

#[test]
fn the_follow_up_rounds_run_in_order_and_the_completion_is_accepted_only_after_the_verification_round()
 {
    let mut r = Rig::new("rounds", &two_rounds(), true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(1, "Starting.", "end_turn"));

    assert!(matches!(r.sim.run_due("s1", &mut io), Flow::Go));
    assert!(
        r.sim.next_due().is_some(),
        "a completed round with the goal active queues the next one"
    );
    assert_eq!(
        io.updates("tool_call").len(),
        1,
        "round one: the pytest call"
    );
    let first_round_texts: Vec<_> = io
        .updates("agent_message_chunk")
        .iter()
        .map(|m| {
            m["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(first_round_texts, ["Running the checks."]);

    assert!(matches!(r.sim.run_due("s1", &mut io), Flow::Go));
    let outputs: Vec<_> = io
        .updates("tool_call_update")
        .iter()
        .filter_map(|m| m.pointer("/params/update/rawOutput").cloned())
        .collect();
    assert_eq!(
        io.updates("tool_call").len(),
        2,
        "the pytest call, then the completion call"
    );
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0]["completed"], true);
    assert_eq!(outputs[0]["summary"], "all done");
    assert_eq!(outputs[0]["goal"], "make the checks pass");
    assert!(
        r.state()["activeGoals"].as_object().unwrap().is_empty(),
        "the goal left the state file"
    );
    assert!(r.sim.next_due().is_none(), "no goal, nothing queued");
    let ends: Vec<_> = r
        .rounds()
        .into_iter()
        .filter(|l| l["ev"] == "end")
        .collect();
    assert_eq!(ends.len(), 3);
    assert_eq!(ends[1]["iterations"], 3, "the script's number");
    assert_eq!(ends[2]["iterations"], 2);

    // the goal is gone: a due round that fires anyway ends the loop
    assert!(matches!(r.sim.run_due("s1", &mut io), Flow::Go));
    assert!(io.said("the goal is no longer active"), "{:?}", io.logs);
}

#[test]
fn a_completion_call_before_the_verification_prompt_is_refused_and_the_goal_stays() {
    let script = json!({"rounds": [{"text": "I am done.", "complete": "early", "refuse": true}, {"text": "Now it is.", "complete": "late"}]});
    let mut r = Rig::new("refuse", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    let out = io.updates("tool_call_update")[0]["params"]["update"]["rawOutput"].clone();
    assert_eq!(out["completed"], false);
    assert!(out["message"].as_str().unwrap().contains("verification"));
    assert!(io.said("completion refused"));
    assert!(
        r.state()["activeGoals"]["s1"].is_object(),
        "the goal stays active"
    );
    // without a first claim the call is refused too
    let mut r = Rig::new(
        "refuse2",
        &json!({"rounds": [{"complete": "x"}]}),
        true,
        true,
    );
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.write_state(&json!({"version": 1, "activeGoals": {"s1": {"goal": "g"}}}));
    r.sim.run_due("s1", &mut io);
    assert_eq!(
        io.updates("tool_call_update")[0]["params"]["update"]["rawOutput"]["completed"],
        false
    );
}

#[test]
fn a_round_can_edit_a_file_fail_with_an_error_and_a_script_that_runs_out_ends_the_loop() {
    let script = json!({"rounds": [
        {"text": "x", "edit": {"path": "x.txt", "text": "a\n"}, "status": "failed", "error": "503 from the model"}]});
    let mut r = Rig::new("edit", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    let diff = io.updates("tool_call_update")[0]["params"]["update"]["content"][0].clone();
    assert_eq!(diff["type"], "diff");
    assert_eq!(diff["path"], "x.txt");
    let last = r.rounds().pop().unwrap();
    assert_eq!(
        (last["status"].as_str(), last["error"].as_str()),
        (Some("failed"), Some("503 from the model"))
    );
    assert!(r.sim.next_due().is_none(), "a failed round queues nothing");
    assert!(io.said("the loop stopped"));

    // a script with one round: the loop ends when it runs out
    let mut r = Rig::new("runout", &json!({"rounds": [{"text": "x"}]}), true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    r.sim.run_due("s1", &mut io);
    assert!(io.said("no more rounds"), "{:?}", io.logs);
    // repeat_last plays the last round again
    let mut r = Rig::new(
        "repeat",
        &json!({"repeat_last": true, "rounds": [{"text": "again"}]}),
        true,
        true,
    );
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    r.sim.run_due("s1", &mut io);
    assert_eq!(io.updates("agent_message_chunk").len(), 2);
}

#[test]
fn a_long_final_message_is_cut_to_its_last_1500_characters_in_the_observers_line() {
    let long = "x".repeat(1400) + &"y".repeat(300);
    let script = json!({"rounds": [{"text": long}]});
    let mut r = Rig::new("tail", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    let end = r.rounds().pop().unwrap();
    let text = end["text"].as_str().unwrap();
    assert_eq!(text.chars().count(), 1500);
    assert!(text.ends_with(&"y".repeat(300)));
}

#[test]
fn a_scenario_without_the_observer_writes_no_rounds_log() {
    let mut script = two_rounds();
    script["observer"] = json!(false);
    let mut r = Rig::new("noobs", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    assert!(r.rounds().is_empty());
}

fn line(v: &Value) -> Recv {
    Recv::Line(v.to_string())
}

#[test]
fn input_that_arrives_while_a_round_waits_is_serviced_a_prompt_is_answered_and_queued_a_cancel_is_ignored()
 {
    let script = json!({"queued": {"text": "Added the README line."}, "rounds": [
        {"text": "slow", "delay_ms": 30, "hold_ms": 30}, {"text": "next"}]});
    let mut r = Rig::new("busy", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    io.input.extend([
        Recv::Line("not json".into()),
        line(&json!({"jsonrpc": "2.0", "id": 9, "method": "session/prompt", "params": {}})),
        line(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}})),
        line(&json!({"jsonrpc": "2.0", "id": 10, "method": "session/something_else"})),
    ]);
    assert!(matches!(r.sim.run_due("s1", &mut io), Flow::Go));
    assert!(
        io.said("prompt queued during a round") && io.said("session/cancel ignored"),
        "{:?}",
        io.logs
    );
    let answers: Vec<_> = io.sent.iter().filter(|m| m["id"] == 9).collect();
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0]["result"]["stopReason"], "end_turn");
    // the queued prompt runs after the round, before the script's next round
    r.sim.run_due("s1", &mut io);
    let texts: Vec<_> = io
        .updates("agent_message_chunk")
        .iter()
        .map(|m| {
            m["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(texts, ["slow", "Added the README line."]);
}

#[test]
fn a_client_that_goes_away_during_a_round_ends_the_run_of_the_engine() {
    for (label, field) in [("delay", "delay_ms"), ("hold", "hold_ms")] {
        let mut round = json!({"text": "slow"});
        round[field] = json!(50);
        let script = json!({ "rounds": [round] });
        let mut r = Rig::new(&format!("closed-{label}"), &script, true, true);
        let mut io = Mock::default();
        r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
        io.input.push_back(Recv::Closed);
        assert!(
            matches!(r.sim.run_due("s1", &mut io), Flow::Exit),
            "{label}"
        );
    }
}

#[test]
fn a_prompt_while_the_next_round_is_queued_is_answered_at_once_and_queued() {
    let mut r = Rig::new("queued", &two_rounds(), true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    assert!(r.sim.next_due().is_some());
    assert!(r.sim.engaged());
    assert!(matches!(
        r.sim.client_prompt("s1", &json!(11), &mut io),
        Flow::Go
    ));
    assert_eq!(io.sent.len(), 1);
    assert_eq!(
        io.sent[0],
        json!({"jsonrpc": "2.0", "id": 11, "result": {"stopReason": "end_turn"}})
    );
    assert!(io.said("prompt queued during a round"));
}

#[test]
fn a_prompt_on_a_dead_loop_runs_as_a_normal_prompt_and_is_answered_by_how_it_ended() {
    let cases = [
        ("completed", json!({"stopReason": "end_turn"}), None),
        ("aborted", json!({"stopReason": "cancelled"}), None),
        ("failed", Value::Null, Some(503)),
    ];
    for (status, result, code) in cases {
        let script = json!({"queued": {"text": "Reply.", "status": status}, "rounds": [{"text": "x", "status": "aborted"}]});
        let mut r = Rig::new(&format!("dead-{status}"), &script, true, true);
        let mut io = Mock::default();
        // the first round of the script aborts, so the loop is dead afterwards
        r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
        r.sim.run_due("s1", &mut io);
        assert!(r.sim.next_due().is_none());
        // the script has no more rounds; the prompt runs the scenario's queued text
        let flow = r.sim.client_prompt("s1", &json!(12), &mut io);
        assert!(matches!(flow, Flow::Go));
        assert!(io.said("prompt runs on a dead loop"));
        let reply = io.sent.last().unwrap();
        assert_eq!(reply["id"], 12, "{status}");
        match code {
            None => assert_eq!(reply["result"], result, "{status}"),
            Some(c) => assert_eq!(reply["error"]["data"]["statusCode"], c),
        }
    }
}

#[test]
fn a_prompt_on_a_dead_loop_takes_the_next_scripted_round_when_there_is_one_and_a_closed_client_ends_it()
 {
    let script = json!({"rounds": [{"text": "x", "status": "aborted"}, {"text": "the prompt's round", "hold_ms": 40}]});
    let mut r = Rig::new("dead-script", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    io.input.push_back(Recv::Closed);
    let flow = r.sim.client_prompt("s1", &json!(13), &mut io);
    assert!(matches!(flow, Flow::Exit));
    assert!(
        io.updates("agent_message_chunk")
            .iter()
            .any(|m| m["params"]["update"]["content"]["text"] == "the prompt's round")
    );
}

#[test]
fn a_cancel_with_no_round_to_cancel_is_logged() {
    let mut r = Rig::new("cancel", &two_rounds(), true, false);
    let mut io = Mock::default();
    r.sim.cancel_seen(&mut io);
    assert!(io.said("session/cancel ignored by the goal loop"));
}

#[test]
fn a_fresh_session_claims_the_goal_written_again_and_starts_its_rounds_over() {
    let mut r = Rig::new("session2", &two_rounds(), true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    assert!(r.sim.engaged_for("s1") && !r.sim.engaged_for("s2"));
    r.sim.run_due("s1", &mut io);
    assert!(r.sim.next_due().is_some());
    // the harness starts session s2 and writes the goal again; the engine's first run in s2 claims it
    r.write_state(&json!({"version": 1, "pendingGoal": {"goal": "the same goal", "createdAt": "2026-10-16T00:00:00Z"}, "activeGoals": {}}));
    r.sim.begin_first_run("s2", &mut io);
    assert!(
        io.said("goal claimed for s2: the same goal"),
        "{:?}",
        io.logs
    );
    assert!(
        r.sim.next_due().is_none(),
        "nothing is queued before the first run of s2 ends"
    );
    assert!(
        !r.sim.engaged_for("s2"),
        "not yet: its first run has not ended"
    );
    r.sim.end_first_run(
        "s2",
        &first_answer(0, "Starting again.", "end_turn"),
        &mut io,
    );
    assert!(r.sim.engaged_for("s2") && !r.sim.engaged_for("s1"));
    assert_eq!(r.state()["activeGoals"]["s2"]["goal"], "the same goal");
    // the rounds of the script start over for the new session
    r.sim.run_due("s2", &mut io);
    let texts: Vec<_> = io
        .updates("agent_message_chunk")
        .iter()
        .map(|m| {
            m["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(texts, ["Running the checks.", "Running the checks."]);
}

fn polling_first(extra: Value) -> Value {
    let mut first = json!({"tools": [{"title": "read_files: a.log", "kind": "read"}, {"title": "run_commands: sleep 5", "kind": "execute"}], "hold_ms": 5000});
    for (k, v) in extra.as_object().unwrap() {
        first[k] = v.clone();
    }
    json!({"first": first, "rounds": [{"text": "next", "complete": "done"}]})
}

#[test]
fn a_script_can_name_the_first_run_and_an_edit_that_comes_before_the_tool_calls() {
    let g = GoalScript::parse(
        &polling_first(json!({"iterations": 4, "status": "completed"})).to_string(),
    )
    .unwrap();
    let first = g.first.expect("a first run");
    assert_eq!(
        (first.tools.len(), first.hold_ms, first.iterations),
        (2, 5000, 4)
    );
    assert!(
        GoalScript::parse(r#"{"rounds": [{}]}"#)
            .unwrap()
            .first
            .is_none()
    );
    let g = GoalScript::parse(
        r#"{"rounds": [{"edit_first": true, "edit": {"path": "x", "text": "y"}}, {}]}"#,
    )
    .unwrap();
    assert!(g.rounds[0].edit_first && !g.rounds[1].edit_first);
}

#[test]
fn the_first_run_of_a_script_is_the_prompt_and_session_cancel_ends_it() {
    let mut r = Rig::new("first-cancel", &polling_first(json!({})), true, true);
    let mut io = Mock::default();
    assert!(r.sim.first_scripted());
    r.sim.begin_first_run("s1", &mut io);
    io.input.push_back(line(
        &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}}),
    ));
    assert!(matches!(
        r.sim.first_prompt_scripted("s1", &json!(3), &mut io),
        Flow::Go
    ));
    assert_eq!(io.updates("tool_call").len(), 2);
    assert!(io.said("session/cancel ends the round"), "{:?}", io.logs);
    assert_eq!(
        io.sent.last().unwrap(),
        &json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "cancelled"}})
    );
    let end = r.rounds().pop().unwrap();
    assert_eq!(end["status"], "aborted");
    assert!(r.sim.next_due().is_none(), "an aborted run queues nothing");
    assert!(r.sim.engaged_for("s1"));
}

#[test]
fn a_cancel_of_a_follow_up_round_is_still_ignored_when_the_first_run_honoured_it() {
    let mut r = Rig::new(
        "first-then-ignore",
        &polling_first(json!({"hold_ms": 10})),
        true,
        true,
    );
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    assert!(matches!(
        r.sim.first_prompt_scripted("s1", &json!(3), &mut io),
        Flow::Go
    ));
    assert_eq!(io.sent.last().unwrap()["result"]["stopReason"], "end_turn");
    assert!(
        r.sim.next_due().is_some(),
        "a completed first run queues the next round"
    );
    // the next round holds and a cancel does not end it
    let mut script = polling_first(json!({"hold_ms": 10}));
    script["rounds"] = json!([{"text": "slow", "hold_ms": 30}]);
    let mut r = Rig::new("first-then-ignore2", &script, true, true);
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    r.sim.first_prompt_scripted("s1", &json!(3), &mut io);
    io.input.push_back(line(
        &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {}}),
    ));
    r.sim.run_due("s1", &mut io);
    assert!(
        io.said("session/cancel ignored by the goal loop"),
        "{:?}",
        io.logs
    );
    assert_eq!(r.rounds().pop().unwrap()["status"], "completed");
}

#[test]
fn a_first_run_that_fails_is_answered_with_the_providers_error_and_a_closed_client_ends_it() {
    let mut r = Rig::new(
        "first-failed",
        &polling_first(json!({"hold_ms": 0, "status": "failed", "error": "boom"})),
        true,
        true,
    );
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    r.sim.first_prompt_scripted("s1", &json!(3), &mut io);
    let reply = io.sent.last().unwrap();
    assert_eq!(reply["id"], 3);
    assert_eq!(reply["error"]["data"]["statusCode"], 503);
    // a client that goes away during the hold ends the engine
    let mut r = Rig::new("first-closed", &polling_first(json!({})), true, true);
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    io.input.push_back(Recv::Closed);
    assert!(matches!(
        r.sim.first_prompt_scripted("s1", &json!(3), &mut io),
        Flow::Exit
    ));
}

#[test]
fn an_edit_set_to_come_first_is_emitted_before_the_tool_calls() {
    let script = json!({"rounds": [{"edit_first": true, "edit": {"path": "x.txt", "text": "y\n"}, "tools": [{"title": "read_files: a", "kind": "read"}]}]});
    let mut r = Rig::new("edit-first", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    r.sim.run_due("s1", &mut io);
    let titles: Vec<_> = io
        .updates("tool_call")
        .iter()
        .map(|m| m["params"]["update"]["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titles, ["Edit x.txt", "read_files: a"]);
}

#[test]
fn tool_calls_can_be_spaced_and_an_edit_can_come_in_the_middle_of_them() {
    let script = json!({"rounds": [{"tool_gap_ms": 5, "edit_after_calls": 2, "edit": {"path": "x.txt", "text": "y\n"},
        "tools": [{"title": "read_files: a", "kind": "read"}, {"title": "read_files: b", "kind": "read"}, {"title": "read_files: c", "kind": "read"}]}]});
    let g = GoalScript::parse(&script.to_string()).unwrap();
    assert_eq!(
        (g.rounds[0].tool_gap_ms, g.rounds[0].edit_after_calls),
        (5, Some(2))
    );
    let mut r = Rig::new("middle-edit", &script, true, true);
    let mut io = Mock::default();
    r.first_prompt(&mut io, &first_answer(0, "Starting.", "end_turn"));
    let t0 = std::time::Instant::now();
    r.sim.run_due("s1", &mut io);
    assert!(
        t0.elapsed() >= Duration::from_millis(10),
        "two gaps of 5 ms"
    );
    let titles: Vec<_> = io
        .updates("tool_call")
        .iter()
        .map(|m| m["params"]["update"]["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        titles,
        [
            "read_files: a",
            "read_files: b",
            "Edit x.txt",
            "read_files: c"
        ]
    );
}

#[test]
fn a_cancel_that_ends_the_first_run_stops_its_remaining_tool_calls_and_a_closed_client_ends_a_gap()
{
    let mut script = polling_first(json!({"hold_ms": 5000, "tool_gap_ms": 20}));
    script["first"]["tools"] = json!([{"title": "a", "kind": "read"}, {"title": "b", "kind": "read"}, {"title": "c", "kind": "read"}]);
    let mut r = Rig::new("gap-cancel", &script, true, true);
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    // the cancel arrives while the sim waits between the first and the second call
    io.input.push_back(line(
        &json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {}}),
    ));
    assert!(matches!(
        r.sim.first_prompt_scripted("s1", &json!(3), &mut io),
        Flow::Go
    ));
    assert_eq!(
        io.updates("tool_call").len(),
        1,
        "the calls after the cancel are not made"
    );
    assert_eq!(io.sent.last().unwrap()["result"]["stopReason"], "cancelled");
    // a client that goes away in a gap ends the engine
    let mut r = Rig::new("gap-closed", &script, true, true);
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    io.input.push_back(Recv::Closed);
    assert!(matches!(
        r.sim.first_prompt_scripted("s1", &json!(3), &mut io),
        Flow::Exit
    ));
}

// ---- a helper-agent call that the engine leaves open --------------------------------------------------------------------

fn helper_first(extra: Value) -> Value {
    let mut v = json!({
        "first": {
            "tools": [
                {"title": "read_files: a.log", "kind": "read"},
                {"title": "spawn_agent: review the change", "kind": "think", "open": true},
                {"title": "read_files: b.log", "kind": "read"}
            ],
            "hold_ms": 5000
        },
        "rounds": [{"text": "Finished without the helper.", "iterations": 1}, {"text": "next", "complete": "done"}]
    });
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

fn cancel_line() -> Recv {
    line(&json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "s1"}}))
}

fn update_of<'a>(io: &'a Mock, kind: &str, title_or_id: &str) -> Option<&'a Value> {
    io.sent.iter().find(|m| {
        let u = &m["params"]["update"];
        u["sessionUpdate"] == kind && (u["title"] == title_or_id || u["toolCallId"] == title_or_id)
    })
}

#[test]
fn b777_an_open_tool_call_is_announced_and_answered_aborted_when_session_cancel_ends_the_round() {
    let mut r = Rig::new("b777-open-cancel", &helper_first(json!({})), true, true);
    let mut io = Mock::default();
    assert!(r.sim.first_scripted());
    r.sim.begin_first_run("s1", &mut io);
    io.input.push_back(cancel_line());
    r.sim.first_prompt_scripted("s1", &json!(3), &mut io);
    let calls = io.updates("tool_call");
    assert_eq!(calls.len(), 3);
    let helper =
        update_of(&io, "tool_call", "spawn_agent: review the change").expect("the helper call");
    let id = helper["params"]["update"]["toolCallId"].clone();
    assert_eq!(helper["params"]["update"]["status"], "pending");
    // the reads are answered at once; the helper call is answered once, after the cancel, and aborted
    let answers: Vec<&Value> = io
        .updates("tool_call_update")
        .into_iter()
        .filter(|m| m["params"]["update"]["toolCallId"] == id)
        .collect();
    assert_eq!(answers.len(), 1);
    let out = &answers[0]["params"]["update"]["rawOutput"];
    assert_eq!(out["finishReason"], "aborted");
    assert_eq!(out["text"], "");
    assert_eq!(
        io.updates("tool_call_update").len(),
        3,
        "two reads and the helper"
    );
    assert_eq!(
        io.sent.last().unwrap(),
        &json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": "cancelled"}}),
        "the answers of the calls come before the prompt's own answer"
    );
}

#[test]
fn b777_an_open_tool_call_is_answered_with_a_result_when_the_round_ends_by_itself() {
    let mut r = Rig::new(
        "b777-open-ends",
        &helper_first(
            json!({"first": {"tools": [{"title": "spawn_agent: review", "kind": "think", "open": true}], "hold_ms": 10}}),
        ),
        true,
        true,
    );
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    r.sim.first_prompt_scripted("s1", &json!(3), &mut io);
    let ups = io.updates("tool_call_update");
    assert_eq!(ups.len(), 1);
    assert_eq!(
        ups[0]["params"]["update"]["rawOutput"]["finishReason"],
        "completed"
    );
    assert!(
        !ups[0]["params"]["update"]["rawOutput"]["text"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn b777_prompts_after_a_cancelled_run_are_dropped_with_zero_usage_as_many_times_as_the_script_says()
{
    let mut r = Rig::new(
        "b777-swallow",
        &helper_first(json!({"swallow_after_cancel": 2})),
        true,
        true,
    );
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    io.input.push_back(cancel_line());
    r.sim.first_prompt_scripted("s1", &json!(3), &mut io);
    for id in [4, 5] {
        let n = io.sent.len();
        r.sim.client_prompt("s1", &json!(id), &mut io);
        assert_eq!(io.sent.len(), n + 1, "dropped: one answer and nothing else");
        let reply = io.sent.last().unwrap();
        assert_eq!(reply["id"], id);
        assert_eq!(reply["result"]["stopReason"], "end_turn");
        assert_eq!(reply["result"]["usage"]["inputTokens"], 0);
        assert_eq!(reply["result"]["usage"]["outputTokens"], 0);
    }
    assert!(io.said("prompt dropped"));
    // the third prompt is taken: it runs the next scripted round
    r.sim.client_prompt("s1", &json!(6), &mut io);
    let texts: Vec<&str> = io
        .updates("agent_message_chunk")
        .iter()
        .map(|m| m["params"]["update"]["content"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(texts, ["Finished without the helper."]);
    assert_eq!(io.sent.last().unwrap()["result"]["stopReason"], "end_turn");
}

#[test]
fn b777_a_prompt_on_a_dead_loop_honours_session_cancel_like_the_first_prompt_does() {
    let script = helper_first(json!({
        "rounds": [
            {"text": "Starting the helper again.", "tools": [{"title": "spawn_agent: again", "kind": "think", "open": true}], "hold_ms": 5000},
            {"text": "next", "complete": "done"}
        ]
    }));
    let mut r = Rig::new("b777-dead-cancel", &script, true, true);
    let mut io = Mock::default();
    r.sim.begin_first_run("s1", &mut io);
    io.input.push_back(cancel_line());
    r.sim.first_prompt_scripted("s1", &json!(3), &mut io);
    // the second prompt runs on a dead loop, opens a helper and is cancelled
    io.input.push_back(cancel_line());
    r.sim.client_prompt("s1", &json!(4), &mut io);
    assert_eq!(
        io.sent.last().unwrap(),
        &json!({"jsonrpc": "2.0", "id": 4, "result": {"stopReason": "cancelled"}})
    );
    let aborted = io
        .updates("tool_call_update")
        .iter()
        .filter(|m| m["params"]["update"]["rawOutput"]["finishReason"] == "aborted")
        .count();
    assert_eq!(
        aborted, 2,
        "the helper of the first run and the helper of the second"
    );
}
