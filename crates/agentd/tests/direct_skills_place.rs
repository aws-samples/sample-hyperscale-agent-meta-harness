//! The skills of a run on disk: each file
//! is the exact bytes the client sent, read-only, in read-only directories under the engine's configuration directory; nothing else is
//! there; an earlier attempt's tree is replaced and never followed; the engine's HOME holds no skills folder; the session starts in the git
//! top level. The wire rules (counts, names, paths, sizes) are `harness_protocol::direct::check_skills`, tested in `harness-protocol`.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::skills::cwd::top_level;
use agentd::direct::skills::home::clear_home_skills;
use agentd::direct::skills::place::{MARKER, place_skills, retire_placed, skill_document};
use agentd::direct::skills::{Settings, skill_refs, skills_event};
use agentd::engine::direct::user_skills_dir;
use harness_protocol::direct::{Skill, SkillFile, skill_sha256};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn file(path: &str, content: &str) -> SkillFile {
    SkillFile {
        path: path.into(),
        sha256: sha(content.as_bytes()),
        size: content.len() as u64,
        content: content.into(),
    }
}

fn doc(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: Use when asked for {name}.\n---\n{body}")
}

fn skill(name: &str, extra: &[(&str, &str)]) -> Skill {
    let mut files = vec![file("SKILL.md", &doc(name, "# Steps\n\n1. Do it.\n"))];
    files.extend(extra.iter().map(|(p, c)| file(p, c)));
    Skill {
        name: name.into(),
        files,
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

/// Gives a tree its write bits back so the scratch directory can be removed by the next run of the helper.
struct Unlock(std::path::PathBuf);

impl Drop for Unlock {
    fn drop(&mut self) {
        fn walk(p: &Path) {
            let Ok(m) = std::fs::symlink_metadata(p) else {
                return;
            };
            if m.is_dir() {
                let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
                if let Ok(rd) = std::fs::read_dir(p) {
                    rd.flatten().for_each(|e| walk(&e.path()));
                }
            }
        }
        walk(&self.0);
    }
}

// ---- placement ---------------------------------------------------------------------------------------------------------

#[test]
fn skills_are_written_byte_for_byte_read_only_in_read_only_directories() {
    let config = common::scratch("skills-place");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    assert_eq!(dir, config.join("skills"));
    let script = "print('ok')\r\nprint(\"caf\u{e9}\")\n\n";
    let a = skill("lifecycle-check", &[("scripts/lifecycle_check.py", script)]);
    let b = skill("lifecycle-loop", &[]);
    let placed = place_skills(&dir, &[a.clone(), b.clone()]).unwrap();

    assert_eq!(
        std::fs::read(dir.join("lifecycle-check/scripts/lifecycle_check.py")).unwrap(),
        script.as_bytes()
    );
    assert_eq!(
        std::fs::read(dir.join("lifecycle-check/SKILL.md")).unwrap(),
        a.files[0].content.as_bytes()
    );
    for f in [
        "lifecycle-check/SKILL.md",
        "lifecycle-check/scripts/lifecycle_check.py",
        "lifecycle-loop/SKILL.md",
    ] {
        assert_eq!(mode(&dir.join(f)), 0o444, "{f}");
    }
    for d in [
        "",
        "lifecycle-check",
        "lifecycle-check/scripts",
        "lifecycle-loop",
    ] {
        assert_eq!(mode(&dir.join(d)), 0o555, "directory {d:?}");
    }

    assert_eq!(placed.len(), 2);
    assert_eq!(placed[0].name, "lifecycle-check");
    assert_eq!(placed[0].dir, dir.join("lifecycle-check"));
    assert_eq!(placed[0].skill_md, dir.join("lifecycle-check/SKILL.md"));
    assert_eq!(placed[0].body, "# Steps\n\n1. Do it.");
    assert_eq!(placed[0].digest, skill_sha256(&a));
    assert_eq!(placed[0].files.len(), 2);
    assert!(
        placed[0]
            .files
            .iter()
            .any(|f| f.path == "scripts/lifecycle_check.py"
                && f.sha256 == sha(script.as_bytes())
                && f.bytes == script.len() as u64)
    );
    assert_eq!(placed[1].digest, skill_sha256(&b));
}

#[test]
fn nothing_but_the_requested_files_is_in_the_directory() {
    let config = common::scratch("skills-only");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    place_skills(
        &dir,
        &[
            skill("one", &[("references/r.md", "r\n")]),
            skill("two", &[]),
        ],
    )
    .unwrap();
    fn names(p: &Path, base: &Path, out: &mut Vec<String>) {
        let mut rd: Vec<_> = std::fs::read_dir(p).unwrap().flatten().collect();
        rd.sort_by_key(std::fs::DirEntry::file_name);
        for e in rd {
            out.push(e.path().strip_prefix(base).unwrap().display().to_string());
            if e.file_type().unwrap().is_dir() {
                names(&e.path(), base, out);
            }
        }
    }
    let mut seen = Vec::new();
    names(&dir, &dir, &mut seen);
    // Nothing but the requested files, and the marker file of each placed skill (the mark that the folder is agentd's).
    assert_eq!(
        seen,
        [
            "one",
            "one/.agentd-placed",
            "one/SKILL.md",
            "one/references",
            "one/references/r.md",
            "two",
            "two/.agentd-placed",
            "two/SKILL.md"
        ]
    );
}

#[test]
fn with_no_skills_nothing_is_created() {
    let config = common::scratch("skills-none");
    let dir = user_skills_dir(&config);
    assert!(place_skills(&dir, &[]).unwrap().is_empty());
    assert!(!dir.exists());
}

#[test]
fn placing_again_replaces_the_read_only_tree_an_earlier_attempt_left() {
    let config = common::scratch("skills-again");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    place_skills(
        &dir,
        &[
            skill("keep", &[("scripts/old.py", "old\n")]),
            skill("gone", &[]),
        ],
    )
    .unwrap();
    place_skills(&dir, &[skill("keep", &[("scripts/new.py", "new\n")])]).unwrap();
    assert!(dir.join("keep/scripts/new.py").is_file());
    assert!(!dir.join("keep/scripts/old.py").exists());
    assert!(!dir.join("gone").exists());
    assert_eq!(mode(&dir), 0o555);
    assert_eq!(mode(&dir.join("keep/scripts")), 0o555);
}

#[test]
fn a_symbolic_link_where_the_skills_directory_belongs_is_refused_and_not_followed() {
    let config = common::scratch("skills-link");
    let outside = config.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "keep\n").unwrap();
    std::os::unix::fs::symlink(&outside, config.join("skills")).unwrap();
    let e = place_skills(&config.join("skills"), &[skill("s", &[])]).unwrap_err();
    assert!(e.contains("symbolic link"), "{e}");
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert!(!outside.join("s").exists());
}

#[test]
fn a_skill_name_that_is_not_a_plain_part_is_not_written() {
    let config = common::scratch("skills-name");
    let dir = user_skills_dir(&config);
    let mut bad = skill("x", &[]);
    bad.name = "../escape".into();
    let e = place_skills(&dir, &[bad]).unwrap_err();
    assert!(e.contains("../escape") && e.contains("name"), "{e}");
    assert!(!config.join("escape").exists());
}

#[test]
fn a_path_that_is_not_a_plain_relative_path_is_not_written() {
    let config = common::scratch("skills-path");
    let dir = user_skills_dir(&config);
    let bad = skill("x", &[("a/../b.md", "b\n")]);
    let e = place_skills(&dir, &[bad]).unwrap_err();
    assert!(e.contains("a/../b.md") && e.contains("file path"), "{e}");
    assert!(!dir.exists() || std::fs::read_dir(&dir).unwrap().next().is_none());
}

#[test]
fn a_file_whose_bytes_are_not_the_declared_hash_or_size_is_not_written() {
    let config = common::scratch("skills-hash");
    let dir = user_skills_dir(&config);
    let mut s = skill("h", &[("scripts/a.py", "print(1)\n")]);
    s.files[1].content = "print(2)\n".into();
    let e = place_skills(&dir, &[s]).unwrap_err();
    assert!(e.contains("scripts/a.py") && e.contains("sha256"), "{e}");
    let mut s = skill("h2", &[("scripts/a.py", "print(1)\n")]);
    s.files[1].size += 1;
    let e = place_skills(&dir, &[s]).unwrap_err();
    assert!(e.contains("scripts/a.py") && e.contains("size"), "{e}");
}

// ---- the SKILL.md the engine reads --------------------------------------------------------------------------------------------

#[test]
fn the_document_is_frontmatter_with_the_folder_name_and_a_description_then_a_body() {
    let d = skill_document(&doc("tdd", "# Body\n\nText.\n"), "tdd").unwrap();
    assert_eq!(d.body, "# Body\n\nText.");
    // The engine keeps the line endings inside the body and trims only its ends (read from the pinned engine).
    let crlf = "---\r\nname: crlf\r\ndescription: Crlf file.\r\n---\r\n# T\r\n\r\nBody crlf.\r\n";
    assert_eq!(
        skill_document(crlf, "crlf").unwrap().body,
        "# T\r\n\r\nBody crlf."
    );
    let quoted = "---\nname: \"quoted\"\ndescription: \"Has: colon and #hash\"\nextra: 1\n---\n\n\nBody quoted.\n\n\n";
    assert_eq!(
        skill_document(quoted, "quoted").unwrap().body,
        "Body quoted."
    );
    let folded =
        "---\nname: folded\ndescription: >-\n  Folded text over\n  two lines.\n---\nBody folded.\n";
    assert_eq!(
        skill_document(folded, "folded").unwrap().body,
        "Body folded."
    );
}

#[test]
fn comment_lines_indented_keys_and_lines_without_a_colon_in_the_frontmatter_are_passed_over() {
    let text = "---\n# a comment line\nmetadata:\n  indented: line\nnotakey\nname: strays\ndescription: ok\n---\nBody.\n";
    let d = skill_document(text, "strays").unwrap();
    assert_eq!(d.name, "strays");
    assert_eq!(d.description, "ok");
    assert_eq!(d.body, "Body.");
}

#[test]
fn a_document_the_engine_would_not_list_or_would_list_under_another_name_is_refused() {
    let cases: [(&str, &str, &str); 7] = [
        ("no frontmatter", "No frontmatter body.\n", "frontmatter"),
        (
            "unclosed",
            "---\nname: d\ndescription: x\nBody.\n",
            "frontmatter",
        ),
        (
            "name differs",
            "---\nname: other-name\ndescription: x\n---\nBody.\n",
            "frontmatter name",
        ),
        (
            "no name",
            "---\ndescription: x\n---\nBody.\n",
            "frontmatter name",
        ),
        (
            "no description",
            "---\nname: d\n---\nBody.\n",
            "description",
        ),
        (
            "empty description",
            "---\nname: d\ndescription:\n---\nBody.\n",
            "description",
        ),
        (
            "empty body",
            "---\nname: d\ndescription: x\n---\n\n  \n",
            "body",
        ),
    ];
    for (what, text, want) in cases {
        let e = skill_document(text, "d").unwrap_err();
        assert!(e.contains(want), "{what}: {e}");
    }
}

// ---- the engine's HOME ---------------------------------------------------------------------------------------------------------

#[test]
fn the_skills_folder_of_the_home_directory_is_removed_and_nothing_else() {
    let home = common::scratch("skills-home");
    let planted = home.join(".agents/skills/planted");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join("SKILL.md"), "x\n").unwrap();
    std::fs::create_dir_all(home.join(".m2")).unwrap();
    std::fs::write(home.join(".m2/settings.xml"), "<settings/>\n").unwrap();
    std::fs::write(home.join(".agents/keep.txt"), "keep\n").unwrap();
    assert!(clear_home_skills(&home).unwrap());
    assert!(!home.join(".agents/skills").exists());
    assert!(home.join(".agents/keep.txt").is_file());
    assert!(home.join(".m2/settings.xml").is_file());
    // Nothing left to remove the second time.
    assert!(!clear_home_skills(&home).unwrap());
}

