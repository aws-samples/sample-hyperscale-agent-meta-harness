//! The repository archive filter: GitHub-style archives built at run time go in, a deterministic
//! filtered tar.gz with a per-file manifest comes out. Every drop has a reason from a fixed table, every cap is `too_large`, every
//! malformed structure is `upstream_error`, and no message echoes an entry name or content.

#![allow(clippy::unwrap_used)]

mod job_support;

use harness_protocol::research::{ResearchErrorCode as C, ResearchLimits};
use job_support::*;
use research_api::archive::{Filtered, filter_archive, reason};
use research_api::ports::Fail;

fn limits() -> ResearchLimits {
    ResearchLimits::default()
}

fn run(g: &[u8]) -> Filtered {
    filter_archive(g, &[], &limits()).unwrap()
}

fn run_with(g: &[u8], paths: &[&str], l: &ResearchLimits) -> Result<Filtered, Fail> {
    let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
    filter_archive(g, &paths, l)
}

fn refused(g: &[u8]) -> Fail {
    filter_archive(g, &[], &limits()).unwrap_err()
}

fn drops(f: &Filtered) -> Vec<(&str, &str)> {
    f.dropped
        .iter()
        .map(|d| (d.path.as_str(), d.reason.as_str()))
        .collect()
}

fn kept(f: &Filtered) -> Vec<&str> {
    f.files.iter().map(|m| m.path.as_str()).collect()
}

// ---- the reason table ---------------------------------------------------------------------------------------------------------------

#[test]
fn the_drop_reasons_are_a_fixed_table_of_words() {
    assert_eq!(
        reason::ALL,
        [
            "symlink",
            "hardlink",
            "device",
            "fifo",
            "sparse",
            "special-entry",
            "invalid-utf8",
            "nul-in-name",
            "backslash",
            "unsupported-character",
            "absolute-path",
            "dotdot",
            "dot-component",
            "empty-component",
            "name-too-long",
            "excluded-dir",
            "outside-paths",
            "env-file",
            "private-key-name",
            "too-large-file",
            "binary",
            "private-key-content",
            "minified"
        ]
    );
    let mut sorted = reason::ALL.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), reason::ALL.len(), "one word per reason");
    assert!(reason::ALL.iter().all(|r| {
        r.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }));
}

// ---- a normal repository --------------------------------------------------------------------------------------------------------------

#[test]
fn a_normal_repository_is_filtered_hashed_and_described() {
    let big = text(2 * 1024 * 1024);
    let files: Vec<(&str, Vec<u8>)> = vec![
        (
            "index.js",
            b"export default function isPlainObject(value) {\n\treturn true;\n}\n".to_vec(),
        ),
        ("license", MIT_BODY.as_bytes().to_vec()),
        (
            "package.json",
            br#"{"name":"is-plain-obj","version":"1.0.0","license":"MIT"}"#.to_vec(),
        ),
        ("readme.md", text(300)),
        ("src/lib/a.txt", text(100)),
        ("big.txt", big),
    ];
    let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let g = archive_of(&borrowed);
    let f = run(&g);
    assert_eq!(f.commit, COMMIT);
    assert_eq!(f.top_dir, top());
    assert_eq!(
        kept(&f),
        [
            "index.js",
            "license",
            "package.json",
            "readme.md",
            "src/lib/a.txt"
        ]
    );
    for m in &f.files {
        let data = &files.iter().find(|(n, _)| *n == m.path).unwrap().1;
        assert_eq!(m.sha256, sha256_hex(data), "{}", m.path);
        assert_eq!(m.bytes, data.len() as u64, "{}", m.path);
    }
    assert_eq!(drops(&f), [("big.txt", reason::TOO_LARGE_FILE)]);
    let total: u64 = f.files.iter().map(|m| m.bytes).sum();
    assert_eq!(f.unpacked_bytes, total);
    assert!(
        f.read_bytes >= 2 * 1024 * 1024,
        "the dropped file was read through the cap: {}",
        f.read_bytes
    );
    // The output archive holds exactly the kept files, byte for byte.
    let out = read_output(&f.tar_gz);
    assert_eq!(out.len(), 5);
    for o in &out {
        assert_eq!(o.data, files.iter().find(|(n, _)| *n == o.path).unwrap().1);
    }
    // Licence evidence comes from the licence file.
    assert_eq!(f.licence.spdx, "MIT");
    assert_eq!(f.licence.source, "file:license");
    assert_eq!(
        f.licence.evidence_sha256.as_deref(),
        Some(sha256_hex(MIT_BODY.as_bytes()).as_str())
    );
}

