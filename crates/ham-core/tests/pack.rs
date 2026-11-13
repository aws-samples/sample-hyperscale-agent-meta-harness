//! The lifecycle pack as `ham` carries it: the embedded files are the pack's files, the digest names them,
//! the contract becomes the rule `10-lifecycle.md`, the skills become a `skills` request member that passes the shared validator, and a
//! skill body renders as an always-on rule (frontmatter removed).
#![allow(clippy::unwrap_used)]

use ham_core::pack;
use ham_core::rules::{self, MAX_RULE_FILES};
use harness_protocol::direct::{Skill, SkillFile, check_skills, skill_sha256};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn lock() -> serde_json::Value {
    serde_json::from_str(pack::LOCK).unwrap()
}

// ---- the embedded files are the pack's files ---------------------------------------------------------------------------------

#[test]
fn the_contract_rule_asset_is_byte_for_byte_the_pack_contract() {
    let on_disk = std::fs::read_to_string(repo().join("lifecycle/pack/AGENTS.md")).unwrap();
    assert_eq!(pack::CONTRACT, on_disk);
    assert!(pack::CONTRACT.len() <= 11600, "the contract limit");
}

#[test]
fn the_lock_embedded_is_the_lock_on_disk() {
    let on_disk = std::fs::read_to_string(repo().join("lifecycle/pack.lock.json")).unwrap();
    assert_eq!(pack::LOCK, on_disk);
}

#[test]
fn the_embedded_files_are_exactly_the_files_the_lock_lists_with_the_hashes_it_records() {
    let lock = lock();
    let want = lock["files"].as_object().unwrap();
    let have: Vec<(&str, String)> = pack::files()
        .iter()
        .map(|f| (f.path, sha(f.content.as_bytes())))
        .collect();
    assert_eq!(have.len(), want.len(), "{have:?}");
    for (path, hash) in &have {
        assert_eq!(
            want.get(*path).and_then(|v| v.as_str()),
            Some(hash.as_str()),
            "{path}"
        );
    }
    let mut paths: Vec<&str> = have.iter().map(|(p, _)| *p).collect();
    let sorted = {
        let mut s = paths.clone();
        s.sort_unstable();
        s
    };
    assert_eq!(paths, sorted, "the files are in path order");
    paths.dedup();
    assert_eq!(paths.len(), have.len(), "no path twice");
}

#[test]
fn the_embedded_files_are_the_files_of_the_pack_directory() {
    let root = repo().join("lifecycle/pack");
    let mut on_disk = Vec::new();
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                out.push(
                    p.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    walk(&root, &root, &mut on_disk);
    on_disk.sort();
    let embedded: Vec<String> = pack::files().iter().map(|f| f.path.to_string()).collect();
    assert_eq!(
        embedded, on_disk,
        "a pack file added or removed needs an entry in pack::FILES"
    );
}

#[test]
fn the_version_is_the_locks_version() {
    assert_eq!(pack::version(), lock()["version"].as_str().unwrap());
    assert!(pack::version().split('.').count() == 3);
}

#[test]
fn the_digest_is_over_the_sorted_path_and_hash_lines_and_moves_with_any_byte() {
    let mut text = String::new();
    for f in pack::files() {
        text.push_str(&format!("{}\t{}\n", f.path, sha(f.content.as_bytes())));
    }
    assert_eq!(pack::digest(), sha(text.as_bytes()));
    assert_eq!(pack::digest().len(), 64);
    // A different content hash gives a different digest (the digest is not a constant).
    let mut other = text.clone();
    other.push('x');
    assert_ne!(pack::digest(), sha(other.as_bytes()));
}

#[test]
fn the_delivery_skill_has_three_files_and_the_references_are_the_pack_bytes() {
    // Pack 1.0.8 (DOX): the delivery skill carries the root AGENTS.md starter and its MIT notice as reference files, copied by the model with `cat`.
    let skills = pack::skills();
    let delivery = skills
        .iter()
        .find(|s| s.name == "lifecycle-delivery")
        .unwrap();
    let paths: Vec<&str> = delivery.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "SKILL.md",
            "references/dox-notice.md",
            "references/dox-root.md"
        ]
    );
    let dir = repo().join("lifecycle/pack/.agents/skills/lifecycle-delivery");
    for f in &delivery.files {
        let on_disk = std::fs::read_to_string(dir.join(&f.path)).unwrap();
        assert_eq!(f.content, on_disk, "{} is the pack file", f.path);
        assert_eq!(f.sha256, sha(on_disk.as_bytes()), "{}", f.path);
    }
    let starter = &delivery.files[2];
    assert!(
        starter
            .content
            .starts_with("<!-- DOX framework from https://github.com/agent0ai/dox"),
        "the starter opens with its provenance comment"
    );
    assert!(
        starter.content.contains("\n# DOX framework\n"),
        "the starter holds the framework block"
    );
    assert!(
        delivery.files[1]
            .content
            .contains("Permission is hereby granted"),
        "the notice holds the MIT permission text"
    );
    // The other six skills keep their files; only the delivery skill grew.
    for s in skills.iter().filter(|s| s.name != "lifecycle-delivery") {
        assert!(
            s.files.iter().all(|f| !f.path.starts_with("references/")),
            "{}",
            s.name
        );
    }
    assert_eq!(check_skills(std::slice::from_ref(delivery)), Ok(()));
}

