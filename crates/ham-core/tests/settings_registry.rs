//! The registry of the client's settings (`ham_core::settings`): one row per key of `config.toml`, from which the reader, `ham config`, the
//! Settings screen, the generated reference table and the run-settings resolver take their facts. The nine rows of v4.02 keep their names,
//! types, ranges, environment variables and spellings; six rows are new (`run.goal`, `run.rules`, `run.skills`, `research.enabled`,
//! `ui.notify`, `ui.bell`).

#![allow(clippy::unwrap_used)]

use ham_core::config::KEYS;
use ham_core::registry::SECURITY_KEYS;
use ham_core::settings::{DefaultValue, Key, Kind, ROWS, reference_markdown};

/// The nine keys of v4.02 as `ham config` lists them: file name, name `ham config` prints, kind, the environment variable that takes
/// part, and the snake spelling the file also accepts.
const LEGACY: [(&str, &str, &str, &str, Option<&str>); 9] = [
    ("apiEndpoint", "api_endpoint", "text", "", None),
    ("region", "region", "text", "AWS_REGION", None),
    ("awsProfile", "aws_profile", "text", "AWS_PROFILE", None),
    ("defaultRepo", "default_repo", "text", "", None),
    (
        "downloadDir",
        "download_dir",
        "text",
        "HAMH_DOWNLOAD_DIR",
        None,
    ),
    (
        "defaultModel",
        "default_model",
        "model",
        "HAMH_MODEL",
        Some("default_model"),
    ),
    (
        "inputWaitMinutes",
        "input_wait_minutes",
        "minutes",
        "HAMH_INPUT_WAIT",
        Some("input_wait_minutes"),
    ),
    ("mouse", "mouse", "mouse", "HAMH_NO_MOUSE", None),
    (
        "reportTests",
        "report_tests",
        "bool",
        "",
        Some("report_tests"),
    ),
];

const NEW: [&str; 6] = [
    "run.goal",
    "run.rules",
    "run.skills",
    "research.enabled",
    "ui.notify",
    "ui.bell",
];

fn file_name(row: &ham_core::settings::Row) -> String {
    if row.file.0.is_empty() {
        row.file.1.to_string()
    } else {
        format!("{}.{}", row.file.0, row.file.1)
    }
}

#[test]
fn every_key_variant_has_one_row_and_every_row_one_variant() {
    assert_eq!(Key::ALL.len(), 46);
    assert_eq!(ROWS.len(), Key::ALL.len());
    for (i, key) in Key::ALL.iter().enumerate() {
        assert_eq!(ROWS[i].key, *key, "row {i} is out of the variant order");
        assert_eq!(key.row().key, *key);
        assert_eq!(*key as usize, i, "the row index is the variant's number");
    }
    let mut seen = std::collections::BTreeSet::new();
    for row in &ROWS {
        assert!(seen.insert(row.key), "{:?} has two rows", row.key);
        assert!(!row.label.trim().is_empty(), "{:?} has no label", row.key);
        assert!(!row.help.trim().is_empty(), "{:?} has no help", row.key);
        assert!(
            !row.default_text.trim().is_empty(),
            "{:?} has no default",
            row.key
        );
        assert!(
            row.label.len() <= 24,
            "{:?}: a label is a short word group",
            row.key
        );
        // A default lies inside the range of its kind.
        match (row.kind, row.default) {
            (_, DefaultValue::Absent) => {}
            (Kind::Bool, DefaultValue::Bool(_)) => {}
            (Kind::Minutes { max }, DefaultValue::Minutes(m)) => {
                assert!(m <= max, "{:?}: default {m} is past {max}", row.key);
            }
            (Kind::Text | Kind::Model | Kind::Engine, DefaultValue::Text(t)) => {
                assert!(!t.is_empty() && !t.contains('\n'), "{:?}", row.key);
            }
            (Kind::Mouse, DefaultValue::Bool(_)) => {}
            (Kind::Number { min, max, .. }, DefaultValue::Number(n)) => {
                assert!(
                    (min..=max).contains(&n),
                    "{:?}: default {n} is outside {min}..={max}",
                    row.key
                );
            }
            (kind, default) => panic!("{:?}: default {default:?} does not fit {kind:?}", row.key),
        }
    }
    let names: std::collections::BTreeSet<_> = ROWS.iter().map(|r| r.name).collect();
    assert_eq!(names.len(), 46, "names are unique");
}

