//! The help names no engine where it means the coding engine of the run. The same help offers `--engine cline|opencode`, so a person who chose
//! OpenCode read text about the other engine: the opening sentence, `--direct`, `--attach`, `ham say`, `ham continue` and the `/help` lines of
//! cancel, say, new-session, now and copy. The engine list (the `--engine` row) still names the engines a person can choose.

#![allow(clippy::unwrap_used)]

use ham_core::{help, registry};

/// The display name of the default engine, as the old help wrote it.
const NAME: &str = "Cline";

fn offending(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.contains(NAME))
        .map(str::to_string)
        .collect()
}

#[test]
fn b801_the_usage_screen_names_no_engine_outside_the_engine_list() {
    let text = help::top_text();
    assert_eq!(offending(&text), Vec::<String>::new(), "{text}");
    // The engine list still names every engine a person can choose.
    assert!(text.contains("--engine <name>"), "{text}");
    assert!(text.contains("cline, opencode"), "{text}");
}

#[test]
fn b801_no_command_help_names_an_engine_where_it_means_the_run_s_engine() {
    for name in registry::subcommand_names() {
        let Some(text) = help::command_text(name) else {
            continue;
        };
        assert_eq!(
            offending(&text),
            Vec::<String>::new(),
            "ham help {name}:\n{text}"
        );
    }
}

#[test]
fn b801_the_engine_help_does_not_say_the_coding_agent_is_one_engine() {
    let text = help::command_text("engine").unwrap();
    assert!(!text.contains("The coding agent is cline"), "{text}");
    assert!(text.contains("~/.config/ham/engines/<name>.toml"), "{text}");
}

#[test]
fn b801_the_chat_help_names_no_engine() {
    let text = registry::help_text();
    assert_eq!(offending(&text), Vec::<String>::new(), "{text}");
    for word in ["cancel", "say", "new-session", "now", "copy"] {
        let one = registry::command_help(word).unwrap();
        assert_eq!(
            offending(&one),
            Vec::<String>::new(),
            "/help {word}:\n{one}"
        );
    }
}

#[test]
fn b801_the_chat_help_says_the_coding_agent() {
    let text = registry::help_text();
    assert!(
        text.contains("Ask the coding agent to stop; the work so far is kept"),
        "{text}"
    );
    assert!(text.contains("Copy the agent's last answer"), "{text}");
}
