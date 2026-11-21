//! What the header of a git bundle says the bundle needs, and which commits it names as its heads.
//!
//! A copy of a run's work may leave out what the run's base bundle holds. git then writes the commits it left out as prerequisite
//! lines in the header of the copy (`-<commit> <subject>`), and whoever reads the copy takes the base first. Every reader (the
//! download route, the client, the worker's restore, the delivery) asks the same two functions: [`needs`] of a copy, and [`heads`] of
//! the base, whose commits the writer leaves out. Both read the first bytes of the file and refuse a header that does not end inside them.

#![allow(clippy::unwrap_used)]

use harness_protocol::bundle_header::{
    BundleError, HEADER_BYTES, HEADER_BYTES_MAX, SNAPSHOT_REF, bundle_head, bundle_head_of, heads,
    is_object_id, needs,
};

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const D: &str = "dddddddddddddddddddddddddddddddddddddddd";
const LONG: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

#[test]
fn b860_header_names_what_a_bundle_needs() {
    // No prerequisite: the bundle clones alone.
    let whole = format!("# v2 git bundle\n{A} {SNAPSHOT_REF}\n\nPACK....");
    assert_eq!(needs(whole.as_bytes()), Ok(vec![]));

    // One, with a subject; the subject is not part of the id and may hold anything but a newline.
    let one = format!("# v2 git bundle\n-{B} harness: base -{C} x\n{A} {SNAPSHOT_REF}\n\nPACK");
    assert_eq!(needs(one.as_bytes()), Ok(vec![B.to_string()]));
    // The head of such a bundle reads as it always did.
    assert_eq!(bundle_head(one.as_bytes()), Ok(A.to_string()));

    // Several, in the header's order, each once; a line without a subject counts too.
    let several = format!(
        "# v2 git bundle\n-{B} first\n-{C}\n-{B} first again\n-{D} \n{A} {SNAPSHOT_REF}\n\nPACK"
    );
    assert_eq!(
        needs(several.as_bytes()),
        Ok(vec![B.to_string(), C.to_string(), D.to_string()])
    );

    // A v3 header with capabilities, and ids of the longer hash.
    let v3 = format!(
        "# v3 git bundle\n@object-format=sha256\n@filter=blob:none\n-{LONG} base\n{LONG} {SNAPSHOT_REF}\n\nPACK"
    );
    assert_eq!(needs(v3.as_bytes()), Ok(vec![LONG.to_string()]));

    // A header that does not end inside the bytes read is refused: a prerequisite could follow.
    let cut = format!("# v2 git bundle\n-{B} base\n{A} {SNAPSHOT_REF}\n");
    assert_eq!(needs(cut.as_bytes()), Err(BundleError::Truncated));
    assert_eq!(
        needs(&one.as_bytes()[..30]),
        Err(BundleError::Truncated),
        "a line the prefix cut in two is not read"
    );
    assert_eq!(needs(b"# v2 git bundle"), Err(BundleError::Truncated));

    // No bundle, and a prerequisite line whose id is no full object id.
    assert_eq!(needs(b"PK\x03\x04 a zip\n\n"), Err(BundleError::NotABundle));
    assert_eq!(needs(b""), Err(BundleError::NotABundle));
    let short = format!("# v2 git bundle\n-abc123 base\n{A} {SNAPSHOT_REF}\n\n");
    assert!(matches!(
        needs(short.as_bytes()),
        Err(BundleError::BadObjectId(_))
    ));
    let glued = format!("# v2 git bundle\n-{B}subject\n{A} {SNAPSHOT_REF}\n\n");
    assert!(matches!(
        needs(glued.as_bytes()),
        Err(BundleError::BadObjectId(_))
    ));
    // A head line that is no head line is refused here as well: the header is read strictly.
    let bad_head = format!("# v2 git bundle\n-{B} base\nnot-an-id {SNAPSHOT_REF}\n\n");
    assert!(matches!(
        needs(bad_head.as_bytes()),
        Err(BundleError::BadObjectId(_))
    ));
}

