//! The strict reader of `config.toml`: what it reads, what it refuses, and the findings it returns with the line of each (`read_config`).
//! Files written for v4.02 read as before; the three tables `[run]`, `[research]` and `[ui]` and `configVersion` are new, and a file from a
//! newer `ham` is read for the keys this build knows while writes are blocked.

#![allow(clippy::unwrap_used)]

use ham_core::config::{
    Config, ConfigError, ResearchTable, RunTable, UiTable, parse_config, read_config,
    render_config, set_key, version_wanted,
};
use ham_core::settings::{Code, Severity};

const OWNER: &str = "# my stage\napiEndpoint = \"https://abc123.execute-api.us-east-1.amazonaws.com/direct/v1\"\n# my region\nregion = \"us-east-1\"\nawsProfile = \"ham-acct-a01\"\n";

#[test]
fn old_files_read_as_before_and_an_untouched_file_renders_the_same() {
    // The owner's three-key file, with its comment lines.
    let cfg = parse_config(OWNER).unwrap();
    assert_eq!(
        cfg.api_endpoint.as_deref(),
        Some("https://abc123.execute-api.us-east-1.amazonaws.com/direct/v1")
    );
    assert_eq!(cfg.region.as_deref(), Some("us-east-1"));
    assert_eq!(cfg.aws_profile.as_deref(), Some("ham-acct-a01"));
    assert_eq!(cfg.config_version, None);
    assert_eq!((cfg.run, cfg.research, cfg.ui), (None, None, None));
    // The snake spellings of the three keys that have them.
    let snake = parse_config(
        "default_model = \"us.openai.gpt-6.1-sol\"\ninput_wait_minutes = 15\nreport_tests = false\n",
    )
    .unwrap();
    assert_eq!(
        snake.default_model.as_deref(),
        Some("us.openai.gpt-6.1-sol")
    );
    assert_eq!(snake.input_wait_minutes, Some(15));
    assert_eq!(snake.report_tests, Some(false));
    // A file in the order of the struct renders to itself.
    let old = "apiEndpoint = \"https://a.example/v1\"\nregion = \"us-west-2\"\nawsProfile = \"p\"\ndefaultRepo = \"cc:x\"\ndownloadDir = \"/d\"\ndefaultModel = \"global.anthropic.claude-sonnet-5-5\"\ninputWaitMinutes = 5\nreportTests = false\nmouse = \"auto\"\n";
    assert_eq!(render_config(&parse_config(old).unwrap()), old);
    assert_eq!(parse_config("").unwrap(), Config::default());
    assert_eq!(
        parse_config("# only a comment\n").unwrap(),
        Config::default()
    );
    // The names that v4.02 refused are still refused.
    assert!(parse_config("api_endpoint = \"https://a.example/v1\"\n").is_err());
    assert!(parse_config("runCache = []\n").is_err());
}

#[test]
fn each_new_table_parses_into_typed_fields() {
    let cfg = parse_config(
        "configVersion = 1\n\n[run]\ngoal = false\nrules = false\nskills = true\n\n[research]\nenabled = true\n\n[ui]\nnotify = true\nbell = false\n",
    )
    .unwrap();
    assert_eq!(cfg.config_version, Some(1));
    assert_eq!(
        cfg.run,
        Some(RunTable {
            goal: Some(false),
            rules: Some(false),
            skills: Some(true),
            ..RunTable::default()
        })
    );
    assert_eq!(
        cfg.research,
        Some(ResearchTable {
            enabled: Some(true)
        })
    );
    assert_eq!(
        cfg.ui,
        Some(UiTable {
            notify: Some(true),
            bell: Some(false),
            mascot: None,
            mascot_motion: None,
            deprecation_notices: None
        })
    );
    // A table with one key leaves the others unset, and the dotted form is the same table.
    let one = parse_config("[run]\ngoal = false\n").unwrap();
    assert_eq!(
        one.run,
        Some(RunTable {
            goal: Some(false),
            ..RunTable::default()
        })
    );
    let dotted = parse_config("run.goal = false\n").unwrap();
    assert_eq!(dotted.run, one.run);
    // What the writer renders, the reader reads back.
    assert_eq!(parse_config(&render_config(&cfg)).unwrap(), cfg);
}

