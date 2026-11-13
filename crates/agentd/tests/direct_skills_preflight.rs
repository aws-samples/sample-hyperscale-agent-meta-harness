//! The skills preflight: before any model
//! call agentd asks the engine which skills it will load, twice: once before the run's skills are placed (a repository skill of the
//! same name would be replaced silently, and nothing may come from outside the repository), once after (every placed skill is listed,
//! once, at its own path, reading the way the file reads, and nothing else is loaded from outside the repository).
//!
//! The sample listing is the output of the pinned engine's `config skills --json` (Cline 3.0.71): `instructions` is the body of SKILL.md without its frontmatter and trimmed, `frontmatter` is the parsed header, `path`
//! is the absolute path of the file, a name comes from the frontmatter, a skill without a description has none.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::skills::place::{PlacedSkill, place_skills};
use agentd::direct::skills::preflight::{
    check_placed_listing, check_placed_listing_in, check_repo_skills, check_repo_skills_in,
};
use agentd::engine::direct::{
    ListedSkill, parse_skills_listing, skills_listing_argv, user_skills_dir,
};
use harness_protocol::direct::{Skill, SkillFile};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

// ---- the listing command and its output -----------------------------------------------------------------------------------------

#[test]
fn the_skills_listing_command_asks_for_the_skills_of_the_config_directory_only() {
    let argv =
        skills_listing_argv("cline", "/opt/harness/bin/cline", Path::new("/c/config")).unwrap();
    assert_eq!(
        argv,
        [
            "/opt/harness/bin/cline",
            "config",
            "skills",
            "--json",
            "--config",
            "/c/config"
        ]
    );
    // `config` refuses --data-dir, as it does for the rules.
    assert!(!argv.iter().any(|a| a == "--data-dir"));
}

#[test]
fn an_engine_with_no_skills_listing_has_no_skills_preflight() {
    assert_eq!(
        skills_listing_argv("fake-acp", "fake-acp-engine", Path::new("/c")),
        None
    );
}

#[test]
fn the_skills_dir_is_skills_inside_the_config_directory() {
    assert_eq!(
        user_skills_dir(Path::new("/c/config")),
        PathBuf::from("/c/config/skills")
    );
}

/// What the pinned engine printed for a placed skill and for a skill with no description (one line, no trailing newline).
const REAL: &str = r##"[{"name":"probe-skill","description":"A probe skill. Use when asked for the probe.","instructions":"# Probe body\n\nStep one: do the probe.","frontmatter":{"name":"probe-skill","description":"A probe skill. Use when asked for the probe."},"path":"/w/config/skills/probe-skill/SKILL.md"},{"name":"nodesc","instructions":"Body nodesc.","frontmatter":{"name":"nodesc"},"path":"/w/repo/.agents/skills/nodesc/SKILL.md"}]"##;

#[test]
fn the_listing_of_the_pinned_engine_is_read() {
    let rows = parse_skills_listing(REAL).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].name, "probe-skill");
    assert_eq!(
        rows[0].description.as_deref(),
        Some("A probe skill. Use when asked for the probe.")
    );
    assert_eq!(
        rows[0].instructions,
        "# Probe body\n\nStep one: do the probe."
    );
    assert_eq!(rows[0].frontmatter["name"], "probe-skill");
    assert_eq!(rows[0].path, "/w/config/skills/probe-skill/SKILL.md");
    assert_eq!(rows[1].description, None);
    // A notice line before the array is skipped; no skill at all is an empty array.
    let noisy = format!("update available\n{REAL}\n");
    assert_eq!(parse_skills_listing(&noisy).unwrap().len(), 2);
    assert!(parse_skills_listing("[]").unwrap().is_empty());
}

#[test]
fn a_listing_that_is_not_the_expected_array_is_an_error_that_quotes_nothing() {
    for bad in [
        "this is not json canary-output",
        "{\"name\":\"x\"}",
        "",
        "[{\"nope\":1}]",
    ] {
        let e = parse_skills_listing(bad).unwrap_err();
        assert!(e.contains("skills listing"), "{e}");
        assert!(!e.contains("canary-output"), "{e}");
    }
}

// ---- the check before the run's skills are placed -----------------------------------------------------------------------------------

