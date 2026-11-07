//! Protocol 1.38.0: the one skills validator that `ham`, control-api and agentd all call, the skill digest,
//! the `continueFrom` shape check and the wire form of both request members.
#![allow(clippy::unwrap_used)]

use harness_protocol::api::v1::StartDirectRunRequest;
use harness_protocol::direct::{
    ContinueFrom, ContinueFromError, MAX_SKILL_DESCRIPTION_CHARS, MAX_SKILL_NAME_BYTES,
    MAX_SKILL_PATH_BYTES, MAX_SKILLS, Skill, SkillError, SkillFile, SkillProblem,
    check_continue_from, check_skills, skill_document, skill_sha256,
};
use serde_json::json;
use sha2::{Digest, Sha256};

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const ENTRY: &str = "---\nname: demo\ndescription: A demo skill.\n---\nDo the demo.\n";

fn skill(name: &str) -> Skill {
    Skill {
        name: name.to_string(),
        files: vec![SkillFile::new("SKILL.md", ENTRY)],
    }
}

/// A skill whose `SKILL.md` carries the frontmatter name of its own folder (what the validator requires).
fn named(name: &str) -> Skill {
    Skill {
        name: name.to_string(),
        files: vec![SkillFile::new(
            "SKILL.md",
            &format!("---\nname: {name}\ndescription: A demo skill.\n---\nDo the demo.\n"),
        )],
    }
}

fn problem(skills: &[Skill]) -> SkillProblem {
    check_skills(skills).unwrap_err().problem
}

fn code(skills: &[Skill]) -> &'static str {
    problem(skills).code()
}

// ---- SkillFile::new -------------------------------------------------------------------------------------------------------

#[test]
fn a_skill_file_carries_the_size_and_hash_of_its_content() {
    let f = SkillFile::new("scripts/run.py", "print(1)\n");
    assert_eq!(f.path, "scripts/run.py");
    assert_eq!(f.size, 9);
    assert_eq!(f.sha256, hex(b"print(1)\n"));
    assert_eq!(f.content, "print(1)\n");
}

#[test]
fn the_size_counts_bytes_not_characters() {
    assert_eq!(SkillFile::new("SKILL.md", "caf\u{e9}").size, 5);
}

// ---- check_skills: what passes ----------------------------------------------------------------------------------------------

#[test]
fn no_skills_pass() {
    assert!(check_skills(&[]).is_ok());
}

#[test]
fn a_skill_with_a_script_and_a_reference_file_passes() {
    let mut s = named("lifecycle-check");
    s.files.push(SkillFile::new(
        "scripts/lifecycle_check.py",
        "print('ok')\n",
    ));
    s.files
        .push(SkillFile::new("references/notes-1.md", "notes\n"));
    assert_eq!(check_skills(&[s]), Ok(()));
}

#[test]
fn the_limits_themselves_pass() {
    let skills: Vec<Skill> = (0..MAX_SKILLS)
        .map(|i| {
            let mut files = named(&format!("skill-{i}")).files;
            for j in 1..12 {
                let body = "x".repeat(if j == 1 { 64 * 1024 } else { 3 });
                files.push(SkillFile::new(&format!("references/r{j}.md"), &body));
            }
            Skill {
                name: format!("skill-{i}"),
                files,
            }
        })
        .collect();
    assert_eq!(skills.len(), MAX_SKILLS);
    assert_eq!(skills[0].files.len(), 12);
    assert_eq!(check_skills(&skills), Ok(()));
}

#[test]
fn a_name_of_the_longest_length_passes() {
    let name = "a".repeat(MAX_SKILL_NAME_BYTES);
    assert_eq!(check_skills(&[named(&name)]), Ok(()));
}

#[test]
fn an_uppercase_hash_is_refused_at_the_request() {
    // One rule for the three ends: 64 lowercase hex digits. agentd compares the hash exactly, so a request that passed here with an
    // uppercase hash would only fail at run start.
    let mut s = skill("demo");
    s.files[0].sha256 = s.files[0].sha256.to_ascii_uppercase();
    assert_eq!(code(&[s]), "sha_mismatch");
}