#[test]
fn an_unknown_key_in_a_table_names_the_line_and_the_nearest_key() {
    let text = "[run]\ngoall = true\n";
    let read = read_config(text);
    let finding = read.findings.errors().next().expect("an error finding");
    assert_eq!(finding.code, Code::UnknownKey);
    assert_eq!(finding.code.as_str(), "unknown_key");
    assert_eq!(finding.severity, Severity::Error);
    assert_eq!(finding.line, Some(2));
    assert_eq!(finding.key.as_deref(), Some("goall"));
    assert_eq!(finding.suggestion.as_deref(), Some("goal"));
    let shown = finding.render();
    assert!(
        shown.contains("line 2")
            && shown.contains("goall")
            && shown.contains("did you mean `goal`"),
        "{shown}"
    );
    let err = parse_config(text).unwrap_err().to_string();
    assert!(err.contains("line 2") && err.contains("goall"), "{err}");
    // A key with no near neighbour names the line and no guess.
    let far = read_config("[ui]\nsomething_else_entirely = 1\n");
    let f = far.findings.errors().next().unwrap();
    assert_eq!((f.line, f.suggestion.as_deref()), (Some(2), None));
    // A top-level key written by the camelCase rule of another tool is guessed too.
    let top = read_config("api_endpoint = \"https://a.example/v1\"\n");
    let f = top.findings.errors().next().unwrap();
    assert_eq!(f.suggestion.as_deref(), Some("apiEndpoint"));
}

#[test]
fn an_unknown_table_names_the_line_and_the_nearest_table() {
    let read = read_config("region = \"us-east-1\"\n\n[serch]\nenabled = true\n");
    let finding = read.findings.errors().next().expect("an error finding");
    assert_eq!(finding.code, Code::UnknownTable);
    assert_eq!(finding.line, Some(3));
    assert_eq!(finding.key.as_deref(), Some("serch"));
    assert_eq!(finding.suggestion.as_deref(), Some("research"));
    assert!(
        finding.render().contains("[serch]") && finding.render().contains("[research]"),
        "{}",
        finding.render()
    );
    // A table with a name that has no near neighbour is still named by its line.
    let f = read_config("[zzzzzzzzzz]\nx = 1\n");
    let f = f.findings.errors().next().unwrap().clone();
    assert_eq!((f.line, f.suggestion), (Some(1), None));
}

#[test]
fn a_legacy_key_typed_under_a_table_gets_the_move_it_hint() {
    let text = "region = \"us-east-1\"\n\n[ui]\nmouse = true\n";
    let read = read_config(text);
    let f = read.findings.errors().next().expect("an error finding");
    assert_eq!(f.code, Code::MovedKey);
    assert_eq!(f.line, Some(4));
    let shown = f.render();
    assert!(
        shown.contains("`mouse` is a top-level key")
            && shown.contains("above the first [table] line"),
        "{shown}"
    );
    // A key of a table typed at the top is told where it belongs.
    let top = read_config("goal = false\n");
    let f = top.findings.errors().next().unwrap();
    assert_eq!(f.code, Code::UnknownKey);
    assert!(f.render().contains("[run]"), "{}", f.render());
}

