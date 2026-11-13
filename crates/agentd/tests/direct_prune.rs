//! Stale cargo build output is pruned while a prompt runs.
//!
//! The cargo target folder is on the session's root volume, 5.7 GiB free at the start. One test binary of the ham repository
//! builds 2.6 GiB and the whole workspace 21 GiB, and nothing removed the copies a changed feature set leaves. These tests pin the pruning
//! over a fake target folder with fresh and stale units, and the settings that control it. Measured on cargo 1.98.1 (2026-10-26): the file
//! `invoked.timestamp` of a unit is touched when the unit is compiled and NOT when a later build finds it fresh, so a timestamp older than
//! the window means "not compiled lately", which a dependency that every build still needs also shows. The rules below follow from that:
//! only an older variant of a unit that has a newer one goes first, a sole unit goes only when the volume is still below the stop mark, a
//! unit compiled inside the window never goes, and nothing is pruned while a cargo build holds its lock files.
#![allow(clippy::unwrap_used)]

mod common;
mod direct_support;

use agentd::config::DirectConfig;
use agentd::direct::prune::{self, FreeProbe, PruneSettings, Pruned, prune_target};
use common::scratch;
use direct_support::{Direct, Opts, RUN};
use rustix::fs::{FlockOperation, flock};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const KEEP: Duration = Duration::from_secs(20 * 60);
const MIN: u64 = 60;

// ---- a fake target folder ----------------------------------------------------------------------------------------------------

/// A profile directory the way cargo 1.98 leaves it: the three lock files, `.fingerprint/`, `deps/` and `build/`.
fn profile(target: &Path, rel: &str) -> PathBuf {
    let p = target.join(rel);
    for d in [".fingerprint", "deps", "build"] {
        std::fs::create_dir_all(p.join(d)).unwrap();
    }
    for l in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        std::fs::write(p.join(l), b"").unwrap();
    }
    p
}

fn set_age(file: &Path, now: SystemTime, age: Duration) {
    let f = File::options().write(true).open(file).unwrap();
    f.set_modified(now - age).unwrap();
}

/// The hash of unit number `n`: 16 lower-case hexadecimal digits.
fn hash(n: u64) -> String {
    format!("{n:016x}")
}

/// One unit as cargo writes it: the fingerprint folder (`invoked.timestamp`, the `<ident>.json` file, the dep-info file), the files of its hash in
/// `deps/` (library, metadata, dep-info and a binary with no extension) and its `build/` folder. `age` is the age of `invoked.timestamp`.
fn unit(p: &Path, name: &str, n: u64, ident: &str, now: SystemTime, age: Duration) -> String {
    // Cargo's fingerprint of a unit: the feature set is what tells two variants of one target apart, so it follows the unit number.
    let json = fingerprint_json(&[("features", format!("\"[{n}]\""))]);
    unit_json(p, name, n, ident, now, age, &json)
}

/// The fields of a cargo fingerprint file that the pruning reads (measured on cargo 1.98.1): every unit of one target has the same values except
/// `features` and `deps`. `change` replaces fields.
fn fingerprint_json(change: &[(&str, String)]) -> String {
    let mut fields: Vec<(&str, String)> = vec![
        ("rustc", "1625334936438085654".into()),
        ("features", "\"[]\"".into()),
        ("target", "11141987858860736541".into()),
        ("profile", "8378823585564974145".into()),
        ("path", "1700584703366870859".into()),
        ("deps", "[]".into()),
        ("rustflags", "[]".into()),
        ("config", "9396254390672932401".into()),
        ("compile_kind", "0".into()),
    ];
    for (k, v) in change {
        fields.iter_mut().find(|(f, _)| f == k).unwrap().1 = v.clone();
    }
    let body: Vec<String> = fields.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect();
    format!("{{{}}}", body.join(","))
}

/// As [`unit`] with the text of the fingerprint file given.
fn unit_json(
    p: &Path,
    name: &str,
    n: u64,
    ident: &str,
    now: SystemTime,
    age: Duration,
    json: &str,
) -> String {
    let h = hash(n);
    let fp = p.join(".fingerprint").join(format!("{name}-{h}"));
    std::fs::create_dir_all(&fp).unwrap();
    std::fs::write(fp.join("invoked.timestamp"), b"").unwrap();
    std::fs::write(fp.join(format!("{ident}.json")), json).unwrap();
    std::fs::write(fp.join(ident), b"abc").unwrap();
    std::fs::write(fp.join(format!("dep-{ident}")), b"abc").unwrap();
    set_age(&fp.join("invoked.timestamp"), now, age);
    let deps = p.join("deps");
    std::fs::write(deps.join(format!("lib{name}-{h}.rlib")), vec![1u8; 8192]).unwrap();
    std::fs::write(deps.join(format!("lib{name}-{h}.rmeta")), vec![1u8; 4096]).unwrap();
    std::fs::write(deps.join(format!("{name}-{h}.d")), vec![1u8; 1024]).unwrap();
    std::fs::write(deps.join(format!("{name}-{h}")), vec![1u8; 16384]).unwrap();
    let out = p.join("build").join(format!("{name}-{h}")).join("out");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("gen.rs"), vec![1u8; 2048]).unwrap();
    h
}

