//! The `code_search` tool: a ripgrep search of the run's work tree, run as the engine user, that answers
//! `path:line:text` lines. It avoids two weaknesses of the engine's own search: one match per file, and an answer that leaves out the matching
//! line. No network, no AWS, no credential.
//!
//! What the model sees (the tool table, `harness_protocol::tools`, holds the input; this file the answer):
//! - Matches: `path:line:text`, in path order, each file's lines in line order. Context lines (`context` > 0) are `path-line-text` and groups are
//!   separated by `--`, as ripgrep prints them. A line over 300 characters is cut to a window of 300 that holds the first match, with `…` at a cut
//!   end; control characters other than tab are shown as U+FFFD; text that is not UTF-8 is shown lossily. Paths under the work tree are relative to
//!   it, paths under the context directory are absolute.
//! - `filesOnly`: the names of the matching files. `count`: `path:matching lines` per file and a `total:` line.
//! - Nothing found: `no matches`. A cut is said in a note line that starts with `[` and names the reason: `maxResults` (the total, default 50),
//!   `maxPerFile` (default 10, said once per file that went over), the size of one answer ([`ANSWER_BUDGET`]), a message over the limit that was
//!   skipped, or a search that could not read every file. The order is ripgrep's `--sort=path`, so the same call returns the same first matches.
//!
//! How it runs:
//! - Argv is built from the typed input and nothing else: `-e <pattern>` and `--glob=<g>` are single arguments, so a value that starts with a
//!   dash is never a flag; the paths come after `--`, already resolved by [`confine`] (canonical, sorted, deduplicated, none inside another).
//!   `--no-config` and `--no-follow` are always there; there is no `-L`.
//! - The child is started through the launch spec of the [`ToolContext`] (the engine's user, the scrubbed environment, the engine's search
//!   path), in its own process group, with its standard error sent to `/dev/null`. ripgrep repeats a pattern in the text of a regular-expression
//!   error and a path in the text of a file error, so that text is never kept: it is not read, not logged and not written to a file, and a failure
//!   is told in a fixed phrase chosen from the exit status and from whether ripgrep finished its search (the `summary` message).
//! - Limits: 10 s of wall time, then the group is killed (`error: timeout`); four searches at once, the fifth is `error: busy`; the group is
//!   killed when the call is cancelled and when the future is dropped; a `--json` message over 4 MiB is skipped.
//! - Nothing here logs. The server logs the tool, the word of the result, the time and the size.

#[path = "confine.rs"]
pub mod confine;

use crate::BoxFuture;
use crate::direct::tools::backend::{
    CancelToken, FailureWord, GroupGuard, ToolBackend, ToolContext, ToolOutput, kill_process_group,
};
use crate::direct::tools::result::DEFAULT_RESULT_CHARS;
use crate::engine;
use confine::{Refusal, Roots, Strategy};
use harness_protocol::tools::{CodeSearchInput, ToolName};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::sync::Semaphore;

/// Characters of one result line's text.
const LINE_CHARS: usize = 300;
/// Characters of a long line shown before its first match.
const BEFORE_MATCH: usize = 40;
/// The longest path or count entry of the `filesOnly` and `count` modes, in bytes.
const MAX_ENTRY_BYTES: usize = 64 * 1024;
/// The text of an answer is held to this many characters of its serialized form, so that the server's own cut (at the profile's 7,500) never
/// takes the notes that say why the answer is short. The rest of the 7,500 is the JSON around the text and the notes.
pub const ANSWER_BUDGET: usize = 7_000;
const _: () = assert!(ANSWER_BUDGET + 400 <= DEFAULT_RESULT_CHARS);

/// The limits of one tool instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Wall time of one search, from the start of the child.
    pub wall: Duration,
    /// Searches at once.
    pub concurrent: usize,
    /// The longest `--json` message that is read; a longer one is skipped.
    pub max_message_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            wall: Duration::from_secs(10),
            concurrent: 4,
            max_message_bytes: 4 * 1024 * 1024,
        }
    }
}

/// The `code_search` backend.
#[derive(Debug)]
pub struct CodeSearch {
    program: String,
    limits: Limits,
    permits: Arc<Semaphore>,
}

impl Default for CodeSearch {
    fn default() -> Self {
        Self::new()
    }
}

impl CodeSearch {
    /// `rg`, found on the search path of the launch spec (the engine's).
    pub fn new() -> Self {
        Self::with_program("rg")
    }

