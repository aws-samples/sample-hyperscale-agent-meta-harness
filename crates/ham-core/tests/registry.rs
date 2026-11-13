//! The command registry: one table that feeds the parser's list of subcommands, `/help` and the popup of the chat box.
//! A subcommand of `ham_core::command::parse` without an entry fails here, and so does an entry that claims to be a subcommand and is
//! not one. Typed forms are the argv forms, so every sample below goes through `parse`. Aligned: an entry
//! declares a set of effects and the class a person sees (read-only, local write, run-changing or off-machine) comes from the set;
//! a command with a text argument says where its payload starts.

#![allow(clippy::unwrap_used)]

use ham_core::command::{
    Command, ConfigCommand, Invocation, PullArtifact, RulesCommand, SUBCOMMANDS, parse,
};
use ham_core::registry::{
    self, Availability, EffectClass, Effects, Entry, Kind, RunArg, TextArg, class_of, closest,
    closest_in, edit_distance, effects_of, entries, find, find_in, help_text, phrase_command,
    popup, subcommand, subcommand_names,
};

/// A canonical ULID, the shape of every run id `ham runs` prints.
const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

/// Sample argv (after the command word) of every form of every subcommand. A new subcommand has to add its samples here, so the
/// test fails until it does.
fn samples() -> Vec<(&'static str, Vec<Vec<&'static str>>)> {
    vec![
        ("runs", vec![vec![], vec!["--probes"]]),
        ("watch", vec![vec![], vec![RUN]]),
        ("cancel", vec![vec![RUN]]),
        ("models", vec![vec![], vec!["--server"]]),
        ("pr", vec![vec![RUN]]),
        ("pull", vec![vec![RUN], vec![RUN, "--artifact", "partial"]]),
        ("clone", vec![vec![RUN], vec![RUN, "somewhere"]]),
        (
            "push",
            vec![
                vec![RUN, "git@example.test:me/repo.git"],
                vec![RUN, "https://example.test/me/repo.git", "--branch", "main"],
            ],
        ),
        ("link", vec![vec![RUN]]),
        ("doctor", vec![vec![], vec!["--catalog-max-age", "24"]]),
        (
            "rules",
            vec![
                vec!["init"],
                vec!["edit"],
                vec!["edit", "continue"],
                vec!["show"],
            ],
        ),
        (
            "config",
            vec![
                vec![],
                vec!["get", "region"],
                vec!["set", "default_model", "opus"],
                vec!["set", "region", "us-east-1"],
                vec!["unset", "aws_profile"],
                vec!["show"],
                vec!["check"],
                vec!["check", "--strict"],
                vec!["edit"],
                vec!["docs"],
            ],
        ),
        (
            "engine",
            vec![
                vec!["cline", "init"],
                vec!["cline", "show"],
                vec!["cline", "check"],
                vec!["cline", "edit"],
                vec!["cline", "set", "reasoning.effort", "low"],
                vec!["cline", "unset", "reasoning.effort"],
                vec!["cline", "import", "my.toml"],
                vec!["cline", "import", "my.toml", "--replace"],
            ],
        ),
        ("say", vec![vec![RUN, "hello"]]),
        ("new-session", vec![vec![RUN], vec![RUN, "start over"]]),
        (
            "try-model",
            vec![vec![RUN, "opus"], vec![RUN, "opus", "try this"]],
        ),
        ("finish", vec![vec![RUN]]),
        ("continue", vec![vec![RUN], vec![RUN, "go on"]]),
        (
            "resume",
            vec![
                vec!["--all"],
                vec!["--all", "--dry-run"],
                vec!["01M5A2R7K3C9XJ4D8E6FQ0WT1N"],
            ],
        ),
        (
            "lifecycle",
            vec![
                vec!["init", "somewhere"],
                vec!["init", "somewhere", "--kiro"],
            ],
        ),
        (
            "update",
            vec![
                vec!["--check"],
                vec![],
                vec!["--yes", "--wait", "30"],
                vec!["--no-wait"],
            ],
        ),
    ]
}

fn argv(word: &str, rest: &[&str]) -> Vec<String> {
    std::iter::once(word)
        .chain(rest.iter().copied())
        .map(str::to_string)
        .collect()
}

#[test]
fn every_subcommand_of_the_parser_has_exactly_one_entry() {
    for name in SUBCOMMANDS {
        let found: Vec<&Entry> = entries().iter().filter(|e| e.name == *name).collect();
        assert_eq!(
            found.len(),
            1,
            "subcommand {name:?} needs exactly one registry entry, found {}",
            found.len()
        );
        assert_eq!(
            found[0].kind,
            Kind::Subcommand,
            "the entry for {name:?} must be a subcommand entry"
        );
        assert!(subcommand(name).is_some(), "{name}");
    }
}