fn exists_unit(p: &Path, name: &str, h: &str) -> (bool, bool, bool) {
    (
        p.join(".fingerprint").join(format!("{name}-{h}")).exists(),
        p.join("deps").join(format!("lib{name}-{h}.rlib")).exists()
            || p.join("deps").join(format!("{name}-{h}")).exists()
            || p.join("deps").join(format!("lib{name}-{h}.rmeta")).exists()
            || p.join("deps").join(format!("{name}-{h}.d")).exists(),
        p.join("build").join(format!("{name}-{h}")).exists(),
    )
}

fn gone(p: &Path, name: &str, h: &str) -> bool {
    exists_unit(p, name, h) == (false, false, false)
}

fn whole(p: &Path, name: &str, h: &str) -> bool {
    exists_unit(p, name, h) == (true, true, true)
}

fn target(label: &str) -> PathBuf {
    let t = scratch(label).join("cargo-target");
    std::fs::create_dir_all(&t).unwrap();
    std::fs::write(
        t.join("CACHEDIR.TAG"),
        b"Signature: 8a477f597d28d172789f06886806bc55",
    )
    .unwrap();
    t
}

fn never() -> bool {
    false
}

fn always() -> bool {
    true
}

// ---- the older variant of a unit goes first -------------------------------------------------------------------------------------

#[test]
fn an_older_variant_with_a_newer_one_is_removed_with_its_files_and_the_newer_one_stays() {
    let now = SystemTime::now();
    let t = target("variants");
    let p = profile(&t, "debug");
    // The same target built with two feature sets: one 40 minutes ago, one 2 minutes ago.
    let old = unit(
        &p,
        "serde",
        1,
        "lib-serde",
        now,
        Duration::from_secs(40 * MIN),
    );
    let new = unit(
        &p,
        "serde",
        2,
        "lib-serde",
        now,
        Duration::from_secs(2 * MIN),
    );
    let got = prune_target(&t, now, KEEP, &never);
    assert_eq!(got.units, 1, "{got:?}");
    assert!(got.bytes >= 8192 + 4096 + 1024 + 16384 + 2048, "{got:?}");
    assert!(gone(&p, "serde", &old), "the old variant is gone");
    assert!(whole(&p, "serde", &new), "the newer variant is whole");
    assert!(
        t.join("CACHEDIR.TAG").exists(),
        "the top of the folder stays"
    );
    for d in [".fingerprint", "deps", "build"] {
        assert!(p.join(d).is_dir(), "{d} stays");
    }
}

#[test]
fn a_unit_compiled_inside_the_window_is_never_removed_even_when_a_newer_variant_exists() {
    let now = SystemTime::now();
    let t = target("fresh");
    let p = profile(&t, "debug");
    let a = unit(
        &p,
        "tokio",
        1,
        "lib-tokio",
        now,
        Duration::from_secs(19 * MIN),
    );
    let b = unit(&p, "tokio", 2, "lib-tokio", now, Duration::from_secs(MIN));
    // Even with the volume below the stop mark.
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got, Pruned::default());
    assert!(whole(&p, "tokio", &a) && whole(&p, "tokio", &b));
}

#[test]
fn a_sole_old_unit_stays_while_the_volume_is_above_the_stop_mark_and_goes_below_it() {
    let now = SystemTime::now();
    let t = target("sole");
    let p = profile(&t, "debug");
    let sole = unit(
        &p,
        "aws-sdk",
        7,
        "lib-aws-sdk",
        now,
        Duration::from_secs(90 * MIN),
    );
    // A dependency every build still needs looks old too: it is not touched while there is room.
    assert_eq!(prune_target(&t, now, KEEP, &never), Pruned::default());
    assert!(whole(&p, "aws-sdk", &sole));
    // Below the stop mark the run would end at the next failed write: the second tier removes it, and the next build compiles it again.
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got.units, 1, "{got:?}");
    assert!(gone(&p, "aws-sdk", &sole));
}

