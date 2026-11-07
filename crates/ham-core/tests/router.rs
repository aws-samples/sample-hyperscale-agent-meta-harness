//! The router of the chat box, as settles it: a pure function from the text of the box to what happens next.
//! No terminal types are used, so the chat box (TC1) and the offline scenario tests call the same code.
//!

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, TargetArg, parse};
use ham_core::registry::{self, EffectClass};
use ham_core::router::{
    CONFIRM_DELAY_MS, ConfirmKey, ConfirmStep, Confirmation, RouteContext, RouteOutcome, Wait,
    complete, prepare_argv, prepare_text, route, route_with, split_words,
};

const RUN: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
const OTHER: &str = "01BX5ZZKBKACTAV9WEVGEMMVRZ";
const URL: &str = "git@example.test:me/repo.git";

fn focus() -> RouteContext {
    RouteContext {
        focused_run: Some(RUN.to_string()),
        tainted: false,
    }
}

fn tainted() -> RouteContext {
    RouteContext {
        focused_run: Some(RUN.to_string()),
        tainted: true,
    }
}

fn none() -> RouteContext {
    RouteContext::default()
}

fn argv_of(o: &RouteOutcome) -> Vec<String> {
    match o {
        RouteOutcome::Command { argv, .. } => argv.clone(),
        other => panic!("not a command: {other:?}"),
    }
}

fn refused(o: &RouteOutcome) -> (&str, &[String]) {
    match o {
        RouteOutcome::Refused {
            reason,
            suggestions,
        } => (reason, suggestions),
        other => panic!("not refused: {other:?}"),
    }
}

fn message(o: &RouteOutcome) -> (&str, bool) {
    match o {
        RouteOutcome::Message { text, literal } => (text, *literal),
        other => panic!("not a message: {other:?}"),
    }
}

/// Every form of every subcommand, typed with a slash and with `ham`, equals the shell's parse of the same argv. A command with a
/// text payload takes its run through `--run` in the slash form.
#[test]
fn every_subcommand_typed_either_way_is_what_the_shell_parses() {
    // (the shell argv, the slash line, the ham line)
    let forms: Vec<(Vec<&str>, String, String)> = {
        let plain = |f: Vec<&'static str>| {
            let (slash, ham) = (format!("/{}", f.join(" ")), format!("ham {}", f.join(" ")));
            (f, slash, ham)
        };
        let mut v: Vec<(Vec<&str>, String, String)> = vec![
            plain(vec!["runs"]),
            plain(vec!["runs", "--probes"]),
            plain(vec!["watch", RUN]),
            plain(vec!["cancel", RUN]),
            plain(vec!["models"]),
            plain(vec!["models", "--server"]),
            plain(vec!["pr", RUN]),
            plain(vec!["pull", RUN]),
            plain(vec!["pull", RUN, "--artifact", "evidence"]),
            plain(vec!["clone", RUN, "dest"]),
            plain(vec!["push", RUN, URL]),
            plain(vec!["push", RUN, URL, "--branch", "work"]),
            plain(vec!["link", RUN]),
            plain(vec!["doctor"]),
            plain(vec!["rules", "init"]),
            plain(vec!["rules", "edit", "continue"]),
            plain(vec!["rules", "show"]),
            plain(vec!["config"]),
            plain(vec!["config", "get", "region"]),
            plain(vec!["config", "set", "default_model", "opus"]),
            plain(vec!["config", "set", "region", "us-west-2"]),
            plain(vec!["config", "unset", "default_repo"]),
            plain(vec!["engine", "cline", "show"]),
            plain(vec!["engine", "cline", "check"]),
            plain(vec!["engine", "cline", "set", "reasoning.effort", "low"]),
            plain(vec!["engine", "cline", "edit"]),
            plain(vec!["finish", RUN]),
            plain(vec!["lifecycle", "init", "somewhere"]),
            plain(vec!["resume", "--all"]),
            plain(vec!["resume", "--all", "--dry-run"]),
            plain(vec!["resume", "01M5A2R7K3C9XJ4D8E6FQ0WT1N"]),
            plain(vec!["rules", "init", "--pack", "lifecycle"]),
        ];
        v.push((
            vec!["say", RUN, "hello"],
            format!("/say --run {RUN} hello"),
            format!("ham say {RUN} hello"),
        ));
        v.push((
            vec!["new-session", RUN],
            format!("/new-session --run {RUN}"),
            format!("ham new-session {RUN}"),
        ));
        v.push((
            vec!["new-session", RUN, "start over"],
            format!("/new-session --run {RUN} start over"),
            format!("ham new-session {RUN} 'start over'"),
        ));
        v.push((
            vec!["continue", RUN, "go on"],
            format!("/continue --run {RUN} go on"),
            format!("ham continue {RUN} 'go on'"),
        ));
        v.push((
            vec!["continue", RUN],
            format!("/continue --run {RUN}"),
            format!("ham continue {RUN}"),
        ));
        v.push((
            vec!["try-model", RUN, "opus"],
            format!("/try-model --run {RUN} opus"),
            format!("ham try-model {RUN} opus"),
        ));
        v.push((
            vec!["try-model", RUN, "opus", "try this"],
            format!("/try-model --run {RUN} opus try this"),
            format!("ham try-model {RUN} opus \"try this\""),
        ));
        v
    };
    let mut seen = std::collections::BTreeSet::new();
    for (f, slash, ham) in &forms {
        let shell = parse(f).unwrap();
        seen.insert(f[0]);
        for line in [slash, ham] {
            match route(line, &none()) {
                RouteOutcome::Command {
                    argv,
                    invocation,
                    class,
                    preview,
                } => {
                    assert_eq!(
                        argv,
                        f.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                        "{line}"
                    );
                    assert_eq!(invocation, shell, "{line}");
                    assert_eq!(parse(&argv).unwrap(), invocation, "{line}");
                    assert_eq!(class, registry::class_of(&shell.command), "{line}");
                    assert_eq!(
                        preview.is_some(),
                        class.needs_confirm(),
                        "{line}: a preview exactly when a second Enter is needed"
                    );
                }
                other => panic!("{line}: {other:?}"),
            }
        }
    }
    // Nothing was left out: every subcommand of the registry has a form above.
    for name in registry::subcommand_names() {
        assert!(seen.contains(name), "no form of {name} in this test");
    }
}

