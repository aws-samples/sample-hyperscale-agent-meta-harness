//! The file-placement check: an engine that cannot list the rules and skills it will load is checked on the disk instead, so a
//! missing, changed, extra or shadowed rule or skill stops the run before any model call, for every engine.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::placement::{verify_rules_placed, verify_skills_placed};
use agentd::direct::rules_place::place_rules;
use agentd::direct::skills::place::place_skills;
use agentd::engine::layout::{
    PlacementCheck, config_layout, placement_check, repo_rule_dirs, repo_skill_dirs,
};
use harness_protocol::api::v1::RuleFile;
use harness_protocol::direct::{Skill, SkillFile};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn rule(name: &str, content: &str) -> RuleFile {
    RuleFile {
        path: name.into(),
        sha256: sha(content.as_bytes()),
        content: content.into(),
    }
}

fn skill(name: &str) -> Skill {
    let text = format!(
        "---\nname: {name}\ndescription: Use when asked for {name}.\n---\n# Steps\n\n1. Do it.\n"
    );
    Skill {
        name: name.into(),
        files: vec![SkillFile {
            path: "SKILL.md".into(),
            sha256: sha(text.as_bytes()),
            size: text.len() as u64,
            content: text,
        }],
    }
}

/// Gives a tree its write bits back so the scratch directory can be reused.
fn unlock(p: &Path) {
    if let Ok(m) = std::fs::symlink_metadata(p)
        && m.is_dir()
    {
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
        if let Ok(rd) = std::fs::read_dir(p) {
            rd.flatten().for_each(|e| unlock(&e.path()));
        }
    }
}

const ENGINES: [&str; 3] = ["cline", "opencode", "fake-acp"];

#[test]
fn b789_each_engine_is_checked_by_its_own_listing_or_by_the_files() {
    assert_eq!(placement_check("cline"), PlacementCheck::Listing);
    assert_eq!(placement_check("opencode"), PlacementCheck::Files);
    assert_eq!(placement_check("fake-acp"), PlacementCheck::Files);
    assert_eq!(
        placement_check("an-engine-with-no-arm"),
        PlacementCheck::Files
    );
    // The argv of the listing is the other half of the same fact.
    for engine in ["opencode", "fake-acp"] {
        assert_eq!(
            agentd::engine::direct::rules_listing_argv(engine, "p", Path::new("/c")),
            None,
            "{engine} cannot list its rules"
        );
    }
    assert!(agentd::engine::direct::rules_listing_argv("cline", "p", Path::new("/c")).is_some());
}

#[test]
fn b789_rules_placed_by_agentd_pass_the_file_check_for_every_engine() {
    for engine in ENGINES {
        let base = common::scratch("b789-rules-ok");
        let layout = config_layout(engine, &base.join("config"));
        std::fs::create_dir_all(&layout.config_dir).unwrap();
        layout.prepare().unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let placed = place_rules(
            &layout.rules,
            &[
                rule("00-my-rules.md", "Write the test first.\n"),
                rule("10-b.md", "b\n"),
            ],
        )
        .unwrap();
        verify_rules_placed(engine, &layout, &placed, &repo)
            .unwrap_or_else(|e| panic!("{engine}: {e}"));
        unlock(&layout.rules);
    }
}