#[test]
fn the_second_tier_is_asked_after_the_first_so_one_removal_can_be_enough() {
    let now = SystemTime::now();
    let t = target("tiers");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(30 * MIN));
    let sole = unit(
        &p,
        "other",
        3,
        "lib-other",
        now,
        Duration::from_secs(60 * MIN),
    );
    // The first tier removes `old` (a newer variant exists, although that one is also older than the window). `still_short` says it is
    // enough after the first removal.
    let asked = Mutex::new(0u32);
    let got = prune_target(&t, now, KEEP, &|| {
        *asked.lock().unwrap() += 1;
        false
    });
    assert_eq!(got.units, 1, "{got:?}");
    assert_eq!(
        *asked.lock().unwrap(),
        1,
        "asked once, after the first tier"
    );
    assert!(gone(&p, "dep", &old));
    assert!(whole(&p, "dep", &new) && whole(&p, "other", &sole));
}

#[test]
fn a_hash_that_belongs_to_a_unit_that_stays_is_never_removed() {
    let now = SystemTime::now();
    let t = target("shared");
    let p = profile(&t, "debug");
    // A stale unit `a` (superseded) and a fresh unit `b` of another name that carries the same hash: the files of that hash are b's too.
    let a = unit(
        &p,
        "alpha",
        5,
        "lib-alpha",
        now,
        Duration::from_secs(60 * MIN),
    );
    let _newer = unit(&p, "alpha", 6, "lib-alpha", now, Duration::from_secs(MIN));
    let b = unit(&p, "beta", 5, "lib-beta", now, Duration::from_secs(MIN));
    assert_eq!(a, b);
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got, Pruned::default(), "{got:?}");
    assert!(whole(&p, "beta", &b));
}

#[test]
fn a_unit_without_a_timestamp_or_without_its_json_file_is_left_alone() {
    let now = SystemTime::now();
    let t = target("unknown");
    let p = profile(&t, "debug");
    let no_stamp = unit(&p, "one", 1, "lib-one", now, Duration::from_secs(60 * MIN));
    std::fs::remove_file(
        p.join(".fingerprint")
            .join(format!("one-{no_stamp}"))
            .join("invoked.timestamp"),
    )
    .unwrap();
    let no_json = unit(&p, "two", 2, "lib-two", now, Duration::from_secs(60 * MIN));
    std::fs::remove_file(
        p.join(".fingerprint")
            .join(format!("two-{no_json}"))
            .join("lib-two.json"),
    )
    .unwrap();
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got, Pruned::default(), "{got:?}");
    assert!(
        p.join("deps")
            .join(format!("libone-{no_stamp}.rlib"))
            .exists()
    );
    assert!(
        p.join("deps")
            .join(format!("libtwo-{no_json}.rlib"))
            .exists()
    );
}

#[test]
fn files_of_other_hashes_and_unknown_names_in_deps_are_not_touched() {
    let now = SystemTime::now();
    let t = target("others");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let deps = p.join("deps");
    // A hash that no fingerprint folder has, a name that only contains the hash, a file with a short hash, a folder and a top-level link.
    std::fs::write(deps.join(format!("orphan-{}.rlib", hash(99))), b"x").unwrap();
    std::fs::write(deps.join(format!("note-about-{old}.txt.bak")), b"x").unwrap();
    std::fs::write(deps.join("libdep-abc.rlib"), b"x").unwrap();
    std::fs::create_dir_all(deps.join(format!("dir-{old}"))).unwrap();
    std::fs::write(p.join("libdep.rlib"), b"top").unwrap();
    let got = prune_target(&t, now, KEEP, &never);
    assert_eq!(got.units, 1);
    assert!(deps.join(format!("orphan-{}.rlib", hash(99))).exists());
    assert!(deps.join(format!("note-about-{old}.txt.bak")).exists());
    assert!(deps.join("libdep-abc.rlib").exists());
    assert!(deps.join(format!("dir-{old}")).is_dir());
    assert!(p.join("libdep.rlib").exists());
}

// ---- links are not followed ---------------------------------------------------------------------------------------------------

#[test]
fn a_link_is_never_followed_out_of_the_target_folder() {
    use std::os::unix::fs::symlink;
    let now = SystemTime::now();
    let t = target("links");
    let outside = scratch("links-outside");
    std::fs::create_dir_all(outside.join("dir")).unwrap();
    std::fs::write(outside.join("file"), b"keep me").unwrap();
    std::fs::write(outside.join("dir").join("inner"), b"keep me too").unwrap();
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    // The old unit's own folders hold links to things outside: a file in deps, a folder in build/out, a folder in the fingerprint folder.
    symlink(
        outside.join("file"),
        p.join("deps").join(format!("libdep-{old}.so")),
    )
    .unwrap();
    symlink(
        &outside,
        p.join("build")
            .join(format!("dep-{old}"))
            .join("out")
            .join("escape"),
    )
    .unwrap();
    symlink(
        &outside,
        p.join(".fingerprint")
            .join(format!("dep-{old}"))
            .join("escape"),
    )
    .unwrap();
    let got = prune_target(&t, now, KEEP, &never);
    assert_eq!(got.units, 1, "{got:?}");
    assert!(
        gone(&p, "dep", &old),
        "the unit and the links inside it are gone"
    );
    assert_eq!(std::fs::read(outside.join("file")).unwrap(), b"keep me");
    assert_eq!(
        std::fs::read(outside.join("dir").join("inner")).unwrap(),
        b"keep me too"
    );
}