    /// Another program (an absolute path, for a test or a build host whose ripgrep is not on the path).
    pub fn with_program(program: impl Into<String>) -> Self {
        let limits = Limits::default();
        Self {
            program: program.into(),
            limits,
            permits: Arc::new(Semaphore::new(limits.concurrent)),
        }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.permits = Arc::new(Semaphore::new(limits.concurrent));
        self.limits = limits;
        self
    }
}

impl ToolBackend for CodeSearch {
    fn name(&self) -> ToolName {
        ToolName::CodeSearch
    }

    fn call<'a>(
        &'a self,
        ctx: &'a ToolContext,
        input: Value,
        cancel: CancelToken,
    ) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { self.run(ctx, input, cancel).await })
    }
}

// ---- the call ------------------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Matches,
    Files,
    Count,
}

/// What was asked, read once.
struct Ask {
    input: CodeSearchInput,
    mode: Mode,
    max_results: usize,
    max_per_file: usize,
    context: usize,
}

fn usage(detail: &str) -> ToolOutput {
    ToolOutput::failed(FailureWord::Usage, detail)
}

fn unavailable(detail: &str) -> ToolOutput {
    ToolOutput::failed(FailureWord::Unavailable, detail)
}

/// How the reading ended.
enum Ended {
    Read(std::io::Result<Finished>),
    Cancelled,
    TimedOut,
}

impl CodeSearch {
    async fn run(&self, ctx: &ToolContext, arguments: Value, cancel: CancelToken) -> ToolOutput {
        let input = match CodeSearchInput::from_arguments(&arguments) {
            Ok(i) => i,
            Err(e) => return usage(&e.to_string()),
        };
        if input.pattern.contains(['\n', '\r']) {
            return usage("pattern: must be one line");
        }
        let Ok(_permit) = self.permits.clone().try_acquire_owned() else {
            return ToolOutput::failed(
                FailureWord::Busy,
                "the searches that may run at once are running; retry shortly",
            );
        };
        let Ok(roots) = Roots::new(&ctx.work_tree, ctx.context_dir.as_deref()) else {
            return unavailable("the work tree is not available");
        };
        let paths = match resolve_paths(&roots, &input.paths) {
            Ok(p) => p,
            Err(out) => return out,
        };
        let ask = Ask {
            mode: match (input.files_only(), input.count()) {
                (true, _) => Mode::Files,
                (_, true) => Mode::Count,
                _ => Mode::Matches,
            },
            max_results: input.max_results() as usize,
            max_per_file: input.max_per_file() as usize,
            context: input.context() as usize,
            input,
        };

        let mut spec = ctx.launch_spec(self.argv(&ask, &paths), "code-search.stderr");
        // ripgrep repeats the pattern and the paths in its error text; none of it is wanted anywhere.
        spec.stderr_log = PathBuf::from("/dev/null");
        let mut child = match engine::spawn(&spec) {
            Ok(c) => c,
            Err(_) => return unavailable("ripgrep could not be started"),
        };
        let Some(pid) = child.id() else {
            return unavailable("ripgrep could not be started");
        };
        let mut guard = GroupGuard::new(pid);
        drop(child.stdin.take());
        let Some(stdout) = child.stdout.take() else {
            kill_process_group(pid).await;
            let _ = child.wait().await;
            guard.disarm();
            return unavailable("ripgrep could not be started");
        };
        let mut reader = BufReader::with_capacity(64 * 1024, stdout);

        let mut out = Out::new();
        let wall = tokio::time::sleep(self.limits.wall);
        tokio::pin!(wall);
        let ended = tokio::select! {
            r = read_all(&mut reader, &ask, &roots, &mut out, self.limits.max_message_bytes) => Ended::Read(r),
            () = cancel.cancelled() => Ended::Cancelled,
            () = &mut wall => Ended::TimedOut,
        };
        let finished = match ended {
            Ended::Read(Ok(finished)) => finished,
            Ended::Read(Err(_)) => {
                end_child(&mut child, pid, &mut guard).await;
                return unavailable("the search output could not be read");
            }
            Ended::Cancelled => {
                end_child(&mut child, pid, &mut guard).await;
                return unavailable("cancelled");
            }
            Ended::TimedOut => {
                end_child(&mut child, pid, &mut guard).await;
                return ToolOutput::failed(
                    FailureWord::Timeout,
                    format!(
                        "the search ran past the limit of {:?}; narrow the paths or the pattern",
                        self.limits.wall
                    ),
                );
            }
        };

        // The reading is over: ripgrep closed its output, or we have all we will show and stop it.
        let status = if finished.stopped_early {
            end_child(&mut child, pid, &mut guard).await;
            None
        } else {
            let waited = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            match waited {
                Ok(Ok(status)) => {
                    guard.disarm();
                    Some(status)
                }
                _ => {
                    end_child(&mut child, pid, &mut guard).await;
                    return unavailable("ripgrep did not finish");
                }
            }
        };
        let mut incomplete = false;
        if let Some(status) = status {
            match status.code() {
                Some(0 | 1) => {}
                Some(2) if finished.summary || out.has_entries() => incomplete = true,
                Some(2) => {
                    return usage(
                        "pattern or glob: not accepted by ripgrep (check the regular expression and the glob syntax; set fixed to search for literal text)",
                    );
                }
                Some(126 | 127) => return unavailable("ripgrep could not be started"),
                Some(_) => return unavailable("ripgrep failed"),
                None => return unavailable("the search was stopped by the system"),
            }
        }
        out.finish(&ask, incomplete)
    }

