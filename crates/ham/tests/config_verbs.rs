//! The verbs of `ham config` and the writer behind them: `get`, `set` and `unset` change one
//! line of `config.toml` and leave every other byte alone; `show`, `check`, `edit` and `docs` read and repair the file; a file with a bad key
//! is repaired by these verbs while `models`, `runs` and a start refuse it and name the line. The real `ham` binary in a temp HOME, and the
//! local `/v1` API where a command talks to one.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use home::{Home, ham_env, home, text};
use serde_json::Value;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, SystemTime};
use support::Fake;

const OPUS: &str = "us.anthropic.claude-opus-5-5";
const ASTRA: &str = "us.openai.gpt-6-astra";
const SOL: &str = "us.openai.gpt-6.1-sol";

/// The owner's file: three keys, two comment lines on top and a comment after a value.
const OWNER: &str = "# ham settings\n# keep this note\napiEndpoint = \"https://abc.execute-api.us-east-1.amazonaws.com/direct/v1\"\nregion = \"us-east-1\"\nawsProfile = \"ham-test\" # the profile\n";

/// A home with no config file at all, as the Home of the other tests but without the endpoint and the profile.
fn bare(label: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        config: root.join("config"),
        state: root.join("state"),
        cache: root.join("cache"),
        downloads: root.join("downloads"),
        root,
    };
    for d in [&h.config, &h.state, &h.cache, &h.downloads] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(h.config.join("ham")).unwrap();
    h
}

fn path(h: &Home) -> PathBuf {
    h.config.join("ham/config.toml")
}

fn write(h: &Home, content: &str) {
    std::fs::write(path(h), content).unwrap();
}

fn read(h: &Home) -> String {
    std::fs::read_to_string(path(h)).unwrap_or_default()
}

