//! A mistyped command word. `ham pul` and `ham psuh` were read as the prompt of a new run and failed with "exactly
//! one of --repo, --new-repo, --s3-only is required", which never says that `pul` is not a command. A single word that is one edit
//! away from a subcommand, with no target flag and no second word, now names the closest commands. Every documented start form
//! keeps working.

#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, SUBCOMMANDS, TargetArg, parse};

fn fails(args: &[&str]) -> String {
    parse(args).expect_err("must fail").to_string()
}

/// The words one edit away from `word`: each letter deleted, each pair of neighbours swapped, each letter replaced by `x` (or `q`
/// when it is an `x`), and an `x` added at the end.
fn one_edit_typos(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut out = Vec::new();
    for i in 0..chars.len() {
        let mut d = chars.clone();
        d.remove(i);
        out.push(d.into_iter().collect());
        let mut r = chars.clone();
        r[i] = if r[i] == 'x' { 'q' } else { 'x' };
        out.push(r.into_iter().collect());
        if i + 1 < chars.len() && chars[i] != chars[i + 1] {
            let mut s = chars.clone();
            s.swap(i, i + 1);
            out.push(s.into_iter().collect());
        }
    }
    out.push(format!("{word}x"));
    out
}

#[test]
fn a_typo_of_every_subcommand_names_the_closest_commands() {
    let mut checked = 0;
    for sub in SUBCOMMANDS {
        for typo in one_edit_typos(sub) {
            if typo.len() < 3 || SUBCOMMANDS.contains(&typo.as_str()) {
                continue; // two letters are no command word, and a typo that is another subcommand is that subcommand
            }
            let msg = fails(&[&typo]);
            assert!(msg.starts_with("usage: "), "{typo}: {msg}");
            assert!(
                msg.contains(&format!("no command \"{typo}\"")),
                "{typo}: {msg}"
            );
            let listed = msg
                .split("closest: ")
                .nth(1)
                .unwrap_or_else(|| panic!("{typo}: {msg}"));
            let listed = listed.split(';').next().unwrap();
            assert!(
                listed.split(", ").any(|w| w == *sub),
                "{typo} should name {sub}: {msg}"
            );
            // Both readings are explained: the way to start a run with this word is named.
            assert!(
                msg.contains("--s3-only") && msg.contains("--direct"),
                "{typo}: {msg}"
            );
            checked += 1;
        }
    }
    assert!(checked > 150, "the generator covered {checked} typos");
}

#[test]
fn the_words_that_started_it() {
    let m = fails(&["pul"]);
    assert!(m.contains("closest: pull"), "{m}");
    let m = fails(&["psuh"]);
    assert!(m.contains("closest: push"), "{m}");
    let m = fails(&["cancle"]);
    assert!(m.contains("closest: cancel"), "{m}");
    // The case alone is a typo.
    let m = fails(&["Pull"]);
    assert!(m.contains("closest: pull"), "{m}");
    // The --json flag and a model option do not hide it.
    assert!(fails(&["pul", "--json"]).contains("closest: pull"));
    assert!(fails(&["pul", "--model", "opus"]).contains("closest: pull"));
    assert!(fails(&["pul", "--direct"]).contains("closest: pull"));
}

#[test]
fn every_documented_start_form_keeps_working() {
    for (args, prompt) in [
        (vec!["pul", "--s3-only", "--direct"], "pul"),
        (vec!["pulls", "--repo", "cc:team-app", "--direct"], "pulls"),
        (
            vec!["psuh", "--new-repo", "cc:team-new", "--direct"],
            "psuh",
        ),
        (vec!["--prompt", "pul", "--s3-only", "--direct"], "pul"),
        (vec!["--prompt", "pull", "--s3-only", "--direct"], "pull"),
        (
            vec!["build a todo api", "--s3-only", "--direct"],
            "build a todo api",
        ),
        (
            vec!["cancle", "--s3-only", "--direct", "--model", "opus"],
            "cancle",
        ),
    ] {
        match parse(&args)
            .unwrap_or_else(|e| panic!("{args:?}: {e}"))
            .command
        {
            Command::StartDirect(d) => {
                assert_eq!(d.prompt, prompt, "{args:?}");
                assert!(matches!(
                    d.target,
                    TargetArg::S3Only | TargetArg::Repo(_) | TargetArg::NewRepo(_)
                ));
            }
            other => panic!("{args:?}: {other:?}"),
        }
    }
}

#[test]
fn a_prompt_that_is_not_near_a_command_gets_the_old_message() {
    for word in [
        "hello",
        "refactor",
        "build",
        "x",
        "tests",
        "an-extremely-long-prompt-word",
    ] {
        let m = fails(&[word]);
        assert!(
            m.contains("exactly one of --repo, --new-repo, --s3-only is required"),
            "{word}: {m}"
        );
        assert!(!m.contains("no command"), "{word}: {m}");
    }
}

#[test]
fn more_than_one_word_behaves_as_before() {
    // Two bare words: the old message about quoting, not a guess about a command.
    let m = fails(&["pul", "the", "thing"]);
    assert!(m.contains("one prompt; quote it"), "{m}");
    assert!(!m.contains("no command"), "{m}");
    // A word that holds a space is a prompt, not a mistyped command.
    let m = fails(&["pul the thing"]);
    assert!(m.contains("exactly one of --repo"), "{m}");
    // A word with other characters is a prompt.
    let m = fails(&["pul."]);
    assert!(m.contains("exactly one of --repo"), "{m}");
    // A target flag means the person is starting a run: the usual messages for what is missing.
    let m = fails(&["pul", "--s3-only"]);
    assert!(m.contains("needs --direct"), "{m}");
    let m = fails(&["pul", "--prompt", "x"]);
    assert!(
        m.contains("prompt is given twice") || m.contains("one prompt"),
        "{m}"
    );
}

#[test]
fn real_subcommands_are_never_flagged() {
    for sub in SUBCOMMANDS {
        let m = parse(&[*sub])
            .map(|_| String::new())
            .unwrap_or_else(|e| e.to_string());
        assert!(!m.contains("no command"), "{sub}: {m}");
    }
}
