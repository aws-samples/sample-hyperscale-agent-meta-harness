//! `ham update`: the command grammar, its help and registry entry, the `update_source` setting (the source and the off switch) and the
//! pure rules of the update: versions compared as numbers, the plan (up to date, which parts, refusals) and the changelog lines between.
#![allow(clippy::unwrap_used)]

use ham_core::command::{Command, SUBCOMMANDS, parse};
use ham_core::settings::Key;
use ham_core::update::{
    DEFAULT_SOURCE, Parts, Plan, Refusal, Source, changes_between, newest_heading, parse_version,
    plan, source,
};

fn update(args: &[&str]) -> Command {
    let mut all = vec!["update"];
    all.extend_from_slice(args);
    parse(&all).unwrap().command
}

#[test]
fn b804_update_parses_its_options_and_renders_them_back() {
    assert!(SUBCOMMANDS.contains(&"update"));
    assert_eq!(
        update(&[]),
        Command::Update {
            check: false,
            yes: false,
            no_wait: false,
            wait_minutes: None
        }
    );
    let c = update(&["--check"]);
    assert!(matches!(c, Command::Update { check: true, .. }));
    let c = update(&["--yes", "--wait", "90"]);
    assert_eq!(
        c,
        Command::Update {
            check: false,
            yes: true,
            no_wait: false,
            wait_minutes: Some(90)
        }
    );
    let c = update(&["-y", "--no-wait"]);
    assert!(matches!(
        c,
        Command::Update {
            yes: true,
            no_wait: true,
            ..
        }
    ));
    for args in [
        vec!["update", "--check"],
        vec!["update", "--yes", "--wait", "5"],
        vec!["update", "--no-wait"],
    ] {
        let inv = parse(&args).unwrap();
        assert_eq!(parse(&inv.to_argv()).unwrap(), inv, "{args:?}");
    }
}

#[test]
fn b804_update_refuses_bad_options() {
    for args in [
        vec!["update", "--wait", "0"],
        vec!["update", "--wait", "1441"],
        vec!["update", "--wait", "soon"],
        vec!["update", "--no-wait", "--wait", "5"],
        vec!["update", "--check", "--yes"],
        vec!["update", "now"],
        vec!["update", "--json"],
    ] {
        let e = parse(&args).unwrap_err().to_string();
        assert!(e.starts_with("usage:"), "{args:?}: {e}");
    }
    // --json goes with --check only: the update itself hands the terminal to setup.
    assert!(parse(&["update", "--check", "--json"]).unwrap().json);
}

#[test]
fn b804_help_and_registry_name_the_command() {
    let text = ham_core::help::command_text("update").unwrap();
    for part in [
        "ham update",
        "--check",
        "--yes",
        "--no-wait",
        "--wait <minutes>",
        "update_source",
    ] {
        assert!(text.contains(part), "{part:?} missing from:\n{text}");
    }
    assert!(text.contains("never cancels"), "{text}");
    assert!(
        ham_core::registry::ENTRIES
            .iter()
            .any(|e| e.name == "update")
    );
}

#[test]
fn b804_update_source_is_a_setting_with_the_public_default_and_an_off_word() {
    let row = Key::lookup("update_source").unwrap().row();
    assert_eq!(Key::lookup("updateSource"), Some(row.key));
    assert_eq!(row.default_text, DEFAULT_SOURCE);
    assert!(row.security, "the source decides which code runs");
    assert!(row.help.contains("off"), "{}", row.help);
    assert_eq!(
        DEFAULT_SOURCE,
        "https://github.com/aws-samples/sample-hyperscale-agent-meta-harness.git"
    );
    assert!(
        ham_core::settings::reference_markdown().contains("`updateSource`"),
        "the generated table names the key"
    );
}

#[test]
fn b804_source_reads_unset_off_and_urls() {
    assert_eq!(source(None).unwrap(), Source::Url(DEFAULT_SOURCE.into()));
    assert_eq!(
        source(Some("")).unwrap(),
        Source::Url(DEFAULT_SOURCE.into())
    );
    assert_eq!(source(Some("off")).unwrap(), Source::Off);
    assert_eq!(
        source(Some("https://example.com/me/fork.git")).unwrap(),
        Source::Url("https://example.com/me/fork.git".into())
    );
    assert_eq!(
        source(Some("file:///srv/mirror.git")).unwrap(),
        Source::Url("file:///srv/mirror.git".into())
    );
    // The URL with a password in it is put together here, so no file of the repository holds one.
    let pw = ["p", "w"].concat();
    let with_password = format!("https://user:{pw}@example.com/x.git");
    for bad in [
        "OFF",
        "http://example.com/x.git",
        "ssh://git@example.com/x.git",
        with_password.as_str(),
        "-uhttps://example.com",
        "/srv/mirror.git",
        "ext::sh -c id",
    ] {
        assert!(source(Some(bad)).is_err(), "{bad:?} must be refused");
    }
}

