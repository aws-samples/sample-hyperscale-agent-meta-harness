//! The rules of the archive filter as pure functions: which names are refused, which directories and files are left out, and the exact
//! boundaries (1 MiB, 8,000 bytes, 8 KiB, 2,000 characters). The reason strings are the fixed table in [`reason`](super::reason).

use super::reason;

/// A file larger than this is not kept (and is not read into memory).
pub(crate) const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// A NUL byte in this many leading bytes makes a file binary.
pub(crate) const BINARY_PROBE: usize = 8000;
/// A private-key block in this many leading bytes makes a text file a key file.
pub(crate) const KEY_PROBE: usize = 8192;
/// A line longer than this many characters makes a text file minified.
pub(crate) const MAX_LINE_CHARS: usize = 2000;
/// The longest entry name the filter looks at.
pub(crate) const MAX_NAME_BYTES: usize = 4096;
/// A dropped path is shown at most this many characters long.
const MAX_SHOWN_PATH: usize = 256;

/// Directories that are never kept, at any depth.
pub(crate) const EXCLUDED_DIRS: [&str; 5] = [".git", ".github", "node_modules", "target", "dist"];

/// Validates an entry name. On success: the name without one trailing slash. On failure: the reason the entry is dropped.
pub(crate) fn check_name(raw: &[u8]) -> Result<&str, &'static str> {
    if raw.len() > MAX_NAME_BYTES {
        return Err(reason::NAME_TOO_LONG);
    }
    if raw.contains(&0) {
        return Err(reason::NUL_IN_NAME);
    }
    let name = std::str::from_utf8(raw).map_err(|_| reason::INVALID_UTF8)?;
    if name.contains('\\') {
        return Err(reason::BACKSLASH);
    }
    if name.chars().any(is_unsupported_name_char) {
        return Err(reason::UNSUPPORTED_CHARACTER);
    }
    if name.starts_with('/') {
        return Err(reason::ABSOLUTE_PATH);
    }
    let trimmed = name.strip_suffix('/').unwrap_or(name);
    for component in trimmed.split('/') {
        match component {
            "" => return Err(reason::EMPTY_COMPONENT),
            ".." => return Err(reason::DOTDOT),
            "." => return Err(reason::DOT_COMPONENT),
            _ => {}
        }
    }
    Ok(trimmed)
}

/// A character a stored name must not hold: a control character (the item manifest and the extractor refuse it in a path), or one that
/// changes how a name is drawn without being drawn itself (a zero-width character, a direction mark, an embedding, an override or an
/// isolate, a line or paragraph separator, the byte-order mark). The set is the one `scan::clean_text` removes from a page, with the
/// direction marks and the separators added.
pub(crate) fn is_unsupported_name_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206f}'
                | '\u{feff}'
        )
}

/// A name as it is shown in the dropped list: printable, at most 256 characters, never a character a stored name must not hold or a backslash.
pub(crate) fn shown(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .map(|c| {
            if is_unsupported_name_char(c) || c == '\\' {
                '?'
            } else {
                c
            }
        })
        .take(MAX_SHOWN_PATH)
        .collect()
}

/// What the dropped list holds for a name: its printable form, and a word for a name that is empty (the manifest needs a path in every
/// dropped entry).
pub(crate) fn listed(path: &str) -> String {
    let s = shown(path.as_bytes());
    if s.is_empty() {
        "(empty name)".to_string()
    } else {
        s
    }
}

/// Whether `rel` (a path relative to the repository root) is inside one of `paths`. An empty list keeps everything. A prefix that ends in a
/// slash is a directory; any other prefix names a file or a directory.
pub(crate) fn matches_paths(rel: &str, paths: &[String]) -> bool {
    paths.is_empty()
        || paths.iter().any(|p| {
            if p.ends_with('/') {
                rel.starts_with(p.as_str())
            } else {
                rel == p
                    || (rel.starts_with(p.as_str()) && rel.as_bytes().get(p.len()) == Some(&b'/'))
            }
        })
}

pub(crate) fn is_env_file(base: &str) -> bool {
    base == ".env" || base.starts_with(".env.")
}

pub(crate) fn is_key_name(base: &str) -> bool {
    matches!(
        base.to_ascii_lowercase().as_str(),
        "id_rsa" | "id_ed25519" | "id_ecdsa" | "id_dsa"
    )
}

pub(crate) fn is_minified_name(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    lower.ends_with(".min.js") || lower.ends_with(".min.css") || lower.ends_with(".map")
}

pub(crate) fn looks_binary(data: &[u8]) -> bool {
    data[..data.len().min(BINARY_PROBE)].contains(&0)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A `-----BEGIN ... PRIVATE KEY-----` line within the first 8 KiB.
pub(crate) fn has_private_key_block(data: &[u8]) -> bool {
    const BEGIN: &[u8] = b"-----BEGIN ";
    const TAIL: &[u8] = b"PRIVATE KEY-----";
    let window = &data[..data.len().min(KEY_PROBE)];
    let mut from = 0;
    while let Some(i) = find(&window[from..], BEGIN) {
        let start = from + i;
        let line_end = window[start..]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(window.len(), |n| start + n);
        if find(&window[start..line_end], TAIL).is_some() {
            return true;
        }
        from = start + BEGIN.len();
    }
    false
}

/// A line of more than 2,000 characters.
pub(crate) fn has_long_line(data: &[u8]) -> bool {
    data.split(|b| *b == b'\n').any(|line| {
        line.len() > MAX_LINE_CHARS
            && String::from_utf8_lossy(line).chars().count() > MAX_LINE_CHARS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_in_a_fixed_order() {
        assert_eq!(check_name(b"top/a.txt"), Ok("top/a.txt"));
        assert_eq!(check_name(b"top/dir/"), Ok("top/dir"));
        assert_eq!(check_name(b"../outside/x"), Err(reason::DOTDOT));
        assert_eq!(check_name(b"/outside/x"), Err(reason::ABSOLUTE_PATH));
        assert_eq!(check_name(b"a/../b"), Err(reason::DOTDOT));
        assert_eq!(check_name(b"a//b"), Err(reason::EMPTY_COMPONENT));
        assert_eq!(check_name(b"a/./b"), Err(reason::DOT_COMPONENT));
        assert_eq!(check_name(b""), Err(reason::EMPTY_COMPONENT));
    }

    #[test]
    fn path_prefixes_match_whole_components() {
        let p = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(matches_paths("a/b", &[]));
        assert!(matches_paths("src/a.rs", &p(&["src/"])));
        assert!(!matches_paths("srcx/a.rs", &p(&["src/"])));
        assert!(matches_paths("docs", &p(&["docs"])));
        assert!(matches_paths("docs/x", &p(&["docs"])));
        assert!(!matches_paths("docsx/y", &p(&["docs"])));
    }

    #[test]
    fn the_key_block_must_be_on_one_line_within_the_window() {
        let begin = ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat();
        assert!(has_private_key_block(begin.as_bytes()));
        assert!(!has_private_key_block(b"-----BEGIN CERTIFICATE-----"));
        let split = ["-----BEGIN ", "x\nPRIVATE KEY-----"].concat();
        assert!(!has_private_key_block(split.as_bytes()));
    }
}