#[test]
fn a_non_entry_file_may_be_empty() {
    let mut s = skill("demo");
    s.files.push(SkillFile::new("references/empty.md", ""));
    assert_eq!(check_skills(&[s]), Ok(()));
}

// ---- check_skills: counts and names ---------------------------------------------------------------------------------------

#[test]
fn one_more_than_the_most_skills_is_too_many() {
    let skills: Vec<Skill> = (0..=MAX_SKILLS).map(|i| skill(&format!("s{i}"))).collect();
    assert_eq!(
        problem(&skills),
        SkillProblem::TooManySkills {
            count: MAX_SKILLS + 1,
            max: MAX_SKILLS
        }
    );
}

#[test]
fn bad_names_are_refused() {
    let long = "a".repeat(MAX_SKILL_NAME_BYTES + 1);
    for bad in [
        "",
        "Demo",
        "demo_skill",
        "-demo",
        "demo-",
        "de--mo",
        "de mo",
        "de/mo",
        "..",
        ".",
        "demo.",
        "d\u{e9}mo",
        "demo\n",
        &long,
    ] {
        assert_eq!(code(&[skill(bad)]), "bad_name", "name {bad:?}");
    }
}

#[test]
fn a_digit_first_name_is_a_name() {
    assert_eq!(check_skills(&[named("2fast")]), Ok(()));
}

#[test]
fn two_skills_of_one_name_are_refused() {
    assert_eq!(code(&[skill("demo"), skill("demo")]), "duplicate_skill");
}

#[test]
fn a_skill_with_no_files_is_refused() {
    let s = Skill {
        name: "demo".into(),
        files: vec![],
    };
    assert_eq!(code(&[s]), "no_files");
}

#[test]
fn a_skill_may_hold_any_number_of_files() {
    // The old limit was twelve files in one skill; the request as a whole is held to its own bound instead.
    let mut s = skill("demo");
    for j in 1..=500 {
        s.files
            .push(SkillFile::new(&format!("references/r{j}.md"), "x"));
    }
    assert_eq!(s.files.len(), 501);
    assert_eq!(check_skills(&[s]), Ok(()));
}

// ---- check_skills: paths ------------------------------------------------------------------------------------------------

fn with_path(path: &str) -> Skill {
    let mut s = skill("demo");
    s.files.push(SkillFile::new(path, "x"));
    s
}

#[test]
fn paths_that_escape_or_are_not_plain_are_refused() {
    let long = format!("{}.md", "a".repeat(MAX_SKILL_PATH_BYTES));
    for bad in [
        "",
        "/etc/passwd",
        "../x.md",
        "a/../x.md",
        "a/./x.md",
        "a//x.md",
        "./x.md",
        "a/",
        ".hidden",
        "a/.hidden",
        ".git/config",
        "sp ace.md",
        "a\\b.md",
        "caf\u{e9}.md",
        "x\0y.md",
        "x\ny.md",
        &long,
    ] {
        assert_eq!(code(&[with_path(bad)]), "bad_path", "path {bad:?}");
    }
}

#[test]
fn a_deep_path_passes() {
    // The old limit was four parts; a path is held to its length only.
    assert_eq!(check_skills(&[with_path("a/b/c/d/e/f/g/h.md")]), Ok(()));
}

#[test]
fn two_paths_that_differ_only_in_case_are_refused() {
    let mut s = skill("demo");
    s.files.push(SkillFile::new("references/Notes.md", "x"));
    s.files.push(SkillFile::new("references/notes.md", "x"));
    assert_eq!(code(&[s]), "duplicate_path");
}

#[test]
fn the_same_path_twice_is_refused() {
    let mut s = skill("demo");
    s.files.push(SkillFile::new("SKILL.md", ENTRY));
    assert_eq!(code(&[s]), "duplicate_path");
}