#[test]
fn a_linked_profile_directory_or_fingerprint_folder_or_unit_folder_is_not_entered() {
    use std::os::unix::fs::symlink;
    let now = SystemTime::now();
    let t = target("linked-dirs");
    // `debug` is a link to a real profile directory outside the target folder: not entered.
    let real = scratch("linked-dirs-real");
    let rp = profile(&real, "debug");
    let old = unit(&rp, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&rp, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    symlink(&rp, t.join("debug")).unwrap();
    // `release` is a real profile directory whose unit folder is a link to a real unit folder outside.
    let rel = profile(&t, "release");
    let outside = scratch("linked-dirs-unit");
    std::fs::write(outside.join("invoked.timestamp"), b"").unwrap();
    std::fs::write(outside.join("lib-x.json"), b"{}").unwrap();
    set_age(
        &outside.join("invoked.timestamp"),
        now,
        Duration::from_secs(80 * MIN),
    );
    symlink(
        &outside,
        rel.join(".fingerprint").join(format!("x-{}", hash(3))),
    )
    .unwrap();
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got, Pruned::default(), "{got:?}");
    assert!(
        whole(&rp, "dep", &old),
        "the linked profile directory was not entered"
    );
    assert!(outside.join("invoked.timestamp").exists() && outside.join("lib-x.json").exists());
}

// ---- cargo's lock files ---------------------------------------------------------------------------------------------------------

#[test]
fn nothing_is_pruned_while_cargo_holds_a_lock_and_it_is_pruned_when_the_lock_is_free() {
    let now = SystemTime::now();
    let t = target("locks");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    // What a running cargo does: an exclusive lock on its build lock file (it holds all three; one is enough to stop the pruning).
    let held = File::open(p.join(".cargo-build-lock")).unwrap();
    flock(&held, FlockOperation::NonBlockingLockExclusive).unwrap();
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got.units, 0, "{got:?}");
    assert_eq!(got.busy, 1, "one profile directory was busy: {got:?}");
    assert!(whole(&p, "dep", &old));
    drop(held);
    let got = prune_target(&t, now, KEEP, &never);
    assert_eq!(got.units, 1, "{got:?}");
    assert_eq!(got.busy, 0);
    assert!(gone(&p, "dep", &old));
}

#[test]
fn the_lock_is_held_while_it_prunes_and_given_back_after() {
    let now = SystemTime::now();
    let t = target("lock-held");
    let p = profile(&t, "debug");
    let _old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let during = Mutex::new(None::<bool>);
    let lock_path = p.join(".cargo-lock");
    // `still_short` is asked between the tiers, while the locks are held: a second try to take the lock fails then.
    let got = prune_target(&t, now, KEEP, &|| {
        let f = File::open(&lock_path).unwrap();
        *during.lock().unwrap() =
            Some(flock(&f, FlockOperation::NonBlockingLockExclusive).is_err());
        false
    });
    assert_eq!(got.units, 1);
    assert_eq!(
        *during.lock().unwrap(),
        Some(true),
        "held during the pruning"
    );
    let f = File::open(&lock_path).unwrap();
    assert!(
        flock(&f, FlockOperation::NonBlockingLockExclusive).is_ok(),
        "free after"
    );
}

#[test]
fn a_profile_directory_with_no_lock_file_is_not_pruned() {
    let now = SystemTime::now();
    let t = target("no-lock");
    let p = profile(&t, "debug");
    for l in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
        std::fs::remove_file(p.join(l)).unwrap();
    }
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got.units, 0, "{got:?}");
    assert!(
        whole(&p, "dep", &old),
        "a missing lock does not show that no build runs"
    );
}

// ---- which directories are profile directories -----------------------------------------------------------------------------------

