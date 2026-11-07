// COMPAT-CAH-DATA: this file names the earlier records folder and the earlier variable spellings, which are read until VERSION 6.00.
//! Where the lifecycle pack's records of a run are. The repository may hold the file `.ham/records`: its first line is `off` or a
//! folder, relative to the repository, that takes the place of `.ham`;
//! a missing file leaves the default places, in `.ham`. One reader serves the
//! clean-checkout report, the observation of the run log and the progress fingerprint of the goal loop, so the three agree with the pack
//! and with the self-check script. The file is the user's: it is read without following a link and holds one short value.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::records::{EARLIER_BASE, LOCATOR, Records, choose};
use common::scratch;

fn moved(dir: &str) -> Records {
    Records::Moved(dir.to_string())
}

/// The cases the self-check script reads too (`scripts/tests/test_lifecycle_check.py`): one file, so the two readers cannot drift.
fn shared_cases() -> Vec<(String, String, Option<String>)> {
    let path = common::repo_root().join("fixtures/lifecycle/records-locator.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    v["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["text"].as_str().unwrap().to_string(),
                c["kind"].as_str().unwrap().to_string(),
                c["dir"].as_str().map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn the_reader_gives_the_value_of_every_shared_case() {
    let cases = shared_cases();
    assert!(cases.len() >= 50, "{}", cases.len());
    for kind in ["default", "off", "moved", "invalid"] {
        assert!(cases.iter().any(|c| c.1 == kind), "no case of kind {kind}");
    }
    for (text, kind, dir) in cases {
        let got = Records::parse(&text);
        let want = match kind.as_str() {
            "default" => Records::Default,
            "off" => Records::Off,
            "moved" => moved(dir.as_deref().unwrap()),
            "invalid" => {
                assert!(
                    matches!(got, Records::Invalid(ref why) if !why.is_empty()),
                    "{text:?} -> {got:?}"
                );
                continue;
            }
            other => panic!("unknown kind {other}"),
        };
        assert_eq!(got, want, "{text:?}");
    }
}

#[test]
fn only_a_space_a_tab_and_a_carriage_return_are_trimmed_from_a_line() {
    // `str::trim` also strips U+00A0, U+0085, U+2028, U+3000 and the other Unicode white space; the contract's script trims a space, a tab and a
    // carriage return only, so the two readers would name different places for the same file.
    for text in [
        "\u{a0}.ham\n",
        ".ham\u{a0}\n",
        "\u{b}.ham\n",
        "\u{c}.ham\n",
        "\u{85}.ham\n",
        "\u{2028}.ham\n",
        "\u{3000}off\n",
        "\u{a0}# note\n.ham\n",
        "# note\n\u{a0}\n.ham\n",
    ] {
        let got = Records::parse(text);
        assert!(matches!(got, Records::Invalid(_)), "{text:?} -> {got:?}");
    }
    assert_eq!(Records::parse(" \t.ham \r\n"), moved(".ham"));
    assert_eq!(Records::parse("\t# note\r\n\toff\n"), Records::Off);
}

#[test]
fn the_places_of_the_default_and_of_a_moved_folder() {
    // Without a locator the records sit below the hidden folder `.ham`, where no docs check of a repository looks
    let d = Records::Default;
    assert_eq!(d.goal().as_deref(), Some(".ham/lifecycle/GOAL.md"));
    assert_eq!(d.log().as_deref(), Some(".ham/lifecycle/LOG.md"));
    assert_eq!(d.red_dir().as_deref(), Some(".ham/lifecycle/red"));
    assert_eq!(d.research_dir().as_deref(), Some(".ham/research"));
    assert_eq!(d.context_index().as_deref(), Some(".ham/context/INDEX.md"));
    assert_eq!(d.progress_excludes(), vec![".ham/lifecycle".to_string()]);
    // a folder takes the place of .ham: the value docs is the layout of
    let m = moved("docs");
    assert_eq!(m.goal().as_deref(), Some("docs/lifecycle/GOAL.md"));
    assert_eq!(m.log().as_deref(), Some("docs/lifecycle/LOG.md"));
    assert_eq!(m.red_dir().as_deref(), Some("docs/lifecycle/red"));
    assert_eq!(m.research_dir().as_deref(), Some("docs/research"));
    assert_eq!(m.context_index().as_deref(), Some("docs/context/INDEX.md"));
    assert_eq!(m.progress_excludes(), vec!["docs/lifecycle".to_string()]);
    let o = Records::Off;
    assert_eq!(
        (
            o.goal(),
            o.log(),
            o.red_dir(),
            o.research_dir(),
            o.context_index()
        ),
        (None, None, None, None, None)
    );
    assert_eq!(o.progress_excludes(), Vec::<String>::new());
}

#[test]
fn the_default_is_the_places_of_the_value_dot_ham_and_the_fixture_names_the_base() {
    // `.ham/records` with the value `.ham` says what a missing file says: one grammar, one set of places, in Rust, in the script and in the scorer.
    let path = common::repo_root().join("fixtures/lifecycle/records-locator.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let base = v["defaultBase"].as_str().unwrap();
    assert_eq!(Records::Default.base(), Some(base));
    let same = moved(base);
    let d = Records::Default;
    assert_eq!(
        (
            d.goal(),
            d.log(),
            d.red_dir(),
            d.research_dir(),
            d.context_index()
        ),
        (
            same.goal(),
            same.log(),
            same.red_dir(),
            same.research_dir(),
            same.context_index()
        )
    );
    assert_ne!(Records::parse("docs\n").log(), Records::Default.log());
    assert_eq!(
        Records::parse("docs\n").log().as_deref(),
        Some("docs/lifecycle/LOG.md")
    );
}

#[test]
fn an_invalid_value_reads_the_default_places_and_says_why() {
    let bad = Records::parse("../x\n");
    assert_eq!(bad.log().as_deref(), Some(".ham/lifecycle/LOG.md"));
    assert!(bad.problem().is_some_and(|p| !p.is_empty()));
    assert_eq!(Records::Default.problem(), None);
    assert_eq!(moved("x").problem(), None);
    assert_eq!(Records::Off.problem(), None);
}

#[test]
fn the_locator_is_read_from_the_work_tree_without_following_a_link() {
    assert_eq!(LOCATOR, ".ham/records");
    // no file
    let d = scratch("records-none");
    assert_eq!(Records::read(&d), Records::Default);
    // a plain file
    let d = scratch("records-file");
    std::fs::create_dir_all(d.join(".ham")).unwrap();
    std::fs::write(d.join(LOCATOR), ".ham/lifecycle\n").unwrap();
    assert_eq!(Records::read(&d), moved(".ham/lifecycle"));
    std::fs::write(d.join(LOCATOR), "off\n").unwrap();
    assert_eq!(Records::read(&d), Records::Off);
    // the file is a link
    let outside = scratch("records-outside");
    std::fs::write(outside.join("elsewhere"), "off\n").unwrap();
    let d = scratch("records-link");
    std::fs::create_dir_all(d.join(".ham")).unwrap();
    std::os::unix::fs::symlink(outside.join("elsewhere"), d.join(LOCATOR)).unwrap();
    assert_eq!(Records::read(&d), Records::Default);
    // the folder .ham is a link
    let d = scratch("records-dir-link");
    std::fs::create_dir_all(outside.join("ham")).unwrap();
    std::fs::write(outside.join("ham/records"), "off\n").unwrap();
    std::os::unix::fs::symlink(outside.join("ham"), d.join(".ham")).unwrap();
    assert_eq!(Records::read(&d), Records::Default);
    // a directory where the file belongs
    let d = scratch("records-dir");
    std::fs::create_dir_all(d.join(LOCATOR)).unwrap();
    assert_eq!(Records::read(&d), Records::Default);
}

#[test]
fn a_locator_that_is_longer_than_a_short_value_needs_is_invalid_and_not_read_whole() {
    let d = scratch("records-big");
    std::fs::create_dir_all(d.join(".ham")).unwrap();
    std::fs::write(d.join(LOCATOR), format!("# {}\noff\n", "x".repeat(8000))).unwrap();
    assert!(
        matches!(Records::read(&d), Records::Invalid(_)),
        "{:?}",
        Records::read(&d)
    );
}

#[test]
fn a_locator_over_the_read_limit_of_the_file_reader_is_invalid_and_not_the_default() {
    // `read_regular_nofollow` gives up on a file over 1 MiB and the script answers "invalid" for any file over 4,096 bytes, so a file of 2 MiB
    // must be invalid here too and not the default places.
    let d = scratch("records-huge");
    std::fs::create_dir_all(d.join(".ham")).unwrap();
    let file = std::fs::File::create(d.join(LOCATOR)).unwrap();
    file.set_len(2 << 20).unwrap();
    drop(file);
    let got = Records::read(&d);
    assert!(
        matches!(got, Records::Invalid(ref why) if why.contains("4096")),
        "{got:?}"
    );
    // a link of that size is still no locator
    let outside = scratch("records-huge-outside");
    let target = std::fs::File::create(outside.join("big")).unwrap();
    target.set_len(2 << 20).unwrap();
    drop(target);
    let d = scratch("records-huge-link");
    std::fs::create_dir_all(d.join(".ham")).unwrap();
    std::os::unix::fs::symlink(outside.join("big"), d.join(LOCATOR)).unwrap();
    assert_eq!(Records::read(&d), Records::Default);
}

// ---- the records folder of the earlier name: one folder per run -----------------------------------------

/// Writes `rel` below `tree` with `text`, making the folders.
fn put(tree: &std::path::Path, rel: &str, text: &str) {
    let path = tree.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn a_repository_that_holds_state_only_in_the_earlier_folder_uses_it() {
    assert_eq!(EARLIER_BASE, ".cah");
    for state in [
        ".cah/lifecycle/LOG.md",
        ".cah/records",
        ".cah/test-command",
        ".cah/research/note.md",
        ".cah/context/INDEX.md",
    ] {
        let d = scratch("records-earlier");
        put(
            &d,
            state,
            if state.ends_with("records") {
                ".cah\n"
            } else {
                "x\n"
            },
        );
        let got = Records::read(&d);
        assert_eq!(got, moved(".cah"), "{state}: {got:?}");
        assert_eq!(
            got.log().as_deref(),
            Some(".cah/lifecycle/LOG.md"),
            "{state}"
        );
        assert_eq!(
            got.goal().as_deref(),
            Some(".cah/lifecycle/GOAL.md"),
            "{state}"
        );
        assert_eq!(
            got.red_dir().as_deref(),
            Some(".cah/lifecycle/red"),
            "{state}"
        );
        assert_eq!(
            got.research_dir().as_deref(),
            Some(".cah/research"),
            "{state}"
        );
        assert_eq!(
            got.context_index().as_deref(),
            Some(".cah/context/INDEX.md"),
            "{state}"
        );
        assert_eq!(got.progress_excludes(), vec![".cah/lifecycle".to_string()]);
        assert_eq!(got.conflict(), None);
    }
}

#[test]
fn a_repository_with_neither_folder_or_only_the_new_one_uses_the_new_one() {
    let d = scratch("records-neither");
    assert_eq!(Records::read(&d), Records::Default);
    let d = scratch("records-ham-only");
    put(&d, ".ham/lifecycle/LOG.md", "x\n");
    assert_eq!(Records::read(&d), Records::Default);
    // The folder name alone is no state: an empty folder, or one that holds only names the records do not use.
    let d = scratch("records-cah-empty");
    std::fs::create_dir_all(d.join(".cah")).unwrap();
    assert_eq!(Records::read(&d), Records::Default);
    let d = scratch("records-cah-stray");
    put(&d, ".cah/notes.txt", "x\n");
    assert_eq!(Records::read(&d), Records::Default);
}

#[test]
fn an_empty_new_folder_never_hides_a_populated_earlier_one() {
    let d = scratch("records-empty-ham");
    std::fs::create_dir_all(d.join(".ham/lifecycle/red")).unwrap();
    std::fs::create_dir_all(d.join(".ham/research")).unwrap();
    put(&d, ".cah/lifecycle/GOAL.md", "1. the tests pass\n");
    assert_eq!(Records::read(&d), moved(".cah"));
    // a tree of folders with no file in it is no state either
    let d = scratch("records-empty-tree");
    std::fs::create_dir_all(d.join(".ham/lifecycle/red/deeper")).unwrap();
    put(&d, ".cah/test-command", "pytest\n");
    assert_eq!(Records::read(&d), moved(".cah"));
}

#[test]
fn a_state_file_is_state_whatever_it_holds_and_a_file_at_any_depth_counts_under_a_state_folder() {
    // the definition of the shared cases: a regular file named records or test-command in the folder, or a regular file at any depth below
    // lifecycle, research or context; a name that is a folder where a file belongs, or a file where a folder belongs, is nothing
    let d = scratch("records-empty-state-file");
    put(&d, ".ham/test-command", "");
    put(&d, ".cah/lifecycle/LOG.md", "x\n");
    assert!(Records::read(&d).conflict().is_some());
    let d = scratch("records-deep");
    put(&d, ".cah/lifecycle/red/a/b/c/01.txt", "x\n");
    assert_eq!(Records::read(&d), moved(".cah"));
    let d = scratch("records-wrong-kind");
    std::fs::create_dir_all(d.join(".cah/records")).unwrap();
    put(&d, ".cah/lifecycle", "a file where the folder belongs\n");
    assert_eq!(Records::read(&d), Records::Default);
}

#[test]
fn a_link_is_no_state_in_either_folder() {
    let outside = scratch("records-link-outside");
    put(&outside, "real/lifecycle/LOG.md", "x\n");
    let d = scratch("records-link-cah");
    std::os::unix::fs::symlink(outside.join("real"), d.join(".cah")).unwrap();
    assert_eq!(Records::read(&d), Records::Default);
    // a link where an entry belongs
    let d = scratch("records-link-entry");
    std::fs::create_dir_all(d.join(".cah")).unwrap();
    std::os::unix::fs::symlink(outside.join("real/lifecycle"), d.join(".cah/lifecycle")).unwrap();
    assert_eq!(Records::read(&d), Records::Default);
}

#[test]
fn state_in_both_folders_is_a_conflict_that_names_both() {
    let d = scratch("records-both");
    put(&d, ".ham/lifecycle/LOG.md", "x\n");
    put(&d, ".cah/lifecycle/LOG.md", "x\n");
    let got = Records::read(&d);
    let why = got
        .conflict()
        .unwrap_or_else(|| panic!("{got:?}"))
        .to_string();
    assert!(why.contains(".ham") && why.contains(".cah"), "{why}");
    assert_eq!(got.problem(), Some(why.as_str()));
    // The places stay the default ones, as for an invalid locator; the run does not start, so nothing reads them for a decision.
    assert_eq!(got.log().as_deref(), Some(".ham/lifecycle/LOG.md"));
}

#[test]
fn the_locator_is_read_from_the_folder_the_run_uses() {
    let d = scratch("records-locator-cah");
    put(&d, ".cah/records", "# the old layout\ndocs\n");
    assert_eq!(Records::read(&d), moved("docs"));
    let d = scratch("records-locator-cah-off");
    put(&d, ".cah/records", "off\n");
    assert_eq!(Records::read(&d), Records::Off);
    // a `.ham/records` is read in a repository that has no earlier folder, as before
    let d = scratch("records-locator-ham");
    put(&d, ".ham/records", "docs\n");
    assert_eq!(Records::read(&d), moved("docs"));
}

#[test]
fn the_test_command_files_are_listed_in_the_order_they_are_read() {
    assert_eq!(
        agentd::direct::records::TEST_COMMAND_FILES,
        [".ham/test-command", ".cah/test-command"]
    );
}

#[test]
fn the_folder_chosen_at_the_start_is_kept_and_its_locator_is_read_again() {
    let d = scratch("records-pinned");
    put(&d, ".cah/lifecycle/LOG.md", "x\n");
    assert_eq!(choose(&d), Ok(".cah"));
    // The engine makes `.ham` later: a new choice would be a conflict, the run keeps the folder it chose.
    put(&d, ".ham/lifecycle/GOAL.md", "x\n");
    assert!(Records::read(&d).conflict().is_some());
    assert_eq!(Records::read_in(&d, ".cah"), moved(".cah"));
    // The locator of the chosen folder may be written during the run.
    put(&d, ".cah/records", "docs\n");
    assert_eq!(Records::read_in(&d, ".cah"), moved("docs"));
    assert_eq!(Records::read_in(&d, ".ham"), Records::Default);
    // Neither folder: the new one.
    assert_eq!(choose(&scratch("records-choose-none")), Ok(".ham"));
    assert!(choose(&d).is_err());
}

/// `fixtures/lifecycle/records-locator.json` `baseCases` (when the fixture has them): each case builds a tree from `files` (a path, or a path with its text),
/// `emptyDirs` and `links` (a path and the relative target), and `expect` is `current` (`.ham`), `legacy` (`.cah`) or `refuse` (state in both).
/// The self-check script of the lifecycle pack and the scorer answer the same cases.
#[test]
fn the_shared_base_cases_give_the_same_answer() {
    let path = common::repo_root().join("fixtures/lifecycle/records-locator.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let Some(cases) = v.get("baseCases").and_then(|c| c.as_array()) else {
        return;
    };
    assert!(cases.len() >= 20, "{}", cases.len());
    let mut seen = std::collections::BTreeSet::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let d = scratch("records-base-case");
        for dir in case["emptyDirs"].as_array().into_iter().flatten() {
            std::fs::create_dir_all(d.join(dir.as_str().unwrap())).unwrap();
        }
        for file in case["files"].as_array().into_iter().flatten() {
            match file {
                serde_json::Value::String(rel) => put(&d, rel, "x\n"),
                other => put(
                    &d,
                    other["path"].as_str().unwrap(),
                    other["text"].as_str().unwrap(),
                ),
            }
        }
        for link in case["links"].as_array().into_iter().flatten() {
            let at = d.join(link["path"].as_str().unwrap());
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(link["target"].as_str().unwrap(), at).unwrap();
        }
        let want = case["expect"].as_str().unwrap();
        seen.insert(want.to_string());
        match (want, choose(&d)) {
            ("current", Ok(folder)) => assert_eq!(folder, ".ham", "{name}"),
            ("legacy", Ok(folder)) => assert_eq!(folder, EARLIER_BASE, "{name}"),
            ("refuse", Err(why)) => assert!(
                why.contains(".ham") && why.contains(EARLIER_BASE),
                "{name}: {why}"
            ),
            (want, got) => panic!("{name}: expected {want}, got {got:?}"),
        }
    }
    assert_eq!(
        seen.into_iter().collect::<Vec<_>>(),
        ["current", "legacy", "refuse"],
        "the cases cover all three answers"
    );
}