#[test]
fn an_unknown_word_sends_nothing_and_names_the_closest_commands() {
    for line in ["/cancle", "ham cancle"] {
        let o = route(line, &focus());
        let (reason, suggestions) = refused(&o);
        assert!(reason.contains("no command \"cancle\""), "{line}: {reason}");
        assert!(reason.contains("closest: cancel"), "{line}: {reason}");
        assert_eq!(
            suggestions.first().map(String::as_str),
            Some("cancel"),
            "{line}"
        );
        assert!(suggestions.len() <= 3, "{line}: {suggestions:?}");
        assert!(
            reason.contains("/send"),
            "the way to send it as chat is named: {reason}"
        );
    }
    let o = route("/nonsense", &none());
    let (reason, suggestions) = refused(&o);
    assert!(
        reason.contains("no command \"nonsense\"") && reason.contains("/help"),
        "{reason}"
    );
    assert!(suggestions.is_empty());
    // `ham` followed by an ordinary word is an attempt at a command too; the way out is /send.
    let o = route("ham is great", &none());
    assert!(refused(&o).0.contains("no command \"is\""));
    assert!(refused(&o).0.contains("/send"));
    // A word with an upper-case letter is chat.
    for line in [
        "/CANCLE", "/Cancel", "/CANCEL", "ham Push", "HAM push", "Ham push",
    ] {
        assert_eq!(message(&route(line, &focus())), (line, false), "{line}");
    }
}

/// The registry with `word` marked as not built yet: no word of the real registry is planned since `/mouse` was built, and the
/// refusal of a planned word is the router's, so it is tested over a table.
fn with_planned(word: &str) -> Vec<registry::Entry> {
    registry::entries()
        .iter()
        .map(|e| {
            if e.name == word {
                registry::Entry {
                    status: registry::Availability::Planned("N0"),
                    ..*e
                }
            } else {
                *e
            }
        })
        .collect()
}