fn row(name: &str, body: &str, path: &Path) -> ListedSkill {
    ListedSkill {
        name: name.into(),
        description: Some(format!("Use when asked for {name}.")),
        instructions: body.into(),
        frontmatter: json!({"name": name, "description": format!("Use when asked for {name}.")}),
        path: path.display().to_string(),
    }
}

/// A repository with one skill of its own at `.agents/skills/<name>/SKILL.md`.
fn repo_with(label: &str, names: &[&str]) -> (PathBuf, Vec<ListedSkill>) {
    let dir = common::scratch(label);
    let repo = dir.join("repo");
    let mut rows = Vec::new();
    for n in names {
        let p = repo.join(format!(".agents/skills/{n}/SKILL.md"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, format!("---\nname: {n}\n---\nrepo body\n")).unwrap();
        rows.push(row(n, "repo body", &p));
    }
    std::fs::create_dir_all(&repo).unwrap();
    (repo, rows)
}

#[test]
fn a_repository_with_other_skills_or_none_passes_the_check_before_placement() {
    let (repo, rows) = repo_with("pre-ok", &["theirs", "also-theirs"]);
    check_repo_skills(&rows, &repo, &["mine".to_string()]).unwrap();
    check_repo_skills(&[], &repo, &["mine".to_string()]).unwrap();
}

#[test]
fn a_repository_skill_of_the_same_name_stops_the_run_and_names_the_file() {
    let (repo, rows) = repo_with("pre-shadow", &["dup", "other"]);
    let e = check_repo_skills(&rows, &repo, &["dup".to_string()]).unwrap_err();
    assert!(
        e.contains("dup")
            && e.contains("already there")
            && e.contains(".agents/skills/dup/SKILL.md"),
        "{e}"
    );
    // Names are compared without case.
    let e = check_repo_skills(&rows, &repo, &["DUP".to_string()]).unwrap_err();
    assert!(e.contains("already there"), "{e}");
}

#[test]
fn a_skill_loaded_from_outside_the_repository_stops_the_run() {
    let (repo, _) = repo_with("pre-outside", &[]);
    let home = repo
        .parent()
        .unwrap()
        .join("home/.agents/skills/homeskill/SKILL.md");
    std::fs::create_dir_all(home.parent().unwrap()).unwrap();
    std::fs::write(&home, "x\n").unwrap();
    let e = check_repo_skills(&[row("homeskill", "x", &home)], &repo, &[]).unwrap_err();
    assert!(
        e.contains("homeskill") && e.contains("outside the repository"),
        "{e}"
    );
    // A row whose file does not exist, or that has no path, cannot be shown to be inside.
    let ghost = repo.join(".agents/skills/ghost/SKILL.md");
    assert!(check_repo_skills(&[row("ghost", "x", &ghost)], &repo, &[]).is_err());
    assert!(check_repo_skills(&[row("nopath", "x", Path::new(""))], &repo, &[]).is_err());
}

// ---- the check after placement ---------------------------------------------------------------------------------------------------

struct Placed {
    repo: PathBuf,
    config: PathBuf,
    placed: Vec<PlacedSkill>,
    /// The rows a correct engine prints for the placed skills.
    rows: Vec<ListedSkill>,
}

impl Drop for Placed {
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
        walk(&self.config);
    }
}

fn placed_two(label: &str) -> Placed {
    let (repo, _) = repo_with(label, &["theirs"]);
    let config = repo.parent().unwrap().join("config");
    let mk = |name: &str, body: &str| {
        let text =
            format!("---\nname: {name}\ndescription: Use when asked for {name}.\n---\n{body}");
        Skill {
            name: name.into(),
            files: vec![SkillFile {
                path: "SKILL.md".into(),
                sha256: sha(text.as_bytes()),
                size: text.len() as u64,
                content: text,
            }],
        }
    };
    let placed = place_skills(
        &user_skills_dir(&config),
        &[
            mk("mine-a", "Do A.\r\nThen B.\r\n"),
            mk("mine-b", "Do B.\n"),
        ],
    )
    .unwrap();
    // The engine prints the body trimmed at both ends and otherwise as the file has it, line endings included (read from the pinned engine).
    let rows = vec![
        row("mine-a", "Do A.\r\nThen B.", &placed[0].skill_md),
        row("mine-b", "Do B.", &placed[1].skill_md),
        row(
            "theirs",
            "repo body",
            &repo.join(".agents/skills/theirs/SKILL.md"),
        ),
    ];
    Placed {
        repo,
        config,
        placed,
        rows,
    }
}