#[test]
fn every_entry_that_claims_to_be_a_subcommand_is_one() {
    for e in entries().iter().filter(|e| e.kind == Kind::Subcommand) {
        assert!(
            SUBCOMMANDS.contains(&e.name),
            "entry {:?} is marked as a subcommand but the parser has no such subcommand",
            e.name
        );
    }
    for e in entries().iter().filter(|e| e.kind == Kind::TuiOnly) {
        assert!(
            !SUBCOMMANDS.contains(&e.name),
            "TUI-only entry {:?} shares its name with a subcommand",
            e.name
        );
    }
    // The registry lists the subcommands in the parser's order, then the TUI-only words.
    assert_eq!(subcommand_names(), SUBCOMMANDS.to_vec());
    let first_tui = entries()
        .iter()
        .position(|e| e.kind == Kind::TuiOnly)
        .unwrap_or(entries().len());
    assert!(
        entries()[first_tui..]
            .iter()
            .all(|e| e.kind == Kind::TuiOnly),
        "TUI-only entries come after the subcommands"
    );
}

#[test]
fn names_and_aliases_are_unique_plain_words() {
    let mut seen = std::collections::BTreeSet::new();
    for e in entries() {
        for w in std::iter::once(e.name).chain(e.aliases.iter().copied()) {
            assert!(!w.is_empty() && w.len() <= 24, "{w:?}");
            assert!(
                w.chars().next().unwrap().is_ascii_lowercase(),
                "{w:?} starts with a lowercase letter"
            );
            assert!(
                w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{w:?}"
            );
            assert!(seen.insert(w), "{w:?} is used twice in the registry");
        }
    }
}

#[test]
fn every_entry_has_one_line_of_help_and_a_usage_shape() {
    for e in entries() {
        assert!(!e.help.trim().is_empty(), "{} has no help", e.name);
        assert!(e.help.len() <= 80, "{}: one short line of help", e.name);
        for text in [e.help, e.usage] {
            assert!(
                !text.chars().any(char::is_control),
                "{}: no control characters",
                e.name
            );
        }
        match e.status {
            Availability::Built => {}
            Availability::Planned(issue) => assert!(
                issue.starts_with("N") || issue.starts_with("B-"),
                "{}: {issue}",
                e.name
            ),
        }
    }
}

#[test]
fn every_sample_parses_and_the_entry_effects_are_the_union_of_the_effects_of_its_forms() {
    let samples = samples();
    for e in entries().iter().filter(|e| e.kind == Kind::Subcommand) {
        let forms = samples
            .iter()
            .find(|(n, _)| *n == e.name)
            .unwrap_or_else(|| panic!("add samples for {:?} to this test", e.name));
        let mut union = Effects::NONE;
        for rest in &forms.1 {
            let inv = parse(&argv(e.name, rest))
                .unwrap_or_else(|err| panic!("{} {rest:?}: {err}", e.name));
            union = union.union(effects_of(&inv.command));
            assert_eq!(class_of(&inv.command), effects_of(&inv.command).class());
        }
        assert_eq!(
            e.effects, union,
            "{}: the entry declares the effects of all its forms",
            e.name
        );
        assert_eq!(e.class(), union.class(), "{}", e.name);
    }
    // No stray samples for words the registry does not have.
    for (name, _) in &samples {
        assert!(
            subcommand(name).is_some(),
            "samples for {name:?}, which is not a subcommand entry"
        );
    }
}

#[test]
fn run_arg_says_where_the_run_id_goes() {
    for e in entries().iter().filter(|e| e.kind == Kind::Subcommand) {
        let forms = samples().into_iter().find(|(n, _)| *n == e.name).unwrap().1;
        match e.run_arg {
            RunArg::None => assert!(
                forms.iter().all(|f| f.first().is_none_or(|w| *w != RUN)),
                "{}: takes no run id, but a sample starts with one",
                e.name
            ),
            RunArg::Required => {
                assert!(
                    forms.iter().all(|f| f.first() == Some(&RUN)),
                    "{}: every sample starts with the run id",
                    e.name
                );
                assert!(
                    parse(&argv(e.name, &[])).is_err(),
                    "{}: a run id is required",
                    e.name
                );
            }
            RunArg::Optional => assert!(
                forms.iter().any(|f| f.is_empty()) && forms.iter().any(|f| f.first() == Some(&RUN)),
                "{}",
                e.name
            ),
        }
    }
}

