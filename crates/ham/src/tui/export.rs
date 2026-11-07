//! `/export`: a chat as a Markdown file. The module draws nothing and reads no event; the command router calls
//! [`to_markdown`] for the model on screen, [`resolve`] for the place the user typed, [`write`] for the file and [`notice`] for the line it shows.
//!
//! * **Structure is ours.** The title, the facts and every heading come from the model's structure ([`ChatModel::role_word`] is made of letters,
//!   digits and `. _ -`), never from block text. A block with no head (a message, a notice, a gap) is quoted with `> ` on every line, empty
//!   lines included, so a line of engine text such as `## you`, `---` or a table row stays inside its quote. A block with a head (a tool call,
//!   a diff, a thought, a card) puts the head in a code span and the body in a fence longer than any run of backticks inside it.
//! * **Literal where it could act.** In quoted text, raw HTML and comments (`<`), image starts (`![`) and link or footnote definitions
//!   (`[x]: ...`) are made literal with a backslash ([`neutralize`]), because a viewer would fetch an image, run HTML or let a definition
//!   retarget a link elsewhere in the file. Fenced code and inline code are left as they were. "Equal to the transcript" therefore means:
//!   the text after the quote marks and those backslashes.
//! * **Masked and clean.** Every piece goes through `display_text` (no escape sequence or control survives) and `redact_tokens` (presigned
//!   links, tokens and bearer headers become `[redacted]`).
//! * **The file.** [`write`] makes a temp file in the destination directory (mode 0600), then hard-links it to the final name, which fails
//!   instead of replacing a file that is there, then removes the temp name. A generated name that is taken gets `-2`, `-3`; a name the user
//!   typed that is taken is an error. Where the filesystem has no hard links the complete temp file is renamed onto a name that was just
//!   found free; a file that appears in that instant could be replaced.

use super::copy::size_text;
use ham_core::chat::{Block, ChatModel};
use ham_core::redact::redact_tokens;
use ham_core::sanitize::{display_line, display_text};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The most `-2`, `-3` ... names tried before giving up.
const MAX_SUFFIX: usize = 999;
/// The longest run id or stamp kept in a file name.
const NAME_PART_MAX: usize = 40;

/// Facts about the run, written at the top of the file.
#[derive(Clone, Copy, Debug)]
pub struct Meta<'a> {
    pub run_id: &'a str,
    pub model: Option<&'a str>,
    /// The time to print, already in the zone the screen shows (the module reads no clock).
    pub exported_at: &'a str,
}

fn clean_line(s: &str) -> String {
    redact_tokens(&display_line(s)).trim().to_string()
}

fn clean_text(s: &str) -> String {
    redact_tokens(&display_text(s))
}

fn longest_run(s: &str, c: char) -> usize {
    let mut best = 0;
    let mut cur = 0;
    for ch in s.chars() {
        if ch == c {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

/// `s` as inline code, with a delimiter longer than any backtick run in it, padded when it starts or ends with a backtick.
fn code_span(s: &str) -> String {
    let delim = "`".repeat(longest_run(s, '`') + 1);
    if s.starts_with('`') || s.ends_with('`') {
        format!("{delim} {s} {delim}")
    } else {
        format!("{delim}{s}{delim}")
    }
}

fn fence(body: &str, info: &str) -> String {
    let f = "`".repeat((longest_run(body, '`') + 1).max(3));
    format!("{f}{info}\n{body}\n{f}")
}

/// The number of backslashes just before char index `i`.
fn backslashes_before(chars: &[char], i: usize) -> usize {
    chars[..i].iter().rev().take_while(|c| **c == '\\').count()
}

/// One line of quoted text, outside fenced code, with the characters that could act made literal.
fn neutralize_line(line: &str, out: &mut String) {
    let chars: Vec<char> = line.chars().collect();
    // A link or footnote definition is global to the file even inside a quote.
    let lead = chars.iter().take_while(|c| **c == ' ').count();
    let definition = chars.get(lead) == Some(&'[') && line.contains("]:");
    // Backtick runs that are not escaped, and the ones that open an inline code span closed on this line.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' && backslashes_before(&chars, i).is_multiple_of(2) {
            let n = chars[i..].iter().take_while(|c| **c == '`').count();
            runs.push((i, n));
            i += n;
        } else {
            i += 1;
        }
    }
    let mut last_of_len: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (k, (_, n)) in runs.iter().enumerate() {
        last_of_len.insert(*n, k);
    }
    // Char ranges (start, end) of code spans, runs included.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < runs.len() {
        let (start, n) = runs[k];
        if last_of_len.get(&n).is_some_and(|last| *last > k) {
            let close = (k + 1..runs.len()).find(|j| runs[*j].1 == n).unwrap_or(k);
            spans.push((start, runs[close].0 + n));
            k = close + 1;
        } else {
            k += 1;
        }
    }
    let mut span = 0;
    for (i, c) in chars.iter().enumerate() {
        while span < spans.len() && spans[span].1 <= i {
            span += 1;
        }
        let in_code = span < spans.len() && spans[span].0 <= i;
        if !in_code {
            let escaped = backslashes_before(&chars, i) % 2 == 1;
            let image = *c == '!' && chars.get(i + 1) == Some(&'[');
            if (definition && i == lead) || (!escaped && (*c == '<' || image)) {
                out.push('\\');
            }
        }
        out.push(*c);
    }
}