#[test]
fn a_link_in_place_of_the_home_agents_folder_is_removed_and_its_target_left_alone() {
    let home = common::scratch("skills-home-agents-link");
    let target = home.join("target");
    std::fs::create_dir_all(target.join("skills/x")).unwrap();
    std::fs::write(target.join("skills/x/SKILL.md"), "x\n").unwrap();
    std::os::unix::fs::symlink(&target, home.join(".agents")).unwrap();
    assert!(clear_home_skills(&home).unwrap());
    assert!(std::fs::symlink_metadata(home.join(".agents")).is_err());
    assert!(target.join("skills/x/SKILL.md").is_file());
}

#[test]
fn a_link_in_place_of_the_home_skills_folder_is_removed_and_its_target_left_alone() {
    let home = common::scratch("skills-home-link");
    let target = home.join("target");
    std::fs::create_dir_all(target.join("skills/x")).unwrap();
    std::fs::write(target.join("skills/x/SKILL.md"), "x\n").unwrap();
    std::fs::create_dir_all(home.join(".agents")).unwrap();
    std::os::unix::fs::symlink(target.join("skills"), home.join(".agents/skills")).unwrap();
    assert!(clear_home_skills(&home).unwrap());
    assert!(std::fs::symlink_metadata(home.join(".agents/skills")).is_err());
    assert!(target.join("skills/x/SKILL.md").is_file());
}

