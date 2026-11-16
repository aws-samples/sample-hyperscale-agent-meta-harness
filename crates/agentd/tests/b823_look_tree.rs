//! What a look costs. A look at a tree that did not change stores nothing and writes nothing,
//! however many follow one another; and the index of the private repository is kept between the looks of a session, so git reads again
//! only the files whose stat data changed. These tests run git and no engine.
#![allow(clippy::unwrap_used)]

mod common;

use agentd::direct::snapshot::{ScratchRepo, Snapshotter};
use common::{git, python_calc, scratch};
use std::path::Path;
use std::time::{Duration, Instant};

/// How many files the folder `dir` holds, in every folder below it.
fn files_below(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| {
            if e.file_type().unwrap().is_dir() {
                files_below(&e.path())
            } else {
                1
            }
        })
        .sum()
}

#[tokio::test]
async fn b823_look_forty_looks_at_an_unchanged_tree_store_nothing() {
    let dir = scratch("b823-look-forty");
    let repo = python_calc(&dir);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut snap = Snapshotter::new(&data, &repo, &base).await.unwrap();
    // The engine wrote a file: the first look copies it.
    std::fs::write(repo.join("src/a.py"), "a = 1\n").unwrap();
    let first = snap.snapshot().await.unwrap().expect("the tree changed");
    let bundle = std::fs::read(&first.bundle).unwrap();
    let objects = data.join("snapshot.git/objects");
    let mut held = 0;
    for n in 0..40 {
        assert_eq!(
            snap.snapshot().await.unwrap(),
            None,
            "look {n} at a tree that did not change made a copy"
        );
        if n == 0 {
            // The look after a copy first drops what no copy needs any more; the 39 looks after it write nothing at all.
            held = files_below(&objects);
        }
    }
    assert_eq!(
        files_below(&objects),
        held,
        "the looks at an unchanged tree wrote objects into the private repository"
    );
    assert_eq!(
        std::fs::read(&first.bundle).unwrap(),
        bundle,
        "the bundle of the last copy is what it was"
    );
    // The next change is copy 2: the looks in between counted nothing.
    std::fs::write(repo.join("src/b.py"), "b = 2\n").unwrap();
    let second = snap.snapshot().await.unwrap().expect("the tree changed");
    assert_eq!((first.count, second.count), (1, 2));
}

fn seconds(took: &[Duration]) -> Vec<String> {
    took.iter()
        .map(|d| format!("{:.3}", d.as_secs_f64()))
        .collect()
}

/// A measurement, no check of a number: what one look costs on a tree of 50,000 small files (500 folders of 100 files, committed as the
/// base of the session), with the index the private repository keeps between its looks and with an index made new for the look
/// (`ScratchRepo::whole_tree`, what every look would cost if the index were not kept). Run it by hand:
/// `cargo test -p agentd --test b823_look_tree -- --ignored --nocapture`; it prints one line that starts with `B823-LOOK-MEASURE`.
#[tokio::test]
#[ignore = "a measurement on 50,000 files; run it by hand"]
async fn b823_look_measure_a_look_on_fifty_thousand_files() {
    let dir = scratch("b823-look-50k");
    let repo = python_calc(&dir);
    for d in 0..500 {
        let folder = repo.join(format!("gen/d{d:03}"));
        std::fs::create_dir_all(&folder).unwrap();
        for f in 0..100 {
            std::fs::write(folder.join(format!("f{f:03}.txt")), format!("{d} {f}\n")).unwrap();
        }
    }
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "50,000 files"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    let data = dir.join("agent-data");
    std::fs::create_dir_all(&data).unwrap();
    let mut snap = Snapshotter::new(&data, &repo, &base).await.unwrap();

    // The first look of a session: the index was seeded from the base and holds no stat data yet, so git reads every file once.
    let t = Instant::now();
    assert_eq!(snap.snapshot().await.unwrap(), None);
    let first = t.elapsed();
    // The looks after it, on the kept index, at a tree that did not change.
    let mut kept = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        assert_eq!(snap.snapshot().await.unwrap(), None);
        kept.push(t.elapsed());
    }
    // One file changed: the look that copies it.
    std::fs::write(repo.join("gen/d250/f050.txt"), "changed\n").unwrap();
    let t = Instant::now();
    let made = snap.snapshot().await.unwrap().expect("the tree changed");
    let changed = t.elapsed();
    // An index made new for each look: every file is read and hashed again.
    let fresh_repo = ScratchRepo::open(data.join("new-index.git"), &repo)
        .await
        .unwrap();
    let mut fresh = Vec::new();
    for _ in 0..3 {
        let t = Instant::now();
        fresh_repo.whole_tree().await.unwrap();
        fresh.push(t.elapsed());
    }
    println!(
        "B823-LOOK-MEASURE files=50000 first_look_of_a_session_s={:.3} unchanged_look_kept_index_s={:?} \
         look_that_copies_one_changed_file_s={:.3} bundle_bytes={} unchanged_look_new_index_s={:?}",
        first.as_secs_f64(),
        seconds(&kept),
        changed.as_secs_f64(),
        made.bundle_bytes,
        seconds(&fresh),
    );
}