#[test]
fn b789_a_missing_changed_extra_or_writable_rule_is_caught_and_named() {
    for engine in ENGINES {
        let base = common::scratch("b789-rules-bad");
        let layout = config_layout(engine, &base.join("config"));
        std::fs::create_dir_all(&layout.config_dir).unwrap();
        layout.prepare().unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let placed = place_rules(
            &layout.rules,
            &[rule("00-my-rules.md", "a\n"), rule("10-b.md", "b\n")],
        )
        .unwrap();
        unlock(&layout.rules);
        let file = layout.rules.join("10-b.md");

        // writable: the engine could rewrite it
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = verify_rules_placed(engine, &layout, &placed, &repo).unwrap_err();
        assert!(
            e.contains("10-b.md") && e.contains("writable"),
            "{engine}: {e}"
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();

        // changed
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&file, "changed\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        let e = verify_rules_placed(engine, &layout, &placed, &repo).unwrap_err();
        assert!(
            e.contains("10-b.md") && e.contains("differs"),
            "{engine}: {e}"
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&file, "b\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        verify_rules_placed(engine, &layout, &placed, &repo).unwrap();

        // extra: a rule nobody sent
        std::fs::write(layout.rules.join("99-extra.md"), "x\n").unwrap();
        let e = verify_rules_placed(engine, &layout, &placed, &repo).unwrap_err();
        assert!(
            e.contains("99-extra.md") && e.contains("unexpected"),
            "{engine}: {e}"
        );
        std::fs::remove_file(layout.rules.join("99-extra.md")).unwrap();

        // missing
        std::fs::remove_file(&file).unwrap();
        let e = verify_rules_placed(engine, &layout, &placed, &repo).unwrap_err();
        assert!(
            e.contains("10-b.md") && e.contains("missing"),
            "{engine}: {e}"
        );
    }
}

#[test]
fn b789_a_rules_directory_that_is_a_link_is_refused() {
    let base = common::scratch("b789-rules-link");
    let layout = config_layout("opencode", &base.join("config"));
    std::fs::create_dir_all(&layout.config_dir).unwrap();
    layout.prepare().unwrap();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let placed = place_rules(&layout.rules, &[rule("a.md", "a\n")]).unwrap();
    let real = base.join("real");
    unlock(&layout.rules);
    std::fs::rename(&layout.rules, &real).unwrap();
    std::os::unix::fs::symlink(&real, &layout.rules).unwrap();
    let e = verify_rules_placed("opencode", &layout, &placed, &repo).unwrap_err();
    assert!(e.contains("symbolic link"), "{e}");
}

#[test]
fn b789_a_repository_file_in_the_engines_rule_folder_that_carries_the_name_of_a_user_rule_shadows_it()
 {
    // The table says where each engine reads rules from the repository by file name. OpenCode reads `AGENTS.md` and its `instructions`, which
    // have no names to clash with; the engines the build has no arm for name no folder.
    assert_eq!(repo_rule_dirs("cline"), [".clinerules"]);
    for engine in ["opencode", "fake-acp", "an-engine-with-no-arm"] {
        assert!(repo_rule_dirs(engine).is_empty(), "{engine}");
    }
    let run = |engine: &str, folder: &str, file: &str| {
        let base = common::scratch("b789-rules-shadow");
        let layout = config_layout(engine, &base.join("config"));
        std::fs::create_dir_all(&layout.config_dir).unwrap();
        layout.prepare().unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(folder)).unwrap();
        std::fs::write(repo.join(folder).join(file), "theirs\n").unwrap();
        let placed = place_rules(&layout.rules, &[rule("00-my-rules.md", "mine\n")]).unwrap();
        let got = verify_rules_placed(engine, &layout, &placed, &repo);
        unlock(&layout.rules);
        got
    };
    // The same file in the folder of the engine that names it is a shadow, and the message names both.
    let e = run("cline", ".clinerules", "00-my-rules.md").unwrap_err();
    assert!(
        e.contains("shadowed") && e.contains("00-my-rules.md") && e.contains(".clinerules"),
        "{e}"
    );
    // The stem alone also clashes (`00-my-rules` and `00-my-rules.md` are one rule to the engine).
    let e = run("cline", ".clinerules", "00-my-rules").unwrap_err();
    assert!(e.contains("shadowed"), "{e}");
    // In a folder that is not the engine's, or for an engine that names no such folder, the same file shadows nothing.
    run("cline", "docs", "00-my-rules.md").unwrap();
    run("opencode", ".clinerules", "00-my-rules.md").unwrap();
    run("fake-acp", ".clinerules", "00-my-rules.md").unwrap();
}

#[test]
fn b789_skills_placed_by_agentd_pass_the_file_check_and_a_changed_or_missing_one_is_caught() {
    for engine in ENGINES {
        let base = common::scratch("b789-skills");
        let layout = config_layout(engine, &base.join("config"));
        std::fs::create_dir_all(&layout.config_dir).unwrap();
        layout.prepare().unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let placed = place_skills(&layout.skills, &[skill("alpha"), skill("beta")]).unwrap();
        verify_skills_placed(engine, &layout, &placed, &repo)
            .unwrap_or_else(|e| panic!("{engine}: {e}"));
        unlock(&layout.skills);
        let md = layout.skills.join("beta/SKILL.md");
        std::fs::set_permissions(&md, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&md, "tampered\n").unwrap();
        let e = verify_skills_placed(engine, &layout, &placed, &repo).unwrap_err();
        assert!(e.contains("beta") && e.contains("differs"), "{engine}: {e}");
        std::fs::remove_file(&md).unwrap();
        let e = verify_skills_placed(engine, &layout, &placed, &repo).unwrap_err();
        assert!(e.contains("beta") && e.contains("missing"), "{engine}: {e}");
    }
}

#[test]
fn b789_a_repository_skill_with_the_name_of_a_requested_one_shadows_it() {
    let dirs = repo_skill_dirs("opencode");
    assert!(
        dirs.contains(&".opencode/skills")
            && dirs.contains(&".claude/skills")
            && dirs.contains(&".agents/skills"),
        "{dirs:?}"
    );
    for d in dirs {
        let base = common::scratch("b789-skills-shadow");
        let layout = config_layout("opencode", &base.join("config"));
        std::fs::create_dir_all(&layout.config_dir).unwrap();
        layout.prepare().unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(d).join("alpha")).unwrap();
        std::fs::write(repo.join(d).join("alpha/SKILL.md"), "theirs\n").unwrap();
        let placed = place_skills(&layout.skills, &[skill("alpha"), skill("beta")]).unwrap();
        let e = verify_skills_placed("opencode", &layout, &placed, &repo).unwrap_err();
        assert!(
            e.contains("shadowed") && e.contains("alpha") && e.contains(d),
            "{d}: {e}"
        );
        unlock(&layout.skills);
    }
    // A skill of another name in the same folder shadows nothing.
    let base = common::scratch("b789-skills-other");
    let layout = config_layout("opencode", &base.join("config"));
    std::fs::create_dir_all(&layout.config_dir).unwrap();
    layout.prepare().unwrap();
    let repo = base.join("repo");
    std::fs::create_dir_all(repo.join(".opencode/skills/gamma")).unwrap();
    let placed = place_skills(&layout.skills, &[skill("alpha")]).unwrap();
    verify_skills_placed("opencode", &layout, &placed, &repo).unwrap();
    unlock(&layout.skills);
}