#[test]
fn a_home_directory_with_no_agents_folder_is_left_alone() {
    let home = common::scratch("skills-home-empty");
    assert!(!clear_home_skills(&home).unwrap());
    std::fs::write(home.join(".agents"), "a file, not a folder\n").unwrap();
    assert!(!clear_home_skills(&home).unwrap());
    assert!(home.join(".agents").is_file());
}

// ---- the session directory -----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_session_directory_is_the_git_top_level_of_the_work_tree() {
    let dir = common::scratch("skills-cwd");
    let repo = common::python_calc(&dir);
    let want = repo.canonicalize().unwrap().display().to_string();
    assert_eq!(top_level(&repo).await.unwrap(), want);
    // Asked from a directory below the top level, the answer is still the top level.
    assert_eq!(top_level(&repo.join("src")).await.unwrap(), want);
}

#[tokio::test]
async fn a_directory_that_is_not_a_work_tree_has_no_top_level() {
    let dir = common::scratch("skills-cwd-none");
    // The scratch directory is under TMPDIR; a TMPDIR inside a repository would have a top level.
    let inside = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        !inside.status.success(),
        "TMPDIR is inside a git repository; point TMPDIR at the workspace tmp"
    );
    let e = top_level(&dir).await.unwrap_err();
    assert!(e.contains("git"), "{e}");
}