#[test]
fn effects_of_covers_every_form() {
    use EffectClass::{ChangesRun, LocalWrite, ReadOnly};
    let e = |args: &[&str]| effects_of(&parse(args).unwrap().command);
    let none = Effects::NONE;
    for args in [
        vec!["runs"],
        vec!["models", "--server"],
        vec!["pr", RUN],
        vec!["link", RUN],
        vec!["doctor"],
        vec!["doctor", "--catalog-max-age", "24"],
        vec!["watch"],
        vec!["rules", "show"],
        vec!["config"],
        vec!["config", "get", "region"],
        vec!["config", "show"],
        vec!["config", "check", "--strict"],
        vec!["config", "docs"],
    ] {
        assert_eq!(e(&args), none, "{args:?}");
        assert_eq!(e(&args).class(), ReadOnly);
    }
    for args in [
        vec!["pull", RUN],
        vec!["clone", RUN],
        vec!["rules", "init"],
        vec!["config", "set", "default_model", "opus"],
        vec!["config", "set", "default_repo", "cc:x"],
        vec!["config", "set", "input_wait_minutes", "5"],
    ] {
        assert_eq!(e(&args), Effects::LOCAL_WRITE, "{args:?}");
        assert_eq!(e(&args).class(), LocalWrite);
    }
    // An editor needs the terminal, so it is a hand-off as well as a write.
    assert_eq!(
        e(&["rules", "edit"]),
        Effects::LOCAL_WRITE.union(Effects::TERMINAL_HANDOFF)
    );
    assert_eq!(
        e(&["config", "edit"]),
        Effects::LOCAL_WRITE.union(Effects::TERMINAL_HANDOFF)
    );
    // The keys that decide where requests go, who signs them and where downloads land.
    for key in [
        "api_endpoint",
        "apiEndpoint",
        "aws_profile",
        "awsProfile",
        "region",
        "download_dir",
        "downloadDir",
    ] {
        for args in [
            vec!["config", "set", key, "v"],
            vec!["config", "unset", key],
        ] {
            let fx = e(&args);
            assert!(fx.contains(Effects::CONFIG_SECURITY), "{args:?}");
            assert!(fx.contains(Effects::LOCAL_WRITE), "{args:?}");
            assert_eq!(fx.class(), ChangesRun, "{args:?}");
        }
    }
    for args in [
        vec!["cancel", RUN],
        vec!["finish", RUN],
        vec!["new-session", RUN],
        vec!["try-model", RUN, "opus"],
        vec!["say", RUN, "hi"],
    ] {
        assert_eq!(e(&args), Effects::REMOTE_MUTATION, "{args:?}");
        assert_eq!(e(&args).class(), ChangesRun);
    }
    // A push downloads and builds a repository, and the data leaves the machine; git may ask for a login on the terminal.
    let push = e(&["push", RUN, "git@example.test:me/repo.git"]);
    for fx in [
        Effects::DATA_LEAVES,
        Effects::LOCAL_WRITE,
        Effects::TERMINAL_HANDOFF,
    ] {
        assert!(push.contains(fx), "{push:?}");
    }
    assert_eq!(push.class(), ChangesRun);
    // A new run is billable and is never started from a typed line.
    let start = Command::StartDirect(
        match parse(&["x", "--s3-only", "--direct"]).unwrap().command {
            Command::StartDirect(a) => a,
            other => panic!("{other:?}"),
        },
    );
    assert_eq!(effects_of(&start), Effects::BILLABLE);
    assert_eq!(class_of(&start), ChangesRun);
    // Unused import guards: the variants below exist and are matched by effects_of's exhaustive match.
    let _ = (ConfigCommand::List, RulesCommand::Init, PullArtifact::Zip);
}