    /// The argument vector of ripgrep for this call; see the module text for each part.
    fn argv(&self, ask: &Ask, paths: &[PathBuf]) -> Vec<String> {
        let mut a: Vec<String> = vec![self.program.clone()];
        let mut push = |s: &str| a.push(s.to_string());
        match ask.mode {
            Mode::Matches => push("--json"),
            Mode::Files => {
                push("--files-with-matches");
                push("--null");
                push("--color=never");
            }
            Mode::Count => {
                push("--count");
                push("--null");
                push("--with-filename");
                push("--color=never");
            }
        }
        for flag in [
            "--no-config",
            "--no-follow",
            "--max-filesize=2M",
            "--sort=path",
            "--case-sensitive",
        ] {
            push(flag);
        }
        if ask.mode == Mode::Matches {
            // One more than is shown, so that a file that has more is known to.
            push(&format!("--max-count={}", ask.max_per_file + 1));
            if ask.context > 0 {
                push(&format!("--context={}", ask.context));
            }
        }
        if ask.input.fixed() {
            push("--fixed-strings");
        }
        if ask.input.include_hidden() {
            push("--hidden");
        }
        if ask.input.include_ignored() {
            push("--no-ignore");
        }
        // First, so that a glob of the caller's comes later and can bring the directory back.
        push("--glob=!.git");
        for g in &ask.input.glob {
            push(&format!("--glob={g}"));
        }
        push("-e");
        push(&ask.input.pattern);
        push("--");
        for p in paths {
            a.push(p.display().to_string());
        }
        a
    }
}

/// Ends the child: its process group is killed and the child is waited for (before the number can be given to another process).
async fn end_child(child: &mut tokio::process::Child, pid: u32, guard: &mut GroupGuard) {
    kill_process_group(pid).await;
    let _ = child.wait().await;
    guard.disarm();
}

/// The entries of `paths` resolved to canonical paths, sorted, without duplicates and without any path inside another; the work tree when
/// there are none.
fn resolve_paths(roots: &Roots, entries: &[String]) -> Result<Vec<PathBuf>, ToolOutput> {
    if entries.is_empty() {
        return Ok(vec![roots.work().to_path_buf()]);
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match roots.resolve(entry, Strategy::Auto) {
            Ok(r) => found.push(r.path),
            Err(Refusal::Outside) => {
                return Err(ToolOutput::failed(
                    FailureWord::Refused,
                    "paths: an entry is outside the work tree and the context directory",
                ));
            }
            Err(Refusal::Missing) => return Err(usage("paths: an entry does not exist")),
            Err(Refusal::Unreadable | Refusal::NoOpenat2) => {
                return Err(ToolOutput::failed(
                    FailureWord::Refused,
                    "paths: an entry could not be opened",
                ));
            }
        }
    }
    found.sort();
    found.dedup();
    let all = found.clone();
    found.retain(|p| !all.iter().any(|q| q != p && p.starts_with(q)));
    Ok(found)
}

// ---- reading ripgrep's output --------------------------------------------------------------------------------------------------------

/// What the reading found out besides the entries.
struct Finished {
    /// ripgrep printed its `summary` message: the search ran to its end.
    summary: bool,
    /// The reading stopped because nothing more would be shown.
    stopped_early: bool,
}

async fn read_all<R: AsyncBufRead + Unpin>(
    r: &mut R,
    ask: &Ask,
    roots: &Roots,
    out: &mut Out,
    max_message: usize,
) -> std::io::Result<Finished> {
    match ask.mode {
        Mode::Matches => read_matches(r, ask, roots, out, max_message).await,
        Mode::Files => read_entries(r, ask, roots, out, b'\0').await,
        Mode::Count => read_entries(r, ask, roots, out, b'\n').await,
    }
}

