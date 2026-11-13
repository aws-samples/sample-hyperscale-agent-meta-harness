//! A randomized differential run of `ham_core::gzip` against Python's zlib, for a person to run before the reader changes
//! (`fixtures/gzip/gen_corpus.py` writes the corpus). Ignored by default: it needs `HAMH_GZIP_CORPUS` to name a corpus directory.
#![allow(clippy::unwrap_used)]

use ham_core::gzip::gunzip_bounded;
use sha2::{Digest, Sha256};

#[test]
#[ignore = "needs HAMH_GZIP_CORPUS=<dir made by fixtures/gzip/gen_corpus.py>"]
fn every_python_made_stream_of_the_corpus_decodes_to_its_recorded_bytes() {
    let dir = std::path::PathBuf::from(
        std::env::var("HAMH_GZIP_CORPUS").expect("HAMH_GZIP_CORPUS names the corpus"),
    );
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("gz") {
            continue;
        }
        let want = std::fs::read_to_string(path.with_extension("sha256")).unwrap();
        let (hash, len) = want.trim().split_once(' ').unwrap();
        let len: usize = len.parse().unwrap();
        let got = gunzip_bounded(&std::fs::read(&path).unwrap(), len)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let have: String = Sha256::digest(&got)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            (have.as_str(), got.len()),
            (hash, len),
            "{}",
            path.display()
        );
        // One byte less must be refused, never cut short.
        if len > 0 {
            assert!(
                gunzip_bounded(&std::fs::read(&path).unwrap(), len - 1).is_err(),
                "{}",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 100, "only {checked} cases in {}", dir.display());
    println!("CORPUS-PASS {checked} streams");
}