#[test]
fn the_manifest_parts_validate_as_an_item_manifest() {
    use harness_protocol::research::{
        BodyRef, ITEM_SCHEMA, ItemKind, ItemManifest, ResearchProvider, s3,
    };
    let g = archive_of(&[("a.txt", b"a\n"), ("license", MIT_BODY.as_bytes())]);
    let f = run(&g);
    let run_id = "01M4XAB0CDEFGHJKMNPQRSTVWX";
    let m = ItemManifest {
        schema: ITEM_SCHEMA.into(),
        item_id: "i-0002".into(),
        kind: ItemKind::Repo,
        run: run_id.into(),
        agent: "direct".into(),
        provider: ResearchProvider::GithubArchive,
        request_id: "req-1".into(),
        origin: "https://github.com/sindresorhus/is-plain-obj".into(),
        commit: Some(f.commit.clone()),
        git_ref: None,
        retrieved_at: "2026-10-16T14:30:02Z".into(),
        licence: f.licence.clone(),
        body: BodyRef {
            key: s3::body_key(run_id, "i-0002", ItemKind::Repo),
            sha256: sha256_hex(&f.tar_gz),
            bytes: f.tar_gz.len() as u64,
        },
        source_sha256: None,
        files: f.files.clone(),
        dropped: f.dropped.clone(),
        caps: None,
        truncated: false,
    };
    m.validate().unwrap();
}

// ---- what is dropped, and why -----------------------------------------------------------------------------------------------------------

#[test]
fn links_devices_and_fifos_are_dropped_with_their_reasons() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/ok.txt"), b"ok\n");
        t.symlink(&format!("{top}/link"), "target.txt");
        t.hardlink(&format!("{top}/hard"), &format!("{top}/ok.txt"));
        t.chardev(&format!("{top}/dev"));
        t.blockdev(&format!("{top}/blk"));
        t.fifo(&format!("{top}/pipe"));
        t.raw(format!("{top}/odd").as_bytes(), b'Z', 0, b"", b"");
        t.gnu_long_link(&[b'x'; 300]);
        t.symlink(&format!("{top}/link2"), "ok.txt");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["ok.txt"]);
    assert_eq!(
        drops(&f),
        [
            ("link", reason::SYMLINK),
            ("hard", reason::HARDLINK),
            ("dev", reason::DEVICE),
            ("blk", reason::DEVICE),
            ("pipe", reason::FIFO),
            ("odd", reason::SPECIAL_ENTRY),
            ("link2", reason::SYMLINK),
        ]
    );
    let out = read_output(&f.tar_gz);
    assert_eq!(out_map(&out), [("ok.txt".to_string(), b"ok\n".to_vec())]);
}

#[test]
fn a_path_outside_the_repository_is_dropped_with_its_reason() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/ok.txt"), b"ok\n");
        t.file_bytes_name(b"../outside/file.txt", b"x");
        t.file_bytes_name(b"/outside/file.txt", b"x");
        t.file_bytes_name(format!("{top}/sub/../file.txt").as_bytes(), b"x");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["ok.txt"]);
    let rows: Vec<(&str, &str)> = drops(&f);
    assert_eq!(
        rows,
        [
            ("../outside/file.txt", reason::DOTDOT),
            ("/outside/file.txt", reason::ABSOLUTE_PATH),
            (
                &format!("{}/sub/../file.txt", top()) as &str,
                reason::DOTDOT
            ),
        ]
    );
    assert_eq!(out_map(&read_output(&f.tar_gz)).len(), 1);
}

#[test]
fn excluded_directories_are_one_dropped_entry_each() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/src/dist.txt"), b"kept\n");
        t.file(&format!("{top}/src/main.rs"), b"fn main() {}\n");
        t.dir(&format!("{top}/node_modules"));
        for n in ["a", "b", "c"] {
            t.file(&format!("{top}/node_modules/{n}/index.js"), b"x\n");
        }
        t.file(&format!("{top}/packages/x/node_modules/y.js"), b"x\n");
        t.file(&format!("{top}/target/debug/app"), b"x\n");
        t.file(&format!("{top}/dist/bundle.js"), b"x\n");
        t.file(&format!("{top}/.git/config"), b"x\n");
        t.file(&format!("{top}/.github/workflows/ci.yml"), b"x\n");
        t.symlink(&format!("{top}/node_modules/.bin/tool"), "../tool");
    }));
    let f = run(&g);
    assert_eq!(kept(&f), ["src/dist.txt", "src/main.rs"]);
    assert_eq!(
        drops(&f),
        [
            ("node_modules/", reason::EXCLUDED_DIR),
            ("packages/x/node_modules/", reason::EXCLUDED_DIR),
            ("target/", reason::EXCLUDED_DIR),
            ("dist/", reason::EXCLUDED_DIR),
            (".git/", reason::EXCLUDED_DIR),
            (".github/", reason::EXCLUDED_DIR),
        ]
    );
}