/// One line of the stream.
enum Line {
    Whole,
    Oversize,
    Eof,
}

/// Reads up to the next `delim` into `buf` (without it). A line over `cap` bytes is dropped as it is read, so memory stays at `cap`.
async fn read_delimited<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    delim: u8,
    cap: usize,
) -> std::io::Result<Line> {
    buf.clear();
    let mut oversize = false;
    let mut any = false;
    loop {
        let (used, done) = {
            let chunk = r.fill_buf().await?;
            if chunk.is_empty() {
                return Ok(match (any, oversize) {
                    (false, _) => Line::Eof,
                    (true, true) => Line::Oversize,
                    (true, false) => Line::Whole,
                });
            }
            any = true;
            let (take, done) = match chunk.iter().position(|b| *b == delim) {
                Some(i) => (i, true),
                None => (chunk.len(), false),
            };
            if !oversize {
                if buf.len() + take > cap {
                    oversize = true;
                    buf.clear();
                } else {
                    buf.extend_from_slice(&chunk[..take]);
                }
            }
            (if done { take + 1 } else { take }, done)
        };
        r.consume(used);
        if done {
            return Ok(if oversize {
                Line::Oversize
            } else {
                Line::Whole
            });
        }
    }
}

fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The bytes of ripgrep's `{"text": ...}` or `{"bytes": <base64>}`.
fn arbitrary_data(v: &Value) -> Option<Vec<u8>> {
    if let Some(t) = v.get("text").and_then(Value::as_str) {
        return Some(t.as_bytes().to_vec());
    }
    decode_base64(v.get("bytes").and_then(Value::as_str)?)
}

/// Control characters other than tab become U+FFFD, so that a line of a file cannot move the cursor, end the line early or paint the screen.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// The path as the model should name it: relative to the work tree, else as it is.
fn display_path(raw: &[u8], roots: &Roots) -> String {
    let p = Path::new(OsStr::from_bytes(raw));
    let shown = match p.strip_prefix(roots.work()) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => p.to_string_lossy().into_owned(),
    };
    clean(&shown)
}

/// A line of text for the answer: at most [`LINE_CHARS`] characters; a match line is windowed so that its first match is in it.
fn show_line(text: &str, match_at: Option<usize>) -> String {
    let text = clean(text.trim_end_matches(['\n', '\r']));
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= LINE_CHARS {
        return text;
    }
    let start = match_at
        .map_or(0, |m| m.saturating_sub(BEFORE_MATCH))
        .min(chars.len());
    let lead = usize::from(start > 0);
    let room = LINE_CHARS - lead;
    let mut end = (start + room).min(chars.len());
    if end < chars.len() {
        end = (start + room - 1).min(chars.len());
    }
    let mut shown = String::new();
    if lead == 1 {
        shown.push('…');
    }
    shown.extend(&chars[start..end]);
    if end < chars.len() {
        shown.push('…');
    }
    shown
}

/// What a character costs in the serialized JSON form of the answer, in UTF-16 units (the rule of `result.rs`).
fn json_units(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf16(),
        })
        .sum()
}

/// One line of a file that is to be shown: a match, or a line of context.
struct Item {
    line: u64,
    text: String,
    is_match: bool,
}

/// What `--json` said about one file until its `end`.
#[derive(Default)]
struct FileAcc {
    path: String,
    /// Matching lines seen, shown or not.
    seen: usize,
    /// More matching lines than `maxPerFile`.
    overflow: bool,
    items: Vec<Item>,
}