/// Makes the characters of quoted engine text that a Markdown viewer would act on literal: `<` (HTML, comments, autolinks), `![` (images) and
/// a line that starts a link or footnote definition. Fenced code (backticks or tildes, with the longer-fence rule) and inline code spans are
/// left as they are. The number of lines does not change.
pub fn neutralize(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut open: Option<(char, usize)> = None;
    for (n, line) in text.split('\n').enumerate() {
        if n > 0 {
            out.push('\n');
        }
        let t = line.trim_start_matches(' ');
        let indent = line.len() - t.len();
        if let Some((c, len)) = open {
            let run = t.chars().take_while(|x| *x == c).count();
            if indent < 4 && run >= len && t[run..].trim().is_empty() {
                open = None;
            }
            out.push_str(line);
            continue;
        }
        if indent < 4
            && let Some(c) = t.chars().next().filter(|c| *c == '`' || *c == '~')
        {
            let run = t.chars().take_while(|x| *x == c).count();
            if run >= 3 && !(c == '`' && t[run..].contains('`')) {
                open = Some((c, run));
                out.push_str(line);
                continue;
            }
        }
        neutralize_line(line, &mut out);
    }
    out
}

fn push_block(out: &mut String, model: &ChatModel, b: &Block) {
    out.push_str(&format!("\n## {}\n", model.role_word(b)));
    let head = clean_line(&b.head);
    let body = clean_text(&b.body);
    if head.is_empty() {
        if body.is_empty() {
            return;
        }
        out.push('\n');
        for line in neutralize(&body).split('\n') {
            if line.is_empty() {
                out.push_str(">\n");
            } else {
                out.push_str(&format!("> {line}\n"));
            }
        }
        return;
    }
    out.push('\n');
    out.push_str(&code_span(&head));
    if let Some(status) = b
        .status
        .as_deref()
        .map(clean_line)
        .filter(|s| !s.is_empty())
    {
        out.push_str(&format!(" \u{b7} {}", code_span(&status)));
    }
    out.push('\n');
    if !body.is_empty() {
        let info = if b.kind == ham_core::chat::BlockKind::Diff {
            "diff"
        } else {
            "text"
        };
        out.push('\n');
        out.push_str(&fence(&body, info));
        out.push('\n');
    }
}

/// The chat as Markdown: a title, the run facts and every block in order.
pub fn to_markdown(model: &ChatModel, meta: &Meta<'_>) -> String {
    let mut out = String::from("# ham chat export\n\n");
    out.push_str(&format!("- Run: {}\n", clean_line(meta.run_id)));
    if let Some(m) = meta.model {
        out.push_str(&format!("- Model: {}\n", clean_line(m)));
    }
    out.push_str(&format!("- Exported: {}\n", clean_line(meta.exported_at)));
    if let Some(label) = model.label() {
        out.push_str(&format!("- Source: {label}\n"));
    }
    for b in &model.blocks {
        push_block(&mut out, model, b);
    }
    out
}

fn safe_part(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(NAME_PART_MAX)
        .collect()
}

/// `ham-chat-<run>-<stamp>.md`, with only letters, digits, `-` and `_` from the run id and the stamp.
pub fn file_name(run_id: &str, stamp: &str) -> String {
    let run = safe_part(run_id);
    let run = if run.is_empty() {
        "run".to_string()
    } else {
        run
    };
    let stamp = safe_part(stamp);
    if stamp.is_empty() {
        format!("ham-chat-{run}.md")
    } else {
        format!("ham-chat-{run}-{stamp}.md")
    }
}