// ---- the module's settings and its event ------------------------------------------------------------------------------------------

#[test]
fn the_module_is_on_by_default_and_has_one_switch_and_one_bound() {
    let s = Settings::from_lookup(|_| None);
    assert!(s.enabled);
    assert_eq!(s.listing_timeout.as_secs(), 20);

    for off in ["0", "false", "FALSE", "off", "Off", "no"] {
        let s = Settings::from_lookup(|k| (k == "AGENTD_SKILLS_ENABLED").then(|| off.to_string()));
        assert!(!s.enabled, "{off}");
    }
    for on in ["1", "true", "on", "yes", "", "anything"] {
        let s = Settings::from_lookup(|k| (k == "AGENTD_SKILLS_ENABLED").then(|| on.to_string()));
        assert!(s.enabled, "{on:?}");
    }
    let secs = |v: &str| {
        Settings::from_lookup(|k| (k == "AGENTD_SKILLS_LISTING_TIMEOUT_S").then(|| v.to_string()))
            .listing_timeout
            .as_secs()
    };
    assert_eq!(secs("5"), 5);
    // Two listings run inside the startup hook's 60 s: one never takes more than 25.
    assert_eq!(secs("25"), 25);
    assert_eq!(secs("999"), 25);
    assert_eq!(secs("0"), 20);
    assert_eq!(secs("soon"), 20);
}

#[test]
fn the_event_names_each_skill_with_its_digest_and_never_its_contents() {
    let config = common::scratch("skills-event");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    let body = "a-line-of-the-script-that-must-not-be-posted";
    let placed = place_skills(
        &dir,
        &[skill("one", &[("scripts/a.py", body)]), skill("two", &[])],
    )
    .unwrap();
    let ev = skills_event(&placed, Some(37));
    assert_eq!(ev["kind"], "skills");
    assert_eq!(ev["turn"], 1);
    assert_eq!(ev["skills"].as_array().unwrap().len(), 2);
    assert_eq!(ev["skills"][0]["name"], "one");
    assert_eq!(ev["skills"][0]["digest"], placed[0].digest.as_str());
    assert_eq!(ev["skills"][0]["files"], 2);
    assert_eq!(ev["skills"][1]["name"], "two");
    assert_eq!(ev["preflight"]["ran"], true);
    assert_eq!(ev["preflight"]["ms"], 37);
    assert!(!ev.to_string().contains(body));
    // No listing command: the event says so.
    let ev = skills_event(&placed, None);
    assert_eq!(ev["preflight"]["ran"], false);
    assert!(ev["preflight"]["ms"].is_null());
}

