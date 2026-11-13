//! `ham engine opencode ...` for its one setting, the shell command time limit. The real `ham` binary in a temp HOME; no verb
//! needs the network or AWS. `set` changes one line of `engines/opencode.toml`, a refused value writes nothing, and the other keys of the first
//! engine stay refused.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use home::{Home, ham_env, text};
use std::path::PathBuf;
use std::process::Output;

const KEY: &str = "commands.timeout_seconds";

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

fn file(h: &Home) -> PathBuf {
    h.config.join("ham/engines/opencode.toml")
}

fn read(h: &Home) -> String {
    std::fs::read_to_string(file(h)).unwrap_or_default()
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

fn both(o: &Output) -> String {
    format!("{}{}", out(o), err(o))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b800_set_writes_the_limit_into_the_file_of_the_second_engine_and_show_words_it() {
    let h = bare("b800-set");
    let o = ham(&h, &["engine", "opencode", "set", KEY, "900"]).await;
    assert_eq!(code(&o), 0, "set: {}", both(&o));
    let written = read(&h);
    assert!(written.contains("schema = 1"), "{written}");
    assert!(
        written.ends_with("\n[commands]\ntimeout_seconds = 900\n"),
        "{written}"
    );
    assert!(
        !h.config.join("ham/engines/cline.toml").exists(),
        "the file of the other engine was made"
    );

    let o = ham(&h, &["engine", "opencode", "show"]).await;
    assert_eq!(code(&o), 0, "show: {}", both(&o));
    assert!(
        out(&o).contains("commands.timeout_seconds = 900 s"),
        "{}",
        out(&o)
    );
    assert!(!out(&o).contains("none yet"), "{}", out(&o));

    let o = ham(&h, &["engine", "opencode", "check"]).await;
    assert_eq!(code(&o), 0, "check: {}", both(&o));
    assert!(
        !both(&o).contains("every setting is the engine's own"),
        "{}",
        both(&o)
    );

    // A second set changes the one line and leaves the rest alone.
    let o = ham(&h, &["engine", "opencode", "set", KEY, "1800"]).await;
    assert_eq!(code(&o), 0, "set: {}", both(&o));
    assert_eq!(read(&h), written.replace("900", "1800"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b800_unset_removes_the_limit_and_the_file_sets_nothing_again() {
    let h = bare("b800-unset");
    assert_eq!(
        code(&ham(&h, &["engine", "opencode", "set", KEY, "30"]).await),
        0
    );
    let o = ham(&h, &["engine", "opencode", "unset", KEY]).await;
    assert_eq!(code(&o), 0, "unset: {}", both(&o));
    assert!(!read(&h).contains("timeout_seconds"), "{}", read(&h));
    let o = ham(&h, &["engine", "opencode", "show"]).await;
    assert_eq!(code(&o), 0, "show: {}", both(&o));
    assert!(out(&o).contains("no setting is set"), "{}", out(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b800_a_value_the_check_refuses_writes_nothing_and_names_the_range() {
    let h = bare("b800-refuse");
    for (value, want) in [
        ("0", "a whole number from 1 to 1800"),
        ("1801", "a whole number from 1 to 1800"),
        ("abc", "takes a whole number"),
        ("900.5", "takes a whole number"),
    ] {
        let o = ham(&h, &["engine", "opencode", "set", KEY, value]).await;
        assert_eq!(code(&o), 2, "{value}: {}", both(&o));
        assert!(err(&o).contains(want), "{value}: {}", err(&o));
        assert!(!file(&h).exists(), "{value}: a file was written");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b800_a_key_of_the_first_engine_is_still_not_a_setting_of_the_second() {
    let h = bare("b800-other-keys");
    assert_eq!(
        code(&ham(&h, &["engine", "opencode", "set", KEY, "900"]).await),
        0
    );
    let before = read(&h);
    for (key, value) in [
        ("reasoning.effort", "low"),
        ("context.compaction", "off"),
        ("context.compaction_trigger", "0.5"),
        ("tools.disable", "spawn_agent"),
    ] {
        let o = ham(&h, &["engine", "opencode", "set", key, value]).await;
        assert_eq!(code(&o), 2, "{key}: {}", both(&o));
        assert!(
            err(&o).contains("is not an engine setting"),
            "{key}: {}",
            err(&o)
        );
        assert_eq!(read(&h), before, "{key}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b800_init_writes_a_template_with_the_limit_commented_out() {
    let h = bare("b800-init");
    let o = ham(&h, &["engine", "opencode", "init"]).await;
    assert_eq!(code(&o), 0, "init: {}", both(&o));
    let template = read(&h);
    assert!(template.contains("# [commands]"), "{template}");
    assert!(template.contains("# timeout_seconds = 600"), "{template}");
    for line in template.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with('#') || line.trim() == "schema = 1",
            "a line is switched on: {line:?}"
        );
    }
    // The template sets nothing: `show` says so, and `check` passes.
    let o = ham(&h, &["engine", "opencode", "show"]).await;
    assert_eq!(code(&o), 0, "show: {}", both(&o));
    assert!(out(&o).contains("no setting is set"), "{}", out(&o));
    let o = ham(&h, &["engine", "opencode", "check"]).await;
    assert_eq!(code(&o), 0, "check: {}", both(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b800_the_json_answer_still_names_a_settings_file() {
    let h = bare("b800-json");
    let o = ham(&h, &["engine", "opencode", "--json"]).await;
    let v: serde_json::Value =
        serde_json::from_str(out(&o).trim()).unwrap_or_else(|e| panic!("{e}: {}", both(&o)));
    assert_eq!(v["engine"], "opencode");
    assert_eq!(v["settingsFile"], true);
}
