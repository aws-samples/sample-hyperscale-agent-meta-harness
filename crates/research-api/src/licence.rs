//! Licence evidence for a repository: the licence files and the declared fields at the repository root, read as data.
//!
//! The result is **evidence, never clearance**. It says what the repository's own files say about its licence, with the file it found
//! that in and a hash of that file; it does not say that anything may be copied, and it says `NOASSERTION` whenever it is not sure. A
//! top-level licence file can leave parts of a repository under other terms, and a recognised text can sit beside a changed one.
//!
//! Rules (the first that applies):
//! 1. Licence files (`LICENSE`, `LICENCE`, `COPYING`, `UNLICENSE`, any case, with an extension or a `-MIT` style suffix) whose text is
//!    recognised by the signature table: one distinct licence gives its SPDX id and `file:<path>`; several give their ids joined with
//!    ` AND ` in path order (`file:<path>,<path>`), unless a declared expression names all of them (`MIT OR Apache-2.0` in `Cargo.toml`
//!    beside `LICENSE-MIT` and `LICENSE-APACHE`), which then is the answer with source `declared:<file>`. An unrecognised licence file
//!    beside a recognised one is ignored.
//! 2. A GNU licence text cannot say whether the project is "only" or "or later" (the standard texts carry the appendix notice, which reads
//!    "or later"): a declared `GPL-3.0-only` style id of the same licence refines it and is the answer, with the licence file as evidence.
//! 3. Only unrecognised licence files: `NOASSERTION` with `file:<path>` of the first.
//! 4. No licence file but a declared field (`package.json` `license` or `licenses`, `Cargo.toml` `license`, `pyproject.toml` `license`):
//!    the declared text (trimmed, at most 128 bytes, no control characters) with `declared:<file>`.
//! 5. Nothing: `NOASSERTION` with source `none`.
//!
//! Evidence hash: the SHA-256 of the one file the answer rests on; of the `<sha256>  <path>` lines (path order) of the recognised files
//! when several do; of the declaring file in rule 4. Askalono is not used: the signature table below is data and a test pins each
//! entry. The detector is pure.

use harness_protocol::research::LicenceEvidence;
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

/// The longest SPDX text the evidence carries (`ItemManifest::validate` refuses more).
pub const MAX_SPDX_BYTES: usize = 128;
/// Only this many leading bytes of a licence file are read for recognition.
const MAX_TEXT_BYTES: usize = 128 * 1024;
const DECLARATION_FILES: [&str; 3] = ["Cargo.toml", "package.json", "pyproject.toml"];
const NO_ASSERTION: &str = "NOASSERTION";

/// A root file the detector reads: its path and bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LicenceCandidate {
    pub path: String,
    pub bytes: Vec<u8>,
}

/// A root file the detector reads, borrowed: the archive filter passes the content it keeps for the output without copying it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LicenceFile<'a> {
    pub path: &'a str,
    pub bytes: &'a [u8],
}

// ---- the signature table ---------------------------------------------------------------------------------------------------------------

/// One licence: every phrase of `all` must be in the normalised text and none of `none`. `gnu` licences get `-only` or `-or-later`.
struct Signature {
    id: &'static str,
    gnu: bool,
    all: &'static [&'static str],
    none: &'static [&'static str],
}

const BSD_CLAUSES: [&str; 3] = [
    "redistribution and use in source and binary forms with or without modification are permitted provided that the following conditions are met",
    "redistributions of source code must retain",
    "redistributions in binary form must reproduce",
];
const ISC_GRANT: &str = "permission to use copy modify and or distribute this software for any purpose with or without fee is hereby granted";