#[test]
fn file_size_binary_and_minified_rules_have_exact_boundaries() {
    let mut one_mib = text(1024 * 1024);
    let mut over = text(1024 * 1024 + 1);
    one_mib[0] = b'a';
    over[0] = b'a';
    let mut bin_at_7999 = text(10_000);
    bin_at_7999[7999] = 0;
    let mut bin_at_8000 = text(10_000);
    bin_at_8000[8000] = 0;
    let line = |n: usize, ch: char| format!("{}\n", ch.to_string().repeat(n)).into_bytes();
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("exact.txt", one_mib),
        ("over.txt", over),
        ("nul7999.dat", bin_at_7999),
        ("nul8000.dat", bin_at_8000),
        ("line2000.txt", line(2000, 'a')),
        ("line2001.txt", line(2001, 'a')),
        ("chars2000.txt", line(2000, 'é')),
        ("chars2001.txt", line(2001, 'é')),
        ("app.min.js", b"var a=1;\n".to_vec()),
        ("site.MIN.css", b"a{b:c}\n".to_vec()),
        ("bundle.js.map", b"{}\n".to_vec()),
        ("empty.txt", Vec::new()),
    ];
    let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let f = run(&archive_of(&borrowed));
    assert_eq!(
        kept(&f),
        [
            "chars2000.txt",
            "empty.txt",
            "exact.txt",
            "line2000.txt",
            "nul8000.dat"
        ]
    );
    assert_eq!(
        drops(&f),
        [
            ("over.txt", reason::TOO_LARGE_FILE),
            ("nul7999.dat", reason::BINARY),
            ("line2001.txt", reason::MINIFIED),
            ("chars2001.txt", reason::MINIFIED),
            ("app.min.js", reason::MINIFIED),
            ("site.MIN.css", reason::MINIFIED),
            ("bundle.js.map", reason::MINIFIED),
        ]
    );
}

#[test]
fn environment_files_and_private_keys_are_dropped_by_name_and_by_content() {
    let pem = pem_block();
    let mut late = vec![b'\n'; 8192];
    late.extend_from_slice(&pem);
    let mut early = text(100);
    early.extend_from_slice(&pem);
    let mut edge = vec![b'\n'; 8100];
    edge.extend_from_slice(&pem);
    let cert = b"-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n".to_vec();
    let files: Vec<(&str, Vec<u8>)> = vec![
        (".env", b"A=1\n".to_vec()),
        (".env.local", b"A=1\n".to_vec()),
        ("config/.env", b"A=1\n".to_vec()),
        (".environment", b"kept\n".to_vec()),
        ("env.txt", b"kept\n".to_vec()),
        ("config.env", b"kept\n".to_vec()),
        ("id_rsa", b"anything\n".to_vec()),
        ("keys/id_ed25519", b"anything\n".to_vec()),
        ("id_ecdsa", b"anything\n".to_vec()),
        ("id_dsa", b"anything\n".to_vec()),
        ("id_rsa.pub", b"ssh-rsa AAAA user\n".to_vec()),
        ("deploy.pem", pem.clone()),
        ("server.key", pem.clone()),
        ("notes.txt", early),
        ("late.txt", late),
        ("edge.txt", edge),
        ("cert.pem", cert),
    ];
    let borrowed: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let f = run(&archive_of(&borrowed));
    assert_eq!(
        kept(&f),
        [
            ".environment",
            "cert.pem",
            "config.env",
            "env.txt",
            "id_rsa.pub",
            "late.txt"
        ]
    );
    assert_eq!(
        drops(&f),
        [
            (".env", reason::ENV_FILE),
            (".env.local", reason::ENV_FILE),
            ("config/.env", reason::ENV_FILE),
            ("id_rsa", reason::PRIVATE_KEY_NAME),
            ("keys/id_ed25519", reason::PRIVATE_KEY_NAME),
            ("id_ecdsa", reason::PRIVATE_KEY_NAME),
            ("id_dsa", reason::PRIVATE_KEY_NAME),
            ("deploy.pem", reason::PRIVATE_KEY_CONTENT),
            ("server.key", reason::PRIVATE_KEY_CONTENT),
            ("notes.txt", reason::PRIVATE_KEY_CONTENT),
            ("edge.txt", reason::PRIVATE_KEY_CONTENT),
        ]
    );
}

#[test]
fn a_git_lfs_pointer_is_kept_as_the_small_pointer_text() {
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 12345\n",
        "ab".repeat(32)
    );
    let f = run(&archive_of(&[
        ("model.bin", pointer.as_bytes()),
        ("a.txt", b"a\n"),
    ]));
    assert_eq!(kept(&f), ["a.txt", "model.bin"]);
    let out = read_output(&f.tar_gz);
    assert_eq!(
        out.iter().find(|o| o.path == "model.bin").unwrap().data,
        pointer.as_bytes()
    );
    assert!(f.dropped.is_empty());
}

// ---- paths ------------------------------------------------------------------------------------------------------------------------