#[test]
fn effect_sets_give_the_class_a_person_sees() {
    use EffectClass::*;
    assert!(ReadOnly.rank() < LocalWrite.rank() && LocalWrite.rank() < ChangesRun.rank());
    assert!(ChangesRun.needs_confirm());
    for c in [ReadOnly, LocalWrite] {
        assert!(!c.needs_confirm(), "{c:?}");
    }
    for c in [ReadOnly, LocalWrite, ChangesRun] {
        assert!(!c.label().is_empty());
    }
    // No effect is read-only; only a local write is a local write; any other effect is run-changing or off-machine.
    assert_eq!(Effects::NONE.class(), ReadOnly);
    assert_eq!(Effects::default(), Effects::NONE);
    assert_eq!(Effects::LOCAL_WRITE.class(), LocalWrite);
    for fx in [
        Effects::REMOTE_MUTATION,
        Effects::DATA_LEAVES,
        Effects::CONFIG_SECURITY,
        Effects::BILLABLE,
        Effects::TERMINAL_HANDOFF,
    ] {
        assert_eq!(fx.class(), ChangesRun, "{fx:?}");
        assert_eq!(fx.union(Effects::LOCAL_WRITE).class(), ChangesRun, "{fx:?}");
        assert!(fx.contains(fx) && !fx.contains(Effects::LOCAL_WRITE));
        assert_eq!(fx.names().len(), 1);
    }
    assert!(Effects::NONE.is_empty() && !Effects::LOCAL_WRITE.is_empty());
    assert!(Effects::NONE.names().is_empty());
    assert_eq!(
        Effects::REMOTE_MUTATION
            .union(Effects::DATA_LEAVES)
            .names()
            .len(),
        2
    );
}

#[test]
fn push_and_link_are_entries_with_the_links_branch_shapes() {
    let push = subcommand("push").unwrap();
    assert_eq!(push.usage, "<runId> <git-url> [--branch <name>]");
    assert_eq!(push.class(), EffectClass::ChangesRun);
    assert_eq!(push.run_arg, RunArg::Required);
    assert_eq!(push.min_args, 2, "the run id and the URL");
    assert_eq!(push.ask, Some("git-url"));
    let link = subcommand("link").unwrap();
    assert_eq!(link.class(), EffectClass::ReadOnly);
    assert_eq!(link.min_args, 1);
}

#[test]
fn commands_with_a_text_argument_say_where_the_payload_starts() {
    // The payload of these is never tokenised; a structured prefix (--run <id>, a model) comes before it.
    let say = subcommand("say").unwrap();
    assert_eq!((say.text, say.lead), (TextArg::Required, 0));
    let ns = subcommand("new-session").unwrap();
    assert_eq!((ns.text, ns.lead), (TextArg::Optional, 0));
    let tm = subcommand("try-model").unwrap();
    assert_eq!((tm.text, tm.lead), (TextArg::Optional, 1));
    // A follow-up: the text for Cline after the run id, optional because the client opens the editor without it.
    let cont = subcommand("continue").unwrap();
    assert_eq!((cont.text, cont.lead), (TextArg::Optional, 0));
    let send = find("send").unwrap();
    assert_eq!(send.kind, Kind::TuiOnly);
    assert_eq!(send.text, TextArg::Required);
    assert!(send.effects.is_empty(), "plain chat, like Enter");
    // `/tag` takes its words as one payload and acts on the run on screen, so it has no run id.
    let tag = find("tag").unwrap();
    assert_eq!(
        (tag.kind, tag.text, tag.lead),
        (Kind::TuiOnly, TextArg::Required, 0)
    );
    assert_eq!(tag.run_arg, RunArg::None);
    for e in entries() {
        if ![
            "say",
            "new-session",
            "try-model",
            "continue",
            "send",
            "now",
            "tag",
        ]
        .contains(&e.name)
        {
            assert_eq!(e.text, TextArg::None, "{} takes no payload", e.name);
            assert_eq!(e.lead, 0, "{}", e.name);
        }
    }
}

#[test]
fn a_planned_word_declares_the_effects_it_will_have() {
    // `now` is a remote mutation, so it asks for a second Enter without another edit; it is built and needs the
    // session's `message.now` control.
    let now = find("now").unwrap();
    assert!(matches!(now.status, Availability::Built));
    assert_eq!(now.needs, Some("message.now"));
    assert_eq!(now.effects, Effects::REMOTE_MUTATION);
    assert_eq!(now.class(), EffectClass::ChangesRun);
    assert_eq!(now.text, TextArg::Required);
    assert_eq!(find("export").unwrap().effects, Effects::LOCAL_WRITE);
}

#[test]
fn an_entry_with_arguments_completes_in_the_popup_and_one_without_runs() {
    assert!(subcommand("cancel").unwrap().has_args());
    assert!(subcommand("push").unwrap().has_args());
    assert!(!subcommand("doctor").unwrap().has_args());
    assert!(!find("new").unwrap().has_args());
}

