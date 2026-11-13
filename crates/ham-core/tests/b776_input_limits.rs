//! The input caps of the client are gone. A rule file, `continue.md`, `review.md`, an attachment, a skill file and an answer may be as large as
//! a request can carry; a file that alone could never be sent is refused before it is read, with the platform bound named. Each test here fails on
//! the old caps (a rule file 64 KiB, 32 rule files, `continue.md` 8 KiB, `review.md` 8 KiB, `max_nudges` 100, an attachment 256 KiB, 32
//! attachments, a skill file 64 KiB, 12 files in a skill, an answer 8,000 characters).

#![allow(clippy::unwrap_used)]

use ham_core::attachments::{Why as AttachWhy, read_attachments};
use ham_core::input::check_text;
use ham_core::rules::{self, Problem};
use ham_core::skills::{Why as SkillWhy, read_skills_path};
use harness_protocol::api::v1::{ContinueSettings, RuleFile};
use harness_protocol::direct::{
    AttachmentProblem, MAX_INPUT_TEXT_BYTES, MAX_REQUEST_BYTES, MAX_RULE_FILES,
    MAX_RULE_NAME_BYTES, SkillProblem,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn file(name: &str, content: &str) -> RuleFile {
    rules::rule_file(name, content)
}

/// A scratch folder under the temp dir (set by the project's env.sh).
fn tmp() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ham-b776-{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, bytes: &[u8]) -> String {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, bytes).unwrap();
    p.display().to_string()
}

/// A file of `len` bytes that holds only holes: the disk is not used, and a reader that reads it gets NUL bytes.
fn sparse(root: &Path, rel: &str, len: u64) -> String {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::File::create(&p).unwrap().set_len(len).unwrap();
    p.display().to_string()
}

// ---- rules ------------------------------------------------------------------------------------------------------------------

#[test]
fn b776_a_rule_file_has_no_size_of_its_own() {
    for n in [64 * 1024 + 1, 1024 * 1024, 5 * 1024 * 1024] {
        rules::validate_rule("big.md", &"x".repeat(n)).unwrap();
    }
}

#[test]
fn b776_a_rule_set_takes_the_number_of_files_the_summary_can_list() {
    assert_eq!(rules::MAX_RULE_FILES, MAX_RULE_FILES);
    assert_eq!(MAX_RULE_FILES, 100);
    let many: Vec<RuleFile> = (0..=MAX_RULE_FILES)
        .map(|i| file(&format!("r{i:03}.md"), "Rule.\n"))
        .collect();
    rules::validate_rule_files(&many[..MAX_RULE_FILES]).unwrap();
    assert_eq!(
        rules::validate_rule_files(&many).unwrap_err().problem,
        Problem::TooMany {
            count: MAX_RULE_FILES + 1,
            max: MAX_RULE_FILES
        }
    );
}

#[test]
fn b776_a_rule_name_takes_255_bytes() {
    assert_eq!(rules::MAX_NAME_BYTES, MAX_RULE_NAME_BYTES);
    let ok = format!("{}.md", "a".repeat(252));
    assert_eq!(ok.len(), 255);
    rules::validate_rule(&ok, "Rule.\n").unwrap();
    let bad = format!("{}.md", "a".repeat(253));
    assert_eq!(
        rules::validate_rule(&bad, "Rule.\n").unwrap_err().problem,
        Problem::BadName
    );
}

// ---- continue.md and review.md ---------------------------------------------------------------------------------------------

#[test]
fn b776_continue_text_has_no_size_of_its_own() {
    for n in [8 * 1024 + 1, 64 * 1024 + 1, 1024 * 1024] {
        let text = format!("---\nmax_nudges: 3\n---\n{}", "x".repeat(n));
        let c = rules::parse_continue(&text).unwrap();
        assert_eq!((c.max_nudges, c.text.len()), (3, n));
        rules::validate_continue(&c).unwrap();
    }
}

#[test]
fn b776_the_nudge_count_has_no_ceiling_below_what_the_wire_holds() {
    let c = rules::parse_continue("---\nmax_nudges: 1000\n---\nGo on.\n").unwrap();
    assert_eq!(c.max_nudges, 1000);
    let mut c = ContinueSettings {
        text: "Go on.\n".into(),
        sha256: sha("Go on.\n"),
        max_nudges: 100_000,
    };
    rules::validate_continue(&c).unwrap();
    c.max_nudges = u32::MAX;
    rules::validate_continue(&c).unwrap();
    for (text, want) in [
        ("---\nmax_nudges: many\n---\nGo.\n", "whole number"),
        ("---\nmax_nudges: -1\n---\nGo.\n", "whole number"),
        ("---\nmax_nudges: 2.5\n---\nGo.\n", "whole number"),
        ("---\nmax_nudges: 4294967296\n---\nGo.\n", "whole number"),
    ] {
        let e = rules::parse_continue(text).expect_err(text);
        assert!(e.to_string().contains(want), "{text:?}: {e}");
        assert!(!e.to_string().contains("highest allowed"), "{e}");
    }
}

#[test]
fn b776_review_text_has_no_size_of_its_own() {
    for n in [8 * 1024 + 1, 200 * 1024] {
        let text = format!("---\nguard_minutes: 30\n---\n{}", "r".repeat(n));
        let o = rules::parse_review(&text).unwrap();
        assert_eq!(o.text.as_ref().unwrap().len(), n);
        rules::validate_review(&o).unwrap();
    }
}

// ---- answers and messages ----------------------------------------------------------------------------------------------------

