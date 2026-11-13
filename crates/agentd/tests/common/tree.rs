//! A generated source tree for the `code_search` tests: many small files with known tokens, and the cases
//! a search has to get right (hidden files, ignored files, a binary file, a file over the size limit, a nested ignore file).
//!
//! The content is deterministic (a fixed seed, no clock, no random crate), so a test that compares two searches of one tree compares
//! like with like. Nothing here deletes: a test makes a fresh directory with `common::scratch` and leaves it.
#![allow(dead_code, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};

/// A token that exactly five files of a 2,000-file tree hold (one line each) and that the hidden, ignored and binary files hold as well.
pub const RARE: &str = "RARE_NEEDLE_ZQX";
/// A token that every file holds twice.
pub const COMMON: &str = "common_token_xy";
/// A token one file in 13 holds once (154 matches in 2,000 files: under the cap of 200).
pub const MEDIUM: &str = "medium_token_q";
/// `needle_word` is in every 17th file and `needle_words` in every 19th: a whole-word search for the first must not find the second.
pub const WORD: &str = "needle_word";
/// The regular expression the tests use: `fn alpha_zz<digits>` is in every 101st file.
pub const REGEX: &str = "fn [a-z]+_zz[0-9]+";

/// The extra files of [`generate`]: what a plain search leaves out and `includeHidden` and `includeIgnored` bring in.
pub const HIDDEN: &[&str] = &[".hidden_dir/h.rs", ".hidden_file"];
pub const IGNORED: &[&str] = &["ignored_dir/skip.rs", "logs/run.log"];

/// A tree on disk.
pub struct Tree {
    pub root: PathBuf,
    pub files: usize,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*: enough for varied words, fixed by the seed.
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

const WORDS: &[&str] = &[
    "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    "lambda", "mu", "nu", "xi", "omicron", "pi", "rho", "sigma", "tau", "upsilon", "phi", "chi",
    "psi", "omega",
];

/// The path of file `i` below the root: 50 directories of 20 subdirectories.
pub fn file_path(i: usize) -> PathBuf {
    PathBuf::from(format!("d{:02}", i % 50))
        .join(format!("s{:02}", (i / 50) % 20))
        .join(format!("f{i:05}.rs"))
}

fn body(i: usize, rng: &mut Rng) -> String {
    let mut out = String::new();
    for line in 1..=20usize {
        let w = |rng: &mut Rng| WORDS[(rng.next() % WORDS.len() as u64) as usize];
        let text = match line {
            3 => format!("// {COMMON} first"),
            9 => format!("// {COMMON} second"),
            7 if i.is_multiple_of(400) => format!("let marker = \"{RARE}\"; // rare"),
            12 if i.is_multiple_of(13) => format!("// {MEDIUM} here"),
            14 if i.is_multiple_of(17) => format!("let a = {WORD}(1);"),
            15 if i.is_multiple_of(19) => format!("let b = {WORD}s(2);"),
            16 if i.is_multiple_of(101) => format!("fn alpha_zz{i}() {{}}"),
            _ => format!(
                "fn {}_{}_{}_{i}_{line}() {{ let v = {}; }}",
                w(rng),
                w(rng),
                w(rng),
                rng.next() % 1000
            ),
        };
        out.push_str(&text);
        out.push('\n');
    }
    out
}

/// `files` source files below `root` (which exists), the cases of the module text when `extras` is true, and an empty `.git` directory so that
/// `ripgrep` reads the ignore file the way it does in a repository.
pub fn generate(root: &Path, files: usize, extras: bool) -> Tree {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for i in 0..files {
        let path = root.join(file_path(i));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body(i, &mut rng)).unwrap();
    }
    fs::create_dir_all(root.join(".git")).unwrap();
    if extras {
        let line = format!("let hit = \"{RARE}\";\n");
        fs::write(root.join(".gitignore"), "ignored_dir/\n*.log\n").unwrap();
        for rel in HIDDEN.iter().chain(IGNORED) {
            let p = root.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, &line).unwrap();
        }
        // A binary file: ripgrep skips a file with a NUL byte.
        fs::create_dir_all(root.join("bin")).unwrap();
        let mut blob = vec![0u8, 1, 2, 3];
        blob.extend_from_slice(RARE.as_bytes());
        fs::write(root.join("bin/blob.dat"), blob).unwrap();
        // A file over the 2 MiB limit of the tool: it holds the token and is never searched.
        fs::create_dir_all(root.join("big")).unwrap();
        let mut huge = format!("{RARE}\n").into_bytes();
        huge.resize(2 * 1024 * 1024 + 4096, b'x');
        fs::write(root.join("big/huge.txt"), huge).unwrap();
    }
    Tree {
        root: root.to_path_buf(),
        files,
    }
}