#[test]
fn the_parsers_usage_line_is_built_from_the_registry() {
    let usage = parse::<&str>(&[]).unwrap_err().to_string();
    let listed = usage
        .rsplit_once('(')
        .unwrap()
        .1
        .trim_end_matches(')')
        .to_string();
    assert_eq!(listed, subcommand_names().join(", "), "{usage}");
}

#[test]
fn find_resolves_names_aliases_and_case() {
    assert_eq!(find("cancel").unwrap().name, "cancel");
    assert_eq!(find("CANCEL").unwrap().name, "cancel");
    assert!(find("cancle").is_none());
    assert!(find("").is_none());
    let table = [
        Entry {
            aliases: &["stop", "halt"],
            ..*subcommand("cancel").unwrap()
        },
        *subcommand("runs").unwrap(),
    ];
    assert_eq!(find_in(&table, "halt").unwrap().name, "cancel");
    assert_eq!(find_in(&table, "Stop").unwrap().name, "cancel");
    assert!(find_in(&table, "pull").is_none());
}

#[test]
fn edit_distance_counts_a_swap_of_two_neighbours_as_one_edit() {
    assert_eq!(edit_distance("cancel", "cancel"), 0);
    assert_eq!(edit_distance("cancle", "cancel"), 1);
    assert_eq!(edit_distance("pul", "pull"), 1);
    assert_eq!(edit_distance("pulls", "pull"), 1);
    assert_eq!(edit_distance("psuh", "push"), 1);
    assert_eq!(edit_distance("kitten", "sitting"), 3);
    assert_eq!(edit_distance("", "abc"), 3);
    assert_eq!(edit_distance("abc", ""), 3);
    assert_eq!(edit_distance("", ""), 0);
}

#[test]
fn closest_names_the_nearest_words_and_nothing_for_nonsense() {
    assert_eq!(closest("cancle", 3).first().copied(), Some("cancel"));
    assert!(closest("pul", 3).contains(&"pull"));
    assert!(closest("psh", 3).contains(&"push"));
    assert!(closest("ca", 3).contains(&"cancel"), "a prefix is close");
    assert!(
        closest("nonsense", 3).is_empty(),
        "{:?}",
        closest("nonsense", 3)
    );
    assert!(closest("", 3).is_empty());
    assert!(closest("cancle", 1).len() <= 1);
    // d: a prefix, or at most two edits (two letters of `runs` missing, a letter wrong and one missing).
    assert!(closest("rn", 3).contains(&"runs"));
    assert_eq!(closest("cnacel", 3).first().copied(), Some("cancel"));
    assert!(closest("pll", 3).contains(&"pull"));
    assert!(closest("zzzzzz", 3).is_empty());
    assert!(closest("clon", 3).contains(&"clone"), "a prefix");
    // An alias leads to its entry's own name, once.
    let table = [Entry {
        aliases: &["stop"],
        ..*subcommand("cancel").unwrap()
    }];
    assert_eq!(closest_in(&table, "stpo", 3), vec!["cancel"]);
    // A planned word is not suggested: it does nothing yet. No word of the registry is planned since `/mouse` was built, so the
    // rule is shown over a table that marks it again; the real registry suggests it.
    let planned: Vec<Entry> = entries()
        .iter()
        .map(|e| {
            if e.name == "mouse" {
                Entry {
                    status: Availability::Planned("N0"),
                    ..*e
                }
            } else {
                *e
            }
        })
        .collect();
    assert!(!closest_in(&planned, "mous", 5).contains(&"mouse"));
    assert!(closest("mous", 5).contains(&"mouse"), "built: suggested");
    // `/engines` is built with the chat client: it is suggested.
    assert!(closest("enginez", 5).contains(&"engines"));
    // Built since the chat client was wired: `/copy` and `/export` are suggested.
    assert!(closest("exprt", 5).contains(&"export"));
    assert!(closest("cpy", 5).contains(&"copy"));
}