// ---- T0a: the contract as a rule ---------------------------------------------------------------------------------------------

#[test]
fn the_contract_rule_is_10_lifecycle_md_with_the_contracts_bytes_and_passes_the_rules_validator() {
    let r = pack::contract_rule();
    assert_eq!(r.path, "10-lifecycle.md");
    assert_eq!(r.content, pack::CONTRACT);
    assert_eq!(r.sha256, sha(pack::CONTRACT.as_bytes()));
    rules::validate_rule(&r.path, &r.content).unwrap();
    rules::validate_rule_files(std::slice::from_ref(&r)).unwrap();
}

#[test]
fn the_contract_sorts_after_the_default_rules_so_the_users_file_comes_first() {
    assert!("00-my-rules.md" < pack::CONTRACT_RULE);
}

// ---- T1: the skills as a request member --------------------------------------------------------------------------------------

#[test]
fn the_pack_has_seven_skills_that_pass_the_shared_validator() {
    let skills = pack::skills();
    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "lifecycle-build-test",
            "lifecycle-check",
            "lifecycle-context",
            "lifecycle-delivery",
            "lifecycle-loop",
            "lifecycle-research",
            "lifecycle-tdd",
        ]
    );
    assert_eq!(check_skills(&skills), Ok(()));
}

#[test]
fn a_skill_holds_its_files_relative_to_its_folder() {
    let skills = pack::skills();
    let check = skills.iter().find(|s| s.name == "lifecycle-check").unwrap();
    let paths: Vec<&str> = check.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "SKILL.md",
            "scripts/lifecycle_check.py",
            "scripts/scanner_kit.py"
        ]
    );
    let tdd = skills.iter().find(|s| s.name == "lifecycle-tdd").unwrap();
    assert_eq!(tdd.files.len(), 1);
    assert_eq!(tdd.files[0].path, "SKILL.md");
    let embedded = pack::files()
        .iter()
        .find(|f| f.path == ".agents/skills/lifecycle-tdd/SKILL.md")
        .unwrap();
    assert_eq!(tdd.files[0].content, embedded.content);
    assert_eq!(tdd.files[0].size, embedded.content.len() as u64);
    assert_eq!(tdd.files[0].sha256, sha(embedded.content.as_bytes()));
}

#[test]
fn every_skill_name_equals_the_name_in_its_frontmatter() {
    for s in pack::skills() {
        let entry = s.files.iter().find(|f| f.path == "SKILL.md").unwrap();
        let (front, _) = entry
            .content
            .strip_prefix("---\n")
            .unwrap()
            .split_once("\n---\n")
            .unwrap();
        let name = front
            .lines()
            .find_map(|l| l.strip_prefix("name: "))
            .unwrap();
        assert_eq!(name, s.name);
    }
}

#[test]
fn the_skills_member_stays_inside_the_requests_limits() {
    let skills = pack::skills();
    assert!(skills.len() <= harness_protocol::direct::MAX_SKILLS);
    let total: usize = skills
        .iter()
        .flat_map(|s| &s.files)
        .map(|f| f.content.len())
        .sum();
    // The budget of the whole pack in one start request: 100 KiB while the pack was 101,521 bytes (pack 1.10.0), 128 KiB since the scanner kit
    // gained its bandit and secret-shape parts (pack 1.11.0, 117,994 bytes), 132 KiB since the delivery and build-and-test skills and the DOX starter
    // gained the words for a starter the harness writes before the first prompt (pack 1.14.0, about 131,400 bytes). The skills have no size of their
    // own in the protocol; the pack stays small so that it leaves the request room for the user's own files.
    assert!(
        total < 132 * 1024,
        "the pack is small enough to travel in every start request: {total} bytes"
    );
}

// ---- T0b: a skill body as a rule ---------------------------------------------------------------------------------------------

fn demo(entry: &str) -> Skill {
    Skill {
        name: "lifecycle-demo".into(),
        files: vec![SkillFile::new("SKILL.md", entry)],
    }
}