#[test]
fn b789_a_placed_file_replaced_by_a_link_to_the_same_bytes_is_refused() {
    let base = common::scratch("b789-link-file");
    let layout = config_layout("opencode", &base.join("config"));
    std::fs::create_dir_all(&layout.config_dir).unwrap();
    layout.prepare().unwrap();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let rules = place_rules(&layout.rules, &[rule("a.md", "same bytes\n")]).unwrap();
    let skills = place_skills(&layout.skills, &[skill("alpha")]).unwrap();
    unlock(&layout.rules);
    unlock(&layout.skills);
    // The copy outside the root holds the very bytes that were placed.
    let outside = base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("a.md"), "same bytes\n").unwrap();
    std::fs::copy(
        layout.skills.join("alpha/SKILL.md"),
        outside.join("SKILL.md"),
    )
    .unwrap();
    std::fs::remove_file(layout.rules.join("a.md")).unwrap();
    std::os::unix::fs::symlink(outside.join("a.md"), layout.rules.join("a.md")).unwrap();
    std::fs::remove_file(layout.skills.join("alpha/SKILL.md")).unwrap();
    std::os::unix::fs::symlink(
        outside.join("SKILL.md"),
        layout.skills.join("alpha/SKILL.md"),
    )
    .unwrap();
    let e = verify_rules_placed("opencode", &layout, &rules, &repo).unwrap_err();
    assert!(e.contains("a.md") && e.contains("symbolic link"), "{e}");
    let e = verify_skills_placed("opencode", &layout, &skills, &repo).unwrap_err();
    assert!(
        e.contains("alpha/SKILL.md") && e.contains("symbolic link"),
        "{e}"
    );
}