/// The answer as it is built.
struct Out {
    lines: Vec<String>,
    /// Matches (or files) in the answer.
    shown: usize,
    /// Characters used by `lines`, counted as serialized.
    used: usize,
    /// The last line printed, for the separator between groups.
    last: Option<(String, u64)>,
    units: usize,
    capped_files: usize,
    skipped_long: usize,
    stop: Option<Stop>,
    /// Count mode: the matching lines of the files listed.
    count_total: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stop {
    MaxResults,
    Size,
}

impl Out {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            shown: 0,
            used: 0,
            last: None,
            units: 0,
            capped_files: 0,
            skipped_long: 0,
            stop: None,
            count_total: 0,
        }
    }

    fn has_entries(&self) -> bool {
        !self.lines.is_empty()
    }

    /// Adds a file's kept matches, each with its context, as units; false when the answer is full.
    fn push_file(&mut self, file: FileAcc, ask: &Ask) -> bool {
        let n = ask.context as u64;
        let matches: Vec<&Item> = file.items.iter().filter(|i| i.is_match).collect();
        let owner = |c: &Item| -> Option<usize> {
            let mut best: Option<(u64, usize)> = None;
            for (k, m) in matches.iter().enumerate() {
                let d = m.line.abs_diff(c.line);
                if d <= n && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, k));
                }
            }
            best.map(|(_, k)| k)
        };
        let mut owned: Vec<Vec<&Item>> = vec![Vec::new(); matches.len()];
        for c in file.items.iter().filter(|i| !i.is_match) {
            if let Some(k) = owner(c) {
                owned[k].push(c);
            }
        }
        for (k, m) in matches.iter().enumerate() {
            if self.shown == ask.max_results {
                self.stop = Some(Stop::MaxResults);
                return false;
            }
            let mut unit: Vec<&Item> = owned[k].clone();
            unit.push(m);
            unit.sort_by_key(|i| i.line);
            let rendered: Vec<(String, u64)> = unit
                .iter()
                .map(|i| {
                    let sep = if i.is_match { ':' } else { '-' };
                    (
                        format!("{}{sep}{}{sep}{}", file.path, i.line, i.text),
                        i.line,
                    )
                })
                .collect();
            let first_line = rendered.first().map_or(0, |r| r.1);
            let separator = ask.context > 0
                && self
                    .last
                    .as_ref()
                    .is_some_and(|(p, l)| *p != file.path || first_line > l + 1);
            let cost: usize = rendered
                .iter()
                .map(|(t, _)| json_units(t) + 2)
                .sum::<usize>()
                + if separator { 4 } else { 0 };
            if self.units > 0 && self.used + cost > ANSWER_BUDGET {
                self.stop = Some(Stop::Size);
                return false;
            }
            if separator {
                self.lines.push("--".to_string());
            }
            for (text, line) in &rendered {
                self.lines.push(text.clone());
                self.last = Some((file.path.clone(), *line));
            }
            self.used += cost;
            self.units += 1;
            self.shown += 1;
        }
        if file.overflow {
            self.capped_files += 1;
        }
        true
    }

    /// The answer: the lines, then the notes.
    fn finish(mut self, ask: &Ask, incomplete: bool) -> ToolOutput {
        let thing = if ask.mode == Mode::Matches {
            "matches"
        } else {
            "files"
        };
        let mut notes: Vec<String> = Vec::new();
        match self.stop {
            Some(Stop::MaxResults) => notes.push(format!(
                "[stopped at {} {thing} (maxResults); narrow the pattern, paths or glob, or raise maxResults]",
                ask.max_results
            )),
            Some(Stop::Size) => notes.push(format!(
                "[stopped at the size limit of one answer after {} {thing}; narrow the pattern, paths or glob]",
                self.shown
            )),
            None => {}
        }
        if self.capped_files > 0 {
            notes.push(format!(
                "[maxPerFile {} reached in {} file{}; raise maxPerFile or narrow paths]",
                ask.max_per_file,
                self.capped_files,
                if self.capped_files == 1 { "" } else { "s" }
            ));
        }
        if self.skipped_long > 0 {
            notes.push(format!(
                "[{} line{} too long to show {} skipped]",
                self.skipped_long,
                if self.skipped_long == 1 { "" } else { "s" },
                if self.skipped_long == 1 {
                    "was"
                } else {
                    "were"
                }
            ));
        }
        if ask.mode == Mode::Count && self.stop.is_none() && !self.lines.is_empty() {
            self.lines.push(format!(
                "total: {} matching lines in {} files",
                self.count_total, self.shown
            ));
        }
        if incomplete {
            notes.push("[search incomplete: some results may be missing]".to_string());
        }
        let truncated = self.stop.is_some() || self.capped_files > 0 || self.skipped_long > 0;
        if self.lines.is_empty() {
            self.lines.push("no matches".to_string());
        }
        self.lines.extend(notes);
        let text = self.lines.join("\n");
        if truncated {
            ToolOutput::shortened(text)
        } else {
            ToolOutput::text(text)
        }
    }
}