#[test]
fn paths_keep_only_the_named_prefixes_and_the_rest_is_outside_paths() {
    let g = archive_of(&[
        ("README.md", b"r\n"),
        ("notes", b"a file named notes\n"),
        ("docs/guide.md", b"g\n"),
        ("docsx/other.md", b"o\n"),
        ("src/a.rs", b"a\n"),
        ("src/deep/b.rs", b"b\n"),
        ("srcfile.rs", b"c\n"),
        ("license", MIT_BODY.as_bytes()),
    ]);
    let f = run_with(&g, &["src/", "README.md", "docs", "notes"], &limits()).unwrap();
    assert_eq!(
        kept(&f),
        [
            "README.md",
            "docs/guide.md",
            "notes",
            "src/a.rs",
            "src/deep/b.rs"
        ]
    );
    assert_eq!(
        drops(&f).iter().map(|(_, r)| *r).collect::<Vec<_>>(),
        [
            reason::OUTSIDE_PATHS,
            reason::OUTSIDE_PATHS,
            reason::OUTSIDE_PATHS
        ],
        "docsx/other.md, srcfile.rs and license"
    );
    // The repository's licence is still found although the licence file is outside the paths asked for.
    assert_eq!(f.licence.spdx, "MIT");
    assert_eq!(f.licence.source, "file:license");
    assert!(!kept(&f).contains(&"license"));
}

// ---- the dropped list cap -----------------------------------------------------------------------------------------------------------

fn with_symlinks(n: usize) -> Vec<u8> {
    gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/ok.txt"), b"ok\n");
        for i in 0..n {
            t.symlink(&format!("{top}/l{i:04}"), "ok.txt");
        }
    }))
}

#[test]
fn the_dropped_list_is_capped_at_200_with_a_summary_as_the_last_entry() {
    let f = run(&with_symlinks(200));
    assert_eq!(f.dropped.len(), 200);
    assert!(f.dropped.iter().all(|d| d.reason == reason::SYMLINK));
    let f = run(&with_symlinks(201));
    assert_eq!(f.dropped.len(), 200);
    assert_eq!(f.dropped[198].reason, reason::SYMLINK);
    assert_eq!(
        (f.dropped[199].path.as_str(), f.dropped[199].reason.as_str()),
        ("*", "2 more entries dropped")
    );
    let f = run(&with_symlinks(250));
    assert_eq!(f.dropped.len(), 200);
    assert_eq!(f.dropped[199].reason, "51 more entries dropped");
    assert_eq!(kept(&f), ["ok.txt"]);
}

// ---- caps -----------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_read_bytes_cap_counts_every_decompressed_byte_also_of_skipped_data() {
    let mut l = limits();
    l.repo_read_bytes = 1024 * 1024;
    // A 2 MiB file in an excluded directory (its data is skipped) and one over the per-file size (skipped as well): both are read through
    // the cap.
    for name in ["node_modules/big.txt", "big.txt"] {
        let g = archive_of(&[(name, &text(2 * 1024 * 1024))]);
        let err = filter_archive(&g, &[], &l).unwrap_err();
        assert_eq!(err.code, C::TooLarge, "{name}");
    }
    // Under the cap the same archive passes.
    l.repo_read_bytes = 4 * 1024 * 1024;
    assert!(filter_archive(&archive_of(&[("big.txt", &text(2 * 1024 * 1024))]), &[], &l).is_ok());
}

#[test]
fn the_entry_count_cap_counts_every_entry() {
    let names: Vec<String> = (0..60).map(|i| format!("f{i}.txt")).collect();
    let files: Vec<(&str, &[u8])> = names
        .iter()
        .map(|n| (n.as_str(), b"x\n".as_slice()))
        .collect();
    let g = archive_of(&files);
    // The global header and the top directory count as entries: 62 in all.
    let mut l = limits();
    l.repo_read_entries = 62;
    assert_eq!(filter_archive(&g, &[], &l).unwrap().files.len(), 60);
    l.repo_read_entries = 61;
    assert_eq!(filter_archive(&g, &[], &l).unwrap_err().code, C::TooLarge);
}

#[test]
fn the_kept_files_and_kept_bytes_caps_are_too_large() {
    let mut l = limits();
    l.repo_files = 3;
    let three = archive_of(&[("a", b"1\n"), ("b", b"2\n"), ("c", b"3\n")]);
    assert_eq!(filter_archive(&three, &[], &l).unwrap().files.len(), 3);
    let four = archive_of(&[("a", b"1\n"), ("b", b"2\n"), ("c", b"3\n"), ("d", b"4\n")]);
    assert_eq!(
        filter_archive(&four, &[], &l).unwrap_err().code,
        C::TooLarge
    );

    let mut l = limits();
    l.repo_unpacked_bytes = 1000;
    let exact = archive_of(&[("a", &text(600)), ("b", &text(400))]);
    assert_eq!(
        filter_archive(&exact, &[], &l).unwrap().unpacked_bytes,
        1000
    );
    let over = archive_of(&[("a", &text(600)), ("b", &text(401))]);
    assert_eq!(
        filter_archive(&over, &[], &l).unwrap_err().code,
        C::TooLarge
    );
}