#[test]
fn a_command_that_is_not_built_yet_says_so_with_a_stable_text() {
    let table = with_planned("mouse");
    let word = "mouse";
    for line in [format!("/{word}"), format!("ham {word}")] {
        let o = route_with(&table, &line, &none());
        let (reason, suggestions) = refused(&o);
        assert_eq!(reason, format!("{word} is not available in this release"));
        assert!(suggestions.is_empty());
    }
    // Built, the same word is the screen's own command.
    match route("/mouse off", &none()) {
        RouteOutcome::Ui { word, args } => {
            assert_eq!(word, "mouse");
            assert_eq!(args, ["off"]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn send_is_the_one_way_to_send_literal_text_and_a_double_slash_is_refused() {
    for (typed, sent) in [
        ("/send /cancel", "/cancel"),
        ("/send //cancel", "//cancel"),
        ("/send ham push", "ham push"),
        ("/send hello there", "hello there"),
        // The payload starts after the single separating space: the rest is byte for byte.
        ("/send   two", "  two"),
        (
            "/send \"quoted\" it's \\ -- --cancel",
            "\"quoted\" it's \\ -- --cancel",
        ),
        ("/send a\r\nb\n\n", "a\r\nb\n\n"),
        ("/send multi\nline  \n", "multi\nline  \n"),
        ("/send\nafter a line break", "after a line break"),
        ("/send\tafter a tab", "after a tab"),
    ] {
        let o = route(typed, &focus());
        assert_eq!(message(&o), (sent, true), "{typed:?}");
        // The same through the other forms of the grammar.
        let o = route(&typed.replacen("/send", "ham send", 1), &focus());
        assert_eq!(message(&o), (sent, true), "{typed:?}");
    }
    for line in ["/send", "/send ", "/send   ", "/send\n"] {
        let o = route(line, &focus());
        assert!(refused(&o).0.contains("send"), "{line:?}: {o:?}");
    }
    // `//` carries no meaning of its own: the line is refused and the hint names /send.
    for line in ["//cancel", "// hello", "//", "//send hi"] {
        let o = route(line, &focus());
        let (reason, suggestions) = refused(&o);
        assert!(reason.contains("/send"), "{line}: {reason}");
        assert!(suggestions.is_empty());
    }
    // A leading space carries no meaning either: the line is chat, exactly as typed (it is not a command attempt).
    for line in [
        " /cancel",
        "  /cancel",
        " ham push",
        " hello there",
        "\t/cancel",
    ] {
        assert_eq!(message(&route(line, &focus())), (line, false), "{line:?}");
    }
}

#[test]
fn ordinary_text_is_a_message_verbatim() {
    for text in [
        "build me a todo api",
        "/usr/bin/env is missing",
        "/src/main.rs has a bug",
        "/2 more things",
        "/cancel.",
        "/cancel,",
        "ham, thanks",
        "hamlet",
        "ham 2 times",
        "Please /cancel the job",
        "line one\nline two",
        "ham",
        "ham ",
        "x",
        "/\u{a0}cancel",
        "/cancel\u{a0}now",
        "ham\u{a0}push",
        "\u{a0}/cancel",
        "/under_score",
    ] {
        assert_eq!(message(&route(text, &focus())), (text, false), "{text:?}");
    }
    assert_eq!(route("", &focus()), RouteOutcome::Empty);
    assert_eq!(route("   \n ", &focus()), RouteOutcome::Empty);
    assert_eq!(route(" ", &focus()), RouteOutcome::Empty);
}

#[test]
fn a_lone_slash_asks_for_a_command_name_and_a_slash_and_a_space_before_text_is_chat() {
    // Nothing after the slash: there is no word to run (the popup handles a `/` it has open).
    for line in ["/", "/ ", "/\n", "/   \n\n"] {
        let o = route(line, &none());
        let (reason, _) = refused(&o);
        assert!(
            reason.contains("command name") && reason.contains("/help"),
            "{line:?}: {reason}"
        );
    }
    // A slash, a space and text is not `/word`: it is chat, as typed.
    for line in ["/ hello", "/ cancel", "/\tcancel", "/ fix the build"] {
        assert_eq!(message(&route(line, &focus())), (line, false), "{line:?}");
    }
}

#[test]
fn tainted_text_previews_first_whatever_its_class() {
    // Read-only commands and local writes that would run at once now ask for a second Enter; nothing else about them changes.
    for line in [
        "/runs",
        "/models",
        "/doctor",
        "/pr",
        "/pull",
        "ham runs --probes",
        "/rules show",
    ] {
        let typed = route(line, &focus());
        let t = route(line, &tainted());
        match (&typed, &t) {
            (
                RouteOutcome::Command {
                    argv: a1,
                    invocation: i1,
                    preview: None,
                    ..
                },
                RouteOutcome::Command {
                    argv: a2,
                    invocation: i2,
                    class,
                    preview: Some(p),
                },
            ) => {
                assert_eq!((a1, i1), (a2, i2), "{line}: the same invocation");
                assert!(!class.needs_confirm(), "{line}: the class is unchanged");
                assert!(
                    p.text.contains(&format!("ham {}", a2.join(" "))),
                    "{line}: the full resolved invocation is shown: {}",
                    p.text
                );
                assert!(!p.text.contains('\n'));
                assert!(p.confirm_with.is_none(), "{line}");
            }
            other => panic!("{line}: {other:?}"),
        }
        assert!(Confirmation::new(&t, 0).is_some(), "{line}");
        assert!(Confirmation::new(&typed, 0).is_none(), "{line}");
    }
    // A command that always previews keeps its own preview.
    assert_eq!(route("/cancel", &tainted()), route("/cancel", &focus()));
    // A refusal is the same; chat is unchanged; a start form only opens the form.
    assert_eq!(route("/cancle", &tainted()), route("/cancle", &focus()));
    assert_eq!(route("/export", &tainted()), route("/export", &focus()));
    assert_eq!(
        route("hello /cancel", &tainted()),
        route("hello /cancel", &focus())
    );
    assert_eq!(
        route("/send /cancel", &tainted()),
        route("/send /cancel", &focus())
    );
    assert!(matches!(
        route("ham \"x\" --s3-only --direct", &tainted()),
        RouteOutcome::StartForm { .. }
    ));
    assert!(matches!(
        route("/help", &tainted()),
        RouteOutcome::Ui { word: "help", .. }
    ));
}

#[test]
fn the_run_id_defaults_to_the_focused_run_by_the_count_of_arguments() {
    let argv = |line: &str, ctx: &RouteContext| argv_of(&route(line, ctx));
    assert_eq!(argv("/cancel", &focus()), ["cancel", RUN]);
    assert_eq!(
        argv("/cancel", &focus()),
        argv(&format!("/cancel {RUN}"), &none())
    );
    // An explicit run id is the one used, whatever has focus; it is the shell's form and the server answers for a stale one.
    assert_eq!(
        argv(&format!("/cancel {OTHER}"), &focus()),
        ["cancel", OTHER]
    );
    assert_eq!(argv("/cancel notarun", &focus()), ["cancel", "notarun"]);
    assert_eq!(
        argv("/pull --artifact partial", &focus()),
        ["pull", RUN, "--artifact", "partial"]
    );
    assert_eq!(
        argv(&format!("/pull {OTHER} --artifact evidence"), &focus()),
        ["pull", OTHER, "--artifact", "evidence"]
    );
    assert_eq!(argv("/clone", &focus()), ["clone", RUN]);
    assert_eq!(argv(&format!("/clone {OTHER}"), &focus()), ["clone", OTHER]);
    assert_eq!(
        argv(&format!("/clone {OTHER} mydir"), &focus()),
        ["clone", OTHER, "mydir"]
    );
    assert_eq!(argv("/watch", &focus()), ["watch", RUN]);
    assert_eq!(argv(&format!("/watch {OTHER}"), &focus()), ["watch", OTHER]);
    assert_eq!(argv("/finish", &focus()), ["finish", RUN]);
    assert_eq!(
        argv("/finish --no-wait", &focus()),
        ["finish", RUN, "--no-wait"]
    );
    assert_eq!(argv("/link", &focus()), ["link", RUN]);
    // With one argument less than the form needs, the missing one is the run id (push: the URL is given, the run is not).
    assert_eq!(argv(&format!("/push {URL}"), &focus()), ["push", RUN, URL]);
    assert_eq!(
        argv(&format!("/push {OTHER} {URL}"), &focus()),
        ["push", OTHER, URL]
    );
    // No focus and no run id: the parser's own words, nothing is guessed.
    let o = route("/cancel", &none());
    assert!(refused(&o).0.contains("ham cancel needs <runId>"), "{o:?}");
    // Commands without a run id are left alone.
    assert_eq!(argv("/runs", &focus()), ["runs"]);
    assert_eq!(
        argv("/config get region", &focus()),
        ["config", "get", "region"]
    );
}

#[test]
fn a_text_payload_is_never_tokenised_and_never_read_as_a_target() {
    let text_of = |o: &RouteOutcome| match o {
        RouteOutcome::Command { invocation, .. } => match &invocation.command {
            Command::Say { run_id, text, .. } => (run_id.clone(), text.clone()),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    // The focused run is the target; the text is the rest of the line after the single separating space.
    assert_eq!(
        text_of(&route("/say hello there", &focus())),
        (RUN.into(), "hello there".into())
    );
    assert_eq!(
        text_of(&route("/say  two", &focus())),
        (RUN.into(), " two".into())
    );
    assert_eq!(
        text_of(&route("/say thanks, carry on", &focus())),
        (RUN.into(), "thanks, carry on".into())
    );
    // Quotes, backslashes, an apostrophe, a dash, a run-id-looking word: all text, exactly as typed.
    for payload in [
        "\"quoted\" it's a \\ test",
        "don't break the tests",
        "-- not an option",
        "--cancel it",
        "-x",
        OTHER,
        "01BX5ZZKBKACTAV9WEVGEMMVRZ and more",
        "a\r\nb\n\n",
        "line one\nline two",
        "trailing   ",
    ] {
        let o = route(&format!("/say {payload}"), &focus());
        assert_eq!(text_of(&o), (RUN.into(), payload.into()), "{payload:?}");
        // What the parser is given reads back as the same invocation.
        let RouteOutcome::Command { invocation, .. } = &o else {
            unreachable!()
        };
        assert_eq!(&parse(&argv_of(&o)).unwrap(), invocation);
    }
    // A payload that starts with a dash goes in as --text, as the shell form needs.
    assert_eq!(
        argv_of(&route("/say --cancel now", &focus())),
        ["say", RUN, "--text", "--cancel now"]
    );
    // Another run only through --run.
    assert_eq!(
        text_of(&route(&format!("/say --run {OTHER} hi"), &focus())),
        (OTHER.into(), "hi".into())
    );
    assert_eq!(
        text_of(&route(&format!("/say --run {OTHER}  hi"), &none())),
        (OTHER.into(), " hi".into()),
        "one separating space after the run id"
    );
    for line in ["/say --run notaulid hi", "/say --run", "/say --run "] {
        let o = route(line, &focus());
        assert!(refused(&o).0.contains("--run"), "{line}: {o:?}");
    }
    // No focus and no --run: nothing is guessed.
    assert!(refused(&route("/say hi", &none())).0.contains("--run"));
    // No text is a usage error from the parser; a lone dash is standard input in the shell and has no meaning in the box.
    assert!(
        refused(&route("/say", &focus()))
            .0
            .contains("ham say needs <text>")
    );
    assert!(
        refused(&route("/say    ", &focus()))
            .0
            .contains("ham say needs <text>")
    );
    let dash = route("/say -", &focus());
    assert!(refused(&dash).0.contains("standard input"), "{dash:?}");
    assert!(
        refused(&dash).0.contains("/send -"),
        "the way to send a dash is named: {dash:?}"
    );
    assert_eq!(message(&route("/send -", &focus())), ("-", true));
    // new-session: the text is optional; try-model: a model first, then the optional text.
    assert_eq!(
        argv_of(&route("/new-session", &focus())),
        ["new-session", RUN]
    );
    assert_eq!(
        argv_of(&route("/new-session start over", &focus())),
        ["new-session", RUN, "start over"]
    );
    assert_eq!(
        argv_of(&route("/try-model opus", &focus())),
        ["try-model", RUN, "opus"]
    );
    assert_eq!(
        argv_of(&route("/try-model opus try this \"again\"", &focus())),
        ["try-model", RUN, "opus", "try this \"again\""]
    );
    assert_eq!(
        argv_of(&route(
            &format!("/try-model --run {OTHER} opus hi"),
            &focus()
        )),
        ["try-model", OTHER, "opus", "hi"]
    );
    assert!(
        refused(&route("/try-model", &focus()))
            .0
            .contains("<model>")
    );
    // The `ham` form is the shell's form, run id first and the text as one word.
    assert_eq!(
        argv_of(&route(&format!("ham say {RUN} \"two words\""), &none())),
        ["say", RUN, "two words"]
    );
}

#[test]
fn the_ham_form_of_a_text_command_never_takes_the_focused_run() {
    // Only a command without a text payload may omit its run id. `ham say <runId> <text>` is the shell form; a run id
    // alone must not become the text of a message to the focused run.
    for line in [
        format!("ham say {OTHER}"),
        "ham say hello".to_string(),
        format!("ham new-session {OTHER} --no-wait"),
        "ham new-session".to_string(),
        "ham try-model opus".to_string(),
        format!("ham try-model {OTHER}"),
    ] {
        let o = route(&line, &focus());
        match &o {
            RouteOutcome::Refused { reason, .. } => {
                assert!(reason.starts_with("usage: "), "{line}: {reason}")
            }
            RouteOutcome::Command { argv, .. } => assert_eq!(
                argv.get(1).map(String::as_str),
                Some(OTHER),
                "{line}: {argv:?}"
            ),
            other => panic!("{line}: {other:?}"),
        }
    }
    // The commands without a payload still default to the focused run in the `ham` form.
    assert_eq!(argv_of(&route("ham cancel", &focus())), ["cancel", RUN]);
    assert_eq!(
        argv_of(&route("ham finish --no-wait", &focus())),
        ["finish", RUN, "--no-wait"]
    );
}

#[test]
fn the_hotkeys_prepare_a_text_command_the_way_the_typed_line_does() {
    // `p`, `s` and `m` hand the run, the model and the text of the Reply screen to prepare_text.
    assert_eq!(
        prepare_text("say", RUN, &[], "-- hi", false),
        route("/say -- hi", &focus())
    );
    assert_eq!(
        prepare_text("say", OTHER, &[], "hello there", false),
        route(&format!("/say --run {OTHER} hello there"), &none())
    );
    assert_eq!(
        prepare_text("new-session", RUN, &[], "", false),
        route("/new-session", &focus())
    );
    assert_eq!(
        prepare_text("new-session", RUN, &[], "start over", false),
        route("/new-session start over", &focus())
    );
    assert_eq!(
        prepare_text("try-model", RUN, &["opus".to_string()], "go on", false),
        route("/try-model opus go on", &focus())
    );
    // The text goes in as typed, quotes and all, and a lone dash is refused as it is in the box.
    assert_eq!(
        argv_of(&prepare_text("say", RUN, &[], "it's \"fine\"", false)),
        ["say", RUN, "it's \"fine\""]
    );
    assert!(
        refused(&prepare_text("say", RUN, &[], "-", false))
            .0
            .contains("/send -")
    );
    // A command that takes no text is refused, as is a missing run, a missing model and an empty message.
    assert!(matches!(
        prepare_text("cancel", RUN, &[], "x", false),
        RouteOutcome::Refused { .. }
    ));
    assert!(matches!(
        prepare_text("try-model", RUN, &[], "x", false),
        RouteOutcome::Refused { .. }
    ));
    assert!(matches!(
        prepare_text("say", RUN, &[], "  ", false),
        RouteOutcome::Refused { .. }
    ));
    // Tainted text previews first, whatever the class would be.
    assert!(Confirmation::new(&prepare_text("say", RUN, &[], "hi", true), 0).is_some());
}

#[test]
fn the_words_of_a_prefix_split_like_a_shell_and_the_payload_does_not() {
    let text_of = |o: &RouteOutcome| match o {
        RouteOutcome::Command { invocation, .. } => match &invocation.command {
            Command::Say { run_id, text, .. } => (run_id.clone(), text.clone()),
            Command::TryModel {
                run_id,
                model,
                text,
                ..
            } => (
                format!("{run_id} {model}"),
                text.clone().unwrap_or_default(),
            ),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    assert_eq!(
        text_of(&route(
            &format!("/say --run \"{OTHER}\" 'quoted' stays"),
            &focus()
        )),
        (OTHER.into(), "'quoted' stays".into())
    );
    assert_eq!(
        text_of(&route("/try-model 'opus' \"go on\"", &focus())),
        (format!("{RUN} opus"), "\"go on\"".into())
    );
    assert_eq!(
        text_of(&route(&format!("/say --run '{OTHER}' hi"), &none())),
        (OTHER.into(), "hi".into())
    );
    // An open quote in the prefix is a usage error; the payload is never read for quotes.
    let o = route("/say --run \"01BX hi", &focus());
    assert!(
        refused(&o).0.contains("quote") || refused(&o).0.contains("--run"),
        "{o:?}"
    );
    assert_eq!(
        text_of(&route("/say don't \"close", &focus())),
        (RUN.into(), "don't \"close".into())
    );
}

#[test]
fn a_command_is_one_line() {
    for line in [
        "/cancel\nblah",
        "ham cancel\nblah",
        "/runs\n\nmore",
        "/config\nget",
    ] {
        let o = route(line, &focus());
        let (reason, _) = refused(&o);
        assert!(
            reason.contains("commands are one line") && reason.contains("/send"),
            "{line:?}: {reason}"
        );
    }
    // Trailing blank lines and trailing spaces are ignored.
    for line in [
        "/cancel\n",
        "/cancel\n\n  \n",
        "/cancel   ",
        "/cancel \t\n",
        "ham cancel \n",
    ] {
        assert_eq!(argv_of(&route(line, &focus())), ["cancel", RUN], "{line:?}");
    }
}

#[test]
fn a_command_that_changes_a_run_shows_a_preview_and_needs_a_second_enter() {
    let o = route("/cancel", &focus());
    match &o {
        RouteOutcome::Command { class, preview, .. } => {
            assert_eq!(*class, EffectClass::ChangesRun);
            let p = preview.as_ref().unwrap();
            assert!(
                p.text.contains(RUN) && p.text.to_lowercase().contains("cancel"),
                "{}",
                p.text
            );
            assert!(!p.text.contains('\n'));
            assert!(p.confirm_with.is_none());
        }
        other => panic!("{other:?}"),
    }
    for (line, words, typed) in [
        ("/finish", vec!["Finish", RUN], None),
        ("/new-session", vec!["fresh", RUN], None),
        (
            "/new-session start over",
            vec!["fresh", RUN, "message"],
            None,
        ),
        ("/try-model opus", vec!["Switch", RUN, "opus"], None),
        ("/say hello", vec!["Send", RUN, "hello"], None),
        // A setting that decides where requests go, who signs them or where downloads land asks for its key name.
        (
            "/config set region us-east-2",
            vec!["region", "us-east-2"],
            Some("region"),
        ),
        (
            "/config unset aws_profile",
            vec!["aws_profile"],
            Some("aws_profile"),
        ),
        (
            "/config set api_endpoint https://example.test/live",
            vec!["api_endpoint", "https://example.test/live"],
            Some("api_endpoint"),
        ),
        (
            "/config set downloadDir /somewhere",
            vec!["download_dir", "/somewhere"],
            Some("download_dir"),
        ),
        // An editor takes the terminal over; with every effect other than a local write the class is run-changing.
        ("/rules edit", vec!["editor"], None),
    ] {
        match route(line, &focus()) {
            RouteOutcome::Command {
                class: EffectClass::ChangesRun,
                preview: Some(p),
                ..
            } => {
                for w in words {
                    assert!(p.text.contains(w), "{line}: {w:?} not in {:?}", p.text);
                }
                assert_eq!(p.confirm_with.as_deref(), typed, "{line}");
            }
            other => panic!("{line}: {other:?}"),
        }
    }
    // Read-only commands and plain local writes run at once: no preview.
    for line in [
        "/runs",
        "/models",
        "/doctor",
        "/pr",
        "/link",
        "/pull",
        "/clone",
        "/config",
        "/config get region",
        "/config set default_model opus",
        "/config set default_repo cc:x",
        "/rules show",
        "/rules init",
    ] {
        match route(line, &focus()) {
            RouteOutcome::Command { preview, class, .. } => {
                assert!(preview.is_none(), "{line}");
                assert!(!class.needs_confirm(), "{line}");
            }
            other => panic!("{line}: {other:?}"),
        }
    }
}

#[test]
fn a_push_preview_names_the_run_the_remote_and_the_branch_and_asks_for_the_last_four_characters() {
    let o = route(&format!("/push {URL}"), &focus());
    let RouteOutcome::Command {
        argv,
        preview: Some(p),
        class,
        ..
    } = o
    else {
        panic!("{o:?}")
    };
    assert_eq!(class, EffectClass::ChangesRun);
    assert_eq!(argv, ["push", RUN, URL]);
    assert!(p.text.contains(RUN) && p.text.contains(URL), "{}", p.text);
    assert!(
        p.text.contains(&format!("harness/{RUN}")),
        "the default branch is named: {}",
        p.text
    );
    assert_eq!(p.confirm_with.as_deref(), Some("5FAV"));
    // A branch that was given is the one named.
    let o = route(&format!("/push {URL} --branch release-1"), &focus());
    let RouteOutcome::Command {
        preview: Some(p), ..
    } = o
    else {
        panic!("{o:?}")
    };
    assert!(
        p.text.contains("release-1") && !p.text.contains("harness/"),
        "{}",
        p.text
    );
    // A URL that carries a credential never reaches a preview: the parser refuses it and the message never repeats it.
    let userinfo = format!("{}:{}", "user", "pw1234");
    let secret_url = format!("https://{userinfo}@example.test/x.git");
    let o = route(&format!("/push {secret_url}"), &focus());
    let (reason, _) = refused(&o);
    assert!(!reason.contains("pw1234"), "{reason}");
}

#[test]
fn push_without_a_url_asks_for_it_and_the_answer_goes_through_the_same_parser() {
    for line in ["/push", "ham push", "/push --branch x"] {
        let o = route(line, &focus());
        match o {
            RouteOutcome::Incomplete {
                ref argv,
                missing,
                class,
            } => {
                assert_eq!(missing, "git-url", "{line}");
                assert_eq!(class, EffectClass::ChangesRun);
                assert_eq!(argv[..2], ["push", RUN], "{line}: {argv:?}");
            }
            other => panic!("{line}: {other:?}"),
        }
    }
    let RouteOutcome::Incomplete { argv, .. } = route("/push", &focus()) else {
        panic!()
    };
    // An empty answer is refused; a good one gives the same command as typing it all.
    assert!(matches!(complete(&argv, ""), RouteOutcome::Refused { .. }));
    assert!(matches!(
        complete(&argv, "   "),
        RouteOutcome::Refused { .. }
    ));
    let full = route(&format!("/push {URL}"), &focus());
    assert_eq!(complete(&argv, URL), full);
    // A bad URL is the parser's refusal, not a command.
    assert!(matches!(
        complete(&argv, "-ofoo=x"),
        RouteOutcome::Refused { .. }
    ));
    // Without a run to push there is nothing to ask for: the parser says what is missing.
    assert!(matches!(
        route("/push", &none()),
        RouteOutcome::Refused { .. }
    ));
}

#[test]
fn the_start_form_opens_the_form_and_starts_nothing() {
    for line in [
        "ham \"build X\" --s3-only --direct",
        "ham 'build X' --s3-only --direct",
        "ham --prompt \"build X\" --s3-only --direct",
        "ham \"build X\" --s3-only --direct --model opus --input-wait 5",
    ] {
        match route(line, &focus()) {
            RouteOutcome::StartForm { args } => {
                assert_eq!(args.prompt, "build X", "{line}");
                assert_eq!(args.target, TargetArg::S3Only);
            }
            other => panic!("{line}: {other:?}"),
        }
    }
    // An unknown bare word with exactly one repository option and --direct is a start form too.
    for (line, prompt) in [
        ("ham build --s3-only --direct", "build"),
        ("ham pul --new-repo cc:team-new --direct", "pul"),
        (
            "ham refactor --repo cc:team-app --direct --model opus",
            "refactor",
        ),
    ] {
        match route(line, &focus()) {
            RouteOutcome::StartForm { args } => assert_eq!(args.prompt, prompt, "{line}"),
            other => panic!("{line}: {other:?}"),
        }
    }
    // It must parse with the shell's parser; otherwise the usage error shows and nothing is sent.
    for line in [
        "ham \"build X\"",
        "ham \"build X\" --s3-only",
        "ham --direct",
        "ham \"a\" \"b\" --s3-only --direct",
        "ham --nope",
        "ham build the app --s3-only --direct",
    ] {
        let o = route(line, &focus());
        let (reason, _) = refused(&o);
        assert!(
            reason.starts_with("usage: ") || reason.contains("no command"),
            "{line}: {reason}"
        );
    }
    // An unknown word without exactly one repository option and --direct is a refusal, not a start form.
    for line in [
        "ham build --s3-only",
        "ham build --direct",
        "ham build --s3-only --repo cc:x --direct",
        "ham build",
    ] {
        let o = route(line, &focus());
        assert!(
            refused(&o).0.contains("no command \"build\""),
            "{line}: {o:?}"
        );
    }
    // A registry word never falls back to a start form: its own parse error is shown.
    let o = route("ham pull --s3-only --direct", &focus());
    assert!(refused(&o).0.starts_with("usage: "), "{o:?}");
    // A quoted word that is a command name is read as that command by the shell; the form is not opened.
    let o = route("ham \"runs\" --s3-only --direct", &focus());
    assert!(matches!(o, RouteOutcome::Refused { .. }), "{o:?}");
    let o = route("ham \"runs\"", &focus());
    let (reason, _) = refused(&o);
    assert!(reason.contains("--prompt"), "{reason}");
    // The slash form has no start form.
    let o = route("/build --s3-only --direct", &focus());
    assert!(refused(&o).0.contains("no command \"build\""), "{o:?}");
}

#[test]
fn words_of_the_terminal_app_are_handed_back_not_parsed() {
    assert_eq!(
        route("/help", &none()),
        RouteOutcome::Ui {
            word: "help",
            args: vec![]
        }
    );
    assert_eq!(
        route("ham help", &none()),
        RouteOutcome::Ui {
            word: "help",
            args: vec![]
        }
    );
    assert_eq!(
        route("/help cancel", &none()),
        RouteOutcome::Ui {
            word: "help",
            args: vec!["cancel".into()]
        }
    );
    assert_eq!(
        route("/new", &none()),
        RouteOutcome::Ui {
            word: "new",
            args: vec![]
        }
    );
    assert_eq!(
        route("/open", &focus()),
        RouteOutcome::Ui {
            word: "open",
            args: vec![RUN.into()]
        }
    );
    assert_eq!(
        route(&format!("/open {OTHER}"), &focus()),
        RouteOutcome::Ui {
            word: "open",
            args: vec![OTHER.into()]
        }
    );
    // The help text comes from the registry.
    assert_eq!(registry::help_for(&[]), registry::help_text());
    assert!(registry::help_for(&["push".to_string()]).contains("<git-url>"));
    assert!(registry::help_for(&["nonsense".to_string()]).contains("no command \"nonsense\""));
}

#[test]
fn quotes_and_escapes_split_like_a_shell_and_an_open_quote_is_a_usage_error() {
    assert_eq!(split_words("a b  c\td").unwrap(), ["a", "b", "c", "d"]);
    assert_eq!(
        split_words("say \"two words\" 'and three words'").unwrap(),
        ["say", "two words", "and three words"]
    );
    assert_eq!(split_words("a\\ b").unwrap(), ["a b"]);
    assert_eq!(
        split_words("\"a \\\"quoted\\\" word\"").unwrap(),
        ["a \"quoted\" word"]
    );
    assert_eq!(split_words("'it''s'").unwrap(), ["its"]);
    assert_eq!(split_words("\"\"").unwrap(), [""]);
    assert_eq!(split_words("a\nb").unwrap(), ["a", "b"]);
    // No expansion of any kind.
    assert_eq!(split_words("$HOME ~ *").unwrap(), ["$HOME", "~", "*"]);
    assert!(split_words("say \"open").is_err());
    assert!(split_words("say 'open").is_err());
    let o = route("/config set region \"unclosed", &focus());
    let (reason, _) = refused(&o);
    assert!(
        reason.starts_with("usage: ") && reason.contains("quote"),
        "{o:?}"
    );
}

#[test]
fn control_characters_in_a_command_are_refused_but_a_payload_is_kept_byte_for_byte() {
    // The words of a command may not hide an escape sequence or a bell.
    for line in [
        "/cancel \u{1b}[2J",
        "ham cancel \u{0}",
        "/say --run \u{7}x hi",
        "/try-model \u{1b}[31mopus go",
        "ham say \u{1b}x hi",
    ] {
        let o = route(line, &focus());
        assert!(matches!(o, RouteOutcome::Refused { .. }), "{line:?}: {o:?}");
        assert!(!matches!(o, RouteOutcome::Command { .. }), "{line:?}");
    }
    // A payload is exactly what was typed: the engine path sanitizes what is shown.
    assert_eq!(
        message(&route("/send \u{1b}[31mred\u{7}", &focus())),
        ("\u{1b}[31mred\u{7}", true)
    );
    match route("/say hi \u{7} there \u{1b}[2J", &focus()) {
        RouteOutcome::Command { invocation, .. } => match invocation.command {
            Command::Say { text, .. } => assert_eq!(text, "hi \u{7} there \u{1b}[2J"),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
    // A message keeps its characters: nothing here runs.
    assert!(matches!(
        route("hello \u{1b}[2J", &focus()),
        RouteOutcome::Message { .. }
    ));
}

#[test]
fn only_ascii_white_space_separates_words() {
    // White space means a space or a tab (and the line break). A no-break space is part of a word.
    assert_eq!(split_words("a\u{a0}b c").unwrap(), ["a\u{a0}b", "c"]);
    assert_eq!(split_words("a\u{3000}b").unwrap(), ["a\u{3000}b"]);
    assert_eq!(split_words("a\r\nb\rc").unwrap(), ["a", "b", "c"]);
    let o = route("/cancel \u{a0}", &focus());
    assert!(matches!(o, RouteOutcome::Refused { .. }), "{o:?}");
    let o = route("/cancel \u{2003}more", &focus());
    assert!(matches!(o, RouteOutcome::Refused { .. }), "{o:?}");
}

#[test]
fn a_bare_carriage_return_ends_the_line_of_a_command() {
    let o = route("/cancel\r01BX5ZZKBKACTAV9WEVGEMMVRZ", &focus());
    assert!(refused(&o).0.contains("commands are one line"), "{o:?}");
    let o = route("/cancel\r\nmore", &focus());
    assert!(refused(&o).0.contains("commands are one line"), "{o:?}");
    for line in ["/cancel\r\n", "/cancel\r", "/cancel \r\n\r\n"] {
        assert_eq!(argv_of(&route(line, &focus())), ["cancel", RUN], "{line:?}");
    }
}

#[test]
fn aliases_lead_to_the_entry_and_the_argv_has_the_canonical_word() {
    let cancel = registry::entries()
        .iter()
        .find(|e| e.name == "cancel")
        .copied()
        .unwrap();
    let table = [registry::Entry {
        aliases: &["stop"],
        ..cancel
    }];
    match route_with(&table, "/stop", &focus()) {
        RouteOutcome::Command { argv, .. } => assert_eq!(argv, ["cancel", RUN]),
        other => panic!("{other:?}"),
    }
    let o = route_with(&table, "/stpo", &focus());
    assert_eq!(refused(&o).1, ["cancel"]);
}

#[test]
fn the_hotkeys_and_the_popup_prepare_the_same_invocation_as_the_typed_line() {
    // `c c`, `f f`, the popup and the chip hand the registry word and the run to prepare_argv.
    assert_eq!(
        prepare_argv(&["cancel".into(), RUN.into()], false),
        route("/cancel", &focus())
    );
    assert_eq!(
        prepare_argv(&["finish".into(), RUN.into()], false),
        route("/finish", &focus())
    );
    assert_eq!(
        prepare_argv(&["runs".into()], true),
        route("/runs", &tainted())
    );
    assert!(Confirmation::new(&prepare_argv(&["cancel".into(), RUN.into()], false), 0).is_some());
    // A word the registry does not have as a subcommand, or an argv the parser refuses, is a refusal.
    assert!(matches!(
        prepare_argv(&["nonsense".into()], false),
        RouteOutcome::Refused { .. }
    ));
    assert!(matches!(
        prepare_argv(&["cancel".into()], false),
        RouteOutcome::Refused { .. }
    ));
    assert!(matches!(
        prepare_argv(&[], false),
        RouteOutcome::Refused { .. }
    ));
}

#[test]
fn a_confirmation_waits_for_a_second_enter_not_too_soon_and_escape_cancels() {
    let o = route("/cancel", &focus());
    let mut c = Confirmation::new(&o, 10_000).expect("a cancel needs a confirmation");
    assert_eq!(c.argv(), ["cancel", RUN]);
    assert_eq!(
        c.invocation().command,
        Command::Cancel { run_id: RUN.into() }
    );
    assert!(c.preview().text.contains(RUN));
    // The Enter that sent the line, and a repeat in the burst after it, do not confirm.
    assert_eq!(
        c.press(ConfirmKey::Enter, 10_000),
        ConfirmStep::Waiting(Wait::TooSoon)
    );
    assert_eq!(
        c.press(ConfirmKey::Enter, 10_000 + CONFIRM_DELAY_MS - 1),
        ConfirmStep::Waiting(Wait::TooSoon)
    );
    // Other keys do nothing; the second Enter after the delay runs it.
    assert_eq!(
        c.press(ConfirmKey::Char('x'), 10_500),
        ConfirmStep::Waiting(Wait::Idle)
    );
    assert_eq!(
        c.press(ConfirmKey::Enter, 10_000 + CONFIRM_DELAY_MS),
        ConfirmStep::Run
    );
    // Escape cancels at any time, even at once.
    let mut c = Confirmation::new(&o, 20_000).unwrap();
    assert_eq!(c.press(ConfirmKey::Esc, 20_000), ConfirmStep::Cancelled);
}

#[test]
fn a_confirmation_is_spent_after_it_ran_or_was_cancelled() {
    let o = route("/cancel", &focus());
    // Run once; a repeated Enter must not run it again.
    let mut c = Confirmation::new(&o, 0).unwrap();
    assert_eq!(
        c.press(ConfirmKey::Enter, CONFIRM_DELAY_MS),
        ConfirmStep::Run
    );
    for key in [ConfirmKey::Enter, ConfirmKey::Char('x'), ConfirmKey::Esc] {
        assert_eq!(
            c.press(key, CONFIRM_DELAY_MS * 3),
            ConfirmStep::Spent,
            "{key:?}"
        );
    }
    // Escape, then a later Enter: nothing runs.
    let mut c = Confirmation::new(&o, 0).unwrap();
    assert_eq!(c.press(ConfirmKey::Esc, 10), ConfirmStep::Cancelled);
    assert_eq!(
        c.press(ConfirmKey::Enter, CONFIRM_DELAY_MS * 3),
        ConfirmStep::Spent
    );
    // A typed answer is spent the same way.
    let mut c = Confirmation::new(&route(&format!("/push {URL}"), &focus()), 0).unwrap();
    for ch in "5FAV".chars() {
        c.press(ConfirmKey::Char(ch), CONFIRM_DELAY_MS);
    }
    assert_eq!(
        c.press(ConfirmKey::Enter, CONFIRM_DELAY_MS),
        ConfirmStep::Run
    );
    assert_eq!(
        c.press(ConfirmKey::Enter, CONFIRM_DELAY_MS),
        ConfirmStep::Spent
    );
}

#[test]
fn a_confirmation_does_not_retarget_when_the_focus_moves() {
    let first = route("/cancel", &focus());
    let c = Confirmation::new(&first, 0).unwrap();
    // The focused run changes while the preview is up: a new line gets the new run, the open confirmation keeps its own.
    let moved = RouteContext {
        focused_run: Some(OTHER.to_string()),
        tainted: false,
    };
    let second = route("/cancel", &moved);
    assert_eq!(argv_of(&second), ["cancel", OTHER]);
    assert_eq!(c.argv(), ["cancel", RUN]);
    assert!(c.preview().text.contains(RUN) && !c.preview().text.contains(OTHER));
}

#[test]
fn push_is_confirmed_by_typing_the_last_four_characters_of_the_run_id() {
    let o = route(&format!("/push {URL}"), &focus());
    let mut c = Confirmation::new(&o, 0).unwrap();
    assert_eq!(c.expected(), Some("5FAV"));
    let late = CONFIRM_DELAY_MS + 50;
    // Enter alone is not enough.
    assert_eq!(
        c.press(ConfirmKey::Enter, late),
        ConfirmStep::Waiting(Wait::TypeAnswer)
    );
    // A wrong answer is kept visible and refused.
    for ch in "5fax".chars() {
        c.press(ConfirmKey::Char(ch), late);
    }
    assert_eq!(c.typed(), "5fax");
    assert_eq!(
        c.press(ConfirmKey::Enter, late),
        ConfirmStep::Waiting(Wait::Mismatch)
    );
    c.press(ConfirmKey::Backspace, late);
    // The answer is exactly the characters asked for, case aside: an extra character is a mismatch, not dropped.
    c.press(ConfirmKey::Char('v'), late);
    c.press(ConfirmKey::Char('v'), late);
    assert_eq!(c.typed(), "5favv");
    assert_eq!(
        c.press(ConfirmKey::Enter, late),
        ConfirmStep::Waiting(Wait::Mismatch)
    );
    c.press(ConfirmKey::Backspace, late);
    assert_eq!(c.typed(), "5fav");
    assert_eq!(c.press(ConfirmKey::Enter, late), ConfirmStep::Run);
    // Typed characters that arrive before the delay (a paste, or an injected burst) are ignored.
    let mut c = Confirmation::new(&o, 1_000).unwrap();
    for ch in "5FAV".chars() {
        c.press(ConfirmKey::Char(ch), 1_010);
    }
    assert_eq!(c.typed(), "");
    assert_eq!(
        c.press(ConfirmKey::Enter, 1_020),
        ConfirmStep::Waiting(Wait::TooSoon)
    );
}

#[test]
fn a_security_setting_is_confirmed_by_typing_its_key_name() {
    let o = route("/config set aws_profile other", &focus());
    let mut c = Confirmation::new(&o, 0).unwrap();
    assert_eq!(c.expected(), Some("aws_profile"));
    let late = CONFIRM_DELAY_MS;
    assert_eq!(
        c.press(ConfirmKey::Enter, late),
        ConfirmStep::Waiting(Wait::TypeAnswer)
    );
    for ch in "aws_profile".chars() {
        c.press(ConfirmKey::Char(ch), late);
    }
    assert_eq!(c.press(ConfirmKey::Enter, late), ConfirmStep::Run);
    assert_eq!(c.argv(), ["config", "set", "aws_profile", "other"]);
}

#[test]
fn only_a_command_with_a_preview_has_a_confirmation() {
    assert!(Confirmation::new(&route("/runs", &focus()), 0).is_none());
    assert!(Confirmation::new(&route("hello", &focus()), 0).is_none());
    assert!(Confirmation::new(&route("/nonsense", &focus()), 0).is_none());
    assert!(Confirmation::new(&RouteOutcome::Empty, 0).is_none());
    // Tainted text gives every command a confirmation.
    assert!(Confirmation::new(&route("/runs", &tainted()), 0).is_some());
    let mut c = Confirmation::new(&route("/runs", &tainted()), 0).unwrap();
    assert_eq!(
        c.press(ConfirmKey::Enter, 10),
        ConfirmStep::Waiting(Wait::TooSoon)
    );
    assert_eq!(
        c.press(ConfirmKey::Enter, CONFIRM_DELAY_MS),
        ConfirmStep::Run
    );
}

#[test]
fn doctor_with_a_catalog_age_limit_runs_at_once_and_a_bad_limit_is_refused() {
    // First slice: `/doctor` keeps running at once (the registry gives it no usage text, so the popup does not wait for arguments),
    // and the option is read by the same parser as the shell form.
    let o = route("/doctor --catalog-max-age 24", &focus());
    let RouteOutcome::Command {
        argv,
        preview,
        class,
        ..
    } = o
    else {
        panic!("{o:?}")
    };
    assert_eq!(argv, ["doctor", "--catalog-max-age", "24"]);
    assert!(preview.is_none() && !class.needs_confirm());
    let o = route("/doctor --catalog-max-age soon", &focus());
    let RouteOutcome::Refused { reason, .. } = o else {
        panic!("{o:?}")
    };
    assert!(
        reason.contains("--catalog-max-age takes whole hours"),
        "{reason}"
    );
}