/// Most specific first. Phrases are written in natural form and normalised like the text (lower case, punctuation as a space).
static SIGNATURES: &[Signature] = &[
    Signature {
        id: "AGPL-3.0",
        gnu: true,
        all: &["gnu affero general public license version 3"],
        none: &[],
    },
    Signature {
        id: "LGPL-3.0",
        gnu: true,
        all: &["gnu lesser general public license version 3"],
        none: &[],
    },
    Signature {
        id: "LGPL-2.1",
        gnu: true,
        all: &["gnu lesser general public license version 2.1"],
        none: &[],
    },
    Signature {
        id: "GPL-3.0",
        gnu: true,
        all: &["gnu general public license version 3"],
        none: &[
            "gnu lesser general public license",
            "gnu affero general public license",
        ],
    },
    Signature {
        id: "GPL-2.0",
        gnu: true,
        all: &["gnu general public license version 2"],
        none: &[
            "gnu lesser general public license",
            "gnu affero general public license",
        ],
    },
    Signature {
        id: "MPL-2.0",
        gnu: false,
        all: &["mozilla public license version 2.0"],
        none: &[],
    },
    Signature {
        id: "Apache-2.0",
        gnu: false,
        all: &["apache license version 2.0"],
        none: &[],
    },
    Signature {
        id: "BSL-1.0",
        gnu: false,
        all: &["boost software license version 1.0"],
        none: &[],
    },
    Signature {
        id: "CC0-1.0",
        gnu: false,
        all: &["cc0 1.0 universal"],
        none: &[],
    },
    Signature {
        id: "Unlicense",
        gnu: false,
        all: &["this is free and unencumbered software released into the public domain"],
        none: &[],
    },
    Signature {
        id: "Zlib",
        gnu: false,
        all: &[
            "this software is provided as is without any express or implied warranty",
            "the origin of this software must not be misrepresented",
        ],
        none: &[],
    },
    Signature {
        id: "0BSD",
        gnu: false,
        all: &[ISC_GRANT],
        none: &["provided that the above copyright notice"],
    },
    Signature {
        id: "ISC",
        gnu: false,
        all: &[
            ISC_GRANT,
            "provided that the above copyright notice and this permission notice appear in all copies",
        ],
        none: &[],
    },
    Signature {
        id: "BSD-3-Clause",
        gnu: false,
        all: &[
            BSD_CLAUSES[0],
            BSD_CLAUSES[1],
            BSD_CLAUSES[2],
            "neither the name of",
        ],
        none: &["all advertising materials mentioning"],
    },
    Signature {
        id: "BSD-2-Clause",
        gnu: false,
        all: &BSD_CLAUSES,
        none: &[
            "neither the name of",
            "all advertising materials mentioning",
            "the name of the author may not",
        ],
    },
    Signature {
        id: "MIT",
        gnu: false,
        all: &[
            "permission is hereby granted free of charge to any person obtaining a copy of this software",
            "the above copyright notice and this permission notice shall be included in all copies or substantial portions of the software",
        ],
        none: &[],
    },
];

/// The base ids the table recognises (a GNU licence is listed without `-only` or `-or-later`).
pub fn signature_ids() -> Vec<&'static str> {
    SIGNATURES.iter().map(|s| s.id).collect()
}

/// Lower case, every run of characters that are not letters or digits becomes one space, and lines that only state a copyright
/// (`Copyright (c) 2019 Name`, `Copyright 2024 Name`) are left out.
fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let lower = line.trim().to_lowercase();
        if is_copyright_line(&lower) {
            continue;
        }
        out.push_str(&lower);
        out.push(' ');
    }
    let mut collapsed = String::with_capacity(out.len());
    let mut space = true;
    for c in out.chars() {
        if c.is_alphanumeric() {
            collapsed.push(c);
            space = false;
        } else if !space {
            collapsed.push(' ');
            space = true;
        }
    }
    collapsed.trim_end().to_string()
}

fn is_copyright_line(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("copyright") else {
        return lower.starts_with("(c)") || lower.starts_with('\u{a9}');
    };
    let rest = rest.trim_start();
    rest.starts_with("(c)")
        || rest.starts_with('\u{a9}')
        || rest.starts_with('[')
        || rest.starts_with(|c: char| c.is_ascii_digit())
}

struct Normalised {
    sig: &'static Signature,
    all: Vec<String>,
    none: Vec<String>,
}

fn table() -> &'static [Normalised] {
    static TABLE: OnceLock<Vec<Normalised>> = OnceLock::new();
    TABLE.get_or_init(|| {
        SIGNATURES
            .iter()
            .map(|sig| Normalised {
                sig,
                all: sig.all.iter().map(|p| normalise(p)).collect(),
                none: sig.none.iter().map(|p| normalise(p)).collect(),
            })
            .collect()
    })
}

/// The SPDX id of a licence text, or `None`.
fn recognise(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_TEXT_BYTES)]);
    let norm = normalise(&text);
    let hit = table().iter().find(|n| {
        n.all.iter().all(|p| norm.contains(p.as_str()))
            && !n.none.iter().any(|p| norm.contains(p.as_str()))
    })?;
    if !hit.sig.gnu {
        return Some(hit.sig.id.to_string());
    }
    let later = norm.contains("or at your option any later version")
        || norm.contains("or any later version");
    Some(format!(
        "{}{}",
        hit.sig.id,
        if later { "-or-later" } else { "-only" }
    ))
}

