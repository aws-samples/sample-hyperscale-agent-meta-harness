//! Facts agentd reads at the end of a run and records as it found them. Today one: the last entry of the run log
//! (`.ham/lifecycle/LOG.md`, the lifecycle pack's record of what was run) when it is a red run. The harness does not judge the work and
//! never uses a word that claims it; it writes down what the log says, so the card can show it beside the label. The lines are the
//! pack's (`RUN | <command> | exit <n> | pass <n or unknown> fail <n or unknown> | <note>` and `NOT RUN | <what> | <reason>`).
//!
//! The file belongs to the engine: no link is followed (not the file, not a folder above it) and only the tail is read. It sits in
//! `.ham/lifecycle` unless `.ham/records` names another folder ([`Records`]; the value `docs` gives `docs/lifecycle`), and when the repository
//! turned the records off nothing is read.

use crate::direct::records::Records;
use harness_protocol::direct::DirectObservation;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The run log, below the work tree, when no `.ham/records` names another folder.
pub const LOG_FILE: &str = ".ham/lifecycle/LOG.md";
/// How much of the end of the log is read.
const TAIL_BYTES: u64 = 256 * 1024;

/// Whether a logged count or code is not zero.
fn nonzero(text: &str) -> bool {
    text.parse::<i128>() != Ok(0)
}

/// The last entry of a run log (the last line that starts a `RUN` or `NOT RUN` entry), as an observation when it is a run with a
/// non-zero exit code or failed tests. `None` for a green last entry, a `NOT RUN` last entry, a line the format does not allow and a log
/// with no entry.
pub fn parse_last_entry(text: &str) -> Option<DirectObservation> {
    let line = text
        .lines()
        .map(str::trim_end)
        .rfind(|l| *l == "RUN" || l.starts_with("RUN |") || l.starts_with("NOT RUN"))?;
    let run = parse_run(line)?;
    let red = nonzero(run.code)
        || (run.failed != "unknown" && nonzero(run.failed) && !note_says_pass(run.note));
    red.then(|| DirectObservation::last_log_entry_red(run.cmd, run.code, Some(run.failed)))
}

/// The fields of a `RUN` line.
struct RunLine<'a> {
    cmd: &'a str,
    code: &'a str,
    failed: &'a str,
    note: &'a str,
}

/// Whether the note of a row says PASS for itself: the upper-case word PASS (a wrapper script that compares a failing suite with the failures the
/// base commit already has writes `NEW-FAILURES PASS`), not after `not`, `no`, `never` or the `t` of `n't`, and no upper-case word FAIL, FAILS,
/// FAILED or FAILING. The words are the letters and digits between other characters, so `NEW-FAILURES` is the words NEW and FAILURES. A row
/// whose note says so is a row whose own verdict is PASS, and a count of failures beside it is not read as a red run. The exit code is not
/// weighed against the note: a non-zero code is red whatever the note says.
/// The self-check script and the scorer read a row the same way; `fixtures/lifecycle/pass-notes.json` holds the cases all three read.
pub fn note_says_pass(note: &str) -> bool {
    let words: Vec<&str> = note
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    if words
        .iter()
        .any(|w| matches!(*w, "FAIL" | "FAILS" | "FAILED" | "FAILING"))
    {
        return false;
    }
    words.iter().enumerate().any(|(i, w)| {
        *w == "PASS"
            && !(i > 0
                && matches!(
                    words[i - 1].to_ascii_lowercase().as_str(),
                    "not" | "no" | "never" | "t"
                ))
    })
}

fn is_count(s: &str) -> bool {
    s == "unknown" || (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn is_code(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `RUN | <command> | exit <n> | pass <n or unknown> fail <n or unknown> | <note>`: the command is the shortest text that lets the rest
/// match, so a command may hold a ` | ` of its own.
fn parse_run(line: &str) -> Option<RunLine<'_>> {
    let rest = line.strip_prefix("RUN | ")?;
    rest.match_indices(" | exit ").find_map(|(at, sep)| {
        let cmd = &rest[..at];
        if cmd.is_empty() || cmd.starts_with(char::is_whitespace) {
            return None;
        }
        let (code, tail) = rest[at + sep.len()..].split_once(" | pass ")?;
        let (passed, tail) = tail.split_once(" fail ")?;
        let (failed, note) = tail.split_once(" | ")?;
        let note_ok = note.chars().next().is_some_and(|c| !c.is_whitespace());
        (is_code(code) && is_count(passed) && is_count(failed) && note_ok).then_some(RunLine {
            cmd,
            code,
            failed,
            note,
        })
    })
}

/// The end of the run log of the work tree `wt`, as [`parse_last_entry`] reads it. `None` when the records are off, or the file is missing,
/// is no regular file, or is reached through a link.
pub fn last_log_observation(wt: &Path) -> Option<DirectObservation> {
    last_log_observation_for(wt, &Records::read(wt))
}

/// [`last_log_observation`] with the records the run chose when it started (COMPAT-CAH-DATA: the folder is chosen once per run), so a folder the engine
/// made later in the other place cannot move the observation.
pub fn last_log_observation_for(wt: &Path, records: &Records) -> Option<DirectObservation> {
    let rel = records.log()?;
    let (folders, _) = rel.rsplit_once('/')?;
    let mut at = String::new();
    for part in folders.split('/') {
        if !at.is_empty() {
            at.push('/');
        }
        at.push_str(part);
        let meta = std::fs::symlink_metadata(wt.join(&at)).ok()?;
        if !meta.file_type().is_dir() {
            return None;
        }
    }
    let path = wt.join(&rel);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(&path)
        .ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .ok()?;
    let mut tail = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
    parse_last_entry(&String::from_utf8_lossy(&tail))
}