#[test]
fn b776_an_answer_takes_256_kib_and_no_character_count() {
    check_text(&"a".repeat(20_000)).unwrap();
    check_text(&"a".repeat(MAX_INPUT_TEXT_BYTES)).unwrap();
    let why = check_text(&"a".repeat(MAX_INPUT_TEXT_BYTES + 1)).unwrap_err();
    assert!(
        why.contains("262,144") && !why.contains("characters"),
        "{why}"
    );
    // Four-byte characters count by their bytes: 65,536 of them are exactly the limit.
    check_text(&"\u{1f600}".repeat(65_536)).unwrap();
    check_text(&"\u{1f600}".repeat(65_537)).unwrap_err();
}

// ---- attachments -----------------------------------------------------------------------------------------------------------

#[test]
fn b776_attachments_have_no_count_and_no_size_of_their_own() {
    let t = tmp();
    let big = write(&t, "big.txt", "a".repeat(1024 * 1024).as_bytes());
    assert_eq!(read_attachments(&[big]).unwrap()[0].size, 1024 * 1024);
    let many: Vec<String> = (0..40)
        .map(|i| write(&t, &format!("f{i}.txt"), "a".repeat(100 * 1024).as_bytes()))
        .collect();
    assert_eq!(read_attachments(&many).unwrap().len(), 40);
}

#[test]
fn b776_a_file_that_alone_could_never_be_sent_is_refused_before_it_is_read() {
    let t = tmp();
    // Holes only: a reader that read the file would find NUL bytes, so the size is what refuses it.
    let huge = sparse(&t, "huge.txt", MAX_REQUEST_BYTES as u64 + 1);
    let e = read_attachments(&[huge]).unwrap_err();
    assert_eq!(
        e.why,
        AttachWhy::Invalid(AttachmentProblem::TooLarge {
            bytes: MAX_REQUEST_BYTES + 1,
            max: MAX_REQUEST_BYTES
        })
    );
    let words = e.to_string();
    for want in [
        "huge.txt",
        "6,225,920 bytes",
        "6 MiB",
        "AWS Lambda",
        "never be sent",
    ] {
        assert!(words.contains(want), "{want} missing from: {words}");
    }
}

#[test]
fn b776_files_named_together_that_could_never_be_sent_are_refused_before_the_last_is_read() {
    let t = tmp();
    let first = write(&t, "first.txt", "a".repeat(4 * 1024 * 1024).as_bytes());
    let second = sparse(&t, "second.txt", 4 * 1024 * 1024);
    let e = read_attachments(&[first, second]).unwrap_err();
    assert_eq!(
        e.why,
        AttachWhy::Invalid(AttachmentProblem::TooLargeTotal {
            bytes: 8 * 1024 * 1024,
            max: MAX_REQUEST_BYTES
        })
    );
}

// ---- skills ------------------------------------------------------------------------------------------------------------------

fn skill_md(name: &str) -> String {
    format!("---\nname: {name}\ndescription: A demo skill.\n---\nDo the demo.\n")
}

#[test]
fn b776_a_skill_may_hold_many_files_of_any_size_in_deep_folders() {
    let t = tmp();
    let root = t.join("skills");
    write(&root, "demo/SKILL.md", skill_md("demo").as_bytes());
    for i in 0..30 {
        write(
            &root,
            &format!("demo/references/r{i}.md"),
            "x".repeat(100 * 1024).as_bytes(),
        );
    }
    write(&root, "demo/a/b/c/d/e/f/deep.md", b"deep\n");
    let skills = read_skills_path(&root).unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].files.len(), 32);
}

#[test]
fn b776_a_skill_file_that_alone_could_never_be_sent_is_refused_before_it_is_read() {
    let t = tmp();
    let root = t.join("skills");
    write(&root, "demo/SKILL.md", skill_md("demo").as_bytes());
    sparse(
        &root,
        "demo/references/huge.md",
        MAX_REQUEST_BYTES as u64 + 1,
    );
    let e = read_skills_path(&root).unwrap_err();
    assert_eq!(
        e.why,
        SkillWhy::Invalid(SkillProblem::TooLarge {
            bytes: MAX_REQUEST_BYTES + 1,
            max: MAX_REQUEST_BYTES
        })
    );
    assert_eq!(e.file.as_deref(), Some("references/huge.md"));
}

#[test]
fn b776_skills_that_together_could_never_be_sent_are_refused_before_the_last_file_is_read() {
    let t = tmp();
    let root = t.join("skills");
    write(&root, "alpha/SKILL.md", skill_md("alpha").as_bytes());
    sparse(&root, "alpha/references/a.md", 4 * 1024 * 1024);
    write(&root, "beta/SKILL.md", skill_md("beta").as_bytes());
    sparse(&root, "beta/references/b.md", 4 * 1024 * 1024);
    let e = read_skills_path(&root).unwrap_err();
    assert!(
        matches!(&e.why, SkillWhy::Invalid(SkillProblem::TooLargeTotal { max, .. }) if *max == MAX_REQUEST_BYTES),
        "{:?}",
        e.why
    );
    assert_eq!(e.skill.as_deref(), Some("beta"));
    assert_eq!(e.file.as_deref(), Some("references/b.md"));
}

#[test]
fn b776_a_path_longer_than_255_bytes_is_refused_before_the_folder_is_walked() {
    let t = tmp();
    let root = t.join("skills");
    write(&root, "demo/SKILL.md", skill_md("demo").as_bytes());
    let deep = format!("{}/{}/f.md", "d".repeat(200), "e".repeat(100));
    write(&root, &format!("demo/{deep}"), b"x\n");
    let e = read_skills_path(&root).unwrap_err();
    assert_eq!(e.why, SkillWhy::Invalid(SkillProblem::BadPath));
    assert!(
        e.file.as_deref().is_some_and(|f| f.len() > 255),
        "{:?}",
        e.file
    );
}