#[test]
fn the_release_profile_a_cross_target_and_a_custom_profile_are_pruned_too() {
    let now = SystemTime::now();
    let t = target("profiles");
    let mut olds = Vec::new();
    for rel in [
        "debug",
        "release",
        "x86_64-unknown-linux-musl/debug",
        "bench",
    ] {
        let p = profile(&t, rel);
        let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
        let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
        olds.push((p, old));
    }
    let got = prune_target(&t, now, KEEP, &never);
    assert_eq!(got.units, 4, "{got:?}");
    for (p, old) in olds {
        assert!(gone(&p, "dep", &old), "{}", p.display());
    }
}

#[test]
fn a_missing_or_empty_target_folder_prunes_nothing_and_does_not_fail() {
    let now = SystemTime::now();
    let t = scratch("missing");
    assert_eq!(
        prune_target(&t.join("absent"), now, KEEP, &always),
        Pruned::default()
    );
    let e = target("empty");
    assert_eq!(prune_target(&e, now, KEEP, &always), Pruned::default());
    // A target folder that is a link is not entered.
    let real = target("real");
    let p = profile(&real, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let link = t.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(prune_target(&link, now, KEEP, &always), Pruned::default());
    assert!(whole(&p, "dep", &old));
}

// ---- what makes two units variants of one target ---------------------------------------------------------------------------------

#[test]
fn units_that_differ_in_more_than_the_feature_set_are_not_variants_of_each_other() {
    // Two versions of one crate (rand 0.7 and 0.8 have the same name and the same target file but another source path), a host copy and a
    // target copy (`compile_kind`), another profile and another target definition are all live at once: the older one is not a spare copy.
    let now = SystemTime::now();
    for (field, other) in [
        ("path", "1700584703366870000"),
        ("compile_kind", "1"),
        ("profile", "8378823585564974000"),
        ("target", "11141987858860730000"),
        ("rustc", "1625334936438080000"),
        ("config", "9396254390672930000"),
        ("rustflags", "[\"-Cdebuginfo=0\"]"),
    ] {
        let t = target("not-variants");
        let p = profile(&t, "debug");
        let old = unit_json(
            &p,
            "rand",
            1,
            "lib-rand",
            now,
            Duration::from_secs(50 * MIN),
            &fingerprint_json(&[]),
        );
        let _new = unit_json(
            &p,
            "rand",
            2,
            "lib-rand",
            now,
            Duration::from_secs(40 * MIN),
            &fingerprint_json(&[(field, other.to_string())]),
        );
        let got = prune_target(&t, now, KEEP, &never);
        assert_eq!(got, Pruned::default(), "{field}: {got:?}");
        assert!(whole(&p, "rand", &old), "{field}");
    }
}

#[test]
fn a_unit_whose_fingerprint_file_cannot_be_read_is_left_alone() {
    let now = SystemTime::now();
    let t = target("bad-json");
    let p = profile(&t, "debug");
    let bad = unit_json(
        &p,
        "one",
        1,
        "lib-one",
        now,
        Duration::from_secs(60 * MIN),
        "not json",
    );
    let few = unit_json(
        &p,
        "two",
        2,
        "lib-two",
        now,
        Duration::from_secs(60 * MIN),
        "{\"features\":\"[]\"}",
    );
    // Each has a newer sibling that would make it a spare copy if its file were readable.
    let _a = unit_json(
        &p,
        "one",
        3,
        "lib-one",
        now,
        Duration::from_secs(MIN),
        "not json",
    );
    let _b = unit_json(
        &p,
        "two",
        4,
        "lib-two",
        now,
        Duration::from_secs(MIN),
        "{\"features\":\"[]\"}",
    );
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got, Pruned::default(), "{got:?}");
    assert!(whole(&p, "one", &bad) && whole(&p, "two", &few));
}

#[test]
fn the_files_of_a_unit_in_examples_go_with_it() {
    let now = SystemTime::now();
    let t = target("examples");
    let p = profile(&t, "debug");
    std::fs::create_dir_all(p.join("examples")).unwrap();
    let old = unit(
        &p,
        "demo",
        1,
        "example-demo",
        now,
        Duration::from_secs(50 * MIN),
    );
    let new = unit(&p, "demo", 2, "example-demo", now, Duration::from_secs(MIN));
    for h in [&old, &new] {
        std::fs::write(
            p.join("examples").join(format!("demo-{h}")),
            vec![1u8; 4096],
        )
        .unwrap();
        std::fs::write(
            p.join("examples").join(format!("demo-{h}.d")),
            vec![1u8; 512],
        )
        .unwrap();
    }
    let got = prune_target(&t, now, KEEP, &never);
    assert_eq!(got.units, 1, "{got:?}");
    assert!(!p.join("examples").join(format!("demo-{old}")).exists());
    assert!(!p.join("examples").join(format!("demo-{old}.d")).exists());
    assert!(p.join("examples").join(format!("demo-{new}")).exists());
    assert!(p.join("examples").join(format!("demo-{new}.d")).exists());
}