#[test]
fn the_nine_legacy_rows_keep_name_type_range_env_and_spelling() {
    assert_eq!(KEYS.len(), 46);
    let today: Vec<(&str, &str)> = LEGACY.iter().map(|(f, s, ..)| (*f, *s)).collect();
    assert_eq!(&KEYS[..9], &today[..]);
    for (i, (file, snake, kind, env, alias)) in LEGACY.iter().enumerate() {
        let row = &ROWS[i];
        assert_eq!(file_name(row), *file);
        assert_eq!(row.name, *snake);
        assert_eq!(row.kind.word(), *kind, "{file}");
        assert!(
            row.env.contains(env),
            "{file}: env {:?} lacks {env:?}",
            row.env
        );
        assert_eq!(
            row.file_alias, *alias,
            "{file}: the snake spelling the file accepts"
        );
        assert!(row.file.0.is_empty(), "{file} is a top-level key");
        assert_eq!(row.since, 0, "{file} needs no configVersion");
    }
    // The range of the wait, and its flag.
    let wait = Key::InputWaitMinutes.row();
    assert!(matches!(wait.kind, Kind::Minutes { max: 43_200 }));
    assert_eq!(wait.flag, "--input-wait");
    assert_eq!(Key::DefaultModel.row().flag, "--model");
    assert_eq!(Key::DefaultRepo.row().flag, "--repo");
    // The six new rows follow in registry order, dotted in both columns, and need configVersion 1.
    for (i, name) in NEW.iter().enumerate() {
        let row = &ROWS[9 + i];
        assert_eq!(row.name, *name);
        assert_eq!(file_name(row), *name);
        assert_eq!(KEYS[9 + i], (*name, *name));
        assert_eq!(row.kind, Kind::Bool);
        assert_eq!(row.since, 1, "{name} is written with configVersion 1");
        assert!(row.file_alias.is_none());
    }
    // The sixteenth row is the mascot: three words, written with configVersion 1 like the rows before it.
    let mascot = &ROWS[15];
    assert_eq!(mascot.name, "ui.mascot");
    assert_eq!(KEYS[15], ("ui.mascot", "ui.mascot"));
    assert_eq!(mascot.kind, Kind::Mascot);
    assert_eq!(mascot.since, 1);
    assert_eq!(mascot.env, "HAMH_NO_MASCOT");
    assert_eq!(Key::RunGoal.row().flag, "--goal on|off");
    assert_eq!(Key::RunGoal.row().wire, Some("goal"));
    assert_eq!(Key::ResearchEnabled.row().wire, Some("research"));
    assert_eq!(Key::UiNotify.row().env, "HAMH_NOTIFY");
    // Lookup takes either spelling and nothing else.
    assert_eq!(Key::lookup("apiEndpoint"), Some(Key::ApiEndpoint));
    assert_eq!(Key::lookup("api_endpoint"), Some(Key::ApiEndpoint));
    assert_eq!(Key::lookup("run.goal"), Some(Key::RunGoal));
    assert_eq!(Key::lookup("run_goal"), None);
    assert_eq!(Key::lookup("Run.Goal"), None);
    assert_eq!(Key::lookup(""), None);
}

#[test]
fn the_security_rows_are_the_seven_of_today() {
    assert_eq!(
        SECURITY_KEYS,
        [
            "api_endpoint",
            "aws_profile",
            "region",
            "download_dir",
            "default_backend",
            "customer_content",
            "update_source"
        ]
    );
    let flagged: std::collections::BTreeSet<&str> =
        ROWS.iter().filter(|r| r.security).map(|r| r.name).collect();
    let want: std::collections::BTreeSet<&str> = SECURITY_KEYS.iter().copied().collect();
    assert_eq!(
        flagged, want,
        "the registry and the TUI's list name the same six"
    );
    for name in NEW {
        assert!(!Key::lookup(name).unwrap().row().security, "{name}");
    }
}

#[test]
fn the_reference_table_lists_every_row_with_origin_flag_env_and_effect() {
    let md = reference_markdown();
    let lines: Vec<&str> = md.lines().collect();
    assert_eq!(
        lines[0],
        "| Key | Type | Default | Flag | Environment variable | Takes effect |"
    );
    assert_eq!(lines[1], "| --- | --- | --- | --- | --- | --- |");
    assert_eq!(lines.len(), 2 + 46, "46 body rows:\n{md}");
    for (line, row) in lines[2..].iter().zip(ROWS.iter()) {
        assert!(
            line.starts_with(&format!("| `{}` |", file_name(row))),
            "{line}"
        );
        assert_eq!(line.matches(" | ").count(), 5, "six cells: {line}");
    }
    assert!(md.ends_with('\n'));
    // Two rows, exactly.
    assert!(
        lines.contains(
            &"| `defaultModel` | model id or alias | Claude Sonnet 5.5 | `--model` | `HAMH_MODEL` | next run |"
        ),
        "{md}"
    );
    assert!(
        lines.contains(
            &"| `run.goal` | true or false | stage default (on) | `--goal on\\|off` | none | next run |"
        ),
        "{md}"
    );
}

#[test]
fn the_precedence_exceptions_are_rows_of_the_registry() {
    use ham_core::settings::Order;
    // One rule (flag, environment, file, default) with the exceptions v4.02 has, each a fact of its row.
    let order = |k: Key| k.row().order;
    assert_eq!(order(Key::DefaultModel), Order::FlagEnvFile);
    assert_eq!(order(Key::InputWaitMinutes), Order::FlagEnvFile);
    assert_eq!(order(Key::Region), Order::FileOverEnv);
    assert_eq!(order(Key::AwsProfile), Order::FileOverEnv);
    assert_eq!(order(Key::DownloadDir), Order::EnvOverFile);
    assert_eq!(order(Key::Mouse), Order::EnvForcesOff);
    assert_eq!(order(Key::UiMascot), Order::EnvForcesOff);
    assert_eq!(order(Key::UiNotify), Order::EnvOverFile);
    for key in [
        Key::RunGoal,
        Key::RunRules,
        Key::RunSkills,
        Key::ResearchEnabled,
    ] {
        assert_eq!(
            order(key),
            Order::FlagFile,
            "{key:?}: a flag, then the file"
        );
        assert!(
            key.row().env.is_empty(),
            "{key:?} has no environment variable"
        );
    }
    for key in [
        Key::ApiEndpoint,
        Key::DefaultRepo,
        Key::ReportTests,
        Key::UiBell,
    ] {
        let want = if key == Key::DefaultRepo {
            Order::FlagFile
        } else {
            Order::FileOnly
        };
        assert_eq!(order(key), want, "{key:?}");
    }
    // The reference table says so in the Environment column.
    let md = reference_markdown();
    assert!(md.contains("`AWS_REGION` (the file wins)"), "{md}");
    assert!(
        md.contains("`HAMH_DOWNLOAD_DIR` (wins over the file)"),
        "{md}"
    );
    assert!(md.contains("`HAMH_NO_MOUSE` (forces off)"), "{md}");
    assert!(md.contains("`HAMH_NO_MASCOT` (forces off)"), "{md}");
}
