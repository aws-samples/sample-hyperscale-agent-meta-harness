//! A task that starts with a command word of the engine. OpenCode reads a prompt whose trimmed text starts with `/` and names
//! an advertised command (`init`, `review`, `goal`, `compact`) as that command: a user's task that began with such a word would run no model
//! turn of its own. The launch installs a guard on the client of this engine that puts a plain line before such a prompt. Cline's prompts, and
//! every prompt that does not start with `/`, go on the wire byte for byte.
#![allow(clippy::unwrap_used)]

mod b790_support;

use agentd::acp::Activity;
use agentd::direct::collector::DirectCollector;
use agentd::direct::driver::{NoProbe, PromptPlan, run_direct_prompt};
use agentd::direct::hooks::{DirectHook, Event, Hooks};
use agentd::direct::retry::RetryPolicy;
use agentd::engine::config::prompt_guard;
use agentd::engine::opencode::guard_prompt;
use agentd::events::EventRecorder;
use b790_support::{MODEL, TOP, engine};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const WORDS: [&str; 4] = ["/goal", "/init", "/review", "/compact"];
const HEADER: &str = "Task (plain text, not a command):\n";

#[test]
fn b790_a_task_that_starts_with_a_command_word_gets_a_plain_line_before_it() {
    for word in WORDS {
        let task = format!("{word} write hello.txt\nand run the tests\n");
        let wire = guard_prompt(&task);
        assert!(wire.starts_with(HEADER), "{word}: {wire:?}");
        assert!(
            wire.ends_with(&task),
            "the task follows unchanged: {wire:?}"
        );
        assert!(!wire.trim().starts_with('/'), "{word}");
    }
}

#[test]
fn b790_blanks_and_a_byte_order_mark_before_the_word_do_not_hide_it() {
    for lead in [
        "  ",
        "\n\n",
        "\t",
        "\u{feff}",
        "\u{a0}",
        "\r\n",
        " \u{feff}\n",
    ] {
        let task = format!("{lead}/init now");
        assert!(guard_prompt(&task).starts_with(HEADER), "{lead:?}");
    }
}

#[test]
fn b790_a_path_or_an_unknown_word_after_the_slash_is_guarded_too_and_costs_one_line() {
    // The guard does not know which words the engine advertises (the list is the engine's and changes with its release).
    for task in ["/usr/bin/env python3 -V", "/nothing-like-it", "/"] {
        let wire = guard_prompt(task);
        assert_eq!(wire, format!("{HEADER}{task}"), "{task}");
    }
}

#[test]
fn b790_any_other_text_goes_on_the_wire_as_it_came_without_a_copy() {
    for task in [
        "Add percent_change with tests",
        "",
        "   ",
        "src/a/b.rs needs a fix",
        "see /goal later",
        "first line\n/goal on the second",
        "\u{200b}/init",
    ] {
        assert!(
            matches!(guard_prompt(task), Cow::Borrowed(t) if t == task),
            "{task:?}"
        );
    }
}

#[test]
fn b790_only_the_engine_that_reads_commands_gets_a_guard() {
    assert!(prompt_guard("opencode").is_some());
    for other in ["cline", "fake-acp", "pi", ""] {
        assert!(prompt_guard(other).is_none(), "{other:?}");
    }
}

/// A module that keeps the text of every prompt it was told about.
#[derive(Default)]
struct Seen(Mutex<Vec<String>>);

impl DirectHook for Seen {
    fn name(&self) -> &'static str {
        "seen"
    }

    fn on_event(&self, e: &Event<'_>) {
        if let Event::PromptSent(p) = e {
            self.0.lock().unwrap().push(p.text.to_string());
        }
    }
}

async fn wire_text_of(engine_name: &str, task: &str) -> (String, Vec<String>) {
    // The engine reports its model in its own spelling: the catalog id for the first engine, the provider-qualified value for this one.
    let (client, received, _) = engine(b790_support::Script {
        model: agentd::engine::config::acp_model_value(engine_name, MODEL),
        ..b790_support::Script::default()
    });
    let activity = Activity::new();
    let seen = Arc::new(Seen::default());
    let hooks = Hooks::new(vec![seen.clone() as Arc<dyn DirectHook>]);
    let mut client = client
        .with_activity(activity.clone())
        .with_tap(hooks.tap().unwrap())
        .with_prompt_guard(prompt_guard(engine_name));
    let mut collector =
        DirectCollector::new(EventRecorder::new("run1", "direct")).with_hooks(hooks);
    let plan = PromptPlan {
        cwd: TOP.into(),
        model: MODEL.into(),
        prompt: task.into(),
        nudge: None,
        cancel: None,
        timeout: Duration::from_secs(1000),
        silence: Duration::from_secs(900),
        retry: RetryPolicy::off(),
        failover: None,
        engine_stop_resumes: 0,
        engine: engine_name.into(),
    };
    run_direct_prompt(&mut client, &mut collector, &activity, &plan, &NoProbe).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let sent = received
        .lock()
        .unwrap()
        .iter()
        .find(|m| m["method"] == "session/prompt")
        .map(|m| {
            m["params"]["prompt"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .unwrap();
    let told = seen.0.lock().unwrap().clone();
    (sent, told)
}

#[tokio::test(start_paused = true)]
async fn b790_the_prompt_of_this_engine_is_guarded_on_the_wire_and_the_modules_see_the_task_as_asked()
 {
    for word in WORDS {
        let task = format!("{word} write hello.txt");
        let (sent, told) = wire_text_of("opencode", &task).await;
        assert_eq!(sent, format!("{HEADER}{task}"), "{word}");
        assert_eq!(
            told,
            std::slice::from_ref(&task),
            "the modules and the recorder see the text as asked"
        );
        let v: Value = serde_json::json!({"text": sent});
        assert!(!v["text"].as_str().unwrap().trim().starts_with('/'));
    }
}

#[tokio::test(start_paused = true)]
async fn b790_the_prompt_of_the_first_engine_is_on_the_wire_byte_for_byte() {
    for word in WORDS {
        let task = format!("{word} write hello.txt\n");
        let (sent, told) = wire_text_of("cline", &task).await;
        assert_eq!(sent, task, "{word}");
        assert_eq!(told, std::slice::from_ref(&task));
    }
}