/// `--json`: begin, match, context, end per file, and one summary.
async fn read_matches<R: AsyncBufRead + Unpin>(
    r: &mut R,
    ask: &Ask,
    roots: &Roots,
    out: &mut Out,
    max_message: usize,
) -> std::io::Result<Finished> {
    let mut buf = Vec::new();
    let mut files: BTreeMap<String, FileAcc> = BTreeMap::new();
    let mut summary = false;
    loop {
        match read_delimited(r, &mut buf, b'\n', max_message).await? {
            Line::Eof => break,
            Line::Oversize => {
                out.skipped_long += 1;
                continue;
            }
            Line::Whole => {}
        }
        let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        let data = &v["data"];
        match v["type"].as_str() {
            Some(kind @ ("match" | "context")) => {
                let Some(raw_path) = data.get("path").and_then(arbitrary_data) else {
                    continue;
                };
                let path = display_path(&raw_path, roots);
                let Some(raw_line) = data.get("lines").and_then(arbitrary_data) else {
                    continue;
                };
                let Some(line_no) = data["line_number"].as_u64() else {
                    continue;
                };
                let file = files.entry(path.clone()).or_insert_with(|| FileAcc {
                    path,
                    ..FileAcc::default()
                });
                let text = String::from_utf8_lossy(&raw_line).into_owned();
                if kind == "match" {
                    file.seen += 1;
                    if file.seen <= ask.max_per_file {
                        let at = data["submatches"][0]["start"].as_u64().map(|s| {
                            let cut = (s as usize).min(raw_line.len());
                            String::from_utf8_lossy(&raw_line[..cut]).chars().count()
                        });
                        file.items.push(Item {
                            line: line_no,
                            text: show_line(&text, at),
                            is_match: true,
                        });
                        continue;
                    }
                    file.overflow = true;
                    // Past the cap a matching line is only context for the last match that is shown, and only close to it.
                    let near = file
                        .items
                        .iter()
                        .rev()
                        .find(|i| i.is_match)
                        .is_some_and(|m| line_no <= m.line + ask.context as u64);
                    if ask.context > 0 && near {
                        file.items.push(Item {
                            line: line_no,
                            text: show_line(&text, None),
                            is_match: false,
                        });
                    }
                } else {
                    file.items.push(Item {
                        line: line_no,
                        text: show_line(&text, None),
                        is_match: false,
                    });
                }
            }
            Some("end") => {
                let Some(raw_path) = data.get("path").and_then(arbitrary_data) else {
                    continue;
                };
                if let Some(file) = files.remove(&display_path(&raw_path, roots))
                    && !out.push_file(file, ask)
                {
                    return Ok(Finished {
                        summary,
                        stopped_early: true,
                    });
                }
            }
            Some("summary") => summary = true,
            _ => {}
        }
    }
    for file in std::mem::take(&mut files).into_values() {
        if !out.push_file(file, ask) {
            return Ok(Finished {
                summary,
                stopped_early: true,
            });
        }
    }
    Ok(Finished {
        summary,
        stopped_early: false,
    })
}

/// `filesOnly` (`path\0`) and `count` (`path\0count\n`).
async fn read_entries<R: AsyncBufRead + Unpin>(
    r: &mut R,
    ask: &Ask,
    roots: &Roots,
    out: &mut Out,
    delim: u8,
) -> std::io::Result<Finished> {
    let mut buf = Vec::new();
    loop {
        match read_delimited(r, &mut buf, delim, MAX_ENTRY_BYTES).await? {
            Line::Eof => break,
            Line::Oversize => continue,
            Line::Whole => {}
        }
        let (path, count) = if ask.mode == Mode::Count {
            let Some(at) = buf.iter().rposition(|b| *b == 0) else {
                continue;
            };
            let n = std::str::from_utf8(&buf[at + 1..])
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
            let Some(n) = n else { continue };
            (&buf[..at], Some(n))
        } else {
            (&buf[..], None)
        };
        if out.shown == ask.max_results {
            out.stop = Some(Stop::MaxResults);
            return Ok(Finished {
                summary: false,
                stopped_early: true,
            });
        }
        let shown = display_path(path, roots);
        let text = match count {
            Some(n) => {
                out.count_total += n;
                format!("{shown}:{n}")
            }
            None => shown,
        };
        let cost = json_units(&text) + 2;
        if out.units > 0 && out.used + cost > ANSWER_BUDGET {
            out.stop = Some(Stop::Size);
            return Ok(Finished {
                summary: false,
                stopped_early: true,
            });
        }
        out.used += cost;
        out.units += 1;
        out.shown += 1;
        out.lines.push(text);
    }
    Ok(Finished {
        summary: false,
        stopped_early: false,
    })
}