#[test]
fn the_refs_for_summary_json_carry_name_digest_file_count_and_total_bytes() {
    let config = common::scratch("skills-refs");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    let s = skill("one", &[("scripts/a.py", "print(1)\n")]);
    let total: u64 = s.files.iter().map(|f| f.size).sum();
    let placed = place_skills(&dir, std::slice::from_ref(&s)).unwrap();
    let refs = skill_refs(&placed);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].name, "one");
    assert_eq!(refs[0].sha256, skill_sha256(&s));
    assert_eq!(refs[0].files, 2);
    assert_eq!(refs[0].bytes, total);
}

// ---- what agentd places is marked, and only that is agentd's ---------------------------------------------------

fn tree_mode_of(p: &Path) -> u32 {
    mode(p)
}

#[test]
fn every_placed_skill_holds_a_marker_file_naming_its_digest_and_the_directory_has_none() {
    let config = common::scratch("skills-marker");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    let a = skill("lifecycle-check", &[("scripts/x.py", "print(1)\n")]);
    place_skills(&dir, &[a.clone(), skill("lifecycle-loop", &[])]).unwrap();
    assert_eq!(MARKER, ".agentd-placed");
    for name in ["lifecycle-check", "lifecycle-loop"] {
        let m = dir.join(name).join(MARKER);
        assert!(m.is_file(), "{name} has a marker");
        assert_eq!(mode(&m), 0o444);
    }
    let text = std::fs::read_to_string(dir.join("lifecycle-check").join(MARKER)).unwrap();
    assert!(
        text.contains(&skill_sha256(&a)),
        "the marker names the digest: {text}"
    );
    assert!(
        !dir.join(MARKER).exists(),
        "no marker in the skills directory itself"
    );
    // A skill's own file can never be called like the marker.
    let mut clash = skill("x", &[]);
    clash.files.push(file(MARKER, "mine\n"));
    assert!(harness_protocol::direct::check_skills(&[clash]).is_err());
}

#[test]
fn content_agentd_did_not_place_is_never_deleted_refused_or_locked() {
    // The research module will put its context beside the run's skills. Whatever is in the skills directory that agentd did not place stays
    // as it is: its files, its modes and the mode of the directory.
    let config = common::scratch("skills-others");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    let other = dir.join("research-notes");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("SKILL.md"), "research\n").unwrap();
    std::fs::write(dir.join("notes.txt"), "stray\n").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o750)).unwrap();

    let first = place_skills(&dir, &[skill("mine-a", &[])]).unwrap();
    assert_eq!(first.len(), 1);
    // Placing again, for another set, replaces what agentd placed and nothing else.
    place_skills(&dir, &[skill("mine-b", &[])]).unwrap();
    assert!(
        !dir.join("mine-a").exists(),
        "agentd's own earlier folder is replaced"
    );
    assert!(dir.join("mine-b/SKILL.md").is_file());
    assert_eq!(
        std::fs::read_to_string(other.join("SKILL.md")).unwrap(),
        "research\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "stray\n"
    );
    assert_eq!(tree_mode_of(&other), 0o750, "not locked");
    assert_eq!(
        tree_mode_of(&dir),
        0o755,
        "a directory agentd did not create keeps its mode"
    );
}

#[test]
fn a_directory_of_a_requested_name_that_agentd_did_not_place_is_refused_and_left_alone() {
    let config = common::scratch("skills-taken");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    let taken = dir.join("taken");
    std::fs::create_dir_all(&taken).unwrap();
    std::fs::write(taken.join("SKILL.md"), "theirs\n").unwrap();
    let e = place_skills(&dir, &[skill("fresh", &[]), skill("taken", &[])]).unwrap_err();
    assert!(e.contains("taken") && e.contains("already there"), "{e}");
    assert_eq!(
        std::fs::read_to_string(taken.join("SKILL.md")).unwrap(),
        "theirs\n"
    );
    assert!(
        !dir.join("fresh").exists(),
        "nothing was written: the clash is found before the first file"
    );
}

