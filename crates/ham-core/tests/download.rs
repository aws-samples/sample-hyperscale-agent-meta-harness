//! Resumable, SHA-256-verified download planning.

#![allow(clippy::unwrap_used)] // Test helpers outside #[test] fns may unwrap.

use ham_core::download::{
    CHUNK_BYTES, DownloadError, etag_path, lock_path, part_path, plan_ranges, resume_from,
    sha256_hex, verify,
};
use proptest::prelude::*;
use std::path::Path;

#[test]
fn chunk_is_16_mib() {
    assert_eq!(CHUNK_BYTES, 16 * 1024 * 1024);
}

#[test]
fn ranges_cover_the_object_in_order() {
    assert_eq!(plan_ranges(10, 4, 0), vec![(0, 3), (4, 7), (8, 9)]);
    assert_eq!(plan_ranges(10, 4, 6), vec![(6, 9)]);
    assert_eq!(plan_ranges(8, 4, 8), Vec::<(u64, u64)>::new());
    assert_eq!(plan_ranges(0, 4, 0), Vec::<(u64, u64)>::new());
}

#[test]
fn sidecar_paths() {
    assert_eq!(
        part_path(Path::new("/d/app-R1-abc1234.zip")),
        Path::new("/d/app-R1-abc1234.zip.part")
    );
    assert_eq!(
        etag_path(Path::new("/d/app-R1-abc1234.zip")),
        Path::new("/d/app-R1-abc1234.zip.part.etag")
    );
    assert_eq!(
        lock_path(Path::new("/d/app-R1-abc1234.zip")),
        Path::new("/d/app-R1-abc1234.zip.part.lock")
    );
}

#[test]
fn resume_only_when_the_etag_matches() {
    assert_eq!(resume_from(100, Some("\"e1\""), "\"e1\"", 1000), 100);
    assert_eq!(resume_from(100, Some("\"e0\""), "\"e1\"", 1000), 0);
    assert_eq!(resume_from(100, None, "\"e1\"", 1000), 0);
    assert_eq!(
        resume_from(2000, Some("\"e1\""), "\"e1\"", 1000),
        0,
        "a part longer than the object restarts"
    );
}

#[test]
fn verify_compares_sha256() {
    let data = b"percent_change";
    let good = sha256_hex(data);
    assert_eq!(good.len(), 64);
    assert!(verify(data, &good).is_ok());
    let err = verify(data, &"0".repeat(64)).expect_err("mismatch");
    assert!(matches!(err, DownloadError::Sha256Mismatch { .. }));
    assert!(err.to_string().contains(&good));
}

proptest! {
    #[test]
    fn plan_is_contiguous_and_complete(size in 0u64..10_000, chunk in 1u64..700, from_frac in 0u64..=100) {
        let from = size * from_frac / 100;
        let plan = plan_ranges(size, chunk, from);
        let mut next = from;
        for (s, e) in &plan {
            prop_assert_eq!(*s, next);
            prop_assert!(e >= s && e - s < chunk);
            next = e + 1;
        }
        prop_assert_eq!(next.max(from), size.max(from));
    }
}
