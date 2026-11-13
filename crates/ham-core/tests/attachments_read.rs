//! Reading the user's attachments from disk: `ham... --attach <file>`, repeatable. The shared validator
//! (`harness_protocol::direct::check_attachments`) judges the wire form; the checks that need the disk (a file that is not regular or not UTF-8,
//! a file too large to read) live here. An attachment is named by its file name, and a problem names the path as the user typed it, never content.
#![allow(clippy::unwrap_used)]

use ham_core::attachments::{Why, read_attachments};
use harness_protocol::direct::{AttachmentProblem, check_attachments};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A scratch folder under the temp dir (set by the project's env.sh); like the other `ham` tests it is not removed by the test.
fn tmp() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ham-attach-{}-{}-{}",
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

fn paths(list: &[&String]) -> Vec<String> {
    list.iter().map(|s| (*s).clone()).collect()
}

#[test]
fn files_are_read_by_their_file_names_in_the_order_given() {
    let t = tmp();
    let a = write(&t, "docs/zeta.md", b"last file\n");
    let b = write(&t, "alpha.csv", b"a,b\n1,2\n");
    let items = read_attachments(&paths(&[&a, &b])).unwrap();
    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["zeta.md", "alpha.csv"]);
    assert_eq!(items[0].content, "last file\n");
    assert_eq!(items[0].size, 10);
    assert_eq!(check_attachments(&items), Ok(()));
}

#[test]
fn no_paths_is_no_attachments() {
    assert_eq!(read_attachments(&[]).unwrap(), vec![]);
}

#[test]
fn a_file_over_the_old_256_kib_cap_is_read() {
    // The old limit was 256 KiB a file; the files named are held to what a request carries, see tests/b776_input_limits.rs.
    let t = tmp();
    let ok = write(&t, "ok.txt", "a".repeat(256 * 1024 + 1).as_bytes());
    assert_eq!(read_attachments(&[ok]).unwrap()[0].size, 256 * 1024 + 1);
}

#[test]
fn a_missing_file_a_folder_and_a_file_that_is_not_text_are_refused_with_the_path() {
    let t = tmp();
    let missing = t.join("nope.md").display().to_string();
    let e = read_attachments(std::slice::from_ref(&missing)).unwrap_err();
    assert!(matches!(e.why, Why::Unreadable(_)), "{e}");
    assert!(e.to_string().contains(&missing), "{e}");

    let dir = t.join("folder");
    std::fs::create_dir_all(&dir).unwrap();
    let e = read_attachments(&[dir.display().to_string()]).unwrap_err();
    assert_eq!(e.why, Why::Invalid(AttachmentProblem::NotRegular));

    let binary = write(&t, "image.png", &[0x89, b'P', b'N', b'G', 0xff, 0xfe, 0x00]);
    let e = read_attachments(&[binary]).unwrap_err();
    assert_eq!(e.why, Why::Invalid(AttachmentProblem::NotText));
}

#[test]
fn an_empty_file_and_a_name_that_is_not_plain_are_refused() {
    let t = tmp();
    let empty = write(&t, "empty.md", b"");
    assert_eq!(
        read_attachments(&[empty]).unwrap_err().why,
        Why::Invalid(AttachmentProblem::Empty)
    );
    let spaced = write(&t, "my notes.md", b"text\n");
    let e = read_attachments(std::slice::from_ref(&spaced)).unwrap_err();
    assert_eq!(e.why, Why::Invalid(AttachmentProblem::BadName));
    assert!(e.to_string().contains("my notes.md"), "{e}");
    let hidden = write(&t, ".env", b"X=1\n");
    assert_eq!(
        read_attachments(&[hidden]).unwrap_err().why,
        Why::Invalid(AttachmentProblem::BadName)
    );
}

#[test]
fn two_files_with_one_name_are_refused() {
    let t = tmp();
    let a = write(&t, "one/notes.md", b"a\n");
    let b = write(&t, "two/Notes.md", b"b\n");
    assert_eq!(
        read_attachments(&[a, b]).unwrap_err().why,
        Why::Invalid(AttachmentProblem::DuplicateName)
    );
}

#[test]
fn a_link_the_user_names_is_followed_to_a_regular_file_and_keeps_the_links_name() {
    let t = tmp();
    let real = write(&t, "real/design.md", b"the design\n");
    let link = t.join("current.md");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let items = read_attachments(&[link.display().to_string()]).unwrap();
    assert_eq!(
        (items[0].name.as_str(), items[0].content.as_str()),
        ("current.md", "the design\n")
    );
}

#[test]
fn a_problem_never_holds_the_content_of_a_file() {
    let t = tmp();
    let p = write(&t, "a b.md", b"plain words in a document body\n");
    let e = read_attachments(&[p]).unwrap_err().to_string();
    assert!(!e.contains("plain words"), "{e}");
}