#[test]
fn an_output_archive_over_the_compressed_cap_is_too_large() {
    let mut l = limits();
    l.repo_compressed_bytes = 10_000;
    let g = archive_of(&[("a.txt", &alnum_noise(50_000, 7))]);
    assert_eq!(filter_archive(&g, &[], &l).unwrap_err().code, C::TooLarge);
    l.repo_compressed_bytes = 200_000;
    let ok = filter_archive(&g, &[], &l).unwrap();
    assert!(ok.tar_gz.len() < 200_000);
}

// ---- commit id and top directory -------------------------------------------------------------------------------------------------

fn with_commit(top: &str, commit: Option<&str>) -> Vec<u8> {
    let mut t = Tar::new();
    if let Some(c) = commit {
        t.pax_global(c);
    }
    t.dir(top);
    t.file(&format!("{top}/a.txt"), b"a\n");
    t.end();
    gz(&t.buf)
}

#[test]
fn the_commit_id_must_be_present_and_40_lower_case_hex() {
    assert_eq!(
        run(&with_commit("repo-v1.2.3", Some(COMMIT))).commit,
        COMMIT
    );
    for bad in [
        None,
        Some("666DF7C10035F7E26F27EC214FE5AE3173435F34"),
        Some("666df7c"),
        Some("g66df7c10035f7e26f27ec214fe5ae3173435f34"),
    ] {
        let err = filter_archive(&with_commit("repo-v1", bad), &[], &limits()).unwrap_err();
        assert_eq!(err.code, C::UpstreamError, "{bad:?}");
    }
}

#[test]
fn a_pax_commit_that_disagrees_with_a_40_hex_top_directory_suffix_is_refused() {
    let other = "1".repeat(40);
    let err = filter_archive(
        &with_commit(&format!("repo-{other}"), Some(COMMIT)),
        &[],
        &limits(),
    )
    .unwrap_err();
    assert_eq!(err.code, C::UpstreamError);
    assert!(!err.message.contains(&other));
    // The same suffix is fine; a suffix that is not 40 hex (a tag or a branch) is not compared.
    assert!(
        filter_archive(
            &with_commit(&format!("repo-{COMMIT}"), Some(COMMIT)),
            &[],
            &limits()
        )
        .is_ok()
    );
    assert!(filter_archive(&with_commit("repo-release-1", Some(COMMIT)), &[], &limits()).is_ok());
}

#[test]
fn a_per_entry_pax_comment_is_not_the_commit() {
    let top = top();
    let mut t = Tar::new();
    t.dir(&top);
    t.pax_next(&[("comment", COMMIT.as_bytes())]);
    t.file(&format!("{top}/a.txt"), b"a\n");
    t.end();
    assert_eq!(
        filter_archive(&gz(&t.buf), &[], &limits())
            .unwrap_err()
            .code,
        C::UpstreamError
    );
}

#[test]
fn entries_outside_the_top_directory_make_the_archive_malformed() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a.txt"), b"a\n");
        t.file("other-project/file.txt", b"x\n");
    }));
    let err = refused(&g);
    assert_eq!(err.code, C::UpstreamError);
    assert!(!err.message.contains("other-project"), "{}", err.message);
    // A file with no directory at all.
    let mut t = Tar::new();
    t.pax_global(COMMIT);
    t.file("README", b"x\n");
    t.end();
    assert_eq!(refused(&gz(&t.buf)).code, C::UpstreamError);
}

#[test]
fn a_path_listed_twice_makes_the_archive_malformed() {
    let g = gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/quarterly-notes.txt"), b"1\n");
        t.file(&format!("{top}/quarterly-notes.txt"), b"2\n");
    }));
    let err = refused(&g);
    assert_eq!(err.code, C::UpstreamError);
    assert!(!err.message.contains("quarterly"), "{}", err.message);
}

// ---- pax and GNU extensions ----------------------------------------------------------------------------------------------------------

fn long_path(top: &str, len: usize) -> String {
    let mut p = format!("{top}/");
    while p.len() < len {
        p.push_str("deep/");
    }
    p.truncate(len - 5);
    p.push_str("f.txt");
    p
}

#[test]
fn a_pax_path_and_a_gnu_long_name_give_the_entry_its_name() {
    let top = top();
    let via_pax = long_path(&top, 200);
    let via_gnu = format!("{top}/gnu/{}", "n".repeat(150));
    let g = gz(&github(&top, COMMIT, |t, _| {
        t.pax_next(&[
            ("path", via_pax.as_bytes()),
            ("mtime", b"1700000000.5"),
            ("uid", b"501"),
            ("comment", b"ignored"),
        ]);
        t.file("PaxHeader-truncated-name", b"pax\n");
        t.gnu_long_name(via_gnu.as_bytes());
        t.file("gnu-truncated-name", b"gnu\n");
    }));
    let f = run(&g);
    let want_pax = via_pax
        .strip_prefix(&format!("{top}/"))
        .unwrap()
        .to_string();
    let want_gnu = via_gnu
        .strip_prefix(&format!("{top}/"))
        .unwrap()
        .to_string();
    let mut want = vec![want_pax.clone(), want_gnu.clone()];
    want.sort();
    assert_eq!(
        f.files.iter().map(|m| m.path.clone()).collect::<Vec<_>>(),
        want
    );
    // The output uses a pax path for a name over 100 bytes and the reader of the test sees the whole name.
    let out = read_output(&f.tar_gz);
    assert!(out.iter().any(|o| o.path == want_pax && o.data == b"pax\n"));
    assert!(out.iter().any(|o| o.path == want_gnu && o.data == b"gnu\n"));
    assert_eq!(
        f.commit, COMMIT,
        "a pax comment on an entry is not the commit"
    );
}