// ---- a program that runs from the target folder ------------------------------------------------------------------------------------

#[test]
fn nothing_is_pruned_while_a_program_runs_from_the_target_folder() {
    // A test run starts its binaries from deps/ after cargo has given its locks back: removing a binary between two of its starts would fail it.
    let now = SystemTime::now();
    let t = target("runs-from");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let runner = p.join("deps").join(format!("runner-{}", hash(9)));
    std::fs::copy("/bin/sleep", &runner).expect("a program to run");
    let mut child = std::process::Command::new(&runner)
        .arg("30")
        .spawn()
        .unwrap();
    // wait until the kernel shows it
    for _ in 0..50 {
        if std::fs::read_link(format!("/proc/{}/exe", child.id()))
            .is_ok_and(|e| e.starts_with(std::fs::canonicalize(&t).unwrap()))
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let got = prune_target(&t, now, KEEP, &always);
    assert_eq!(got.units, 0, "{got:?}");
    assert_eq!(got.busy, 1, "{got:?}");
    assert!(whole(&p, "dep", &old));
    child.kill().unwrap();
    child.wait().unwrap();
    let got = prune_target(&t, now, KEEP, &never);
    assert_eq!(got.units, 1, "{got:?}");
    assert!(gone(&p, "dep", &old));
}

// ---- the settings -------------------------------------------------------------------------------------------------------------

fn config(pairs: &[(&str, &str)]) -> DirectConfig {
    let m: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    DirectConfig::from_vars(&|k| m.get(k).cloned())
}

#[test]
fn the_settings_default_to_a_look_every_minute_below_1536_mib_keeping_the_last_20_minutes() {
    let d = config(&[]);
    assert_eq!(d.disk_check, Duration::from_secs(60));
    assert_eq!(d.disk_prune_mb, 1536);
    assert_eq!(d.cargo_keep, Duration::from_secs(20 * 60));
    let s = PruneSettings::from_config(&d).expect("on by default");
    assert_eq!(s.every, Duration::from_secs(60));
    assert_eq!(s.below_mb, 1536);
    assert_eq!(s.keep, Duration::from_secs(20 * 60));
    assert_eq!(s.low_mb, 512, "the stop mark of AGENTD_DISK_LOW_MB");
}

#[test]
fn the_deployments_values_reach_the_settings_and_a_bad_value_is_the_default() {
    let d = config(&[
        ("AGENTD_DISK_CHECK_S", "15"),
        ("AGENTD_DISK_PRUNE_MB", "2048"),
        ("AGENTD_CARGO_KEEP_MIN", "45"),
        ("AGENTD_DISK_LOW_MB", "700"),
    ]);
    let s = PruneSettings::from_config(&d).unwrap();
    assert_eq!(s.every, Duration::from_secs(15));
    assert_eq!(s.below_mb, 2048);
    assert_eq!(s.keep, Duration::from_secs(45 * 60));
    assert_eq!(s.low_mb, 700);
    for bad in ["", "soon", "-3", "1.5", "20min"] {
        let d = config(&[
            ("AGENTD_DISK_CHECK_S", bad),
            ("AGENTD_DISK_PRUNE_MB", bad),
            ("AGENTD_CARGO_KEEP_MIN", bad),
        ]);
        assert_eq!(
            (d.disk_check, d.disk_prune_mb, d.cargo_keep),
            (Duration::from_secs(60), 1536, Duration::from_secs(20 * 60)),
            "{bad:?} is the default"
        );
    }
    // Above the largest value is the largest value.
    let d = config(&[
        ("AGENTD_DISK_CHECK_S", "999999"),
        ("AGENTD_CARGO_KEEP_MIN", "999999"),
    ]);
    assert_eq!(d.disk_check, Duration::from_secs(3600));
    assert_eq!(d.cargo_keep, Duration::from_secs(1440 * 60));
}

#[test]
fn zero_switches_each_part_off() {
    for key in [
        "AGENTD_DISK_CHECK_S",
        "AGENTD_DISK_PRUNE_MB",
        "AGENTD_CARGO_KEEP_MIN",
    ] {
        let d = config(&[(key, "0")]);
        assert!(
            PruneSettings::from_config(&d).is_none(),
            "{key}=0 switches the pruning off"
        );
    }
    // The stop mark at 0 does not switch the pruning off: it only keeps the second tier from running.
    let d = config(&[("AGENTD_DISK_LOW_MB", "0")]);
    let s = PruneSettings::from_config(&d).unwrap();
    assert_eq!(s.low_mb, 0);
}

// ---- the look that runs while a prompt runs ----------------------------------------------------------------------------------

/// What the task reported: each prune and the free space after it.
type Reports = Arc<Mutex<Vec<(Pruned, Option<u64>)>>>;

/// A volume whose free space a test sets.
#[derive(Debug)]
struct Free(AtomicU64, AtomicU64);

impl Free {
    fn new(mb: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(mb), AtomicU64::new(0)))
    }
    fn set(&self, mb: u64) {
        self.0.store(mb, Ordering::SeqCst);
    }
    fn asked(&self) -> u64 {
        self.1.load(Ordering::SeqCst)
    }
}

