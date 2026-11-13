//! Reading the user's skills from disk: `~/.config/ham/skills/<name>/SKILL.md` (and the files beside it), or the folder
//! `--skills <path>` names. The disk checks that the shared validator cannot make (a symbolic link, a name that is not UTF-8, the
//! frontmatter of `SKILL.md`) live here; everything else is `harness_protocol::direct::check_skills`.
#![allow(clippy::unwrap_used)]

use ham_core::skills::{Why, describe, read_default, read_skills_path, skills_dir};
use harness_protocol::direct::{SkillProblem, check_skills};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn entry(name: &str, desc: &str) -> String {
    format!("---\nname: {name}\ndescription: {desc}\n---\nDo the {name} work.\n")
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A scratch folder under the temp dir (set by the project's env.sh); like the other `ham` tests it is not removed by the test.
struct Tmp(PathBuf);

impl Tmp {
    fn path(&self) -> &Path {
        &self.0
    }
}

fn tmp() -> Tmp {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ham-skills-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    Tmp(dir)
}

fn fail(root: &Path) -> ham_core::skills::SkillsError {
    read_skills_path(root).unwrap_err()
}

#[test]
fn a_folder_of_skills_is_read_in_name_order_and_passes_the_validator() {
    let t = tmp();
    write(t.path(), "zeta/SKILL.md", &entry("zeta", "Last."));
    write(t.path(), "alpha/SKILL.md", &entry("alpha", "First."));
    write(t.path(), "alpha/scripts/run.py", "print('ok')\n");
    write(t.path(), "alpha/references/notes.md", "notes\n");
    let skills = read_skills_path(t.path()).unwrap();
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["alpha", "zeta"]);
    let paths: Vec<&str> = skills[0].files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        ["SKILL.md", "references/notes.md", "scripts/run.py"],
        "SKILL.md first, then by path"
    );
    assert_eq!(skills[0].files[2].content, "print('ok')\n");
    assert_eq!(skills[0].files[2].size, 12);
    assert_eq!(check_skills(&skills), Ok(()));
}

#[test]
fn one_skill_folder_is_a_skill_named_by_its_folder() {
    let t = tmp();
    write(t.path(), "my-skill/SKILL.md", &entry("my-skill", "Mine."));
    let skills = read_skills_path(&t.path().join("my-skill")).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "my-skill");
}

#[test]
fn hidden_entries_are_skipped_and_nothing_else_is() {
    let t = tmp();
    write(t.path(), ".DS_Store", "x");
    write(t.path(), ".git/config", "x");
    write(t.path(), "alpha/SKILL.md", &entry("alpha", "A."));
    write(t.path(), "alpha/.DS_Store", "x");
    write(t.path(), "alpha/.swp", "x");
    let skills = read_skills_path(t.path()).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].files.len(), 1);
}

#[test]
fn an_empty_folder_is_no_skills_and_a_loose_file_is_an_error() {
    let t = tmp();
    assert_eq!(read_skills_path(t.path()).unwrap(), vec![]);
    write(t.path(), "README.md", "loose");
    let e = fail(t.path());
    assert!(e.to_string().contains("README.md"), "{e}");
}

#[cfg(unix)]
#[test]
fn a_link_inside_a_skill_is_refused_but_a_linked_skill_folder_is_followed() {
    let t = tmp();
    write(t.path(), "alpha/SKILL.md", &entry("alpha", "A."));
    let outside = tmp();
    write(outside.path(), "secret.txt", "do not send");
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        t.path().join("alpha/leak.txt"),
    )
    .unwrap();
    let e = fail(t.path());
    assert_eq!(e.skill.as_deref(), Some("alpha"));
    assert_eq!(e.file.as_deref(), Some("leak.txt"));
    assert_eq!(e.why, Why::Invalid(SkillProblem::NotRegular));
    assert!(!e.to_string().contains("do not send"));
    // A dotfile manager links whole skill folders and the skills folder itself; those are followed.
    let t2 = tmp();
    write(outside.path(), "beta/SKILL.md", &entry("beta", "B."));
    std::os::unix::fs::symlink(outside.path().join("beta"), t2.path().join("beta")).unwrap();
    let skills = read_skills_path(t2.path()).unwrap();
    assert_eq!(skills[0].name, "beta");
    let t3 = tmp();
    let link = t3.path().join("whole");
    std::os::unix::fs::symlink(t2.path(), &link).unwrap();
    assert_eq!(read_skills_path(&link).unwrap().len(), 1);
}