#[test]
fn a_gnu_long_name_over_4096_bytes_is_refused() {
    let g = gz(&github(&top(), COMMIT, |t, _| {
        t.gnu_long_name_exact(&[b'b'; 5000]);
        t.file("x", b"x\n");
    }));
    assert_eq!(refused(&g).code, C::UpstreamError);
}

// ---- broken input ---------------------------------------------------------------------------------------------------------------------------

#[test]
fn broken_gzip_and_broken_tar_are_refused() {
    let tar = github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a.txt"), &text(5000));
        t.file(&format!("{top}/b.txt"), &text(5000));
    });
    let good = gz(&tar);
    assert!(filter_archive(&good, &[], &limits()).is_ok());

    // Truncated gzip: the trailer is gone, then half the stream is gone.
    assert_eq!(refused(&good[..good.len() - 8]).code, C::UpstreamError);
    assert_eq!(refused(&good[..good.len() / 2]).code, C::UpstreamError);
    // A tar cut inside a file's data and gzipped properly.
    let cut = tar.len() - 1024 - 3000;
    assert_eq!(refused(&gz(&tar[..cut])).code, C::UpstreamError);
    // A tar cut inside a header.
    assert_eq!(refused(&gz(&tar[..1024 + 100])).code, C::UpstreamError);
    // A tar with no end blocks.
    assert_eq!(
        refused(&gz(&tar[..tar.len() - 1024])).code,
        C::UpstreamError
    );
    // A header with a bad checksum.
    let mut bad = tar.clone();
    bad[1024 + 148] = b'9';
    assert_eq!(refused(&gz(&bad)).code, C::UpstreamError);
    // Nothing, garbage, and an empty tar.
    assert_eq!(refused(&[]).code, C::UpstreamError);
    assert_eq!(refused(b"this is not gzip at all").code, C::UpstreamError);
    assert_eq!(refused(&gz(&[0u8; 1024])).code, C::UpstreamError);
    assert_eq!(refused(&gz(b"")).code, C::UpstreamError);
}

#[test]
fn error_messages_never_echo_entry_names_or_contents() {
    let marker = "quarterly-notes";
    let mut cases: Vec<Vec<u8>> = Vec::new();
    cases.push(gz(&github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/{marker}.txt"), b"1\n");
        t.file(&format!("{top}/{marker}.txt"), b"2\n");
    })));
    cases.push(gz(&github(
        &format!("{marker}-{}", "2".repeat(40)),
        COMMIT,
        |t, top| {
            t.file(&format!("{top}/a.txt"), b"1\n");
        },
    )));
    cases.push(gz(&github(&top(), marker, |t, top| {
        t.file(&format!("{top}/a.txt"), b"1\n");
    })));
    for g in cases {
        let err = refused(&g);
        assert!(!err.message.contains(marker), "{}", err.message);
        assert!(err.message.len() < 200, "{}", err.message);
    }
}

// ---- determinism ------------------------------------------------------------------------------------------------------------------------

#[test]
fn the_output_is_deterministic_whatever_the_order_or_the_compression_of_the_input() {
    let top = top();
    let build = |order: &[usize]| {
        let names = ["z.txt", "a/b.txt", "m.md", "a/a.txt"];
        github(&top, COMMIT, |t, top| {
            for i in order {
                t.file(
                    &format!("{top}/{}", names[*i]),
                    format!("content {i}\n").as_bytes(),
                );
            }
        })
    };
    let first = run(&gz(&build(&[0, 1, 2, 3])));
    let second = run(&gz(&build(&[0, 1, 2, 3])));
    let shuffled = run(&gz(&build(&[3, 1, 0, 2])));
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let mut e = GzEncoder::new(Vec::new(), Compression::best());
    e.write_all(&build(&[2, 0, 1, 3])).unwrap();
    let other_level = run(&e.finish().unwrap());
    assert_eq!(first.tar_gz, second.tar_gz);
    assert_eq!(first.tar_gz, shuffled.tar_gz);
    assert_eq!(first.tar_gz, other_level.tar_gz);
    assert_eq!(sha256_hex(&first.tar_gz), sha256_hex(&shuffled.tar_gz));
    assert_eq!(kept(&first), ["a/a.txt", "a/b.txt", "m.md", "z.txt"]);
    assert_eq!(first.files, shuffled.files);
}

