//! `ham engine <name>` for the engines a person can choose: with no action it says whether the engine can be chosen and how, and what its
//! settings file offers. OpenCode's key table holds one key:
//! with no file, `show`, `check` and `init` work on it, a key of Cline's table is refused, and only `init` writes. The real `ham` binary in a
//! temp HOME; no verb needs the network or AWS.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use home::{Home, ham_env, text};
use std::process::Output;

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

async fn ham(h: &Home, args: &[&str]) -> Output {
    ham_env(h, args, &[]).await
}

fn out(o: &Output) -> String {
    text(&o.stdout)
}

fn err(o: &Output) -> String {
    text(&o.stderr)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_an_engine_name_alone_says_the_engine_is_available_and_how_to_choose_it() {
    let h = bare("b787-eng-alone");
    let o = ham(&h, &["engine", "opencode"]).await;
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    let say = out(&o);
    for want in [
        "OpenCode (opencode) is available",
        "--engine opencode",
        "/engine opencode",
        "ham config set default_engine opencode",
        "Its settings file is engines/opencode.toml",
    ] {
        assert!(say.contains(want), "{want:?} missing from:\n{say}");
    }
    assert!(!h.config.join("ham/engines").exists(), "a folder was made");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_the_default_engine_describes_its_settings_file() {
    let h = bare("b787-eng-cline");
    let o = ham(&h, &["engine", "cline"]).await;
    assert_eq!(o.status.code(), Some(0), "{}{}", out(&o), err(&o));
    let say = out(&o);
    assert!(say.contains("Cline (cline) is available"), "{say}");
    assert!(say.contains("engines/cline.toml"), "{say}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_an_engine_that_is_not_available_says_so_and_a_word_that_is_no_engine_lists_them() {
    let h = bare("b787-eng-na");
    let o = ham(&h, &["engine", "pi"]).await;
    assert_eq!(o.status.code(), Some(2), "{}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("not available in this release"),
        "{}",
        out(&o)
    );
    let o = ham(&h, &["engine", "nope"]).await;
    assert_eq!(o.status.code(), Some(2));
    assert!(
        err(&o).contains("unknown engine") && err(&o).contains("opencode"),
        "{}",
        err(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_the_settings_verbs_of_opencode_work_on_its_file_and_refuse_a_key_of_clines_table() {
    let h = bare("b787-eng-verbs");
    let file = h.config.join("ham/engines/opencode.toml");
    // `show` and `check` read; with no file they say every setting is the engine's own and write nothing.
    let o = ham(&h, &["engine", "opencode", "show"]).await;
    assert_eq!(o.status.code(), Some(0), "show: {}{}", out(&o), err(&o));
    assert!(out(&o).contains("none yet"), "{}", out(&o));
    let o = ham(&h, &["engine", "opencode", "check"]).await;
    assert_eq!(o.status.code(), Some(0), "check: {}{}", out(&o), err(&o));
    assert!(
        out(&o).contains("every setting is the engine's own"),
        "{}",
        out(&o)
    );
    assert!(!h.config.join("ham/engines").exists(), "a folder was made");
    // `init` writes the template, and in it no setting is switched on.
    let o = ham(&h, &["engine", "opencode", "init"]).await;
    assert_eq!(o.status.code(), Some(0), "init: {}{}", out(&o), err(&o));
    let template = std::fs::read_to_string(&file).unwrap();
    for line in template.lines().filter(|l| !l.trim().is_empty()) {
        assert!(
            line.starts_with('#') || line.trim() == "schema = 1",
            "a line is switched on: {line:?}"
        );
    }
    // `reasoning.effort` is no key of this table, so it is refused and the file stays as `init` wrote it.
    let o = ham(
        &h,
        &["engine", "opencode", "set", "reasoning.effort", "low"],
    )
    .await;
    assert_eq!(o.status.code(), Some(2), "set: {}{}", out(&o), err(&o));
    assert!(err(&o).contains("is not an engine setting"), "{}", err(&o));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), template);
    let o = ham(&h, &["engine", "opencode", "check"]).await;
    assert_eq!(o.status.code(), Some(0), "check: {}{}", out(&o), err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b787_json_names_the_engine_and_whether_it_has_a_settings_file() {
    let h = bare("b787-eng-json");
    let o = ham(&h, &["engine", "opencode", "--json"]).await;
    // `--json` is accepted anywhere; the answer is one line of JSON.
    let v: serde_json::Value = serde_json::from_str(out(&o).trim())
        .unwrap_or_else(|e| panic!("{e}: {}{}", out(&o), err(&o)));
    assert_eq!(v["engine"], "opencode");
    assert_eq!(v["available"], true);
    // OpenCode has a key table, so the engine has a settings file.
    assert_eq!(v["settingsFile"], true);
}
