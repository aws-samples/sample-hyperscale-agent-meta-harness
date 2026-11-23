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

/// The counter in the names of this process's test folders.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// The name of a test folder: the process id and a counter. The id is unique among the processes that run now, not over time: the kernel
/// hands an id out again and nextest starts one process for each test, so a later run can ask for a name that an earlier run used.
fn folder_name(n: u64) -> String {
    format!("ham-skills-ns-{}-{n}", std::process::id())
}

/// A new, empty folder below `base` for one test. `create_dir` refuses a folder that exists, so a folder that an earlier process with
/// this process id left behind (these tests remove nothing) is passed over and never handed to a test with its old content.
fn new_dir_in(base: &Path) -> PathBuf {
    loop {
        let dir = base.join(folder_name(NEXT.fetch_add(1, Ordering::SeqCst)));
        let made = std::fs::create_dir(&dir);
        if made
            .as_ref()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists)
        {
            continue;
        }
        made.unwrap();
        return dir;
    }
}

fn tmp() -> PathBuf {
    new_dir_in(&std::env::temp_dir())
}

/// The names of the folders in `dir` in the order the file system lists them: no sort, the order a reader gets that takes the listing as it
/// comes.
fn listed_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// The first name, in the order given, that the namespace reserves: it starts with the pack's prefix and is none of the pack's own names.
fn first_reserved(names: &[String]) -> String {
    names
        .iter()
        .find(|n| n.starts_with(pack::SKILL_PREFIX) && !pack::is_skill_name(n))
        .cloned()
        .unwrap()
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

#[test]
fn a_test_folder_is_new_even_when_an_earlier_process_with_this_id_left_one() {
    // What a reused process id finds: folders under the names this process takes next, as the default-folder test above leaves them
    // (`ham/skills/<skill>/SKILL.md` below a stand-in XDG_CONFIG_HOME). They are planted in a base of this test's own, so no other
    // test of this binary can be handed one; the other tests take at most 7 names while this one runs.
    let base = tmp();
    let next = NEXT.load(Ordering::SeqCst);
    for n in next..next + 16 {
        write(
            &base,
            &format!("{}/ham/skills/lifecycle-mine/SKILL.md", folder_name(n)),
            &entry("lifecycle-mine"),
        );
    }
    let dir = new_dir_in(&base);
    let left: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        left.is_empty(),
        "a test was handed a folder with an earlier run's content: {left:?}"
    );
    // The reserved-names case of this file in the folder a test is handed: the name is the reserved skill's, never a leftover's.
    write(&dir, "alpha/SKILL.md", &entry("alpha"));
    write(&dir, "lifecycle-extra/SKILL.md", &entry("lifecycle-extra"));
    write(&dir, "lifecycle-tdd/SKILL.md", &entry("lifecycle-tdd"));
    let e = read_skills_path(&dir).unwrap_err();
    assert_eq!(e.skill.as_deref(), Some("lifecycle-extra"), "{e}");
    // The base is this test's own and new: its planted folders do not stay behind.
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_name_in_the_message_does_not_depend_on_the_order_the_folders_were_made_in() {
    // Two reserved names in one set: the reader sorts the folder names, so the message names the same skill on every read, whatever
    // order the folders were made in and whatever order the disk lists them in.
    let orders = [
        ["alpha", "lifecycle-beta", "lifecycle-zeta", "omega"],
        ["omega", "lifecycle-zeta", "lifecycle-beta", "alpha"],
    ];
    for order in orders {
        let dir = tmp();
        for name in order {
            write(&dir, &format!("{name}/SKILL.md"), &entry(name));
        }
        for _ in 0..3 {
            let e = read_skills_path(&dir).unwrap_err();
            assert_eq!(e.skill.as_deref(), Some("lifecycle-beta"), "{order:?}: {e}");
        }
    }
    // The four names above cannot show the sort where the disk happens to list `lifecycle-beta` before `lifecycle-zeta`, whatever order
    // they were made in. So eight larger sets follow, made forwards and backwards in turn, and for each one the test reads the listing of
    // the disk itself. A set proves the sort only when the reserved name the disk lists first is another one than the reserved name that
    // sorts first: a reader that took the listing as it comes would name that other skill.
    let mut proving = 0;
    for set in 0..8 {
        let mut names: Vec<String> = (0..10).map(|n| format!("lifecycle-s{set}-n{n}")).collect();
        names.push(format!("alpha-{set}"));
        names.push(format!("omega-{set}"));
        if set % 2 == 1 {
            names.reverse();
        }
        let dir = tmp();
        for name in &names {
            write(&dir, &format!("{name}/SKILL.md"), &entry(name));
        }
        let listed = listed_names(&dir);
        let mut sorted = listed.clone();
        sorted.sort();
        let first_sorted = first_reserved(&sorted);
        assert_eq!(first_sorted, format!("lifecycle-s{set}-n0"));
        if first_reserved(&listed) != first_sorted {
            proving += 1;
        }
        for _ in 0..3 {
            let e = read_skills_path(&dir).unwrap_err();
            assert_eq!(
                e.skill.as_deref(),
                Some(first_sorted.as_str()),
                "the disk lists {listed:?}: {e}"
            );
        }
    }
    if proving == 0 {
        // Rust's test harness has no skipped state at run time, so the test says it, as the other tests of this workspace do.
        eprintln!(
            "SKIPPED the proof of the sort: the file system of {} listed the reserved name that sorts first in front in all 8 sets, so \
             no set here can tell a reader that sorts from one that takes the listing as it comes",
            std::env::temp_dir().display()
        );
    } else {
        eprintln!("the sort is proved by {proving} of 8 sets on this file system");
    }
}