impl FreeProbe for Free {
    fn free_mb(&self) -> Option<u64> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Some(self.0.load(Ordering::SeqCst))
    }
}

fn settings() -> PruneSettings {
    PruneSettings {
        every: Duration::from_millis(40),
        below_mb: 1536,
        keep: KEEP,
        low_mb: 512,
    }
}

#[tokio::test]
async fn nothing_is_removed_when_the_free_space_is_above_the_mark() {
    let now = SystemTime::now();
    let t = target("tick-room");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let free = Free::new(1536);
    let got = prune::tick(&settings(), &t, free.clone(), now).await;
    assert!(got.is_none(), "1536 MiB free is not below the mark of 1536");
    assert_eq!(free.asked(), 1, "the free space was read once");
    assert!(whole(&p, "dep", &old));
    free.set(1535);
    let got = prune::tick(&settings(), &t, free.clone(), now)
        .await
        .unwrap();
    assert_eq!(got.0.units, 1);
    assert!(gone(&p, "dep", &old));
}

#[tokio::test]
async fn a_free_space_that_cannot_be_read_prunes_nothing() {
    #[derive(Debug)]
    struct Unreadable;
    impl FreeProbe for Unreadable {
        fn free_mb(&self) -> Option<u64> {
            None
        }
    }
    let now = SystemTime::now();
    let t = target("tick-unreadable");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    assert!(
        prune::tick(&settings(), &t, Arc::new(Unreadable), now)
            .await
            .is_none()
    );
    assert!(whole(&p, "dep", &old));
}

#[tokio::test]
async fn the_second_tier_asks_the_volume_again_after_the_first() {
    let now = SystemTime::now();
    let t = target("tick-tiers");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let sole = unit(
        &p,
        "sole",
        3,
        "lib-sole",
        now,
        Duration::from_secs(70 * MIN),
    );
    // 1000 MiB free: below the prune mark, above the stop mark of 512: only the older variant goes.
    let free = Free::new(1000);
    let got = prune::tick(&settings(), &t, free.clone(), now)
        .await
        .unwrap();
    assert_eq!(got.0.units, 1);
    assert!(gone(&p, "dep", &old) && whole(&p, "sole", &sole));
    // 300 MiB free (below the stop mark) after the first tier: the sole old unit goes too.
    let t2 = target("tick-tiers-2");
    let p2 = profile(&t2, "debug");
    let old2 = unit(&p2, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new2 = unit(&p2, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    let sole2 = unit(
        &p2,
        "sole",
        3,
        "lib-sole",
        now,
        Duration::from_secs(70 * MIN),
    );
    let free2 = Free::new(300);
    let got = prune::tick(&settings(), &t2, free2.clone(), now)
        .await
        .unwrap();
    assert_eq!(got.0.units, 2, "{got:?}");
    assert!(gone(&p2, "dep", &old2) && gone(&p2, "sole", &sole2));
}

#[tokio::test]
async fn the_event_text_names_the_units_the_size_and_the_free_space() {
    let text = prune::notice(
        &Pruned {
            units: 3,
            bytes: 812 << 20,
            busy: 0,
        },
        Some(1204),
    );
    assert!(text.contains("3 units"), "{text}");
    assert!(text.contains("812 MiB freed"), "{text}");
    assert!(text.contains("1204 MiB free"), "{text}");
    let one = prune::notice(
        &Pruned {
            units: 1,
            bytes: 300 << 10,
            busy: 0,
        },
        None,
    );
    assert!(one.contains("1 unit,") || one.contains("1 unit "), "{one}");
    assert!(one.contains("300 KiB freed"), "{one}");
    assert!(!one.contains("MiB free."), "{one}");
}

#[tokio::test]
async fn the_task_prunes_on_its_interval_reports_each_prune_once_and_stops_with_the_run() {
    // The task reads the clock when it wakes, so the ages are made from the clock now.
    let now = SystemTime::now();
    let t = target("task");
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 11, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 12, "lib-dep", now, Duration::from_secs(MIN));
    let free = Free::new(1000);
    let reports: Reports = Arc::default();
    let sink = reports.clone();
    let task = prune::start(
        settings(),
        t.clone(),
        free.clone(),
        Arc::new(move |p: &Pruned, f: Option<u64>| sink.lock().unwrap().push((*p, f))),
    );
    for _ in 0..100 {
        if !reports.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let first = reports.lock().unwrap().clone();
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0].0.units, 1);
    assert!(gone(&p, "dep", &old));
    // More wake-ups with nothing left to prune write no second report.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let later = reports.lock().unwrap().clone();
    assert_eq!(later.len(), 1, "{later:?}");
    // Stopped, it looks no more.
    task.stop().await;
    let asked = free.asked();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(free.asked(), asked, "the task is stopped");
}