#[test]
fn b804_versions_compare_as_numbers() {
    assert_eq!(parse_version("5.14"), Some((5, 14)));
    assert_eq!(parse_version("10.02"), Some((10, 2)));
    for bad in ["5.1", "5.140", "v5.14", "5", "", "5.1x", " 5.14"] {
        assert_eq!(parse_version(bad), None, "{bad:?}");
    }
    assert!(parse_version("5.14") < parse_version("5.15"));
    assert!(parse_version("5.99") < parse_version("6.00"));
}

#[test]
fn b804_plan_names_the_parts_and_refuses_what_it_must() {
    assert_eq!(plan("5.14", Some("5.14"), "5.14"), Plan::UpToDate);
    assert_eq!(
        plan("5.14", Some("5.14"), "5.16"),
        Plan::Update(Parts::BackendAndClient)
    );
    assert_eq!(
        plan("5.14", Some("5.16"), "5.16"),
        Plan::Update(Parts::ClientOnly)
    );
    assert_eq!(
        plan("5.16", Some("5.14"), "5.16"),
        Plan::Update(Parts::BackendOnly)
    );
    assert_eq!(
        plan("5.14", None, "5.16"),
        Plan::Refuse(Refusal::BackendUnknown)
    );
    assert!(matches!(
        plan("5.14", Some("5.17"), "5.16"),
        Plan::Refuse(Refusal::Downgrade { .. })
    ));
    assert!(matches!(
        plan("5.18", Some("5.14"), "5.16"),
        Plan::Refuse(Refusal::Downgrade { .. })
    ));
    assert!(matches!(
        plan("5.14", Some("5.14"), "5.1"),
        Plan::Refuse(Refusal::BadVersion(_))
    ));
}

const CHANGELOG: &str = "# Changelog\n\nIntro.\n\n## [Unreleased]\n\n- not yet\n\n## [5.16] - 2026-11-20\n\n### Added\n\n- Sixteen.\n\n## [5.15] - 2026-11-15\n\n### Fixed\n\n- Fifteen.\n\n## [5.14] - 2026-11-10\n\n- Fourteen.\n";

#[test]
fn b804_changes_between_are_the_sections_above_the_older_version() {
    let s = changes_between(CHANGELOG, "5.14", "5.16");
    let versions: Vec<&str> = s.iter().map(|c| c.version.as_str()).collect();
    assert_eq!(versions, ["5.16", "5.15"]);
    assert!(s[0].heading.starts_with("## [5.16] - 2026-11-20"));
    assert!(s[0].body.contains("- Sixteen."));
    assert!(!s[0].body.contains("Fifteen"));
    assert!(s[1].body.contains("- Fifteen."));
    assert!(changes_between(CHANGELOG, "5.16", "5.16").is_empty());
    assert!(
        !changes_between(CHANGELOG, "5.00", "5.16")
            .iter()
            .any(|c| c.body.contains("not yet")),
        "Unreleased is never shown"
    );
    assert_eq!(newest_heading(CHANGELOG).as_deref(), Some("5.16"));
}

#[test]
fn b804_help_says_what_the_wait_covers_and_what_it_does_not() {
    // The help is wrapped at 78 columns, so the sentences are compared with the line breaks taken out.
    let text = ham_core::help::command_text("update").unwrap();
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    for part in [
        "waits for the runs of everyone on the stage",
        "the stage starts no new runs",
        "can still let a new run start",
        "setup stops with exit 7 at once while any run on the stage is active",
        "never cancels a run",
    ] {
        assert!(flat.contains(part), "{part:?} missing from:\n{text}");
    }
    // The sentences of the release that had no refusal of new runs are gone.
    for gone in [
        "waits only for your own runs",
        "does not stop anyone from starting a run",
    ] {
        assert!(!flat.contains(gone), "{gone:?} is still in:\n{text}");
    }
}
