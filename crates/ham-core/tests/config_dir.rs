// COMPAT-CAH: this file names the folder of the earlier command name on purpose: the folder rule is a convenience of the
// window of the earlier names and ends at VERSION 5.50.
//! Where the files of the client are while the folder of the earlier command name may still hold them: `~/.config/ham` is used only when it
//! holds user content (`config.toml`, `engines/`, `rules/`, `skills/`, `continue.md`, `review.md`; an empty folder or an empty subfolder does
//! not count), otherwise `~/.config/cah` is read and never written. `ham config migrate` copies the files through a temporary sibling folder.
#![allow(clippy::unwrap_used)]

use ham_core::config::config_path;
use ham_core::config_dir::{
    Dirs, MIGRATING, Refusal, State, USER_ENTRIES, apply_migrate, dirs, holds_user_content,
    plan_migrate, write_refusal,
};
use std::collections::HashMap;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn scratch(label: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "ham-core-config-dir-{label}-{}-{nanos}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn env_of(pairs: Vec<(&str, String)>) -> impl Fn(&str) -> Option<String> + use<> {
    let map: HashMap<String, String> = pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    move |k| map.get(k).cloned()
}

/// A home with `.config/`; the closure gives `HOME` only.
fn home(label: &str) -> (PathBuf, impl Fn(&str) -> Option<String> + use<>) {
    let h = scratch(label);
    std::fs::create_dir_all(h.join(".config")).unwrap();
    let env = env_of(vec![("HOME", h.display().to_string())]);
    (h, env)
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn in_use(d: &Dirs) -> PathBuf {
    d.in_use().to_path_buf()
}

#[test]
fn with_no_folder_the_current_name_is_where_files_will_be_made() {
    let (h, env) = home("none");
    let d = dirs(&env).unwrap();
    assert_eq!(d.state, State::Fresh);
    assert_eq!(in_use(&d), h.join(".config/ham"));
    assert_eq!(d.earlier, h.join(".config/cah"));
    assert!(!d.earlier_unread);
    assert_eq!(config_path(&env), Some(h.join(".config/ham/config.toml")));
}

#[test]
fn no_home_and_no_xdg_is_no_location() {
    assert!(dirs(&env_of(vec![])).is_none());
    assert!(dirs(&env_of(vec![("HOME", String::new())])).is_none());
}

#[test]
fn both_names_follow_xdg_config_home() {
    let x = scratch("xdg");
    let env = env_of(vec![
        ("XDG_CONFIG_HOME", x.display().to_string()),
        ("HOME", "/nowhere".into()),
    ]);
    let d = dirs(&env).unwrap();
    assert_eq!(d.current, x.join("ham"));
    assert_eq!(d.earlier, x.join("cah"));
    write(
        &x.join("cah/config.toml"),
        "apiEndpoint = \"https://x/direct\"\n",
    );
    let d = dirs(&env).unwrap();
    assert_eq!(d.state, State::EarlierOnly);
    assert_eq!(config_path(&env), Some(x.join("cah/config.toml")));
}

#[test]
fn only_the_current_folder_with_content_is_the_current_folder() {
    let (h, env) = home("current");
    write(&h.join(".config/ham/config.toml"), "x = 1\n");
    let d = dirs(&env).unwrap();
    assert_eq!(d.state, State::Current);
    assert!(!d.earlier_unread);
    assert_eq!(in_use(&d), h.join(".config/ham"));
}

#[test]
fn only_the_earlier_folder_with_content_is_read_in_its_place() {
    let (h, env) = home("earlier");
    write(&h.join(".config/cah/config.toml"), "x = 1\n");
    let d = dirs(&env).unwrap();
    assert_eq!(d.state, State::EarlierOnly);
    assert_eq!(in_use(&d), h.join(".config/cah"));
    assert_eq!(config_path(&env), Some(h.join(".config/cah/config.toml")));
}

#[test]
fn an_empty_current_folder_does_not_hide_the_earlier_one() {
    let (h, env) = home("empty-current");
    write(&h.join(".config/cah/config.toml"), "x = 1\n");
    std::fs::create_dir_all(h.join(".config/ham")).unwrap();
    assert_eq!(dirs(&env).unwrap().state, State::EarlierOnly);
}

#[test]
fn empty_subfolders_and_other_files_do_not_count_as_content() {
    let (h, env) = home("empty-subs");
    write(&h.join(".config/cah/config.toml"), "x = 1\n");
    for sub in ["rules", "skills", "engines"] {
        std::fs::create_dir_all(h.join(".config/ham").join(sub)).unwrap();
    }
    write(&h.join(".config/ham/setup/receipt.json"), "{}\n");
    write(&h.join(".config/ham/notes.txt"), "mine\n");
    assert!(!holds_user_content(&h.join(".config/ham")));
    assert_eq!(dirs(&env).unwrap().state, State::EarlierOnly);
}

#[test]
fn each_of_the_six_entries_counts_as_content() {
    assert_eq!(
        USER_ENTRIES,
        [
            "config.toml",
            "engines",
            "rules",
            "skills",
            "continue.md",
            "review.md"
        ]
    );
    for entry in USER_ENTRIES {
        let dir = scratch("entry");
        let path = dir.join(entry);
        if matches!(entry, "engines" | "rules" | "skills") {
            write(&path.join("x.md"), "text\n");
        } else {
            write(&path, "text\n");
        }
        assert!(holds_user_content(&dir), "{entry}");
    }
    assert!(!holds_user_content(&scratch("nothing")));
    assert!(!holds_user_content(&scratch("nothing").join("missing")));
}

#[test]
fn when_both_folders_hold_content_the_current_one_wins_and_the_earlier_one_is_not_read() {
    let (h, env) = home("both");
    write(&h.join(".config/ham/continue.md"), "go on\n");
    write(&h.join(".config/cah/config.toml"), "x = 1\n");
    let d = dirs(&env).unwrap();
    assert_eq!(d.state, State::Current);
    assert!(d.earlier_unread);
    assert_eq!(in_use(&d), h.join(".config/ham"));
}

#[test]
fn a_write_is_refused_only_while_the_earlier_folder_is_what_is_read() {
    let (h, env) = home("refusal");
    assert_eq!(write_refusal(&env), None);
    write(&h.join(".config/cah/config.toml"), "x = 1\n");
    let text = write_refusal(&env).unwrap();
    assert!(text.contains("ham config migrate"), "{text}");
    assert!(text.contains(".config/cah"), "{text}");
    assert!(text.contains(".config/ham"), "{text}");
    write(&h.join(".config/ham/config.toml"), "x = 1\n");
    assert_eq!(write_refusal(&env), None);
}

fn filled_earlier(label: &str) -> (PathBuf, impl Fn(&str) -> Option<String> + use<>) {
    let (h, env) = home(label);
    let old = h.join(".config/cah");
    write(
        &old.join("config.toml"),
        "apiEndpoint = \"https://x/direct\"\n",
    );
    write(&old.join("engines/cline.toml"), "schema = 1\n");
    write(&old.join("rules/00-my-rules.md"), "be brief\n");
    write(&old.join("rules/10-lifecycle.md"), "flow\n");
    write(&old.join("skills/lifecycle-check/SKILL.md"), "skill\n");
    write(&old.join("continue.md"), "go on\n");
    write(&old.join("review.md"), "review\n");
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
    (h, env)
}

fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for p in names {
            let rel = p.strip_prefix(base).unwrap().display().to_string();
            if p.is_dir() {
                out.push(format!("{rel}/"));
                walk(base, &p, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn migrate_copies_the_six_entries_and_never_the_setup_folder() {
    let (h, env) = filled_earlier("copy");
    let plan = plan_migrate(&env).unwrap();
    assert_eq!(plan.from, h.join(".config/cah"));
    assert_eq!(plan.to, h.join(".config/ham"));
    assert!(plan.whole_folder);
    apply_migrate(&plan).unwrap();
    assert_eq!(
        tree(&h.join(".config/ham")),
        [
            "config.toml",
            "continue.md",
            "engines/",
            "engines/cline.toml",
            "review.md",
            "rules/",
            "rules/00-my-rules.md",
            "rules/10-lifecycle.md",
            "skills/",
            "skills/lifecycle-check/",
            "skills/lifecycle-check/SKILL.md",
        ]
    );
    for rel in [
        "config.toml",
        "engines/cline.toml",
        "rules/00-my-rules.md",
        "skills/lifecycle-check/SKILL.md",
        "continue.md",
        "review.md",
    ] {
        assert_eq!(
            std::fs::read(h.join(".config/cah").join(rel)).unwrap(),
            std::fs::read(h.join(".config/ham").join(rel)).unwrap(),
            "{rel} is byte-equal"
        );
    }
}

#[test]
fn migrate_keeps_the_mode_of_each_file_and_leaves_the_old_folder_as_it_was() {
    let (h, env) = filled_earlier("mode");
    let before = tree(&h.join(".config/cah"));
    let old_mtime = std::fs::metadata(h.join(".config/cah/config.toml"))
        .unwrap()
        .mtime();
    apply_migrate(&plan_migrate(&env).unwrap()).unwrap();
    let mode = |p: &str| {
        std::fs::metadata(h.join(".config/ham").join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("config.toml"), 0o600);
    assert_eq!(mode("continue.md"), 0o644);
    assert_eq!(tree(&h.join(".config/cah")), before);
    assert_eq!(
        std::fs::metadata(h.join(".config/cah/config.toml"))
            .unwrap()
            .mtime(),
        old_mtime
    );
}

#[test]
fn a_second_migrate_has_nothing_to_copy_and_changes_nothing() {
    let (h, env) = filled_earlier("twice");
    apply_migrate(&plan_migrate(&env).unwrap()).unwrap();
    let plan = plan_migrate(&env).unwrap();
    assert!(!plan.whole_folder);
    assert!(plan.copy.is_empty(), "{:?}", plan.copy);
    assert_eq!(plan.same.len(), 7, "{:?}", plan.same);
    let before = tree(&h.join(".config/ham"));
    apply_migrate(&plan).unwrap();
    assert_eq!(tree(&h.join(".config/ham")), before);
}

#[test]
fn a_file_in_the_new_folder_that_differs_is_never_overwritten() {
    let (h, env) = filled_earlier("differs");
    write(
        &h.join(".config/ham/config.toml"),
        "apiEndpoint = \"https://other/direct\"\n",
    );
    let refusal = plan_migrate(&env).unwrap_err();
    match refusal {
        Refusal::Differs(names) => assert_eq!(names, ["config.toml"]),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(h.join(".config/ham/config.toml")).unwrap(),
        "apiEndpoint = \"https://other/direct\"\n"
    );
}

#[test]
fn files_that_are_missing_in_a_new_folder_with_content_are_added() {
    let (h, env) = filled_earlier("fill");
    let same = std::fs::read(h.join(".config/cah/config.toml")).unwrap();
    write(
        &h.join(".config/ham/config.toml"),
        std::str::from_utf8(&same).unwrap(),
    );
    let plan = plan_migrate(&env).unwrap();
    assert!(!plan.whole_folder);
    assert!(plan.same.iter().any(|s| s == "config.toml"));
    apply_migrate(&plan).unwrap();
    assert!(h.join(".config/ham/rules/00-my-rules.md").is_file());
    assert_eq!(
        std::fs::read(h.join(".config/ham/config.toml")).unwrap(),
        same
    );
}

#[test]
fn a_symbolic_link_in_the_old_folder_is_refused_and_named() {
    let (h, env) = filled_earlier("link");
    symlink("/etc/hostname", h.join(".config/cah/rules/linked.md")).unwrap();
    match plan_migrate(&env).unwrap_err() {
        Refusal::Symlink(names) => assert_eq!(names, ["rules/linked.md"]),
        other => panic!("{other:?}"),
    }
    assert!(!h.join(".config/ham").exists());
}

#[test]
fn a_linked_earlier_folder_is_refused() {
    let (h, env) = home("linked-folder");
    let real = scratch("real-cah");
    write(&real.join("config.toml"), "x = 1\n");
    symlink(&real, h.join(".config/cah")).unwrap();
    assert!(matches!(
        plan_migrate(&env).unwrap_err(),
        Refusal::Symlink(_)
    ));
}

#[test]
fn nothing_to_migrate_without_an_earlier_folder_with_content() {
    let (_h, env) = home("nothing");
    assert!(matches!(
        plan_migrate(&env).unwrap_err(),
        Refusal::NothingToMigrate { .. }
    ));
    let (h, env) = home("nothing-empty");
    std::fs::create_dir_all(h.join(".config/cah/setup")).unwrap();
    assert!(matches!(
        plan_migrate(&env).unwrap_err(),
        Refusal::NothingToMigrate { .. }
    ));
    assert!(matches!(
        plan_migrate(&env_of(vec![])).unwrap_err(),
        Refusal::NoLocation
    ));
}

#[test]
fn an_empty_new_folder_is_filled_in_place_by_the_rename() {
    let (h, env) = filled_earlier("empty-new");
    std::fs::create_dir_all(h.join(".config/ham/rules")).unwrap();
    let plan = plan_migrate(&env).unwrap();
    assert!(plan.whole_folder);
    apply_migrate(&plan).unwrap();
    assert!(h.join(".config/ham/rules/00-my-rules.md").is_file());
}

#[test]
fn a_copy_that_fails_leaves_no_new_folder_and_no_temporary_folder() {
    let (h, env) = filled_earlier("fail");
    let blocked = h.join(".config/cah/rules/10-lifecycle.md");
    // The file is readable when the plan is made and not when the copy reaches it: an error of the copy, not of the plan.
    let plan = plan_migrate(&env).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = apply_migrate(&plan);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(result.is_err(), "reading a file with no permission fails");
    assert!(!h.join(".config/ham").exists(), "no partial new folder");
    let names = tree(&h.join(".config"));
    assert!(
        !names.iter().any(|n| n.contains("migrate")),
        "no temporary sibling is left: {names:?}"
    );
}

#[test]
fn a_new_folder_that_holds_only_setup_files_is_filled_beside_them() {
    // `setup.sh` keeps its receipts in `~/.config/ham/setup/` before anything of the user's is there: the folder exists, holds no user content,
    // and the finished copy cannot take its place by one rename.
    let (h, env) = filled_earlier("beside-setup");
    write(
        &h.join(".config/ham/setup/acct/region/receipt.json"),
        "{\"ok\":true}\n",
    );
    assert_eq!(dirs(&env).unwrap().state, State::EarlierOnly);
    let plan = plan_migrate(&env).unwrap();
    assert!(plan.whole_folder);
    apply_migrate(&plan).unwrap();
    assert_eq!(
        std::fs::read_to_string(h.join(".config/ham/setup/acct/region/receipt.json")).unwrap(),
        "{\"ok\":true}\n"
    );
    assert_eq!(
        std::fs::read(h.join(".config/ham/config.toml")).unwrap(),
        std::fs::read(h.join(".config/cah/config.toml")).unwrap()
    );
    assert!(h.join(".config/ham/rules/00-my-rules.md").is_file());
    assert_eq!(dirs(&env).unwrap().state, State::Current);
    let names = tree(&h.join(".config"));
    assert!(
        !names.iter().any(|n| n.contains("migrate")),
        "no temporary folder is left: {names:?}"
    );
    // The earlier folder is as it was, `setup/` of the earlier folder included.
    assert!(
        h.join(".config/cah/setup/acct/region/receipt.json")
            .is_file()
    );
}

// ---- review findings: the copy into a folder that exists, races, links and silent skips ---------

/// The state after a copy that failed half way: `ham` holds some of the files and the marker, and the earlier folder is still the one read.
#[test]
fn a_copy_that_fails_after_some_entries_were_placed_never_shadows_the_earlier_folder() {
    let (h, env) = filled_earlier("unfinished");
    write(
        &h.join(".config/ham/setup/acct/region/receipt.json"),
        "{}\n",
    );
    let blocked = h.join(".config/cah/skills/lifecycle-check/SKILL.md");
    let plan = plan_migrate(&env).unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let failed = apply_migrate(&plan);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(failed, Err(Refusal::Io(_))), "{failed:?}");
    assert!(
        h.join(".config/ham").join(MIGRATING).is_file(),
        "the marker says the copy is unfinished"
    );
    assert!(
        h.join(".config/ham/config.toml").is_file(),
        "some entries are in place"
    );
    let d = dirs(&env).unwrap();
    assert_eq!(
        d.state,
        State::EarlierOnly,
        "the unfinished folder does not win"
    );
    assert_eq!(in_use(&d), h.join(".config/cah"));
    assert!(write_refusal(&env).is_some());
    // The next run finishes it: what is in place is the same, the rest is added, the marker goes, and then `ham` is the one read.
    let plan = plan_migrate(&env).unwrap();
    assert!(!plan.whole_folder);
    assert!(
        plan.same.iter().any(|s| s == "config.toml"),
        "{:?}",
        plan.same
    );
    apply_migrate(&plan).unwrap();
    assert!(!h.join(".config/ham").join(MIGRATING).exists());
    assert_eq!(dirs(&env).unwrap().state, State::Current);
    assert_eq!(
        std::fs::read(h.join(".config/ham/skills/lifecycle-check/SKILL.md")).unwrap(),
        std::fs::read(blocked).unwrap()
    );
    assert!(
        h.join(".config/ham/setup/acct/region/receipt.json")
            .is_file()
    );
}

#[test]
fn a_marker_in_a_folder_whose_source_is_gone_leaves_the_folder_in_use() {
    // The earlier folder was removed after a half copy: nothing is left to prefer, so what is in `ham` is used as it is.
    let (h, env) = home("marker-alone");
    write(&h.join(".config/ham/config.toml"), "x = 1\n");
    write(&h.join(".config/ham").join(MIGRATING), "unfinished\n");
    assert_eq!(dirs(&env).unwrap().state, State::Current);
}

#[test]
fn a_file_that_appears_in_the_new_folder_after_the_plan_is_never_overwritten() {
    let (h, env) = filled_earlier("race");
    let same = std::fs::read_to_string(h.join(".config/cah/config.toml")).unwrap();
    write(&h.join(".config/ham/config.toml"), &same);
    let plan = plan_migrate(&env).unwrap();
    write(
        &h.join(".config/ham/rules/00-my-rules.md"),
        "written meanwhile\n",
    );
    match apply_migrate(&plan) {
        Err(Refusal::Differs(names)) => assert_eq!(names, ["rules/00-my-rules.md"]),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(h.join(".config/ham/rules/00-my-rules.md")).unwrap(),
        "written meanwhile\n"
    );
}

#[test]
fn a_file_that_appears_in_the_new_folder_after_a_whole_folder_plan_is_never_overwritten() {
    let (h, env) = filled_earlier("race-whole");
    write(&h.join(".config/ham/setup/receipt.json"), "{}\n");
    let plan = plan_migrate(&env).unwrap();
    assert!(plan.whole_folder);
    write(&h.join(".config/ham/config.toml"), "written meanwhile\n");
    assert!(matches!(apply_migrate(&plan), Err(Refusal::Differs(_))));
    assert_eq!(
        std::fs::read_to_string(h.join(".config/ham/config.toml")).unwrap(),
        "written meanwhile\n"
    );
}

#[test]
fn a_new_folder_that_is_a_link_is_refused() {
    let (h, env) = filled_earlier("ham-link");
    let real = scratch("real-ham");
    symlink(&real, h.join(".config/ham")).unwrap();
    match plan_migrate(&env).unwrap_err() {
        Refusal::Symlink(names) => assert!(names[0].ends_with(".config/ham"), "{names:?}"),
        other => panic!("{other:?}"),
    }
    assert!(
        tree(&real).is_empty(),
        "nothing was written through the link"
    );
}

#[test]
fn an_entry_of_the_new_folder_that_is_a_link_is_refused_by_name() {
    let (h, env) = filled_earlier("entry-link");
    let same = std::fs::read_to_string(h.join(".config/cah/config.toml")).unwrap();
    write(&h.join(".config/ham/config.toml"), &same);
    let real = scratch("real-rules");
    symlink(&real, h.join(".config/ham/rules")).unwrap();
    match plan_migrate(&env).unwrap_err() {
        Refusal::Symlink(names) => assert_eq!(names, ["rules"]),
        other => panic!("{other:?}"),
    }
    assert!(tree(&real).is_empty());
}

#[cfg(target_os = "linux")]
#[test]
fn a_file_name_that_is_not_text_is_refused_and_never_skipped() {
    use std::os::unix::ffi::OsStrExt;
    let (h, env) = filled_earlier("odd-name");
    let odd = std::ffi::OsStr::from_bytes(b"rules/odd-\xff-name.md");
    std::fs::write(h.join(".config/cah").join(odd), "x\n").unwrap();
    match plan_migrate(&env).unwrap_err() {
        Refusal::NotRegular(names) => assert!(names[0].contains("odd-"), "{names:?}"),
        other => panic!("{other:?}"),
    }
    assert!(!h.join(".config/ham").exists());
}

#[test]
fn a_finder_file_alone_does_not_make_a_folder_hold_content() {
    let (h, env) = filled_earlier("ds-store");
    write(&h.join(".config/ham/rules/.DS_Store"), "x");
    assert!(!holds_user_content(&h.join(".config/ham")));
    assert_eq!(dirs(&env).unwrap().state, State::EarlierOnly);
}

#[test]
fn a_folder_that_cannot_be_read_counts_as_content_and_is_not_taken_for_empty() {
    let dir = scratch("unreadable");
    write(&dir.join("engines/cline.toml"), "schema = 1\n");
    std::fs::set_permissions(dir.join("engines"), std::fs::Permissions::from_mode(0o000)).unwrap();
    let held = holds_user_content(&dir);
    std::fs::set_permissions(dir.join("engines"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(held);
}

#[test]
fn the_plan_of_a_whole_copy_refuses_a_file_it_cannot_read_like_the_copy_would() {
    let (h, env) = filled_earlier("unreadable-source");
    let blocked = h.join(".config/cah/rules/10-lifecycle.md");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let planned = plan_migrate(&env);
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(planned, Err(Refusal::Io(_))), "{planned:?}");
}

#[test]
fn folders_made_in_an_existing_new_folder_get_the_mode_of_their_source() {
    // A source folder of mode 0700 never shows up in the new place with the default mode, whatever the umask.
    let (h, env) = filled_earlier("modes");
    std::fs::set_permissions(
        h.join(".config/cah/rules"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    write(&h.join(".config/ham/setup/receipt.json"), "{}\n");
    apply_migrate(&plan_migrate(&env).unwrap()).unwrap();
    let mode = std::fs::metadata(h.join(".config/ham/rules"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
}