#[test]
fn a_skill_without_skill_md_at_its_top_is_refused() {
    let s = Skill {
        name: "demo".into(),
        files: vec![SkillFile::new("references/SKILL.md", ENTRY)],
    };
    assert_eq!(code(&[s]), "missing_entry");
    let lower = Skill {
        name: "demo".into(),
        files: vec![SkillFile::new("skill.md", ENTRY)],
    };
    assert_eq!(code(&[lower]), "missing_entry");
}

#[test]
fn an_empty_skill_md_is_refused() {
    let s = Skill {
        name: "demo".into(),
        files: vec![SkillFile::new("SKILL.md", "")],
    };
    assert_eq!(code(&[s]), "empty_entry");
}

// ---- check_skills: content ----------------------------------------------------------------------------------------------

#[test]
fn a_file_over_the_old_64_kib_cap_passes() {
    let mut s = skill("demo");
    s.files.push(SkillFile::new(
        "references/big.md",
        &"x".repeat(64 * 1024 + 1),
    ));
    assert_eq!(check_skills(&[s]), Ok(()));
}

#[test]
fn a_size_that_is_not_the_content_length_is_refused() {
    let mut s = skill("demo");
    s.files[0].size += 1;
    assert_eq!(code(&[s]), "size_mismatch");
}

#[test]
fn a_hash_that_is_not_the_content_hash_is_refused() {
    let mut s = skill("demo");
    s.files[0].sha256 = hex(b"something else");
    assert_eq!(code(&[s]), "sha_mismatch");
    let mut short = skill("demo");
    short.files[0].sha256 = "abc".into();
    assert_eq!(code(&[short]), "sha_mismatch");
}

#[test]
fn a_nul_byte_is_refused() {
    let mut s = skill("demo");
    s.files.push(SkillFile::new("scripts/x.sh", "echo a\0b\n"));
    assert_eq!(code(&[s]), "nul");
}

#[test]
fn the_first_problem_wins_and_names_the_skill_and_the_file() {
    let mut bad = named("second");
    bad.files.push(SkillFile::new("references/x.md", ""));
    bad.files[1].size = 7;
    let e: SkillError = check_skills(&[named("first"), bad]).unwrap_err();
    assert_eq!(e.skill.as_deref(), Some("second"));
    assert_eq!(e.file.as_deref(), Some("references/x.md"));
    assert_eq!(e.problem.code(), "size_mismatch");
    let text = e.to_string();
    assert!(
        text.contains("second") && text.contains("references/x.md"),
        "{text}"
    );
}

#[test]
fn a_message_never_quotes_content_and_a_bad_name_is_made_printable() {
    let mut s = skill("demo");
    s.files
        .push(SkillFile::new("references/x.md", "TOP-SECRET-VALUE\0"));
    let text = check_skills(&[s]).unwrap_err().to_string();
    assert!(!text.contains("TOP-SECRET"), "{text}");
    let hostile = Skill {
        name: "bad\u{1b}[31mname\n".into(),
        files: vec![SkillFile::new("SKILL.md", ENTRY)],
    };
    let text = check_skills(&[hostile]).unwrap_err().to_string();
    assert!(!text.contains('\u{1b}') && !text.contains('\n'), "{text:?}");
    let long = skill(&"a-".repeat(80));
    let text = check_skills(&[long]).unwrap_err().to_string();
    assert!(text.len() < 400, "{} bytes", text.len());
}