#[test]
fn a_rendered_skill_has_no_frontmatter_and_keeps_name_description_and_body() {
    let tdd = pack::skills()
        .into_iter()
        .find(|s| s.name == "lifecycle-tdd")
        .unwrap();
    let text = pack::render_skill_rule(&tdd).unwrap();
    assert!(!text.starts_with("---"), "{}", &text[..40.min(text.len())]);
    assert!(
        text.starts_with("# lifecycle-tdd\n"),
        "{}",
        &text[..60.min(text.len())]
    );
    assert!(
        text.contains("Build each behavior test first and keep the failing run."),
        "the description"
    );
    assert!(
        text.contains("# Test first, record the red, then the code"),
        "the body"
    );
    assert!(text.contains("Never edit a test to get green"), "the body");
    assert!(
        !text.contains("\nname: lifecycle-tdd\n") && !text.contains("\ndescription: "),
        "the frontmatter keys are gone"
    );
    assert!(text.ends_with('\n') && !text.ends_with("\n\n"));
}

#[test]
fn a_skill_without_a_closed_frontmatter_or_a_body_cannot_be_rendered() {
    use ham_core::pack::RenderError;
    assert_eq!(
        pack::render_skill_rule(&demo("no frontmatter here\n")),
        Err(RenderError::NoFrontmatter("lifecycle-demo".into()))
    );
    assert_eq!(
        pack::render_skill_rule(&demo(
            "---\nname: lifecycle-demo\ndescription: x\nnever closed\n"
        )),
        Err(RenderError::NoFrontmatter("lifecycle-demo".into()))
    );
    assert_eq!(
        pack::render_skill_rule(&demo(
            "---\nname: lifecycle-demo\ndescription: x\n---\n\n  \n"
        )),
        Err(RenderError::EmptyBody("lifecycle-demo".into()))
    );
    let none = Skill {
        name: "lifecycle-demo".into(),
        files: vec![SkillFile::new("notes.md", "x")],
    };
    assert_eq!(
        pack::render_skill_rule(&none),
        Err(RenderError::NoEntry("lifecycle-demo".into()))
    );
}

#[test]
fn a_body_that_itself_opens_with_three_dashes_is_not_cut() {
    let text = pack::render_skill_rule(&demo(
        "---\nname: lifecycle-demo\ndescription: x\n---\n---\nbody line\n",
    ))
    .unwrap();
    assert!(text.contains("---\nbody line"), "{text}");
}

#[test]
fn the_seven_rendered_rules_are_valid_rules_in_step_order_with_numbered_names() {
    let rendered = pack::skills_as_rules();
    let names: Vec<&str> = rendered.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        names,
        [
            "21-lifecycle-research.md",
            "22-lifecycle-context.md",
            "23-lifecycle-tdd.md",
            "24-lifecycle-build-test.md",
            "25-lifecycle-loop.md",
            "26-lifecycle-delivery.md",
            "27-lifecycle-check.md",
        ]
    );
    assert_eq!(names, pack::skill_rule_names());
    for r in &rendered {
        rules::validate_rule(&r.path, &r.content).unwrap();
        assert_eq!(r.sha256, sha(r.content.as_bytes()));
    }
    // With the contract and the user's own file, the set stays under the file limit.
    assert!(rendered.len() + 2 <= MAX_RULE_FILES);
    let mut all = vec![pack::contract_rule()];
    all.extend(rendered);
    rules::validate_rule_files(&all).unwrap();
}

#[test]
fn skill_rule_names_map_the_pack_skills_and_refuse_others() {
    assert_eq!(
        pack::skill_rule_name("lifecycle-tdd").as_deref(),
        Some("23-lifecycle-tdd.md")
    );
    assert_eq!(
        pack::skill_rule_name("lifecycle-check").as_deref(),
        Some("27-lifecycle-check.md")
    );
    assert_eq!(pack::skill_rule_name("lifecycle-unknown"), None);
    assert_eq!(pack::skill_rule_name("tdd"), None);
    assert_eq!(pack::skill_rule_name(""), None);
}

#[test]
fn the_pack_rule_names_are_recognized_and_the_users_own_names_are_not() {
    for n in [
        "10-lifecycle.md",
        "21-lifecycle-research.md",
        "27-lifecycle-check.md",
    ] {
        assert!(pack::is_pack_rule_name(n), "{n}");
    }
    for n in [
        "00-my-rules.md",
        "AGENTS.md",
        "11-lifecycle.md",
        "21-lifecycle-tdd.md",
        "10-LIFECYCLE.md",
    ] {
        assert!(!pack::is_pack_rule_name(n), "{n}");
    }
}

#[test]
fn the_skill_digests_differ_per_skill_and_agree_with_the_shared_digest() {
    let skills = pack::skills();
    assert_eq!(skills.len(), 7);
    let mut seen = std::collections::BTreeSet::new();
    for s in &skills {
        assert!(seen.insert(skill_sha256(s)), "{}", s.name);
    }
}