// ---- choosing the candidates -----------------------------------------------------------------------------------------------------------

fn strip_doc_extension(rest: &str) -> &str {
    for ext in [".markdown", ".md", ".txt", ".rst"] {
        if let Some(r) = rest.strip_suffix(ext) {
            return r;
        }
    }
    rest
}

fn is_licence_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    ["license", "licence", "copying", "unlicense"]
        .iter()
        .any(|base| {
            let Some(rest) = lower.strip_prefix(base) else {
                return false;
            };
            let rest = strip_doc_extension(rest);
            let mut chars = rest.chars();
            match chars.next() {
                None => true,
                Some('-' | '_' | '.') => {
                    let tail = chars.as_str();
                    !tail.is_empty() && tail.chars().all(|c| c.is_ascii_alphanumeric())
                }
                Some(_) => false,
            }
        })
}

/// A root-level licence file or a declaration file the detector reads.
pub fn is_candidate_name(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('/')
        && (is_licence_name(path) || DECLARATION_FILES.contains(&path))
}

/// The candidates among `(path, bytes)` pairs: root-level licence and declaration files, in path order.
pub fn select_candidates<'a>(
    files: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Vec<LicenceCandidate> {
    let mut out: Vec<LicenceCandidate> = files
        .into_iter()
        .filter(|(path, _)| is_candidate_name(path))
        .map(|(path, bytes)| LicenceCandidate {
            path: path.to_string(),
            bytes: bytes.to_vec(),
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

// ---- declared fields -------------------------------------------------------------------------------------------------------------------

fn clean_declared(text: &str) -> Option<String> {
    let t = text.trim();
    (!t.is_empty() && t.len() <= MAX_SPDX_BYTES && !t.chars().any(char::is_control))
        .then(|| t.to_string())
}

fn package_json(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(bytes);
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    match v.get("license") {
        Some(serde_json::Value::String(s)) => return clean_declared(s),
        Some(serde_json::Value::Object(o)) => {
            if let Some(serde_json::Value::String(s)) = o.get("type") {
                return clean_declared(s);
            }
        }
        _ => {}
    }
    let types: Vec<&str> = v
        .get("licenses")?
        .as_array()?
        .iter()
        .filter_map(|l| l.get("type")?.as_str())
        .collect();
    if types.is_empty() {
        None
    } else {
        clean_declared(&types.join(" OR "))
    }
}

/// A TOML string literal at the start of `value` (basic or literal, one line), without its quotes.
fn toml_string(value: &str) -> Option<&str> {
    let value = value.trim_start();
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let inner = &value[1..];
    Some(&inner[..inner.find(quote)?])
}

/// `key = "value"` or `key = { text = "value" }` of the first matching line inside one of `tables`.
fn toml_value(text: &str, tables: &[&str], key: &str, inline_key: Option<&str>) -> Option<String> {
    let mut current = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            current = header
                .trim_start_matches('[')
                .split(']')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            continue;
        }
        if !tables.contains(&current.as_str()) {
            continue;
        }
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        if let Some(s) = toml_string(value) {
            return clean_declared(s);
        }
        if let Some(inner) = inline_key.and_then(|k| value.trim().strip_prefix('{').map(|i| (k, i)))
        {
            let (k, inner) = inner;
            if let Some(pos) = inner.find(k)
                && let Some(after) = inner[pos + k.len()..].trim_start().strip_prefix('=')
                && let Some(s) = toml_string(after)
            {
                return clean_declared(s);
            }
        }
    }
    None
}

fn declared(c: &LicenceFile<'_>) -> Option<String> {
    match c.path {
        "package.json" => package_json(c.bytes),
        "Cargo.toml" => toml_value(
            &String::from_utf8_lossy(c.bytes),
            &["package", "workspace.package"],
            "license",
            None,
        ),
        "pyproject.toml" => {
            let text = String::from_utf8_lossy(c.bytes);
            toml_value(&text, &["project", "tool.poetry"], "license", Some("text"))
                .or_else(|| toml_value(&text, &["project.license"], "text", None))
        }
        _ => None,
    }
}

// ---- the answer --------------------------------------------------------------------------------------------------------------------------

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn base_id(id: &str) -> &str {
    let id = id.strip_suffix('+').unwrap_or(id);
    id.strip_suffix("-only")
        .or_else(|| id.strip_suffix("-or-later"))
        .unwrap_or(id)
}

fn expression_ids(expression: &str) -> Vec<&str> {
    expression
        .split(|c: char| c.is_whitespace() || c == '(' || c == ')')
        .filter(|t| {
            !t.is_empty() && !["and", "or", "with"].contains(&t.to_ascii_lowercase().as_str())
        })
        .collect()
}

fn covers(expression: &str, ids: &[String]) -> bool {
    let tokens = expression_ids(expression);
    ids.iter().all(|id| {
        tokens
            .iter()
            .any(|t| base_id(t).eq_ignore_ascii_case(base_id(id)))
    })
}

fn evidence(spdx: String, source: String, sha: Option<String>) -> LicenceEvidence {
    LicenceEvidence {
        spdx,
        source,
        evidence_sha256: sha,
    }
}

/// Reads the candidates (root-level licence files and declaration files; others are ignored) and answers the evidence.
pub fn detect(candidates: &[LicenceCandidate]) -> LicenceEvidence {
    let files: Vec<LicenceFile<'_>> = candidates
        .iter()
        .map(|c| LicenceFile {
            path: c.path.as_str(),
            bytes: c.bytes.as_slice(),
        })
        .collect();
    detect_files(&files)
}

/// [`detect`] over borrowed files. Memory: the files are read in place; the detector allocates only the normalised text of one licence
/// file at a time (at most 128 KiB read) and the evidence.
pub fn detect_files(candidates: &[LicenceFile<'_>]) -> LicenceEvidence {
    let mut files: Vec<&LicenceFile<'_>> = candidates
        .iter()
        .filter(|c| !c.path.contains('/') && is_licence_name(c.path))
        .collect();
    files.sort_by(|a, b| a.path.cmp(b.path));
    let mut decls: Vec<(&str, String, &LicenceFile<'_>)> = candidates
        .iter()
        .filter(|c| DECLARATION_FILES.contains(&c.path))
        .filter_map(|c| declared(c).map(|text| (c.path, text, c)))
        .collect();
    decls.sort_by(|a, b| a.0.cmp(b.0));

    let mut recognised: Vec<(&str, String, String)> = Vec::new();
    let mut unrecognised: Vec<(&str, String)> = Vec::new();
    for f in &files {
        let sha = sha256_hex(f.bytes);
        match recognise(f.bytes) {
            Some(id) => recognised.push((f.path, id, sha)),
            None => unrecognised.push((f.path, sha)),
        }
    }

    if !recognised.is_empty() {
        let mut ids: Vec<String> = Vec::new();
        for (_, id, _) in &recognised {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        if ids.len() == 1 {
            let (path, id, sha) = &recognised[0];
            let gnu = SIGNATURES.iter().any(|s| s.gnu && s.id == base_id(id));
            if gnu
                && let Some((decl_path, text, _)) = decls.iter().find(|(_, t, _)| {
                    base_id(t) == base_id(id) && (t.ends_with("-only") || t.ends_with("-or-later"))
                })
            {
                return evidence(
                    text.clone(),
                    format!("declared:{decl_path}"),
                    Some(sha.clone()),
                );
            }
            return evidence(id.clone(), format!("file:{path}"), Some(sha.clone()));
        }
        let lines: String = recognised
            .iter()
            .map(|(path, _, sha)| format!("{sha}  {path}\n"))
            .collect();
        let composite = sha256_hex(lines.as_bytes());
        if let Some((decl_path, text, _)) = decls.iter().find(|(_, t, _)| covers(t, &ids)) {
            return evidence(
                text.clone(),
                format!("declared:{decl_path}"),
                Some(composite),
            );
        }
        let paths: Vec<&str> = recognised.iter().map(|(p, _, _)| *p).collect();
        return evidence(
            ids.join(" AND "),
            format!("file:{}", paths.join(",")),
            Some(composite),
        );
    }
    if let Some((path, sha)) = unrecognised.first() {
        return evidence(
            NO_ASSERTION.to_string(),
            format!("file:{path}"),
            Some(sha.clone()),
        );
    }
    if let Some((path, text, c)) = decls.first() {
        return evidence(
            text.clone(),
            format!("declared:{path}"),
            Some(sha256_hex(c.bytes)),
        );
    }
    evidence(NO_ASSERTION.to_string(), "none".to_string(), None)
}