// ---- the follow-up review: sizes of non-file entries, what follows the end, extension headers, path conflicts, licence memory ----------------------

fn small_tar(extra: impl FnOnce(&mut Tar, &str)) -> Vec<u8> {
    github(&top(), COMMIT, |t, top| {
        t.file(&format!("{top}/a.txt"), b"a\n");
        extra(t, top);
    })
}

#[test]
fn a_directory_link_device_or_fifo_that_declares_data_is_malformed() {
    for (flag, what) in [
        (b'5', "directory"),
        (b'2', "symlink"),
        (b'1', "hardlink"),
        (b'3', "character device"),
        (b'4', "block device"),
        (b'6', "fifo"),
    ] {
        let g = gz(&github(&top(), COMMIT, |t, top| {
            t.raw(
                format!("{top}/entry").as_bytes(),
                flag,
                512,
                b"",
                &[0u8; 512],
            );
            t.file(&format!("{top}/after.txt"), b"after\n");
        }));
        assert_eq!(refused(&g).code, C::UpstreamError, "{what}");
    }
}

#[test]
fn only_zero_bytes_may_follow_the_end_blocks() {
    let tar = small_tar(|_, _| {});
    // The padding GNU tar and git add to fill a record is zeros.
    let mut padded = tar.clone();
    padded.extend(std::iter::repeat_n(0u8, 10240));
    assert!(filter_archive(&gz(&padded), &[], &limits()).is_ok());
    let mut stray = tar.clone();
    stray.extend_from_slice(b"some stray text after the end");
    assert_eq!(refused(&gz(&stray)).code, C::UpstreamError);
    let mut late = tar.clone();
    late.extend(std::iter::repeat_n(0u8, 4096));
    late.push(1);
    assert_eq!(refused(&gz(&late)).code, C::UpstreamError);
    // Zeros that go on past the read cap are over the cap.
    let mut l = limits();
    l.repo_read_bytes = 1024 * 1024;
    let mut long_zeros = tar.clone();
    long_zeros.extend(std::iter::repeat_n(0u8, 3 * 1024 * 1024));
    assert_eq!(
        filter_archive(&gz(&long_zeros), &[], &l).unwrap_err().code,
        C::TooLarge
    );
}

#[test]
fn the_gzip_input_is_exactly_one_member() {
    let good = gz(&small_tar(|_, _| {}));
    assert!(filter_archive(&good, &[], &limits()).is_ok());
    let mut two = good.clone();
    two.extend_from_slice(&gz(&small_tar(|_, _| {})));
    assert_eq!(refused(&two).code, C::UpstreamError, "a second member");
    let mut text_after = good.clone();
    text_after.extend_from_slice(b"trailing text");
    assert_eq!(refused(&text_after).code, C::UpstreamError, "trailing text");
    let mut zeros_after = good.clone();
    zeros_after.extend_from_slice(&[0u8; 8]);
    assert_eq!(
        refused(&zeros_after).code,
        C::UpstreamError,
        "trailing zeros"
    );
}

#[test]
fn a_global_pax_header_may_not_name_a_path_a_size_or_a_sparse_map() {
    let top = top();
    for (key, value) in [
        ("size", b"5".as_slice()),
        ("path", b"somewhere/else".as_slice()),
        ("GNU.sparse.major", b"1".as_slice()),
    ] {
        let mut t = Tar::new();
        t.pax_global_records(&[("comment", COMMIT.as_bytes()), (key, value)]);
        t.dir(&top);
        t.file(&format!("{top}/a.txt"), b"a\n");
        t.end();
        assert_eq!(refused(&gz(&t.buf)).code, C::UpstreamError, "{key}");
    }
    // A global header with only a comment and ordinary fields is fine.
    let mut t = Tar::new();
    t.pax_global_records(&[("comment", COMMIT.as_bytes()), ("mtime", b"1700000000")]);
    t.dir(&top);
    t.file(&format!("{top}/a.txt"), b"a\n");
    t.end();
    assert!(filter_archive(&gz(&t.buf), &[], &limits()).is_ok());
}

#[test]
fn an_extension_header_with_no_entry_after_it_is_malformed_whatever_it_carries() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "pax with an ordinary field",
            small_tar(|t, _| {
                t.pax_next(&[("mtime", b"1700000000")]);
            }),
        ),
        (
            "pax with a path",
            small_tar(|t, top| {
                t.pax_next(&[("path", format!("{top}/b.txt").as_bytes())]);
            }),
        ),
        (
            "gnu long link",
            small_tar(|t, _| {
                t.gnu_long_link(b"target.txt");
            }),
        ),
        (
            "gnu long name",
            small_tar(|t, top| {
                t.gnu_long_name(format!("{top}/b.txt").as_bytes());
            }),
        ),
    ];
    for (what, tar) in cases {
        assert_eq!(refused(&gz(&tar)).code, C::UpstreamError, "{what}");
    }
    // The same headers in front of an entry are fine.
    let ok = small_tar(|t, top| {
        t.pax_next(&[("mtime", b"1700000000")]);
        t.file(&format!("{top}/b.txt"), b"b\n");
        t.gnu_long_link(b"target.txt");
        t.symlink(&format!("{top}/link"), "target.txt");
    });
    assert!(filter_archive(&gz(&ok), &[], &limits()).is_ok());
}

