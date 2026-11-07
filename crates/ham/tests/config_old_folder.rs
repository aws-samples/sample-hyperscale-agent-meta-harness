// COMPAT-CAH: this file names the folder `cah` and the variables `CAH_*` of the earlier command name on purpose.
//! The compiled `ham` when the files of the client are still in `~/.config/cah`, the folder of the earlier command name: the folder is read and
//! never written, a command that would write there exits 2 and names `ham config migrate`, one notice is printed on standard error (and only
//! one), the notice goes off with `HAMH_NO_DEPRECATION_NOTICES=1` or `ui.deprecation_notices = false`, and the earlier names of the variables
//! answer after the new ones with a notice of their own.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use std::path::Path;

const ENDPOINT: &str = "https://ham-old-folder.invalid/direct";
const NOTICE: &str = "Your settings are read from ";

/// A home whose settings are in the folder of the earlier name: `config/cah/config.toml` and nothing in `config/ham`.
fn earlier_home(label: &str) -> home::Home {
    let h = home::home_with(ENDPOINT, label, "ham-test");
    std::fs::rename(h.config.join("ham"), h.config.join("cah")).unwrap();
    h
}

fn bytes_of(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for p in names {
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push((
                    p.strip_prefix(base).unwrap().display().to_string(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

#[tokio::test]
async fn the_settings_are_read_from_the_earlier_folder_with_one_notice() {
    let h = earlier_home("old-read");
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    let (out, err) = (home::text(&o.stdout), home::text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert_eq!(out.trim_end(), ENDPOINT);
    assert_eq!(err.matches(NOTICE).count(), 1, "{err}");
    assert!(
        err.contains(&format!(
            "{NOTICE}{}, the folder of the old name. Copy them with: ham config migrate",
            h.config.join("cah").display()
        )),
        "{err}"
    );
    assert!(!h.config.join("ham").exists(), "ham made no folder");
}

#[tokio::test]
async fn the_notice_is_one_line_per_process_whatever_the_command_reads() {
    let h = earlier_home("old-once");
    let o = home::ham(&h, &["config", "show"]).await;
    let err = home::text(&o.stderr);
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert_eq!(err.matches(NOTICE).count(), 1, "{err}");
    assert_eq!(err.lines().count(), 1, "{err}");
}

#[tokio::test]
async fn the_new_folder_alone_makes_no_notice() {
    let h = home::home_with(ENDPOINT, "new-only", "ham-test");
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(home::text(&o.stderr), "");
}

#[tokio::test]
async fn a_command_that_would_write_exits_2_names_the_migrate_command_and_writes_nothing() {
    let h = earlier_home("old-write");
    let before = bytes_of(&h.config);
    for args in [
        vec!["config", "set", "mouse", "false"],
        vec!["config", "unset", "mouse"],
        vec!["rules", "init"],
        vec!["rules", "init", "--no-skills"],
        vec!["engine", "cline", "init"],
        vec!["engine", "cline", "set", "temperature", "0.2"],
    ] {
        let o = home::ham_env(&h, &args, &[("EDITOR", "true")]).await;
        let err = home::text(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {err}");
        assert!(err.contains("ham config migrate"), "{args:?}: {err}");
        assert!(
            err.contains(&h.config.join("cah").display().to_string()),
            "{args:?}: {err}"
        );
        assert!(
            err.contains(&h.config.join("ham").display().to_string()),
            "{args:?}: {err}"
        );
        assert!(home::text(&o.stdout).is_empty(), "{args:?}");
    }
    assert!(!h.config.join("ham").exists(), "no new folder was made");
    assert_eq!(
        bytes_of(&h.config),
        before,
        "the earlier folder is as it was"
    );
}

#[tokio::test]
async fn the_edit_commands_refuse_before_they_open_an_editor() {
    let h = earlier_home("old-edit");
    let marker = h.root.join("editor-ran");
    let script = h.root.join("editor.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    for args in [
        vec!["config", "edit"],
        vec!["rules", "edit"],
        vec!["engine", "cline", "edit"],
    ] {
        let o = home::ham_env(&h, &args, &[("EDITOR", script.to_str().unwrap())]).await;
        assert_eq!(
            o.status.code(),
            Some(2),
            "{args:?}: {}",
            home::text(&o.stderr)
        );
        assert!(!marker.exists(), "{args:?} opened the editor");
    }
}

#[tokio::test]
async fn reading_commands_still_work_while_only_the_earlier_folder_exists() {
    let h = earlier_home("old-reading");
    for args in [
        vec!["config", "check"],
        vec!["config", "docs"],
        vec!["config"],
        vec!["rules", "show"],
        vec!["engine", "cline", "show"],
    ] {
        let o = home::ham(&h, &args).await;
        assert_ne!(
            o.status.code(),
            Some(2),
            "{args:?}: {}",
            home::text(&o.stderr)
        );
    }
}

#[tokio::test]
async fn an_empty_new_folder_does_not_hide_the_earlier_one() {
    let h = earlier_home("old-empty-new");
    for sub in ["ham", "ham/rules", "ham/skills", "ham/engines"] {
        std::fs::create_dir_all(h.config.join(sub)).unwrap();
    }
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", home::text(&o.stderr));
    assert_eq!(home::text(&o.stdout).trim_end(), ENDPOINT);
    assert_eq!(home::text(&o.stderr).matches(NOTICE).count(), 1);
}

#[tokio::test]
async fn when_both_folders_hold_files_the_new_one_wins_and_config_show_says_the_other_is_not_read()
{
    let h = earlier_home("old-both");
    std::fs::create_dir_all(h.config.join("ham")).unwrap();
    std::fs::write(
        h.config.join("ham/config.toml"),
        "apiEndpoint = \"https://ham-new-folder.invalid/direct\"\n",
    )
    .unwrap();
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    assert_eq!(
        home::text(&o.stdout).trim_end(),
        "https://ham-new-folder.invalid/direct"
    );
    assert_eq!(
        home::text(&o.stderr),
        "",
        "no notice: nothing is read from the old folder"
    );
    let o = home::ham(&h, &["config", "show"]).await;
    let out = home::text(&o.stdout);
    let line = format!(
        "{} also exists and is not read.",
        h.config.join("cah").display()
    );
    assert_eq!(out.matches(&line).count(), 1, "{out}");
    let o = home::ham_env(&h, &["config", "show", "--json"], &[]).await;
    let json: serde_json::Value = serde_json::from_str(home::text(&o.stdout).trim()).unwrap();
    assert_eq!(
        json["earlierFolderNotRead"],
        h.config.join("cah").display().to_string()
    );
}

#[tokio::test]
async fn an_earlier_variable_answers_after_the_new_one_with_a_notice_of_its_own() {
    let h = home::home_with(ENDPOINT, "old-var", "ham-test");
    let o = home::ham_env(&h, &["config", "show"], &[("CAH_MODEL", "opus")]).await;
    let (out, err) = (home::text(&o.stdout), home::text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert_eq!(
        err.trim_end(),
        "CAH_MODEL is the old name of HAMH_MODEL. It still works and will stop working in a later release."
    );
    let row = out
        .lines()
        .find(|l| l.starts_with("default_model"))
        .unwrap();
    assert!(
        row.contains("us.anthropic.claude-opus-5-5") && row.contains("env CAH_MODEL"),
        "{row}"
    );
}

#[tokio::test]
async fn the_new_name_wins_when_both_are_set_and_the_notice_says_so() {
    let h = home::home_with(ENDPOINT, "old-var-both", "ham-test");
    let o = home::ham_env(
        &h,
        &["config", "show"],
        &[("CAH_MODEL", "sonnet"), ("HAMH_MODEL", "opus")],
    )
    .await;
    let (out, err) = (home::text(&o.stdout), home::text(&o.stderr));
    assert_eq!(
        err.trim_end(),
        "Both CAH_MODEL and HAMH_MODEL are set. HAMH_MODEL is used."
    );
    let row = out
        .lines()
        .find(|l| l.starts_with("default_model"))
        .unwrap();
    assert!(
        row.contains("us.anthropic.claude-opus-5-5") && row.contains("env HAMH_MODEL"),
        "{row}"
    );
}

#[tokio::test]
async fn every_earlier_variable_makes_one_notice_and_no_other_name_does() {
    let h = home::home_with(ENDPOINT, "old-var-many", "ham-test");
    let o = home::ham_env(
        &h,
        &["config", "get", "api_endpoint"],
        &[
            ("CAH_NOTIFY", "1"),
            ("CAH_NO_MOUSE", "1"),
            ("CAH_SHELL_ENDPOINT", "x"),
        ],
    )
    .await;
    let err = home::text(&o.stderr);
    assert_eq!(err.lines().count(), 2, "{err}");
    assert!(err.contains("CAH_NOTIFY is the old name of HAMH_NOTIFY"));
    assert!(err.contains("CAH_NO_MOUSE is the old name of HAMH_NO_MOUSE"));
    assert!(
        !err.contains("SHELL_ENDPOINT"),
        "a name with no earlier name is not read: {err}"
    );
}

#[tokio::test]
async fn the_switch_silences_every_notice_and_changes_nothing_else() {
    let h = earlier_home("old-switch");
    let o = home::ham_env(
        &h,
        &["config", "get", "api_endpoint"],
        &[("HAMH_NO_DEPRECATION_NOTICES", "1"), ("CAH_MODEL", "opus")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0));
    assert_eq!(home::text(&o.stdout).trim_end(), ENDPOINT);
    assert_eq!(home::text(&o.stderr), "");
}

#[tokio::test]
async fn the_setting_silences_every_notice() {
    let h = earlier_home("old-setting");
    std::fs::write(
        h.config.join("cah/config.toml"),
        format!(
            "apiEndpoint = \"{ENDPOINT}\"\nconfigVersion = 1\n[ui]\ndeprecation_notices = false\n"
        ),
    )
    .unwrap();
    let o = home::ham_env(
        &h,
        &["config", "get", "api_endpoint"],
        &[("CAH_MODEL", "opus")],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", home::text(&o.stderr));
    assert_eq!(home::text(&o.stderr), "");
}

#[tokio::test]
async fn the_switch_shows_in_config_show_as_the_origin_of_the_setting() {
    let h = home::home_with(ENDPOINT, "old-switch-show", "ham-test");
    let o = home::ham_env(
        &h,
        &["config", "show"],
        &[("HAMH_NO_DEPRECATION_NOTICES", "1")],
    )
    .await;
    let out = home::text(&o.stdout);
    let row = out
        .lines()
        .find(|l| l.starts_with("ui.deprecation_notices"))
        .unwrap();
    assert!(
        row.contains("false") && row.contains("env HAMH_NO_DEPRECATION_NOTICES"),
        "{row}"
    );
    assert!(row.trim_end().ends_with("restart"), "{row}");
}