#[test]
fn a_higher_config_version_reads_known_keys_and_flags_writes_and_unknown_run_steering_keys() {
    // An unknown key under [ui]: a warning, writes blocked, a run may start.
    let ui = "region = \"us-west-2\"\nconfigVersion = 2\n\n[ui]\nfuture = 1\n";
    let read = read_config(ui);
    assert_eq!(read.config.region.as_deref(), Some("us-west-2"));
    assert_eq!(read.config.config_version, Some(2));
    assert!(!read.has_errors(), "{:?}", read.findings);
    assert!(read.writes_blocked());
    assert!(!read.run_blocked());
    let codes: Vec<(&str, Severity)> = read
        .findings
        .iter()
        .map(|f| (f.code.as_str(), f.severity))
        .collect();
    assert!(
        codes.contains(&("unknown_key", Severity::Warning)),
        "{codes:?}"
    );
    assert!(
        codes.contains(&("newer_version", Severity::Warning)),
        "{codes:?}"
    );
    assert!(parse_config(ui).is_ok());
    // The write refuses with the version named.
    assert!(matches!(
        set_key(&read.config, "region", "us-east-2"),
        Err(ConfigError::Newer { version: 2 })
    ));
    // An unknown key under [run] or [research], a top-level key or a table of its own: no run may start.
    for text in [
        "configVersion = 2\n\n[run]\nfuture = 1\n",
        "configVersion = 2\n\n[research]\nfuture = 1\n",
        "configVersion = 2\nfutureKey = 1\n",
        "configVersion = 2\n\n[future]\nx = 1\n",
    ] {
        let read = read_config(text);
        assert!(!read.has_errors(), "{text:?}: {:?}", read.findings);
        assert!(read.writes_blocked() && read.run_blocked(), "{text:?}");
        let err = parse_config(text).unwrap_err().to_string();
        assert!(
            err.contains("newer ham") && err.contains("configVersion 2"),
            "{err}"
        );
    }
    // The same unknown key at the version this build knows is an error.
    let same = read_config("configVersion = 1\n\n[ui]\nfuture = 1\n");
    assert!(same.has_errors());
    assert!(!same.writes_blocked());
    // A bad value of a known key stays an error in a newer file.
    assert!(read_config("configVersion = 2\n\n[ui]\nbell = 3\n").has_errors());
    // The version itself must be a whole number from 1.
    for bad in [
        "configVersion = 0\n",
        "configVersion = \"1\"\n",
        "configVersion = 1.5\n",
        "configVersion = -1\n",
    ] {
        assert!(read_config(bad).has_errors(), "{bad:?}");
    }
}

#[test]
fn the_config_version_is_wanted_exactly_while_a_new_key_is_set() {
    assert!(!version_wanted(&Config::default()));
    let legacy = parse_config(
        "apiEndpoint = \"https://a.example/v1\"\nregion = \"r\"\nawsProfile = \"p\"\ndefaultRepo = \"cc:x\"\ndownloadDir = \"/d\"\ndefaultModel = \"m\"\ninputWaitMinutes = 5\nreportTests = false\nmouse = false\n",
    )
    .unwrap();
    assert!(
        !version_wanted(&legacy),
        "the nine keys of v4.02 need no version"
    );
    for (key, value) in [
        ("run.goal", "false"),
        ("run.rules", "false"),
        ("run.skills", "false"),
        ("research.enabled", "true"),
        ("ui.notify", "true"),
        ("ui.bell", "false"),
    ] {
        let cfg = set_key(&Config::default(), key, value).unwrap();
        assert!(version_wanted(&cfg), "{key}");
        assert_eq!(
            cfg.config_version,
            Some(1),
            "{key}: the writer adds the version"
        );
    }
}

#[test]
fn a_wrong_type_or_range_names_the_key_and_the_line_and_never_echoes_the_value() {
    for (text, key, line) in [
        ("[run]\ngoal = \"yes please\"\n", "run.goal", 2),
        ("[ui]\nbell = 1\n", "ui.bell", 2),
        ("region = 5\n", "region", 1),
        ("mouse = \"sometimes\"\n", "mouse", 1),
        ("inputWaitMinutes = 241\n", "input_wait_minutes", 1),
        ("inputWaitMinutes = -3\n", "input_wait_minutes", 1),
        ("reportTests = \"off\"\n", "report_tests", 1),
    ] {
        let read = read_config(text);
        let f = read
            .findings
            .errors()
            .next()
            .unwrap_or_else(|| panic!("{text:?}: no finding"));
        assert_eq!(f.line, Some(line), "{text:?}");
        let shown = f.render();
        assert!(shown.contains(key), "{text:?}: {shown}");
        assert!(
            !shown.contains("yes please") && !shown.contains("sometimes"),
            "the value is not echoed: {shown}"
        );
        assert!(parse_config(text).is_err(), "{text:?}");
    }
    // The same key in both spellings is ambiguous.
    let e = parse_config("defaultModel = \"a\"\ndefault_model = \"b\"\n").unwrap_err();
    assert!(e.to_string().contains("default_model"), "{e}");
}

#[test]
fn findings_come_in_line_order_and_a_file_with_two_faults_reports_both() {
    let read = read_config("[run]\ngoall = true\nrulez = false\n\n[ui]\nbel = true\n");
    let lines: Vec<usize> = read.findings.errors().filter_map(|f| f.line).collect();
    assert_eq!(lines, vec![2, 3, 6]);
}