#[test]
fn a_listing_that_shows_every_placed_skill_at_its_path_passes() {
    let p = placed_two("post-ok");
    check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap();
}

#[test]
fn a_placed_skill_the_engine_does_not_list_stops_the_run() {
    let mut p = placed_two("post-missing");
    p.rows.remove(0);
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(
        e.contains("mine-a") && e.contains("missing from the skills the engine will load"),
        "{e}"
    );
}

#[test]
fn a_placed_skill_listed_from_another_file_is_shadowed() {
    let mut p = placed_two("post-shadow");
    p.rows[1].path = p
        .repo
        .join(".agents/skills/theirs/SKILL.md")
        .display()
        .to_string();
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(e.contains("mine-b") && e.contains("shadowed"), "{e}");
    // The same name twice in the listing is not one skill either.
    let mut p = placed_two("post-twice");
    let again = p.rows[0].clone();
    p.rows.push(again);
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(e.contains("mine-a") && e.contains("more than once"), "{e}");
}

#[test]
fn a_placed_skill_that_reads_differently_stops_the_run() {
    let mut p = placed_two("post-changed");
    p.rows[0].instructions = "Do A.\r\nThen B. (changed)".into();
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(
        e.contains("mine-a") && e.contains("differs from the file placed"),
        "{e}"
    );

    // The engine reads the name from the frontmatter: another name is another skill, and the placed one is missing.
    let mut p = placed_two("post-name");
    p.rows[0].name = "other".into();
    p.rows[0].frontmatter = json!({"name": "other"});
    assert!(check_placed_listing(&p.rows, &p.placed, &p.repo).is_err());
    let mut p = placed_two("post-fm-name");
    p.rows[0].frontmatter = json!({"name": "other", "description": "d"});
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(e.contains("frontmatter name"), "{e}");

    let mut p = placed_two("post-desc");
    p.rows[1].description = None;
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(e.contains("mine-b") && e.contains("no description"), "{e}");
}

#[test]
fn the_file_on_disk_must_still_be_the_file_that_was_placed() {
    let p = placed_two("post-disk");
    // The tree is root-owned in a run; a test user can lift the write bit to play the engine that rewrote a file.
    let f = &p.placed[0].skill_md;
    let dir = f.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(f, "---\nname: mine-a\ndescription: x\n---\nrewritten\n").unwrap();
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(e.contains("mine-a") && e.contains("changed on disk"), "{e}");
}

#[test]
fn a_listed_skill_that_is_neither_placed_nor_in_the_repository_stops_the_run() {
    let mut p = placed_two("post-extra");
    let home = p
        .repo
        .parent()
        .unwrap()
        .join("home/.agents/skills/planted/SKILL.md");
    std::fs::create_dir_all(home.parent().unwrap()).unwrap();
    std::fs::write(&home, "x\n").unwrap();
    p.rows.push(row("planted", "x", &home));
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(
        e.contains("planted") && e.contains("outside the repository"),
        "{e}"
    );
}

// ---- paths that do not resolve ---------------------------------------------------------------------------------------------------

#[test]
fn a_row_whose_file_does_not_exist_and_is_not_in_the_repository_is_outside_it() {
    let (repo, _) = repo_with("pre-ghost-out", &[]);
    let ghost = repo.parent().unwrap().join("elsewhere/ghost/SKILL.md");
    let e = check_repo_skills(&[row("ghost", "x", &ghost)], &repo, &[]).unwrap_err();
    assert!(e.contains("outside the repository"), "{e}");
}

#[test]
fn a_row_path_with_dot_parts_that_names_a_missing_repository_file_cannot_be_read() {
    let (repo, _) = repo_with("pre-ghost-dots", &["real"]);
    let odd = format!("{}/.agents/skills/./real/../ghost/SKILL.md", repo.display());
    let e = check_repo_skills(&[row("ghost", "x", Path::new(&odd))], &repo, &[]).unwrap_err();
    assert!(
        e.contains("cannot be placed in the repository") && e.contains("cannot be read"),
        "{e}"
    );
}

#[test]
fn a_listed_row_whose_file_cannot_be_read_is_not_the_placed_file() {
    let mut p = placed_two("post-unreadable");
    p.rows[1].path = p
        .config
        .join("skills/mine-b/nowhere/SKILL.md")
        .display()
        .to_string();
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(e.contains("mine-b") && e.contains("shadowed"), "{e}");
}