/// Where `/export` writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Destination {
    /// A directory that exists; the file name is generated and made free.
    Dir(PathBuf),
    /// The exact file the user named; it must not exist.
    File(PathBuf),
}

/// Reads the place from what the user typed after `/export`: nothing is the default directory, a name ending in `.md` or `.markdown` is a
/// file, anything else a directory. `~` is the home directory. A relative path is relative to the directory `ham` was started in.
pub fn resolve(arg: Option<&str>, default_dir: &Path, home: Option<&Path>) -> Destination {
    let Some(raw) = arg.map(str::trim).filter(|a| !a.is_empty()) else {
        return Destination::Dir(default_dir.to_path_buf());
    };
    let path = match (raw, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (r, Some(h)) if r.starts_with("~/") => h.join(&r[2..]),
        _ => PathBuf::from(raw),
    };
    let is_file = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));
    if is_file {
        Destination::File(path)
    } else {
        Destination::Dir(path)
    }
}

fn temp_in(dir: &Path, content: &str) -> io::Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = dir.join(format!(
        ".ham-export-{}-{nanos}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    let done = f.write_all(content.as_bytes()).and_then(|()| f.sync_all());
    if let Err(e) = done {
        let _ = fs::remove_file(&path);
        return Err(e);
    }
    Ok(path)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::metadata(a), fs::symlink_metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

fn exists(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok()
}

/// Writes `content` to `dest` without replacing anything. Returns the path written.
pub fn write(dest: &Destination, run_id: &str, stamp: &str, content: &str) -> io::Result<PathBuf> {
    write_with(dest, run_id, stamp, content, &|from, to| {
        fs::hard_link(from, to)
    })
}

/// [`write`] with the way a name is claimed passed in (a test stands in for a filesystem without hard links).
pub fn write_with(
    dest: &Destination,
    run_id: &str,
    stamp: &str,
    content: &str,
    link: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    let (dir, candidates): (PathBuf, Vec<PathBuf>) = match dest {
        Destination::Dir(d) => {
            match fs::metadata(d) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        format!("{} is not a directory", d.display()),
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("the directory {} does not exist", d.display()),
                    ));
                }
                Err(e) => return Err(e),
            }
            let name = file_name(run_id, stamp);
            let stem = name.strip_suffix(".md").unwrap_or(&name).to_string();
            let mut all = vec![d.join(&name)];
            all.extend((2..=MAX_SUFFIX).map(|n| d.join(format!("{stem}-{n}.md"))));
            (d.clone(), all)
        }
        Destination::File(p) => {
            let parent = match p.parent() {
                Some(x) if !x.as_os_str().is_empty() => x.to_path_buf(),
                _ => PathBuf::from("."),
            };
            if !fs::metadata(&parent).is_ok_and(|m| m.is_dir()) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("the directory {} does not exist", parent.display()),
                ));
            }
            (parent, vec![p.clone()])
        }
    };
    let temp = temp_in(&dir, content)?;
    let result = claim(
        &temp,
        &candidates,
        matches!(dest, Destination::File(_)),
        link,
    );
    // After a rename the temp name is gone; after a hard link it is the second name of the file.
    if temp.exists() {
        let _ = fs::remove_file(&temp);
    }
    if result.is_ok() {
        // Best effort: make the new directory entry durable.
        if let Ok(d) = fs::File::open(&dir) {
            let _ = d.sync_all();
        }
    }
    result
}

fn claim(
    temp: &Path,
    candidates: &[PathBuf],
    explicit: bool,
    link: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> io::Result<PathBuf> {
    for final_path in candidates {
        match link(temp, final_path) {
            Ok(()) => return Ok(final_path.clone()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                // A network filesystem can answer "exists" to a request it carried out.
                if same_file(temp, final_path) {
                    return Ok(final_path.clone());
                }
            }
            Err(_) => {
                // No hard links here: rename the complete file onto a name that is free now.
                if !exists(final_path) {
                    fs::rename(temp, final_path)?;
                    return Ok(final_path.clone());
                }
            }
        }
        if explicit {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} already exists; nothing was written",
                    final_path.display()
                ),
            ));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every numbered name is taken; nothing was written",
    ))
}

/// The line the screen shows after an export.
pub fn notice(path: &Path, blocks: usize, bytes: usize) -> String {
    let noun = if blocks == 1 { "block" } else { "blocks" };
    format!(
        "Exported {blocks} {noun} ({}) to {}.",
        size_text(bytes),
        path.display()
    )
}