#[test]
fn retiring_removes_the_folders_agentd_placed_and_follows_no_link() {
    let config = common::scratch("skills-retire");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    place_skills(
        &dir,
        &[
            skill("one", &[("references/r.md", "r\n")]),
            skill("two", &[]),
        ],
    )
    .unwrap();
    // Another module's folder, and a link to a folder that holds a marker (not agentd's: it is a link).
    let other = dir.join("other");
    let outside = config.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join(MARKER), "x\n").unwrap();
    std::fs::write(outside.join("keep.txt"), "keep\n").unwrap();
    // The directory is locked (0555) as the placement left it; retiring works through that.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("SKILL.md"), "other\n").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("linked")).unwrap();

    let mut gone = retire_placed(&dir).unwrap();
    gone.sort();
    assert_eq!(gone, ["one", "two"]);
    assert!(!dir.join("one").exists() && !dir.join("two").exists());
    assert!(other.join("SKILL.md").is_file());
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt")).unwrap(),
        "keep\n"
    );
    assert!(dir.join("linked").is_symlink());
    // A directory that is not there holds nothing agentd placed.
    assert_eq!(
        retire_placed(&config.join("nowhere")).unwrap(),
        Vec::<String>::new()
    );
}

#[test]
fn a_directory_agentd_created_is_locked_and_still_works_for_the_next_attempt() {
    let config = common::scratch("skills-created");
    let _unlock = Unlock(config.clone());
    let dir = user_skills_dir(&config);
    place_skills(&dir, &[skill("one", &[])]).unwrap();
    assert_eq!(tree_mode_of(&dir), 0o555, "created by agentd, so locked");
    place_skills(&dir, &[skill("one", &[])]).unwrap();
    assert_eq!(tree_mode_of(&dir), 0o555);
    assert_eq!(tree_mode_of(&dir.join("one")), 0o555);
    assert!(dir.join("one").join(MARKER).is_file());
}

// ---- one reader for SKILL.md -------------------------------------------------------------------------------------

#[test]
fn valid_yaml_the_old_reader_refused_is_read_like_the_validator_reads_it() {
    // (text, description) -- each is accepted by `harness_protocol::direct::check_skills` too; this reader and the validator are one function.
    let cases = [
        (
            "---\nname: d\ndescription:\n  On the next line.\n---\nBody.\n",
            "On the next line.",
        ),
        (
            "---\nname: d # the folder\ndescription: ok\n---\nBody.\n",
            "ok",
        ),
        (
            "---\nname: d\ndescription: First half\n  second half\n---\nBody.\n",
            "First half second half",
        ),
    ];
    for (text, want) in cases {
        let d = skill_document(text, "d").unwrap_or_else(|e| panic!("{text:?}: {e}"));
        assert_eq!(
            (d.name.as_str(), d.description.as_str()),
            ("d", want),
            "{text:?}"
        );
        let s = Skill {
            name: "d".into(),
            files: vec![file("SKILL.md", text)],
        };
        assert_eq!(
            harness_protocol::direct::check_skills(&[s]),
            Ok(()),
            "{text:?}"
        );
    }
}

#[test]
fn what_the_validator_refuses_this_reader_refuses_with_the_same_words() {
    for text in [
        "---\nname: d\ndescription: Use when: asked\n---\nBody.\n",
        "---\nname: d\ndescription: [a, b]\n---\nBody.\n",
        "---\nname: d\nname: d\ndescription: x\n---\nBody.\n",
    ] {
        let from_reader = skill_document(text, "d").unwrap_err();
        let s = Skill {
            name: "d".into(),
            files: vec![file("SKILL.md", text)],
        };
        let from_validator = harness_protocol::direct::check_skills(&[s])
            .unwrap_err()
            .to_string();
        assert!(
            from_validator.contains(from_reader.trim_start_matches("SKILL.md ")),
            "{from_reader} / {from_validator}"
        );
    }
}