#[test]
fn b860_header_names_the_heads_of_a_base() {
    // The base bundle of a run: `git bundle create repo.bundle HEAD <base ref>`, two lines and one commit.
    let base = format!("# v2 git bundle\n{A} HEAD\n{A} refs/heads/main\n\nPACK");
    assert_eq!(heads(base.as_bytes()), Ok(vec![A.to_string()]));

    // Several refs: the commit of the line named HEAD first, then the others in the header's order, each once.
    let many = format!(
        "# v2 git bundle\n{B} refs/heads/a\n{C} refs/heads/b\n{A} HEAD\n{B} refs/tags/t\n{A} refs/heads/main\n\nPACK"
    );
    assert_eq!(
        heads(many.as_bytes()),
        Ok(vec![A.to_string(), B.to_string(), C.to_string()])
    );

    // No line named HEAD: the header's order.
    let no_head = format!("# v2 git bundle\n{C} refs/heads/b\n{B} refs/heads/a\n\nPACK");
    assert_eq!(
        heads(no_head.as_bytes()),
        Ok(vec![C.to_string(), B.to_string()])
    );

    // A v3 header.
    let v3 = format!("# v3 git bundle\n@object-format=sha256\n{LONG} HEAD\n\nPACK");
    assert_eq!(heads(v3.as_bytes()), Ok(vec![LONG.to_string()]));

    // A base that itself needs other commits gives no floor: what it holds cannot be told from its header.
    let thin = format!("# v2 git bundle\n-{B} below\n{A} HEAD\n\nPACK");
    assert_eq!(heads(thin.as_bytes()), Err(BundleError::NeedsOthers));
    assert!(
        BundleError::NeedsOthers
            .to_string()
            .contains("needs commits it does not hold"),
        "{}",
        BundleError::NeedsOthers
    );

    // Cut, no bundle, a bad id.
    let cut = format!("# v2 git bundle\n{A} HEAD\n{A} refs/heads/main\n");
    assert_eq!(heads(cut.as_bytes()), Err(BundleError::Truncated));
    assert_eq!(heads(b"# v"), Err(BundleError::Truncated));
    assert_eq!(heads(b"not a bundle\n\n"), Err(BundleError::NotABundle));
    let bad = format!("# v2 git bundle\n{A}\n\n");
    assert!(matches!(
        heads(bad.as_bytes()),
        Err(BundleError::BadObjectId(_))
    ));
    // A header with no head line names no commit.
    assert_eq!(heads(b"# v2 git bundle\n\nPACK"), Ok(vec![]));
}

#[test]
fn b860_the_moved_items_read_as_they_did() {
    assert_eq!(SNAPSHOT_REF, "refs/heads/snapshot");
    assert_eq!(HEADER_BYTES, 8192);
    assert_eq!(HEADER_BYTES_MAX, 65_536);
    assert!(is_object_id(A) && is_object_id(LONG));
    assert!(!is_object_id("abc") && !is_object_id(&A.to_uppercase()));
    let b =
        format!("# v2 git bundle\n-{B} base\n{A} refs/heads/harness/x\n{C} {SNAPSHOT_REF}\n\nPACK");
    assert_eq!(bundle_head(b.as_bytes()), Ok(C.to_string()));
    assert_eq!(
        bundle_head_of(b.as_bytes(), "refs/heads/harness/x"),
        Ok(A.to_string())
    );
    assert_eq!(
        bundle_head_of(b.as_bytes(), "refs/heads/none"),
        Err(BundleError::MissingRef)
    );
    let none = format!("# v2 git bundle\n{A} refs/heads/main\n\n");
    assert_eq!(
        bundle_head(none.as_bytes()),
        Err(BundleError::NoSnapshotRef)
    );
}

/// The same items through the path the service and the client have always imported them from.
#[test]
fn b860_a_real_header_of_git_is_read() {
    // The header git 2.47 writes for `git bundle create --version=2 f refs/heads/snapshot ^<floor>`, byte for byte.
    let real = b"# v2 git bundle\n-6f1d0c1a2b3c4d5e6f708192a3b4c5d6e7f80910 harness: base\n0123456789abcdef0123456789abcdef01234567 refs/heads/snapshot\n\nPACK\x00\x00\x00\x02";
    assert_eq!(
        needs(real),
        Ok(vec!["6f1d0c1a2b3c4d5e6f708192a3b4c5d6e7f80910".to_string()])
    );
    assert_eq!(heads(real), Err(BundleError::NeedsOthers));
    assert_eq!(
        bundle_head(real),
        Ok("0123456789abcdef0123456789abcdef01234567".to_string())
    );
}
