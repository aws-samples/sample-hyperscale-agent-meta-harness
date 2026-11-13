// COMPAT-CAH: this file names the folder `cah` of the earlier command name on purpose.
//! `ham config migrate [--dry-run]`: the files of the folder `~/.config/cah` are copied into `~/.config/ham` byte for byte with their modes,
//! the old folder is left as it is, a second run changes nothing, a differing file in the new folder is never overwritten (exit 2), a link is
//! refused (exit 2), and `--dry-run` writes nothing.
#![allow(clippy::unwrap_used)]

mod home;
mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const ENDPOINT: &str = "https://ham-migrate.invalid/direct";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A home whose settings, rules, skills, engine file and nudge are in the folder of the earlier name, with a `setup/` folder beside them.
fn earlier_home(label: &str) -> home::Home {
    let h = home::home_with(ENDPOINT, label, "ham-test");
    std::fs::rename(h.config.join("ham"), h.config.join("cah")).unwrap();
    let old = h.config.join("cah");
    write(&old.join("engines/cline.toml"), "schema = 1\n");
    write(&old.join("rules/00-my-rules.md"), "my rule\n");
    write(&old.join("skills/one/SKILL.md"), "a skill\n");
    write(&old.join("continue.md"), "keep going\n");
    write(&old.join("review.md"), "review it\n");
    write(&old.join("setup/acct/region/receipt.json"), "{}\n");
    std::fs::set_permissions(
        old.join("config.toml"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::set_permissions(
        old.join("continue.md"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    h
}

#[tokio::test]
async fn migrate_copies_the_files_and_leaves_the_old_folder_and_the_setup_folder_alone() {
    let h = earlier_home("migrate-copy");
    let o = home::ham(&h, &["config", "migrate"]).await;
    let (out, err) = (home::text(&o.stdout), home::text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{err}");
    for rel in [
        "config.toml",
        "engines/cline.toml",
        "rules/00-my-rules.md",
        "skills/one/SKILL.md",
        "continue.md",
        "review.md",
    ] {
        assert_eq!(
            std::fs::read(h.config.join("cah").join(rel)).unwrap(),
            std::fs::read(h.config.join("ham").join(rel)).unwrap(),
            "{rel}"
        );
        assert!(out.contains(rel), "{rel} is listed:\n{out}");
    }
    assert!(
        !h.config.join("ham/setup").exists(),
        "setup/ is never copied"
    );
    assert!(
        h.config
            .join("cah/setup/acct/region/receipt.json")
            .is_file()
    );
    let mode = |p: &str| {
        std::fs::metadata(h.config.join("ham").join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!((mode("config.toml"), mode("continue.md")), (0o600, 0o644));
    assert!(out.contains("The old folder is unchanged"), "{out}");
    assert!(
        !err.contains("Your settings are read from"),
        "no notice after the copy: {err}"
    );
    // Now the new folder is the one read: no notice and the same value.
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    assert_eq!(home::text(&o.stdout).trim_end(), ENDPOINT);
    assert_eq!(home::text(&o.stderr), "");
}

#[tokio::test]
async fn a_dry_run_lists_the_files_and_writes_nothing() {
    let h = earlier_home("migrate-dry");
    let o = home::ham(&h, &["config", "migrate", "--dry-run"]).await;
    let (out, err) = (home::text(&o.stdout), home::text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert!(out.contains("Dry run"), "{out}");
    assert!(
        out.contains("config.toml") && out.contains("rules/00-my-rules.md"),
        "{out}"
    );
    assert!(!h.config.join("ham").exists());
    let names: Vec<String> = std::fs::read_dir(&h.config)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().all(|n| !n.contains("migrate")), "{names:?}");
}

#[tokio::test]
async fn a_second_run_changes_nothing() {
    let h = earlier_home("migrate-twice");
    assert_eq!(
        home::ham(&h, &["config", "migrate"]).await.status.code(),
        Some(0)
    );
    let before = std::fs::metadata(h.config.join("ham/config.toml"))
        .unwrap()
        .modified()
        .unwrap();
    let o = home::ham(&h, &["config", "migrate"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", home::text(&o.stderr));
    let out = home::text(&o.stdout);
    assert!(out.contains("already") || out.contains("nothing"), "{out}");
    assert_eq!(
        std::fs::metadata(h.config.join("ham/config.toml"))
            .unwrap()
            .modified()
            .unwrap(),
        before
    );
}

#[tokio::test]
async fn a_differing_file_in_the_new_folder_is_never_overwritten_and_exits_2() {
    let h = earlier_home("migrate-differs");
    write(
        &h.config.join("ham/config.toml"),
        "apiEndpoint = \"https://other.invalid/direct\"\n",
    );
    let o = home::ham(&h, &["config", "migrate"]).await;
    let err = home::text(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(err.contains("config.toml"), "{err}");
    assert_eq!(
        std::fs::read_to_string(h.config.join("ham/config.toml")).unwrap(),
        "apiEndpoint = \"https://other.invalid/direct\"\n"
    );
    assert!(!h.config.join("ham/rules").exists(), "nothing was copied");
}

#[tokio::test]
async fn a_link_in_the_old_folder_is_refused_with_exit_2() {
    let h = earlier_home("migrate-link");
    std::os::unix::fs::symlink("/etc/hostname", h.config.join("cah/rules/linked.md")).unwrap();
    let o = home::ham(&h, &["config", "migrate"]).await;
    let err = home::text(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("rules/linked.md") && err.contains("symbolic link"),
        "{err}"
    );
    assert!(!h.config.join("ham").exists());
}

#[tokio::test]
async fn with_nothing_in_the_old_folder_there_is_nothing_to_migrate() {
    let h = home::home_with(ENDPOINT, "migrate-nothing", "ham-test");
    let o = home::ham(&h, &["config", "migrate"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", home::text(&o.stderr));
    assert!(
        home::text(&o.stdout).contains("nothing to migrate"),
        "{}",
        home::text(&o.stdout)
    );
}

#[tokio::test]
async fn both_names_follow_xdg_config_home() {
    let h = earlier_home("migrate-xdg");
    let o = home::ham_env(
        &h,
        &["config", "migrate"],
        &[("XDG_CONFIG_HOME", h.config.to_str().unwrap())],
    )
    .await;
    assert_eq!(o.status.code(), Some(0), "{}", home::text(&o.stderr));
    assert!(h.config.join("ham/config.toml").is_file());
}

#[tokio::test]
async fn the_json_form_names_the_folders_and_the_files() {
    let h = earlier_home("migrate-json");
    let o = home::ham(&h, &["config", "migrate", "--dry-run", "--json"]).await;
    let json: serde_json::Value = serde_json::from_str(home::text(&o.stdout).trim()).unwrap();
    assert_eq!(json["dryRun"], true);
    assert_eq!(json["from"], h.config.join("cah").display().to_string());
    assert_eq!(json["to"], h.config.join("ham").display().to_string());
    let files: Vec<&str> = json["copy"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        files.contains(&"config.toml") && files.contains(&"skills/one/SKILL.md"),
        "{files:?}"
    );
}

#[tokio::test]
async fn a_plain_ham_never_migrates_by_itself() {
    let h = earlier_home("migrate-never");
    for args in [
        vec!["config", "get", "api_endpoint"],
        vec!["config", "show"],
        vec!["rules", "show"],
    ] {
        let _ = home::ham(&h, &args).await;
    }
    assert!(!h.config.join("ham").exists());
}

#[tokio::test]
async fn a_new_folder_that_holds_only_the_setup_files_is_filled_beside_them() {
    // `setup.sh` keeps its receipts in `config/ham/setup/` before the user has any file there.
    let h = earlier_home("migrate-beside-setup");
    write(&h.config.join("ham/setup/acct/region/receipt.json"), "{}\n");
    let o = home::ham(&h, &["config", "migrate"]).await;
    assert_eq!(o.status.code(), Some(0), "{}", home::text(&o.stderr));
    assert!(h.config.join("ham/config.toml").is_file());
    assert!(h.config.join("ham/rules/00-my-rules.md").is_file());
    assert_eq!(
        std::fs::read_to_string(h.config.join("ham/setup/acct/region/receipt.json")).unwrap(),
        "{}\n"
    );
    assert!(!h.config.join("ham/.ham-migrating").exists());
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    assert_eq!(home::text(&o.stdout).trim_end(), ENDPOINT);
    assert_eq!(
        home::text(&o.stderr),
        "",
        "the new folder is the one read now"
    );
}

#[tokio::test]
async fn a_copy_that_stopped_half_way_is_not_read_and_the_next_run_finishes_it() {
    let h = earlier_home("migrate-unfinished");
    // What a stopped copy leaves: the marker and the first file, in a folder with the setup files.
    write(&h.config.join("ham/setup/receipt.json"), "{}\n");
    std::fs::copy(
        h.config.join("cah/config.toml"),
        h.config.join("ham/config.toml"),
    )
    .unwrap();
    write(&h.config.join("ham/.ham-migrating"), "unfinished\n");
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    assert_eq!(
        home::text(&o.stdout).trim_end(),
        ENDPOINT,
        "the earlier files are the ones read"
    );
    assert!(home::text(&o.stderr).contains("ham config migrate"));
    let o = home::ham(&h, &["config", "migrate"]).await;
    let (out, err) = (home::text(&o.stdout), home::text(&o.stderr));
    assert_eq!(o.status.code(), Some(0), "{err}");
    assert!(
        out.contains("already"),
        "the file that was placed is recognised: {out}"
    );
    assert!(!h.config.join("ham/.ham-migrating").exists());
    assert!(h.config.join("ham/skills/one/SKILL.md").is_file());
    let o = home::ham(&h, &["config", "get", "api_endpoint"]).await;
    assert_eq!(home::text(&o.stdout).trim_end(), ENDPOINT);
    assert_eq!(
        home::text(&o.stderr),
        "",
        "the new folder is the one read now"
    );
}