#[test]
fn a_path_listed_twice_is_malformed_also_inside_an_excluded_directory_and_as_a_directory() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "file in an excluded directory",
            small_tar(|t, top| {
                t.file(&format!("{top}/node_modules/pkg/index.js"), b"1\n");
                t.file(&format!("{top}/node_modules/pkg/index.js"), b"2\n");
            }),
        ),
        (
            "directory entry twice",
            small_tar(|t, top| {
                t.dir(&format!("{top}/docs"));
                t.dir(&format!("{top}/docs"));
            }),
        ),
        (
            "excluded directory entry twice",
            small_tar(|t, top| {
                t.dir(&format!("{top}/target"));
                t.dir(&format!("{top}/target"));
            }),
        ),
    ];
    for (what, tar) in cases {
        assert_eq!(refused(&gz(&tar)).code, C::UpstreamError, "{what}");
    }
}

#[test]
fn a_file_and_an_entry_below_the_same_name_conflict_in_either_order() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "file then file below it",
            small_tar(|t, top| {
                t.file(&format!("{top}/docs"), b"1\n");
                t.file(&format!("{top}/docs/guide.md"), b"2\n");
            }),
        ),
        (
            "file below then file",
            small_tar(|t, top| {
                t.file(&format!("{top}/docs/guide.md"), b"2\n");
                t.file(&format!("{top}/docs"), b"1\n");
            }),
        ),
        (
            "file then directory entry",
            small_tar(|t, top| {
                t.file(&format!("{top}/docs"), b"1\n");
                t.dir(&format!("{top}/docs"));
            }),
        ),
        (
            "directory entry then file",
            small_tar(|t, top| {
                t.dir(&format!("{top}/docs"));
                t.file(&format!("{top}/docs"), b"1\n");
            }),
        ),
        (
            "file then a deeper directory",
            small_tar(|t, top| {
                t.file(&format!("{top}/docs"), b"1\n");
                t.dir(&format!("{top}/docs/sub"));
            }),
        ),
        (
            "inside an excluded directory",
            small_tar(|t, top| {
                t.file(&format!("{top}/node_modules/pkg"), b"1\n");
                t.file(&format!("{top}/node_modules/pkg/index.js"), b"2\n");
            }),
        ),
    ];
    for (what, tar) in cases {
        assert_eq!(refused(&gz(&tar)).code, C::UpstreamError, "{what}");
    }
    // A directory entry beside its files, before or after them, is an ordinary archive.
    let before = small_tar(|t, top| {
        t.dir(&format!("{top}/docs"));
        t.file(&format!("{top}/docs/guide.md"), b"g\n");
    });
    let after = small_tar(|t, top| {
        t.file(&format!("{top}/docs/guide.md"), b"g\n");
        t.dir(&format!("{top}/docs"));
    });
    for tar in [before, after] {
        let f = filter_archive(&gz(&tar), &[], &limits()).unwrap();
        assert_eq!(kept(&f), ["a.txt", "docs/guide.md"]);
    }
}

#[test]
fn licence_files_beyond_a_total_budget_are_not_read_for_detection() {
    // Six root licence files of 900 KiB: four fit in the 4 MiB budget, the fifth and sixth do not. They are all kept in the output.
    let mit = padded_text(MIT_BODY, 900 * 1024);
    let apache = padded_text(APACHE_TITLE, 900 * 1024);
    let bsl = padded_text(BSL_TITLE, 900 * 1024);
    let files: Vec<(&str, &[u8])> = vec![
        ("LICENSE-A", &mit),
        ("LICENSE-B", &mit),
        ("LICENSE-C", &mit),
        ("LICENSE-D", &mit),
        ("LICENSE-E", &apache),
        ("LICENSE-F", &bsl),
    ];
    let f = run(&archive_of(&files));
    assert_eq!(f.files.len(), 6);
    assert_eq!(f.licence.spdx, "MIT");
    assert_eq!(f.licence.source, "file:LICENSE-A");
}

#[test]
fn a_declaring_file_the_content_rules_drop_still_gives_the_declared_licence() {
    // A package.json with one very long line is dropped as minified and is still read for its `license`.
    let json = format!("{{\"license\":\"ISC\",\"pad\":\"{}\"}}\n", "x".repeat(3000));
    let f = run(&archive_of(&[
        ("package.json", json.as_bytes()),
        ("a.txt", b"a\n"),
    ]));
    assert_eq!(kept(&f), ["a.txt"]);
    assert_eq!(
        (f.licence.spdx.as_str(), f.licence.source.as_str()),
        ("ISC", "declared:package.json")
    );
}