#[test]
fn the_frontmatter_must_name_the_folder_and_describe_the_skill() {
    for (text, want) in [
        ("no frontmatter\n".to_string(), "frontmatter"),
        ("---\nname: alpha\n".to_string(), "ends it is missing"),
        (
            "---\nname: other\ndescription: x\n---\nbody\n".to_string(),
            "frontmatter name",
        ),
        ("---\ndescription: x\n---\nbody\n".to_string(), "name"),
        ("---\nname: alpha\n---\nbody\n".to_string(), "description"),
        (
            "---\nname: alpha\ndescription: \"\"\n---\nbody\n".to_string(),
            "description",
        ),
        (
            format!(
                "---\nname: alpha\ndescription: {}\n---\nbody\n",
                "d".repeat(1025)
            ),
            "1024",
        ),
        ("---\nname: [alpha\n---\nbody\n".to_string(), "YAML"),
        ("---\n- a\n- b\n---\nbody\n".to_string(), "frontmatter name"),
    ] {
        let t = tmp();
        write(t.path(), "alpha/SKILL.md", &text);
        let e = fail(t.path());
        assert_eq!(e.skill.as_deref(), Some("alpha"), "{text:?}");
        assert_eq!(e.file.as_deref(), Some("SKILL.md"), "{text:?}");
        assert!(e.to_string().contains(want), "{text:?}: {e}");
    }
}

#[test]
fn other_frontmatter_keys_are_the_engines_business() {
    let t = tmp();
    write(
        t.path(),
        "alpha/SKILL.md",
        "---\nname: alpha\ndescription: A.\nlicense: MIT\nmetadata:\n  owner: me\n---\nbody\n",
    );
    assert_eq!(read_skills_path(t.path()).unwrap().len(), 1);
}

#[test]
fn the_limits_are_reported_with_the_skill_and_the_file() {
    // A folder name that is no skill name.
    let t = tmp();
    write(t.path(), "Bad Name/SKILL.md", &entry("Bad Name", "x"));
    assert_eq!(fail(t.path()).why, Why::Invalid(SkillProblem::BadName));
    // A file over the old 64 KiB, and thirteen files in one skill, are read.
    let t = tmp();
    write(t.path(), "alpha/SKILL.md", &entry("alpha", "A."));
    write(t.path(), "alpha/big.md", &"x".repeat(65 * 1024));
    for i in 0..12 {
        write(t.path(), &format!("alpha/references/r{i}.md"), "x");
    }
    assert_eq!(read_skills_path(t.path()).unwrap()[0].files.len(), 14);
    // Thirty-three skills.
    let t = tmp();
    for i in 0..33 {
        write(
            t.path(),
            &format!("s{i}/SKILL.md"),
            &entry(&format!("s{i}"), "x"),
        );
    }
    assert!(matches!(
        fail(t.path()).why,
        Why::Invalid(SkillProblem::TooManySkills { count: 33, max: 32 })
    ));
}

#[test]
fn text_that_is_not_utf8_or_holds_a_nul_is_refused() {
    let t = tmp();
    write(t.path(), "alpha/SKILL.md", &entry("alpha", "A."));
    std::fs::write(t.path().join("alpha/blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
    let e = fail(t.path());
    assert_eq!((e.file.as_deref(), e.why), (Some("blob.bin"), Why::NotUtf8));
    let t = tmp();
    write(t.path(), "alpha/SKILL.md", &entry("alpha", "A."));
    write(t.path(), "alpha/x.sh", "echo a\0b\n");
    assert_eq!(fail(t.path()).why, Why::Invalid(SkillProblem::Nul));
}

#[test]
fn a_missing_folder_is_an_error_but_the_default_folder_may_be_absent() {
    let t = tmp();
    let e = read_skills_path(&t.path().join("nope")).unwrap_err();
    assert!(matches!(e.why, Why::Unreadable(_)), "{e}");
    let env = |k: &str| (k == "XDG_CONFIG_HOME").then(|| t.path().to_string_lossy().into_owned());
    assert_eq!(skills_dir(&env), Some(t.path().join("ham").join("skills")));
    assert_eq!(read_default(&env).unwrap(), vec![]);
    write(t.path(), "ham/skills/alpha/SKILL.md", &entry("alpha", "A."));
    assert_eq!(read_default(&env).unwrap().len(), 1);
    let none = |_: &str| None;
    assert_eq!(skills_dir(&none), None);
    assert_eq!(read_default(&none).unwrap(), vec![]);
}

#[test]
fn the_summary_lines_name_each_skill_with_its_digest() {
    let t = tmp();
    write(t.path(), "alpha/SKILL.md", &entry("alpha", "A."));
    write(t.path(), "alpha/scripts/run.py", "print(1)\n");
    let skills = read_skills_path(t.path()).unwrap();
    let lines = describe(&skills);
    assert_eq!(lines.len(), 1);
    let digest = harness_protocol::direct::skill_sha256(&skills[0]);
    assert!(lines[0].starts_with("alpha "), "{lines:?}");
    assert!(
        lines[0].contains(&digest[..12]) && lines[0].contains("2 files"),
        "{lines:?}"
    );
    assert!(!lines[0].contains(&digest), "the line shows 12 hex digits");
}