fn names_in_config_dir(h: &Home) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(h.config.join("ham"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

async fn ham(h: &Home, args: &[&str]) -> Output {
    ham_env(h, args, &[]).await
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn out(o: &Output) -> String {
    text(&o.stdout)
}

fn err(o: &Output) -> String {
    text(&o.stderr)
}

/// A shell script in the home that stands in for the user's editor.
/// A script for `$EDITOR`, as the string `sh <script>`: the editor string is run by `sh -c`, so the file is read and never executed. Executing a file that
/// was written a moment ago can fail with "Text file busy" under plain `cargo test`, where another test thread's fork may still hold the write handle.
fn editor(h: &Home, body: &str) -> String {
    let p = h.root.join("editor.sh");
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    format!("sh {}", p.display())
}

fn set_mtime_long_ago(h: &Home) -> SystemTime {
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path(h))
        .unwrap();
    f.set_modified(then).unwrap();
    then
}

fn mtime(h: &Home) -> SystemTime {
    std::fs::metadata(path(h)).unwrap().modified().unwrap()
}

// ---- set and unset: one line changes ------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_keeps_comments_order_and_every_other_byte_and_a_noop_set_writes_nothing() {
    let h = bare("verbs-keep");
    write(&h, OWNER);
    let o = ham(&h, &["config", "set", "default_model", "opus"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).trim(), format!("default_model = {OPUS}"));
    assert_eq!(read(&h), format!("{OWNER}defaultModel = \"{OPUS}\"\n"));
    assert_eq!(names_in_config_dir(&h), ["config.toml"], "no temp file");
    // A value on a line that has a comment keeps the comment; the other lines do not move.
    let o = ham(&h, &["config", "set", "region", "us-west-2"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(
        read(&h),
        OWNER.replace("us-east-1\"\nawsProfile", "us-west-2\"\nawsProfile")
            + &format!("defaultModel = \"{OPUS}\"\n")
    );
    // Setting what a key already holds writes nothing: the bytes and the modification time stay.
    let then = set_mtime_long_ago(&h);
    let before = read(&h);
    for (key, value) in [
        ("default_model", "opus"),
        ("defaultModel", OPUS),
        ("region", "us-west-2"),
    ] {
        let o = ham(&h, &["config", "set", key, value]).await;
        assert_eq!(code(&o), 0, "{key}: {}", err(&o));
        assert_eq!(read(&h), before, "{key}");
        assert_eq!(mtime(&h), then, "{key}: the file was not written");
    }
    // Unset removes the one line and keeps the rest.
    let o = ham(&h, &["config", "unset", "default_model"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).trim(), "default_model = (not set)");
    assert_eq!(
        read(&h),
        OWNER.replace("us-east-1\"\nawsProfile", "us-west-2\"\nawsProfile")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_run_compute_writes_the_word_in_lower_case_and_another_word_changes_nothing() {
    // The machine setting takes the three words in any case, stores them in lower case under [run], and refuses every other word without
    // touching the file.
    let h = bare("verbs-compute");
    write(&h, OWNER);
    let o = ham(&h, &["config", "set", "run.compute", "Instances"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).trim(), "run.compute = instances");
    let text = read(&h);
    assert!(text.contains("[run]\ncompute = \"instances\"\n"), "{text}");
    let after = read(&h);
    let o = ham(&h, &["config", "set", "run.compute", "gpu"]).await;
    assert_eq!(code(&o), 1, "{}", out(&o));
    let said = format!("{}{}", out(&o), err(&o));
    assert!(
        said.contains("auto, microvm or instances"),
        "the refusal lists the words: {said}"
    );
    assert_eq!(read(&h), after, "a refused word writes nothing");
    let o = ham(&h, &["config", "unset", "run.compute"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).trim(), "run.compute = (not set)");
    assert!(!read(&h).contains("compute"), "{}", read(&h));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_machine_set_writes_exactly_the_one_line_it_wrote_before() {
    let h = bare("verbs-fresh");
    assert!(!path(&h).exists());
    let o = ham(&h, &["config", "set", "default_model", "astra"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(read(&h), format!("defaultModel = \"{ASTRA}\"\n"));
    assert_eq!(names_in_config_dir(&h), ["config.toml"]);
    let mode = std::fs::metadata(path(&h)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a new file is private");
    assert!(
        err(&o).is_empty(),
        "a legacy key prints no notice: {}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_new_key_prints_the_door_note_once_after_the_write_in_the_cli() {
    let h = bare("verbs-door");
    write(&h, OWNER);
    let o = ham(&h, &["config", "set", "run.goal", "off"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).trim(), "run.goal = false");
    assert_eq!(
        read(&h),
        format!("{OWNER}configVersion = 1\n\n[run]\ngoal = false\n")
    );
    let note = err(&o);
    assert!(
        note.contains(&format!(
            "ham versions before v{} refuse this file",
            ham::version::CLIENT
        )),
        "{note}"
    );
    assert!(
        note.contains("`ham config unset run.goal`")
            && note.contains("`[run]` and `configVersion`"),
        "the note names the way back: {note}"
    );
    // The second key, in the same table or in another, crosses no door: no note.
    for args in [
        ["config", "set", "run.rules", "off"],
        ["config", "set", "research.enabled", "on"],
        ["config", "set", "ui.bell", "off"],
    ] {
        let o = ham(&h, &args).await;
        assert_eq!(code(&o), 0, "{args:?}: {}", err(&o));
        assert!(
            !err(&o).contains("refuse this file"),
            "{args:?}: {}",
            err(&o)
        );
    }
    // JSON keeps standard output to one JSON line; the note goes to standard error.
    let h2 = bare("verbs-door-json");
    let o = ham(&h2, &["config", "set", "ui.notify", "on", "--json"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["key"], "ui.notify");
    assert_eq!(v["value"], "true");
    assert!(err(&o).contains("refuse this file"));
    // A legacy key on a fresh machine prints no note, and unsetting the last new key brings the file back to the form older builds read.
    let o = ham(&h2, &["config", "unset", "ui.notify"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(read(&h2), "");
    assert!(!err(&o).contains("refuse this file"));
}

// ---- repair: the verbs run on a file with a bad key, the commands that start work refuse it ------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_verbs_run_on_a_file_with_a_bad_key_while_models_and_runs_refuse_naming_the_line() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "verbs-badkey");
    let base = read(&h);
    assert_eq!(base.lines().count(), 3);
    // A typo in a table: line 7 of the file.
    write(
        &h,
        &format!("{base}configVersion = 1\n\n[run]\ngoall = true\n"),
    );
    // The commands that read the file still run, and say what is wrong with it.
    let o = ham(&h, &["config"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(out(&o).contains("api_endpoint = http"), "{}", out(&o));
    assert!(
        err(&o).contains("line 7")
            && err(&o).contains("goall")
            && err(&o).contains("did you mean `goal`"),
        "{}",
        err(&o)
    );
    let o = ham(&h, &["config", "get", "region"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).trim(), support::REGION);
    assert!(err(&o).contains("line 7"), "{}", err(&o));
    for verb in [["config", "show"], ["config", "docs"]] {
        let o = ham(&h, &verb).await;
        assert_eq!(code(&o), 0, "{verb:?}: {}", err(&o));
    }
    let o = ham(&h, &["config", "check"]).await;
    assert_eq!(code(&o), 1, "{}", out(&o));
    assert!(
        out(&o).contains("line 7") && out(&o).contains("goall"),
        "{}",
        out(&o)
    );
    // Doctor lists the finding on a line of its own and goes on with the checks that need the stage.
    let o = ham(&h, &["doctor"]).await;
    let doctor = out(&o);
    assert_eq!(code(&o), 1, "{doctor}{}", err(&o));
    assert!(
        doctor.lines().next().unwrap().starts_with("FAIL config ")
            && doctor.lines().next().unwrap().contains("line 7"),
        "{doctor}"
    );
    assert!(doctor.contains("PASS identity "), "{doctor}");
    // `--version` reads no file.
    let o = ham(&h, &["--version"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    // The commands that start work or read the stage refuse, name the line, and send nothing.
    let seen_before = fake.mem.seen().len();
    for args in [
        vec!["models"],
        vec!["runs"],
        vec!["Add a function", "--s3-only", "--direct"],
        vec!["pull", "01M4Y8N48K0MSH6F59M0JN69PC"],
    ] {
        let o = ham(&h, &args).await;
        assert_eq!(code(&o), 1, "{args:?}: {}", out(&o));
        assert!(
            err(&o).contains("line 7") && err(&o).contains("goall"),
            "{args:?}: {}",
            err(&o)
        );
    }
    assert_eq!(
        fake.mem.seen().len(),
        seen_before,
        "nothing reached the stage"
    );
    // A set patches around the bad key and says it is still there; the typo is not touched.
    let o = ham(&h, &["config", "set", "default_model", "opus"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(read(&h).contains("[run]\ngoall = true\n"));
    assert!(
        err(&o).contains("goall") && err(&o).contains("line 8"),
        "{}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_value_is_repaired_by_set_or_unset_and_unset_removes_exactly_that_line() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "verbs-repair");
    let base = read(&h);
    write(&h, &format!("{base}mouse = \"maybe\"\n"));
    let o = ham(&h, &["models"]).await;
    assert_eq!(code(&o), 1);
    assert!(
        err(&o).contains("line 4") && err(&o).contains("mouse"),
        "{}",
        err(&o)
    );
    let o = ham(&h, &["config", "unset", "mouse"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(read(&h), base);
    assert_eq!(code(&ham(&h, &["models"]).await), 0);
    write(&h, &format!("{base}mouse = \"maybe\"\n"));
    let o = ham(&h, &["config", "set", "mouse", "false"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(read(&h), format!("{base}mouse = false\n"));
    assert_eq!(code(&ham(&h, &["models"]).await), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn set_refuses_a_file_that_is_not_toml_and_writes_nothing() {
    let h = bare("verbs-nottoml");
    for broken in [
        "this is not toml {\n",
        "region = \nmouse = true\n",
        "region = \"us-east-1\"\nmouse = maybe\n",
        "\u{feff}region = \"us-east-1\"\n",
    ] {
        write(&h, broken);
        let before = std::fs::read(path(&h)).unwrap();
        for args in [
            vec!["config", "set", "default_model", "opus"],
            vec!["config", "unset", "region"],
        ] {
            let o = ham(&h, &args).await;
            assert_eq!(code(&o), 1, "{args:?}: {}", out(&o));
            assert!(
                err(&o).contains("config.toml") && err(&o).contains("line"),
                "{args:?}: {}",
                err(&o)
            );
            assert!(
                !err(&o).contains("maybe"),
                "no value of the file is echoed: {}",
                err(&o)
            );
            assert_eq!(std::fs::read(path(&h)).unwrap(), before, "{broken:?}");
            assert_eq!(names_in_config_dir(&h), ["config.toml"]);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_from_a_newer_ham_is_read_and_never_changed() {
    let h = bare("verbs-newer");
    let file = "configVersion = 2\nregion = \"us-east-1\"\n\n[ui]\nfuture = 1\n";
    write(&h, file);
    let o = ham(&h, &["config", "get", "region"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o).trim(), "us-east-1");
    let o = ham(&h, &["config", "set", "region", "us-west-2"]).await;
    assert_eq!(code(&o), 1);
    assert!(
        err(&o).contains("newer ham") && err(&o).contains("Nothing was written"),
        "{}",
        err(&o)
    );
    assert_eq!(read(&h), file);
    let o = ham(&h, &["config", "unset", "region"]).await;
    assert_eq!(code(&o), 1);
    assert_eq!(read(&h), file);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_symlinked_file_keeps_its_link_and_mode_and_leaves_no_temp_file() {
    let h = bare("verbs-link");
    let real = h.root.join("dotfiles/ham-config.toml");
    std::fs::create_dir_all(real.parent().unwrap()).unwrap();
    std::fs::write(&real, OWNER).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o664)).unwrap();
    symlink(&real, path(&h)).unwrap();
    let o = ham(&h, &["config", "set", "default_model", "opus"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        std::fs::symlink_metadata(path(&h))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        format!("{OWNER}defaultModel = \"{OPUS}\"\n")
    );
    let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o664,
        "the file keeps its permission bits whatever the umask"
    );
    for dir in [h.config.join("ham"), h.root.join("dotfiles")] {
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains(".tmp")),
            "{dir:?}: {names:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_config_file_that_resolves_to_something_that_is_not_a_regular_file_is_refused() {
    let h = bare("verbs-notregular");
    let dir = h.root.join("a-directory");
    std::fs::create_dir_all(&dir).unwrap();
    symlink(&dir, path(&h)).unwrap();
    for args in [
        vec!["config", "set", "default_model", "opus"],
        vec!["config", "unset", "region"],
        vec!["config", "get", "region"],
        vec!["config"],
        vec!["config", "check"],
        vec!["models"],
    ] {
        let o = ham(&h, &args).await;
        assert_eq!(code(&o), 1, "{args:?}: {}{}", out(&o), err(&o));
        assert!(
            err(&o).contains("not a regular file"),
            "{args:?}: {}",
            err(&o)
        );
    }
    assert!(
        std::fs::symlink_metadata(path(&h))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "nothing was written in the folder"
    );
    assert_eq!(names_in_config_dir(&h), ["config.toml"]);
    // A file past the cap is refused by its size, read only as far as the cap.
    let h = bare("verbs-huge");
    write(&h, &format!("# {}\n", "a".repeat(70_000)));
    let o = ham(&h, &["config", "get", "region"]).await;
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("65536"), "{}", err(&o));
}

// ---- list, get and the JSON shapes ---------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_and_json_shapes_and_exit_codes_are_unchanged_except_for_the_new_lines() {
    let h = bare("verbs-shapes");
    write(&h, &format!("{OWNER}defaultModel = \"{OPUS}\"\n"));
    let o = ham(&h, &["config"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(
        out(&o),
        format!(
            "api_endpoint = https://abc.execute-api.us-east-1.amazonaws.com/direct/v1\nregion = us-east-1\naws_profile = ham-test\ndefault_repo = (not set)\ndownload_dir = (not set)\ndefault_model = {OPUS}\ninput_wait_minutes = (not set)\nmouse = (not set)\nreport_tests = (not set)\nrun.goal = (not set)\nrun.rules = (not set)\nrun.skills = (not set)\nresearch.enabled = (not set)\nui.notify = (not set)\nui.bell = (not set)\nui.mascot = (not set)\nrun.report_command_minutes = (not set)\nrun.stall_calls = (not set)\nrun.stall_window_minutes = (not set)\nrun.stall_min_calls = (not set)\nrun.command_grace_minutes = (not set)\nrun.final_message = (not set)\nrun.lifecycle_flow = (not set)\nrun.review_round = (not set)\nrun.compute = (not set)\nrun.stall_new_calls = (not set)\nrun.cargo_no_debug_info = (not set)\nrun.cargo_no_incremental = (not set)\nrun.exclude_target_dir = (not set)\nrun.outside_checkout_note = (not set)\nui.deprecation_notices = (not set)\nui.mascot_motion = (not set)\nrun.max_run_hours = (not set)\nrun.provider_sharing = (not set)\ndefault_backend = (not set)\ncustomer_content = (not set)\nui.copy_limit_kib = (not set)\nrun.max_interrupts = (not set)\nrun.max_questions = (not set)\nrun.max_fresh_sessions = (not set)\nresearch.max_file_mib = (not set)\nresearch.max_unpacked_mib = (not set)\nresearch.max_run_mib = (not set)\nrun.container_signals = (not set)\ndefault_engine = (not set)\nupdate_source = (not set)\n"
        )
    );
    let o = ham(&h, &["config", "get"]).await;
    assert_eq!(out(&o).lines().count(), 46);
    // The JSON object: the forty-six names in snake case with the value or null, and `path`.
    let o = ham(&h, &["config", "--json"]).await;
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    let obj = v.as_object().unwrap();
    assert_eq!(obj.len(), 47);
    assert_eq!(v["region"], "us-east-1");
    assert!(v["run.goal"].is_null() && v["default_repo"].is_null());
    assert_eq!(v["default_model"], OPUS);
    assert!(v["path"].as_str().unwrap().ends_with("ham/config.toml"));
    // `get`: the value alone; an unset key prints nothing on standard output, says so on standard error and exits 1.
    let o = ham(&h, &["config", "get", "region"]).await;
    assert_eq!((code(&o), out(&o).as_str()), (0, "us-east-1\n"));
    let o = ham(&h, &["config", "get", "run.goal"]).await;
    assert_eq!((code(&o), out(&o).as_str()), (1, ""));
    assert!(err(&o).contains("ham: run.goal is not set"), "{}", err(&o));
    let o = ham(&h, &["config", "get", "region", "--json"]).await;
    assert_eq!(out(&o).trim(), r#"{"key":"region","value":"us-east-1"}"#);
    let o = ham(&h, &["config", "get", "run.goal", "--json"]).await;
    assert_eq!(code(&o), 1);
    assert_eq!(out(&o).trim(), r#"{"key":"run.goal","value":null}"#);
    // `set` and `unset` keep their object shapes.
    let o = ham(&h, &["config", "set", "mouse", "off", "--json"]).await;
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["key"], "mouse");
    assert_eq!(v["value"], "false");
    assert!(v["path"].as_str().unwrap().ends_with("ham/config.toml"));
    assert_eq!(v.as_object().unwrap().len(), 3);
    let o = ham(&h, &["config", "unset", "mouse", "--json"]).await;
    assert_eq!(out(&o).trim(), r#"{"key":"mouse","value":null}"#);
    // Usage errors are exit 2; an unknown key is exit 1 and lists the keys.
    assert_eq!(code(&ham(&h, &["config", "set", "mouse"]).await), 2);
    assert_eq!(code(&ham(&h, &["config", "frobnicate"]).await), 2);
    let o = ham(&h, &["config", "get", "nokey"]).await;
    assert_eq!(code(&o), 1);
    assert!(
        err(&o).contains("api_endpoint") && err(&o).contains("ui.bell"),
        "{}",
        err(&o)
    );
}

// ---- show, check, docs, edit -------------------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_prints_origin_and_applies_for_every_key_then_the_locked_rows() {
    let h = bare("verbs-show");
    write(
        &h,
        &format!(
            "{OWNER}defaultModel = \"{OPUS}\"\nmouse = false\nconfigVersion = 1\n\n[run]\ngoal = false\n\n[ui]\nnotify = false\n"
        ),
    );
    let env = [
        ("HAMH_MODEL", "sol"),
        ("HAMH_INPUT_WAIT", "12"),
        ("HAMH_NOTIFY", "1"),
        ("AWS_REGION", "eu-west-1"),
    ];
    let o = ham_env(&h, &["config", "show"], &env).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    let shown = out(&o);
    let row = |name: &str| -> String {
        shown
            .lines()
            .find(|l| l.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("no row for {name}:\n{shown}"))
            .to_string()
    };
    let has = |name: &str, needles: &[&str]| {
        let r = row(name);
        for n in needles {
            assert!(r.contains(n), "{name}: wanted {n:?} in {r:?}");
        }
    };
    // The file wins over AWS_REGION; the environment wins over the file for the model, the wait, the download folder and the notification.
    has("region", &["us-east-1", "file", "restart"]);
    has("default_model", &[SOL, "env HAMH_MODEL", "next run"]);
    has(
        "input_wait_minutes",
        &["12", "env HAMH_INPUT_WAIT", "next run"],
    );
    // The test harness sets HAMH_DOWNLOAD_DIR to the downloads folder of the home.
    has(
        "download_dir",
        &[
            &h.downloads.display().to_string(),
            "env HAMH_DOWNLOAD_DIR",
            "restart",
        ],
    );
    has("ui.notify", &["true", "env HAMH_NOTIFY", "now"]);
    // The file when it is set and no variable takes part; the default otherwise, with the stage's own word for the switches it decides.
    has("mouse", &["false", "file", "restart"]);
    has("run.goal", &["false", "file", "next run"]);
    has("run.rules", &["on", "default", "next run"]);
    has(
        "research.enabled",
        &["stage default", "default", "next run"],
    );
    has("default_repo", &["none", "default", "next composer"]);
    has("ui.bell", &["on", "default", "now"]);
    has("ui.mascot_motion", &["on", "default", "now"]);
    has(
        "api_endpoint",
        &["https://abc.execute-api", "file", "restart"],
    );
    for key in ham_core::config::KEYS.iter().map(|(_, printed)| *printed) {
        let _ = row(key);
    }
    // The locked rows come after the keys, one line per class with its reason.
    let at = shown
        .find("Locked by the harness")
        .expect("a locked section");
    assert!(shown.rfind("ui.bell ").unwrap() < at);
    let locked = &shown[at..];
    for class in [
        "Credentials and identity",
        "Provider, model, endpoint",
        "Telemetry and update",
        "Processes and paths",
        "Permissions",
        "Network",
        "Owned elsewhere",
    ] {
        assert!(locked.contains(class), "{class} missing:\n{shown}");
    }
    assert!(
        locked.contains("the harness supplies credentials itself; this cannot be set")
            && locked.contains("telemetry and updates are off for every run and stay off"),
        "each class says why:\n{shown}"
    );
    // HAMH_NO_MOUSE forces the mouse off whatever the file says.
    let o = ham_env(&h, &["config", "show"], &[("HAMH_NO_MOUSE", "1")]).await;
    let shown = out(&o);
    let mouse = shown.lines().find(|l| l.starts_with("mouse ")).unwrap();
    assert!(
        mouse.contains("false") && mouse.contains("env HAMH_NO_MOUSE"),
        "{mouse}"
    );
    // JSON: the keys in registry order, each with value, origin and when it applies, and the locked classes.
    let o = ham_env(&h, &["config", "show", "--json"], &env).await;
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    let keys = v["settings"].as_array().unwrap();
    assert_eq!(keys.len(), 46);
    assert_eq!(keys[0]["key"], "api_endpoint");
    assert_eq!(keys[5]["key"], "default_model");
    assert_eq!(keys[5]["origin"], "env HAMH_MODEL");
    assert_eq!(keys[5]["value"], SOL);
    assert_eq!(keys[9]["key"], "run.goal");
    assert_eq!(keys[9]["origin"], "file");
    assert_eq!(keys[9]["applies"], "next run");
    assert_eq!(v["locked"].as_array().unwrap().len(), 7);
    assert!(v["path"].as_str().unwrap().ends_with("ham/config.toml"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn show_on_a_machine_with_no_file_says_every_key_is_at_its_default() {
    let h = bare("verbs-show-fresh");
    let o = ham(&h, &["config", "show"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    let shown = out(&o);
    assert!(shown.contains("Locked by the harness"));
    for line in shown.lines().take(15) {
        // The harness sets HAMH_DOWNLOAD_DIR; every other key is at its default.
        assert!(
            line.contains("default") || line.starts_with("download_dir "),
            "{line}"
        );
    }
    assert!(!path(&h).exists(), "show writes nothing");
    assert_eq!(names_in_config_dir(&h), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_exits_0_1_2_and_strict_turns_warnings_into_errors() {
    let h = bare("verbs-check");
    // No file, an empty file and a clean file are clean.
    let o = ham(&h, &["config", "check"]).await;
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("every setting is at its default"),
        "{}",
        out(&o)
    );
    write(&h, "");
    assert_eq!(code(&ham(&h, &["config", "check"]).await), 0);
    write(
        &h,
        &format!("{OWNER}configVersion = 1\n\n[run]\ngoal = false\n"),
    );
    let o = ham(&h, &["config", "check", "--strict"]).await;
    assert_eq!(code(&o), 0, "{}", out(&o));
    // An error finding exits 1 and names the line; nothing about the value is printed.
    write(&h, &format!("{OWNER}mouse = \"maybe\"\n"));
    let o = ham(&h, &["config", "check"]).await;
    assert_eq!(code(&o), 1);
    assert!(
        out(&o).contains("line 6") && out(&o).contains("mouse"),
        "{}",
        out(&o)
    );
    assert!(!out(&o).contains("maybe"), "{}", out(&o));
    // A file from a newer ham warns: exit 0, and 1 with --strict.
    write(
        &h,
        "configVersion = 2\nregion = \"us-east-1\"\n\n[ui]\nfuture = 1\n",
    );
    let o = ham(&h, &["config", "check"]).await;
    assert_eq!(code(&o), 0, "{}", out(&o));
    assert!(
        out(&o).contains("warning") && out(&o).contains("line 5"),
        "{}",
        out(&o)
    );
    let o = ham(&h, &["config", "check", "--strict"]).await;
    assert_eq!(code(&o), 1, "{}", out(&o));
    // A newer file whose unknown key may steer a run blocks run starts, which a check must not call fine.
    write(&h, "configVersion = 2\n\n[run]\nfuture = true\n");
    let o = ham(&h, &["config", "check"]).await;
    assert_eq!(code(&o), 1, "{}", out(&o));
    assert!(out(&o).contains("newer ham"), "{}", out(&o));
    // A file that is not TOML.
    write(&h, "region = \n");
    let o = ham(&h, &["config", "check"]).await;
    assert_eq!(code(&o), 1);
    assert!(out(&o).contains("line 1"), "{}", out(&o));
    // JSON.
    write(&h, &format!("{OWNER}goal = true\n"));
    let o = ham(&h, &["config", "check", "--json"]).await;
    assert_eq!(code(&o), 1);
    let v: Value = serde_json::from_str(out(&o).trim()).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["errors"], 1);
    assert_eq!(v["findings"][0]["severity"], "error");
    assert_eq!(v["findings"][0]["code"], "unknown_key");
    assert_eq!(v["findings"][0]["line"], 6);
    // Usage.
    assert_eq!(code(&ham(&h, &["config", "check", "--loose"]).await), 2);
    assert_eq!(code(&ham(&h, &["config", "check", "extra"]).await), 2);
    assert_eq!(code(&ham(&h, &["config", "show", "extra"]).await), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn docs_prints_the_reference_table_of_the_registry() {
    let h = bare("verbs-docs");
    let o = ham(&h, &["config", "docs"]).await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert_eq!(out(&o), ham_core::settings::reference_markdown());
    assert_eq!(
        out(&o).lines().count(),
        48,
        "a header, a rule and forty-six rows"
    );
    assert!(!path(&h).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_opens_the_editor_on_the_file_and_checks_what_it_left() {
    let h = bare("verbs-edit");
    write(&h, OWNER);
    // The editor appends a good key: exit 0 and the check has nothing to say.
    let ed = editor(&h, r#"printf 'mouse = false\n' >> "$1""#);
    let o = ham_env(&h, &["config", "edit"], &[("EDITOR", ed.as_str())]).await;
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(read(&h), format!("{OWNER}mouse = false\n"));
    // VISUAL wins over EDITOR.
    let visual = editor(&h, r#"printf '# visual\n' >> "$1""#);
    let plain = h.root.join("plain.sh");
    std::fs::write(&plain, "#!/bin/sh\nprintf '# editor\\n' >> \"$1\"\n").unwrap();
    let plain = format!("sh {}", plain.display());
    let o = ham_env(
        &h,
        &["config", "edit"],
        &[("VISUAL", visual.as_str()), ("EDITOR", plain.as_str())],
    )
    .await;
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(
        read(&h).ends_with("mouse = false\n# visual\n"),
        "{}",
        read(&h)
    );
    // The editor leaves a bad key: exit 1, the finding with its line, and how to fix it.
    write(&h, OWNER);
    let bad = editor(&h, r#"printf 'surprise = 1\n' >> "$1""#);
    let o = ham_env(&h, &["config", "edit"], &[("EDITOR", bad.as_str())]).await;
    assert_eq!(code(&o), 1);
    let all = format!("{}{}", out(&o), err(&o));
    assert!(all.contains("surprise") && all.contains("line 6"), "{all}");
    assert!(all.contains("ham config edit"), "says how to fix it: {all}");
    // The same command repairs it.
    let fix = editor(
        &h,
        r#"grep -v surprise "$1" > "$1.new"; cat "$1.new" > "$1""#,
    );
    let o = ham_env(&h, &["config", "edit"], &[("EDITOR", fix.as_str())]).await;
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(read(&h), OWNER);
    // With no file the folder is there and the editor makes the file.
    let h = bare("verbs-edit-fresh");
    let make = editor(&h, r#"printf 'region = "eu-west-1"\n' > "$1""#);
    let o = ham_env(&h, &["config", "edit"], &[("EDITOR", make.as_str())]).await;
    assert_eq!(code(&o), 0, "{}{}", out(&o), err(&o));
    assert_eq!(read(&h), "region = \"eu-west-1\"\n");
    // An editor that fails is reported.
    let fails = editor(&h, "exit 3");
    let o = ham_env(&h, &["config", "edit"], &[("EDITOR", fails.as_str())]).await;
    assert_eq!(code(&o), 1);
    assert!(
        err(&o).contains("editor") && err(&o).contains('3'),
        "{}",
        err(&o)
    );
}

// ---- the callers that read the file with other tools ---------------------------------------------------------------------------------------

/// The `sed` of `setup.sh` (`read_cfg_key`) over the file, for a camelCase key.
fn setup_sh_read(h: &Home, key: &str) -> String {
    let script = format!("s/^{key}[[:space:]]*=[[:space:]]*\"\\(.*\\)\"[[:space:]]*$/\\1/p");
    let o = std::process::Command::new("sed")
        .arg("-n")
        .arg(script)
        .arg(path(h))
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn lane_a_port(text: &str) -> Option<u16> {
    for (at, _) in text.match_indices("ndpoint") {
        let head = &text[..at];
        let name_ok = ["apiE", "apie", "api_E", "api_e"]
            .iter()
            .any(|n| head.ends_with(n));
        if !name_ok {
            continue;
        }
        let rest = text[at + "ndpoint".len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        let rest = rest
            .strip_prefix("https://")
            .or_else(|| rest.strip_prefix("http://"))?;
        let host_len = rest.find(['"', ':', '/'])?;
        let rest = rest[host_len..].strip_prefix(':')?;
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        return digits.parse().ok();
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_sh_style_calls_still_work_on_a_commented_file_the_editor_wrote() {
    let h = bare("verbs-setup");
    write(
        &h,
        "# written by hand\napiEndpoint = \"http://127.0.0.1:8080/v1\"\n# the Region\nregion = \"us-east-1\"\nawsProfile = \"ham-test\" # my profile\n",
    );
    for args in [
        ["config", "set", "default_model", "opus"],
        ["config", "set", "default_repo", "cc:my-service"],
        ["config", "set", "run.goal", "off"],
        ["config", "set", "ui.bell", "off"],
        ["config", "set", "region", "us-west-2"],
    ] {
        let o = ham(&h, &args).await;
        assert_eq!(code(&o), 0, "{args:?}: {}", err(&o));
    }
    let file = read(&h);
    // `setup.sh` reads a top-level `key = "value"` line with sed; every legacy key the editor wrote is one.
    assert_eq!(setup_sh_read(&h, "apiEndpoint"), "http://127.0.0.1:8080/v1");
    assert_eq!(setup_sh_read(&h, "region"), "us-west-2");
    // A trailing comment of the user's own on a line is theirs; `setup.sh` has never read such a line, and the editor keeps the comment.
    assert!(
        file.contains("awsProfile = \"ham-test\" # my profile\n"),
        "{file}"
    );
    assert_eq!(setup_sh_read(&h, "defaultModel"), OPUS);
    assert_eq!(setup_sh_read(&h, "defaultRepo"), "cc:my-service");
    // The Python drivers find the port of the endpoint.
    assert_eq!(lane_a_port(&file), Some(8080), "{file}");
    // The comments and the lines the editor did not touch are where they were.
    assert!(
        file.starts_with(
            "# written by hand\napiEndpoint = \"http://127.0.0.1:8080/v1\"\n# the Region\nregion = \"us-west-2\"\n"
        ),
        "{file}"
    );
    // `setup.sh` calls `ham config get` and `ham config set`, and branches on the exit code.
    assert_eq!(code(&ham(&h, &["config", "get", "region"]).await), 0);
    assert_eq!(code(&ham(&h, &["config", "get", "download_dir"]).await), 1);
    assert_eq!(
        code(&ham(&h, &["config", "set", "region", "us-west-2"]).await),
        0
    );
}