#[test]
fn the_popup_filters_by_prefix_and_lists_the_built_words() {
    let all = popup("");
    assert!(all.iter().all(|e| matches!(e.status, Availability::Built)));
    assert!(all.iter().any(|e| e.name == "push") && all.iter().any(|e| e.name == "help"));
    let ca: Vec<&str> = popup("ca").iter().map(|e| e.name).collect();
    assert_eq!(ca, vec!["cancel"]);
    let p: Vec<&str> = popup("p").iter().map(|e| e.name).collect();
    assert!(
        p.contains(&"pr") && p.contains(&"pull") && p.contains(&"push"),
        "{p:?}"
    );
    assert!(popup("zzz").is_empty());
    let s: Vec<&str> = popup("s").iter().map(|e| e.name).collect();
    assert!(s.contains(&"say") && s.contains(&"send"), "{s:?}");
    assert!(s.contains(&"sessions"), "built: {s:?}");
    assert_eq!(
        popup("m").iter().map(|e| e.name).collect::<Vec<_>>(),
        vec!["models", "mouse"],
        "/mouse is built"
    );
    assert_eq!(
        popup("CA").iter().map(|e| e.name).collect::<Vec<_>>(),
        vec!["cancel"],
        "case does not matter"
    );
    assert_eq!(
        popup("MOU").iter().map(|e| e.name).collect::<Vec<_>>(),
        vec!["mouse"],
        "built"
    );
    assert_eq!(
        popup("ENGINES").iter().map(|e| e.name).collect::<Vec<_>>(),
        vec!["engines"],
        "built"
    );
    assert_eq!(
        popup("EXP").iter().map(|e| e.name).collect::<Vec<_>>(),
        vec!["export"],
        "built"
    );
    assert_eq!(
        popup("cop").iter().map(|e| e.name).collect::<Vec<_>>(),
        vec!["copy"],
        "built"
    );
}

#[test]
fn help_text_lists_every_available_command_with_its_usage() {
    let text = help_text();
    for e in entries()
        .iter()
        .filter(|e| matches!(e.status, Availability::Built))
    {
        assert!(
            text.contains(&format!("/{}", e.name)),
            "{} missing:\n{text}",
            e.name
        );
        assert!(text.contains(e.help), "{} help missing", e.name);
    }
    assert!(
        text.contains("/push <runId> <git-url> [--branch <name>]"),
        "{text}"
    );
    assert!(
        text.contains("/send <text>"),
        "the way to send text that starts with a slash is explained:\n{text}"
    );
    assert!(
        !text.contains("//"),
        "the double slash is no escape:\n{text}"
    );
    assert!(
        text.lines().all(|l| l.chars().count() <= 100),
        "lines fit a narrow panel"
    );
    // Planned commands are not advertised as working.
    for e in entries()
        .iter()
        .filter(|e| matches!(e.status, Availability::Planned(_)))
    {
        assert!(
            !text.contains(&format!("/{} ", e.name)),
            "{} is planned, not available",
            e.name
        );
    }
    assert!(
        registry::command_help("cancel")
            .unwrap()
            .contains("Ask the coding agent to stop")
    );
    assert!(registry::command_help("nonsense").is_none());
}

#[test]
fn typed_forms_are_the_argv_forms() {
    // The registry never builds a command of its own: a sample's Invocation is exactly what parse returns for its argv.
    for (name, forms) in samples() {
        for rest in forms {
            let a = argv(name, &rest);
            let inv: Invocation = parse(&a).unwrap();
            assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{a:?}");
        }
    }
}

// ---- the plain phrases -------------------------------------------------------------------------------

#[test]
fn the_plain_phrases_name_built_commands_and_are_never_one_word() {
    assert!(!registry::PHRASES.is_empty());
    for (phrase, word) in registry::PHRASES {
        assert!(
            phrase.split_whitespace().count() >= 2,
            "{phrase:?} is a single word"
        );
        assert_eq!(
            *phrase,
            phrase.to_lowercase(),
            "{phrase:?} is not lower case"
        );
        let entry = subcommand(word).unwrap_or_else(|| panic!("{word} is no command"));
        assert_eq!(entry.status, Availability::Built, "{word}");
    }
    // The table of the decision: five phrases for four commands.
    let words: std::collections::BTreeSet<&str> =
        registry::PHRASES.iter().map(|(_, w)| *w).collect();
    assert_eq!(words, ["cancel", "pull", "push", "runs"].into());
}

#[test]
fn a_phrase_is_matched_as_the_whole_submission_ignoring_case_and_blanks() {
    assert_eq!(phrase_command("push my code"), Some("push"));
    assert_eq!(phrase_command("  Push   My CODE \t"), Some("push"));
    assert_eq!(phrase_command("push this run"), Some("push"));
    assert_eq!(phrase_command("pull the zip"), Some("pull"));
    assert_eq!(phrase_command("show my runs"), Some("runs"));
    assert_eq!(phrase_command("cancel this run"), Some("cancel"));
    for no in [
        "",
        "push",
        "cancel",
        "please push my code",
        "push my code now",
        "push my code\nand more",
        "/push my code",
        "ham push my code",
    ] {
        assert_eq!(phrase_command(no), None, "{no:?}");
    }
}
