//! The namespace of the platform's skills. The lifecycle pack's seven skills are
//! `lifecycle-<step>`; the client reserves that prefix for them, so a skill of the user's own cannot take a name inside it and look like
//! one of the platform's. The pack's own seven names stay free to use, also with the user's edits (the files are theirs), and every other
//! name, including look-alikes of the prefix, is the user's.
//!
//! The guard is the client's: the shared validator in `harness_protocol` is part of the worker image, so the server-side half of the
//! reservation is not built here (the integration report says so). Everything below goes through the readers a run uses
//! (`read_skills_path` for `--skills <path>`, `read_default` for `~/.config/ham/skills`).
#![allow(clippy::unwrap_used)]

use ham_core::pack;
use ham_core::skills::{read_default, read_skills_path, skills_dir};
use harness_protocol::direct::skill_sha256;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn entry(name: &str) -> String {
    format!("---\nname: {name}\ndescription: Mine.\n---\nDo the {name} work.\n")
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn tmp() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ham-skills-ns-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The seven names the pack owns, from the pack itself.
fn pack_names() -> Vec<String> {
    pack::skills().into_iter().map(|s| s.name).collect()
}

#[test]
fn a_user_skill_inside_the_lifecycle_namespace_is_refused_and_the_message_says_what_to_do() {
    let dir = tmp();
    write(&dir, "lifecycle-notes/SKILL.md", &entry("lifecycle-notes"));
    write(
        &dir,
        "lifecycle-notes/notes.md",
        "private words of the user\n",
    );
    let e = read_skills_path(&dir).unwrap_err();
    assert_eq!(e.skill.as_deref(), Some("lifecycle-notes"), "{e}");
    let text = e.to_string();
    assert!(text.contains("lifecycle-notes"), "{text}");
    assert!(text.contains("reserved"), "{text}");
    assert!(
        text.contains("lifecycle-tdd") && text.contains("lifecycle-check"),
        "the message names the pack's skills so the person sees what the prefix is for: {text}"
    );
    assert!(
        text.contains("rename") || text.contains("another name"),
        "{text}"
    );
    assert!(
        !text.contains("private words"),
        "a message names skills and files, never their content: {text}"
    );
    println!("SKILLS-NAMESPACE TRIGGER PRESENT: lifecycle-notes refused: {text}");
}

#[test]
fn the_guard_applies_to_the_default_folder_and_to_a_single_skill_folder() {
    // ~/.config/ham/skills (XDG_CONFIG_HOME/ham/skills).
    let cfg = tmp();
    let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| cfg.display().to_string());
    let skills = skills_dir(&env).unwrap();
    write(&skills, "lifecycle-mine/SKILL.md", &entry("lifecycle-mine"));
    let e = read_default(&env).unwrap_err();
    assert!(e.to_string().contains("reserved"), "{e}");
    // `--skills <the folder of one skill>`.
    let one = tmp();
    write(&one, "lifecycle-solo/SKILL.md", &entry("lifecycle-solo"));
    let e = read_skills_path(&one.join("lifecycle-solo")).unwrap_err();
    assert_eq!(e.skill.as_deref(), Some("lifecycle-solo"), "{e}");
}

#[test]
fn the_packs_own_seven_names_stay_free_and_a_users_edit_of_one_is_still_the_users_to_make() {
    let names = pack_names();
    assert_eq!(names.len(), 7, "{names:?}");
    let dir = tmp();
    for name in &names {
        // The user's own edit: a different body, an extra file.
        write(&dir, &format!("{name}/SKILL.md"), &entry(name));
        write(&dir, &format!("{name}/extra.md"), "an addition\n");
    }
    let skills = read_skills_path(&dir).unwrap();
    let mut got: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
    got.sort();
    let mut want = names.clone();
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn the_installed_pack_reads_back_whole_and_unchanged() {
    // What `ham rules init --skills` writes is what the next run sends: the guard must not refuse the pack itself.
    let dir = tmp();
    let pack_skills = pack::skills();
    for s in &pack_skills {
        for f in &s.files {
            write(&dir, &format!("{}/{}", s.name, f.path), &f.content);
        }
    }
    let read = read_skills_path(&dir).unwrap();
    assert_eq!(read.len(), 7);
    for want in &pack_skills {
        let got = read.iter().find(|s| s.name == want.name).unwrap();
        assert_eq!(skill_sha256(got), skill_sha256(want), "{}", want.name);
    }
}

#[test]
fn names_that_only_look_like_the_prefix_are_the_users() {
    let dir = tmp();
    for name in [
        "lifecycle",
        "lifecycle2-x",
        "lifecyclex-a",
        "my-lifecycle-a",
        "life-cycle-a",
    ] {
        write(&dir, &format!("{name}/SKILL.md"), &entry(name));
    }
    let skills = read_skills_path(&dir).unwrap();
    assert_eq!(skills.len(), 5);
}

#[test]
fn one_reserved_name_among_good_ones_stops_the_whole_set() {
    let dir = tmp();
    write(&dir, "alpha/SKILL.md", &entry("alpha"));
    write(&dir, "lifecycle-extra/SKILL.md", &entry("lifecycle-extra"));
    write(&dir, "lifecycle-tdd/SKILL.md", &entry("lifecycle-tdd"));
    let e = read_skills_path(&dir).unwrap_err();
    assert_eq!(e.skill.as_deref(), Some("lifecycle-extra"), "{e}");
}
