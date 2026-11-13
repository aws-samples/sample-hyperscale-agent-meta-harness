//! Wave 2 (backend lambdas): the file-size rule of the bundle validator follows the largest file a git host takes (2 GB over git for
//! CodeCommit), not the 100 MiB of a host the product does not deliver to. The bundle bound (2 GiB) stays the other limit.
#![allow(clippy::unwrap_used)]

use delivery::git::{Change, TreeEntry};
use delivery::validator::{Limits, Rule, evaluate};
use std::collections::BTreeMap;

fn world(size: u64) -> (Vec<Change>, BTreeMap<String, TreeEntry>) {
    let path = "src/data.bin";
    let changes = vec![Change {
        status: 'A',
        path: path.into(),
    }];
    let tree = BTreeMap::from([(
        path.to_string(),
        TreeEntry {
            mode: "100644".into(),
            kind: "blob".into(),
            sha: "5".repeat(40),
            size: Some(size),
            path: path.into(),
        },
    )]);
    (changes, tree)
}

const GIB: u64 = 1024 * 1024 * 1024;

#[test]
fn b776w_a_file_of_1_5_gib_is_no_violation_by_default() {
    let (changes, tree) = world(3 * GIB / 2);
    let globs = vec!["src/**".to_string()];
    let v = evaluate(&changes, &tree, &globs, &Limits::default());
    assert!(v.is_empty(), "{v:?}");
    // The old limit still works when a caller names it.
    let tight = Limits {
        max_file_bytes: 100 * 1024 * 1024,
        ..Limits::default()
    };
    let v = evaluate(&changes, &tree, &globs, &tight);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].rule, Rule::FileSize);
}

#[test]
fn b776w_a_file_past_what_a_git_host_takes_is_still_refused_and_the_refusal_names_the_size() {
    let (changes, tree) = world(2_000_000_001);
    let globs = vec!["src/**".to_string()];
    let v = evaluate(&changes, &tree, &globs, &Limits::default());
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].rule, Rule::FileSize);
    assert!(v[0].detail.contains("2000000001 bytes"), "{}", v[0].detail);
    assert!(v[0].detail.contains("2000000000"), "{}", v[0].detail);
    let (changes, tree) = world(2_000_000_000);
    assert!(evaluate(&changes, &tree, &globs, &Limits::default()).is_empty());
    assert_eq!(Limits::default().max_bundle_bytes, 2 * GIB);
}