#[test]
fn a_listed_repository_row_that_cannot_be_read_stops_the_run() {
    let mut p = placed_two("post-ghost-repo");
    let ghost = p.repo.join(".agents/skills/ghost/SKILL.md");
    p.rows.push(row("ghost", "x", &ghost));
    let e = check_placed_listing(&p.rows, &p.placed, &p.repo).unwrap_err();
    assert!(
        e.contains("ghost") && e.contains("cannot be placed in the repository"),
        "{e}"
    );
}

// ---- skills of other modules in the skills directory --------------------------------------------------------------

/// A repository with one skill of its own, a configuration directory, and one skill another module put in the skills directory.
fn with_module_skill(label: &str) -> (PathBuf, PathBuf, Vec<ListedSkill>) {
    let (repo, mut rows) = repo_with(label, &["theirs"]);
    let config = repo.parent().unwrap().join("config");
    let skills = user_skills_dir(&config);
    let file = skills.join("research-notes/SKILL.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        "---\nname: research-notes\ndescription: x\n---\nnotes\n",
    )
    .unwrap();
    rows.push(row("research-notes", "notes", &file));
    (repo, skills, rows)
}

#[test]
fn before_placement_a_skill_another_module_put_in_the_skills_directory_is_tolerated() {
    let (repo, skills, rows) = with_module_skill("pre-module");
    // As a skill from outside the repository it stops the run, as before, when the directory is not named ...
    assert!(check_repo_skills(&rows, &repo, &["mine".to_string()]).is_err());
    // ... and when the directory is named it is the research module's and stays.
    check_repo_skills_in(&rows, &repo, &["mine".to_string()], Some(&skills)).unwrap();
    // A requested skill with its name would clash with it: the same words as a repository clash.
    let e = check_repo_skills_in(&rows, &repo, &["research-notes".to_string()], Some(&skills))
        .unwrap_err();
    assert!(
        e.contains("research-notes") && e.contains("already there"),
        "{e}"
    );
}

#[test]
fn a_skill_from_anywhere_else_outside_the_repository_still_stops_the_run() {
    let (repo, skills, _) = with_module_skill("pre-elsewhere");
    let home = repo
        .parent()
        .unwrap()
        .join("home/.agents/skills/homeskill/SKILL.md");
    std::fs::create_dir_all(home.parent().unwrap()).unwrap();
    std::fs::write(&home, "x\n").unwrap();
    let e = check_repo_skills_in(&[row("homeskill", "x", &home)], &repo, &[], Some(&skills))
        .unwrap_err();
    assert!(
        e.contains("homeskill") && e.contains("outside the repository"),
        "{e}"
    );
}

#[test]
fn after_placement_a_skill_another_module_put_in_the_skills_directory_is_tolerated() {
    let (repo, skills, mut rows) = with_module_skill("post-module");
    let config = skills.parent().unwrap().to_path_buf();
    let text = "---\nname: mine-a\ndescription: Use when asked for mine-a.\n---\nDo A.\n";
    let mine = Skill {
        name: "mine-a".into(),
        files: vec![SkillFile {
            path: "SKILL.md".into(),
            sha256: sha(text.as_bytes()),
            size: text.len() as u64,
            content: text.into(),
        }],
    };
    let placed = place_skills(&skills, &[mine]).unwrap();
    rows.push(row("mine-a", "Do A.", &placed[0].skill_md));
    let _unlock = Placed {
        repo: repo.clone(),
        config,
        placed: Vec::new(),
        rows: Vec::new(),
    };
    check_placed_listing_in(&rows, &placed, &repo, Some(&skills)).unwrap();
    // Without the directory named the module's skill is foreign, as before.
    assert!(check_placed_listing(&rows, &placed, &repo).is_err());
    // A skill from HOME is foreign either way.
    let home = repo
        .parent()
        .unwrap()
        .join("home/.agents/skills/homeskill/SKILL.md");
    std::fs::create_dir_all(home.parent().unwrap()).unwrap();
    std::fs::write(&home, "x\n").unwrap();
    rows.push(row("homeskill", "x", &home));
    let e = check_placed_listing_in(&rows, &placed, &repo, Some(&skills)).unwrap_err();
    assert!(
        e.contains("homeskill") && e.contains("outside the repository"),
        "{e}"
    );
}