/// A volume whose second read, the one a prune makes on its blocking thread, takes a while and notes when it is over.
#[derive(Debug)]
struct Holding {
    reads: AtomicU64,
    inside: AtomicBool,
    over: AtomicBool,
}

impl FreeProbe for Holding {
    fn free_mb(&self) -> Option<u64> {
        if self.reads.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
            self.inside.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(400));
            self.over.store(true, Ordering::SeqCst);
        }
        Some(100)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn b783_stop_returns_only_after_a_prune_that_is_running_has_ended() {
    let now = SystemTime::now();
    let t = target("b783-stop");
    let p = profile(&t, "debug");
    let _old = unit(&p, "dep", 21, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 22, "lib-dep", now, Duration::from_secs(MIN));
    let probe = Arc::new(Holding {
        reads: AtomicU64::new(0),
        inside: AtomicBool::new(false),
        over: AtomicBool::new(false),
    });
    let task = prune::start(
        settings(),
        t.clone(),
        probe.clone(),
        Arc::new(|_: &Pruned, _: Option<u64>| {}),
    );
    for _ in 0..300 {
        if probe.inside.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        probe.inside.load(Ordering::SeqCst),
        "the prune is running on its blocking thread"
    );
    task.stop().await;
    assert!(
        probe.over.load(Ordering::SeqCst),
        "stop returned while the prune was still running on its thread"
    );
}

// ---- a whole run -----------------------------------------------------------------------------------------------------------------

/// A target folder in the place a run keeps it (`<data root>/<run>/direct/cargo-target`) with one old and one new variant of a unit.
fn seeded(d: &Direct) -> (PathBuf, String) {
    let now = SystemTime::now();
    let t = d
        .app
        .cfg
        .data_root
        .join(RUN)
        .join("direct")
        .join("cargo-target");
    std::fs::create_dir_all(&t).unwrap();
    let p = profile(&t, "debug");
    let old = unit(&p, "dep", 1, "lib-dep", now, Duration::from_secs(50 * MIN));
    let _new = unit(&p, "dep", 2, "lib-dep", now, Duration::from_secs(MIN));
    (p, old)
}

fn start_run(check: Duration) -> impl std::future::Future<Output = Direct> {
    Direct::start(Opts {
        scenario: "end_turn_done",
        engine_env: vec![("FAKE_ACP_SILENCE_S", "3".into())],
        edit_cfg: Some(Box::new(move |c| {
            c.direct.disk_check = check;
            // always below the mark: the host has terabytes
            c.direct.disk_prune_mb = 100_000_000;
        })),
        ..Opts::default()
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_prunes_its_target_folder_while_the_prompt_runs_and_says_so_once() {
    let d = start_run(Duration::from_millis(300)).await;
    let (p, old) = seeded(&d);
    d.invoke().await;
    assert!(gone(&p, "dep", &old), "{}", d.log.lines().join("\n"));
    let notes: Vec<_> = d
        .aws
        .progress("notify")
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| v["reason"] == "disk_pruned")
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    let detail = notes[0]["detail"].as_str().unwrap();
    assert!(
        detail.starts_with("Pruned stale cargo build output: 1 unit,"),
        "{detail}"
    );
    assert!(detail.contains("MiB free."), "{detail}");
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.starts_with("direct turn disk pruned: ")),
        "{:?}",
        d.log.lines()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_with_the_look_switched_off_prunes_nothing() {
    // TRIGGER PRESENT: the same seeded folder, the same mark, the same run as above; only the look is off
    let d = start_run(Duration::ZERO).await;
    let (p, old) = seeded(&d);
    d.invoke().await;
    assert!(whole(&p, "dep", &old));
    assert!(
        d.aws
            .progress("notify")
            .into_iter()
            .all(|(_, v)| v["reason"] != "disk_pruned")
    );
    assert!(
        d.log
            .lines()
            .iter()
            .any(|l| l.contains("direct turn disk prune off: a setting switches it off"))
    );
}