#[test]
fn every_problem_has_its_own_stable_code() {
    let all = [
        (
            SkillProblem::TooManySkills { count: 33, max: 32 },
            "too_many_skills",
        ),
        (SkillProblem::BadName, "bad_name"),
        (SkillProblem::DuplicateSkill, "duplicate_skill"),
        (SkillProblem::NoFiles, "no_files"),
        (SkillProblem::BadPath, "bad_path"),
        (SkillProblem::DuplicatePath, "duplicate_path"),
        (SkillProblem::MissingEntry, "missing_entry"),
        (SkillProblem::EmptyEntry, "empty_entry"),
        (
            SkillProblem::TooLarge {
                bytes: 70_000,
                max: 65_536,
            },
            "too_large",
        ),
        (SkillProblem::SizeMismatch, "size_mismatch"),
        (SkillProblem::ShaMismatch, "sha_mismatch"),
        (SkillProblem::Nul, "nul"),
        (SkillProblem::PathConflict, "path_conflict"),
        (
            SkillProblem::TooLargeTotal {
                bytes: 3_000_000,
                max: 2_097_152,
            },
            "too_large_total",
        ),
        (SkillProblem::BadEntry("x"), "bad_entry"),
        (SkillProblem::NotRegular, "not_regular"),
        (SkillProblem::Shadowed, "shadowed"),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (p, want) in all {
        assert_eq!(p.code(), want);
        assert!(seen.insert(want), "{want} is used twice");
    }
}

// ---- check_skills: a path cannot be a file and a folder -------------------------------------------------------------------------

#[test]
fn a_path_that_is_both_a_file_and_a_folder_is_refused_at_the_request() {
    // SKILL.md as a file and SKILL.md/x below it cannot both be written on a disk (ENOTDIR): the request is refused with a 400's words,
    // not with a failure inside the run after it was billed.
    let mut s = skill("demo");
    s.files.push(SkillFile::new("SKILL.md/x", "x"));
    let e = check_skills(&[s]).unwrap_err();
    assert_eq!(e.problem.code(), "path_conflict", "{e}");
    assert_eq!(e.skill.as_deref(), Some("demo"));
    assert_eq!(
        e.file.as_deref(),
        Some("SKILL.md/x"),
        "names the path that cannot be written"
    );
    // The same with another file, in either order, below the top, and compared without case.
    for (a, b) in [
        ("references", "references/a.md"),
        ("references/a.md", "references"),
        ("references/a", "references/a/b.md"),
        ("References", "references/a.md"),
    ] {
        let mut s = skill("demo");
        s.files.push(SkillFile::new(a, "x"));
        s.files.push(SkillFile::new(b, "x"));
        assert_eq!(code(&[s]), "path_conflict", "{a} and {b}");
    }
}

#[test]
fn paths_that_only_share_a_leading_text_are_not_a_conflict() {
    let mut s = skill("demo");
    s.files.push(SkillFile::new("ref", "x"));
    s.files.push(SkillFile::new("references/a.md", "x"));
    s.files.push(SkillFile::new("references/a.md.bak", "x"));
    s.files.push(SkillFile::new("references2/a.md", "x"));
    assert_eq!(check_skills(&[s]), Ok(()));
}

// ---- SKILL.md: the documented subset of the frontmatter -------------------------------------------------------------------------

fn entry(name: &str, front_rest: &str, body: &str) -> String {
    format!("---\nname: {name}\n{front_rest}---\n{body}")
}

fn with_entry(text: &str) -> Skill {
    Skill {
        name: "demo".into(),
        files: vec![SkillFile::new("SKILL.md", text)],
    }
}

#[test]
fn the_documented_forms_of_name_and_description_are_read() {
    // (what, SKILL.md text, the description read)
    let cases: [(&str, String, &str); 12] = [
        ("plain", entry("demo", "description: Use when asked.\n", "B.\n"), "Use when asked."),
        ("crlf", "---\r\nname: demo\r\ndescription: Crlf.\r\n---\r\nB.\r\n".to_string(), "Crlf."),
        ("double quoted", entry("demo", "description: \"Has: colon and #hash\"\n", "B.\n"), "Has: colon and #hash"),
        ("single quoted", entry("demo", "description: 'It''s: fine'\n", "B.\n"), "It's: fine"),
        ("quoted name", "---\nname: \"demo\"\ndescription: x\n---\nB.\n".to_string(), "x"),
        ("comment after a plain value", "---\nname: demo # the folder\ndescription: x # why\n---\nB.\n".to_string(), "x"),
        ("comment after a quoted value", entry("demo", "description: \"x\" # why\n", "B.\n"), "x"),
        ("folded", entry("demo", "description: >-\n  Folded text over\n  two lines.\n", "B.\n"), "Folded text over two lines."),
        ("literal", entry("demo", "description: |\n  Two\n  lines\n", "B.\n"), "Two lines"),
        ("text on the next indented line", entry("demo", "description:\n  On the next line.\n", "B.\n"), "On the next line."),
        ("plain over two lines", entry("demo", "description: First half\n  second half\n", "B.\n"), "First half second half"),
        (
            "other keys, comments and a nested map are passed over",
            "---\n# a comment\nlicense: MIT\nmetadata:\n  author: me\n  tags: [a, b]\n\nname: demo\ndescription: ok\n---\nB.\n".to_string(),
            "ok",
        ),
    ];
    for (what, text, want) in cases {
        let d = skill_document(&text, "demo").unwrap_or_else(|e| panic!("{what}: {e}"));
        assert_eq!(
            (d.name.as_str(), d.description.as_str()),
            ("demo", want),
            "{what}"
        );
        assert_eq!(
            check_skills(&[with_entry(&text)]),
            Ok(()),
            "{what}: the validator accepts what the reader reads"
        );
    }
}

#[test]
fn the_body_is_trimmed_at_both_ends_and_keeps_its_line_endings_and_rules() {
    let text =
        "---\r\nname: demo\r\ndescription: x\r\n---\r\n\r\n# T\r\n\r\n---\r\nafter a rule\r\n\r\n";
    assert_eq!(
        skill_document(text, "demo").unwrap().body,
        "# T\r\n\r\n---\r\nafter a rule"
    );
}

#[test]
fn a_skill_md_outside_the_subset_is_refused_at_the_request_with_its_own_words() {
    // (what, SKILL.md text, a word the reason holds)
    let cases: [(&str, String, &str); 17] = [
        (
            "no frontmatter",
            "No frontmatter body.\n".to_string(),
            "frontmatter",
        ),
        (
            "unclosed",
            "---\nname: demo\ndescription: x\nBody.\n".to_string(),
            "frontmatter",
        ),
        (
            "name differs",
            entry("other", "description: x\n", "B.\n"),
            "frontmatter name",
        ),
        (
            "no name",
            "---\ndescription: x\n---\nB.\n".to_string(),
            "frontmatter name",
        ),
        (
            "name in a block",
            "---\nname: >-\n  demo\ndescription: x\n---\nB.\n".to_string(),
            "name",
        ),
        (
            "no description",
            "---\nname: demo\n---\nB.\n".to_string(),
            "description",
        ),
        (
            "empty description",
            entry("demo", "description:\n", "B.\n"),
            "description",
        ),
        (
            "a comment is no description",
            entry("demo", "description: # none\n", "B.\n"),
            "description",
        ),
        (
            "empty body",
            entry("demo", "description: x\n", "\n  \n"),
            "body",
        ),
        (
            "a flow list",
            entry("demo", "description: [a, b]\n", "B.\n"),
            "quote",
        ),
        (
            "a colon inside a plain value",
            entry("demo", "description: Use when: asked\n", "B.\n"),
            "quote",
        ),
        (
            "an anchor",
            entry("demo", "description: &a x\n", "B.\n"),
            "quote",
        ),
        (
            "a tag",
            entry("demo", "description: !!str x\n", "B.\n"),
            "quote",
        ),
        (
            "an escape in a double-quoted value",
            entry("demo", "description: \"a\\nb\"\n", "B.\n"),
            "quote",
        ),
        (
            "a double-quoted value that does not end",
            entry("demo", "description: \"abc\n", "B.\n"),
            "quote",
        ),
        (
            "name twice",
            "---\nname: demo\nname: demo\ndescription: x\n---\nB.\n".to_string(),
            "twice",
        ),
        (
            "a nested map where the description belongs",
            entry("demo", "description:\n  en: x\n", "B.\n"),
            "quote",
        ),
    ];
    for (what, text, word) in cases {
        let r = skill_document(&text, "demo");
        let reason = r.unwrap_err();
        assert!(reason.contains(word), "{what}: {reason}");
        let e = check_skills(&[with_entry(&text)]).unwrap_err();
        assert_eq!(e.problem.code(), "bad_entry", "{what}: {e}");
        assert_eq!(
            (e.skill.as_deref(), e.file.as_deref()),
            (Some("demo"), Some("SKILL.md")),
            "{what}"
        );
        assert!(e.to_string().contains(word), "{what}: {e}");
    }
}

#[test]
fn an_empty_skill_md_is_still_empty_entry_and_the_reader_never_quotes_the_text() {
    assert_eq!(code(&[with_entry("")]), "empty_entry");
    let secret = entry("demo", "description: TOP-SECRET-VALUE: x\n", "B.\n");
    let e = check_skills(&[with_entry(&secret)])
        .unwrap_err()
        .to_string();
    assert!(!e.contains("TOP-SECRET"), "{e}");
}

// ---- skill_sha256 -------------------------------------------------------------------------------------------------------

#[test]
fn the_skill_digest_is_over_the_sorted_path_and_hash_lines() {
    let a = SkillFile::new("SKILL.md", ENTRY);
    let b = SkillFile::new("scripts/x.py", "print(1)\n");
    let want = hex(format!("SKILL.md\t{}\nscripts/x.py\t{}\n", a.sha256, b.sha256).as_bytes());
    let s = Skill {
        name: "demo".into(),
        files: vec![b.clone(), a.clone()],
    };
    assert_eq!(skill_sha256(&s), want);
    let sorted = Skill {
        name: "demo".into(),
        files: vec![a, b],
    };
    assert_eq!(
        skill_sha256(&sorted),
        want,
        "the order of the files does not matter"
    );
}

#[test]
fn the_skill_digest_changes_with_content_and_path_but_not_with_the_name() {
    let base = skill("demo");
    let mut changed = skill("demo");
    changed.files[0] = SkillFile::new(
        "SKILL.md",
        "---\nname: demo\ndescription: Changed.\n---\nx\n",
    );
    let renamed_file = Skill {
        name: "demo".into(),
        files: vec![SkillFile::new("skill-two.md", ENTRY)],
    };
    assert_ne!(skill_sha256(&base), skill_sha256(&changed));
    assert_ne!(skill_sha256(&base), skill_sha256(&renamed_file));
    assert_eq!(skill_sha256(&base), skill_sha256(&skill("other")));
    assert_eq!(skill_sha256(&base).len(), 64);
}

// ---- continue_from ------------------------------------------------------------------------------------------------------

const RUN: &str = "01M50E3V5XKE4WYKXZ2CZFX688";
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn cf(run: &str, head: Option<&str>) -> ContinueFrom {
    ContinueFrom {
        run_id: run.into(),
        head_sha: head.map(str::to_string),
    }
}

#[test]
fn a_run_id_alone_or_with_a_full_head_passes() {
    assert_eq!(check_continue_from(&cf(RUN, None)), Ok(()));
    assert_eq!(check_continue_from(&cf(RUN, Some(HEAD))), Ok(()));
}

#[test]
fn a_bad_run_id_is_refused() {
    let short = &RUN[..25];
    let long = format!("{RUN}8");
    for bad in [
        "",
        "short",
        short,
        long.as_str(),
        "01M50E3V5XKE4WYKXZ2CZFX68!",
        "01M50E3V5XKE4WYKXZ2CZFX6/8",
        "harness/branch-name-123456",
    ] {
        assert_eq!(
            check_continue_from(&cf(bad, None)),
            Err(ContinueFromError::BadRunId),
            "run id {bad:?}"
        );
    }
}

#[test]
fn a_bad_head_is_refused() {
    for bad in [
        "",
        "abc",
        &HEAD[..39],
        "0123456789ABCDEF0123456789abcdef01234567",
        "g123456789abcdef0123456789abcdef01234567",
        "0123456789abcdef0123456789abcdef012345678",
    ] {
        assert_eq!(
            check_continue_from(&cf(RUN, Some(bad))),
            Err(ContinueFromError::BadHeadSha),
            "head {bad:?}"
        );
    }
}

// ---- the request members on the wire --------------------------------------------------------------------------------------

fn base() -> serde_json::Value {
    json!({
        "prompt": "do it",
        "target": {"mode": "s3-only"},
        "model": "us.anthropic.claude-sonnet-5-5",
        "rules": []
    })
}

#[test]
fn a_request_without_the_members_leaves_them_out_when_written_back() {
    let r: StartDirectRunRequest = serde_json::from_value(base()).unwrap();
    assert!(r.skills.is_empty() && r.continue_from.is_none());
    let back = serde_json::to_value(&r).unwrap();
    assert!(
        back.get("skills").is_none() && back.get("continueFrom").is_none(),
        "{back}"
    );
}

#[test]
fn a_request_with_skills_and_continue_from_round_trips() {
    let mut v = base();
    v["skills"] = serde_json::to_value(vec![skill("demo")]).unwrap();
    v["continueFrom"] = json!({"runId": RUN, "headSha": HEAD});
    let r: StartDirectRunRequest = serde_json::from_value(v.clone()).unwrap();
    assert_eq!(r.skills, vec![skill("demo")]);
    assert_eq!(r.continue_from, Some(cf(RUN, Some(HEAD))));
    assert_eq!(serde_json::to_value(&r).unwrap(), v);
    assert_eq!(v["skills"][0]["files"][0]["size"], json!(ENTRY.len()));
}

#[test]
fn the_new_types_refuse_unknown_members() {
    let mut v = base();
    v["continueFrom"] = json!({"runId": RUN, "base": "main"});
    assert!(serde_json::from_value::<StartDirectRunRequest>(v).is_err());
    let mut v = base();
    v["skills"] = json!([{"name": "demo", "files": [], "extra": 1}]);
    assert!(serde_json::from_value::<StartDirectRunRequest>(v).is_err());
    let mut v = base();
    v["skills"] = json!([{"name": "demo", "files": [{"path": "SKILL.md", "sha256": "", "size": 0, "content": "", "mode": 493}]}]);
    assert!(serde_json::from_value::<StartDirectRunRequest>(v).is_err());
}

#[test]
fn a_description_may_hold_1024_characters_and_not_one_more() {
    assert_eq!(MAX_SKILL_DESCRIPTION_CHARS, 1024);
    // The description is listed in the system prompt of every turn; the limit is on characters, not bytes.
    for d in ["d".repeat(1024), "\u{e9}".repeat(1024)] {
        let text = entry("demo", &format!("description: {d}\n"), "B.\n");
        assert_eq!(
            skill_document(&text, "demo")
                .unwrap()
                .description
                .chars()
                .count(),
            1024
        );
        assert_eq!(check_skills(&[with_entry(&text)]), Ok(()));
    }
    let long = entry(
        "demo",
        &format!("description: {}\n", "d".repeat(1025)),
        "B.\n",
    );
    let reason = skill_document(&long, "demo").unwrap_err();
    assert!(reason.contains("1024"), "{reason}");
    let e = check_skills(&[with_entry(&long)]).unwrap_err();
    assert_eq!(
        (e.problem.code(), e.file.as_deref()),
        ("bad_entry", Some("SKILL.md")),
        "{e}"
    );
    assert!(
        !e.to_string().contains(&"d".repeat(40)),
        "the text is not quoted"
    );
}
