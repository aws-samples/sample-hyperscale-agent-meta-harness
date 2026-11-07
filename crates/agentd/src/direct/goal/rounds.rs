//! The rounds log of the goal loop: one JSON line when an engine run starts and one when it ends, written by the observer
//! plugin into the engine's data directory. The goal plugin queues its follow-up prompts itself, so those runs have no `session/prompt`
//! response; this file is the only place that shows where each round begins and ends and how it ended.
//!
//! The engine user can write the file, so it is read as untrusted data: opened without following a link, never more than a bounded piece at
//! a time, each line bounded, and a file that shrank or changed identity is "damaged" (the supervisor then falls back to the ACP stream, the
//! silence watchdog and the deadline). It steers and measures the loop; the done claim rests on the ACP stream, which the model cannot write.

use rustix::fs::{FileType, Mode, OFlags, fstat, open};
use serde_json::Value;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

/// The most bytes one poll reads.
pub const POLL_BYTES: usize = 256 * 1024;
/// The longest line kept; a longer one is counted as bad and skipped to its end.
pub const MAX_LINE: usize = 64 * 1024;
/// The most characters of a round's final text kept (the observer writes at most 1,500).
pub const MAX_TEXT_CHARS: usize = 2000;
/// The most characters of a blocked line shown in words a person reads.
pub const SHOWN_CHARS: usize = 120;

/// How one engine run ended, from its end line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundEnd {
    pub n: u64,
    /// The engine's own word for the end of the run (`completed`, `aborted`, `failed`, ...); `unknown` when the line has none.
    pub status: String,
    /// Model calls in the run; 1 is a text answer with no tool call.
    pub iterations: u32,
    /// The end of the run's final text.
    pub text: String,
    /// One short line, when the run ended with an error.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoundLine {
    Start { n: u64 },
    End(RoundEnd),
}

/// One line of the log, or `None` for anything that is not a start or an end line.
pub fn parse_line(line: &str) -> Option<RoundLine> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    let obj = v.as_object()?;
    let n = obj.get("n")?.as_u64()?;
    match obj.get("ev")?.as_str()? {
        "start" => Some(RoundLine::Start { n }),
        "end" => Some(RoundLine::End(RoundEnd {
            n,
            status: obj
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string(),
            iterations: obj
                .get("iterations")
                .and_then(Value::as_u64)
                .map_or(0, |i| u32::try_from(i).unwrap_or(u32::MAX)),
            text: obj
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .chars()
                .take(MAX_TEXT_CHARS)
                .collect(),
            error: obj
                .get("error")
                .and_then(Value::as_str)
                .map(|e| e.split_whitespace().collect::<Vec<_>>().join(" ")),
        })),
        _ => None,
    }
}

/// The last non-empty line of `text`, trimmed.
pub fn last_line(text: &str) -> Option<&str> {
    text.lines().rev().map(str::trim).find(|l| !l.is_empty())
}

/// The last line of a round when it starts with `BLOCKED:` (the pack's way to end a run that cannot go on).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockedLine {
    /// The whole line, lower case: two rounds are the same when the keys are equal.
    pub key: String,
    /// The start of the line as written, for words a person reads.
    pub shown: String,
}

pub fn blocked_line(text: &str) -> Option<BlockedLine> {
    let line = last_line(text)?;
    line.starts_with("BLOCKED:").then(|| BlockedLine {
        key: line.to_lowercase(),
        shown: line.chars().take(SHOWN_CHARS).collect(),
    })
}

/// What one read of the log found.
#[derive(Debug, Default)]
pub struct Polled {
    /// The file exists and is a regular file that was not reached through a link.
    pub present: bool,
    pub lines: Vec<RoundLine>,
    /// Lines in this read that were not round lines or were too long.
    pub bad_lines: u32,
    /// The file shrank or is another file than the one first read: nothing it says is trusted from then on.
    pub damaged: bool,
}

/// Reads the log a piece at a time, keeping its place.
#[derive(Debug)]
pub struct RoundsReader {
    path: PathBuf,
    offset: u64,
    /// The start of a line whose end has not been written yet.
    partial: Vec<u8>,
    /// Inside a line longer than [`MAX_LINE`]: skip to the next newline.
    skipping: bool,
    /// Device and inode of the file first read.
    identity: Option<(u64, u64)>,
    damaged: bool,
}

impl RoundsReader {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            partial: Vec::new(),
            skipping: false,
            identity: None,
            damaged: false,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// The file is there and is a regular file reached without a link (it has not been read).
    pub fn exists(&self) -> bool {
        open(
            &self.path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
        .and_then(|fd| fstat(&fd).ok())
        .is_some_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::RegularFile)
    }

    /// The new complete lines since the last call, at most [`POLL_BYTES`] bytes of them.
    pub fn poll(&mut self) -> Polled {
        let mut out = Polled {
            damaged: self.damaged,
            ..Polled::default()
        };
        let Ok(fd) = open(
            &self.path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) else {
            return out;
        };
        let Ok(st) = fstat(&fd) else {
            return out;
        };
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return out;
        }
        out.present = true;
        if self.damaged {
            return out;
        }
        let identity: (u64, u64) = (st.st_dev, st.st_ino);
        let len = u64::try_from(st.st_size).unwrap_or(0);
        match self.identity {
            None => self.identity = Some(identity),
            Some(first) if first != identity => {
                self.damaged = true;
                out.damaged = true;
                return out;
            }
            Some(_) => {}
        }
        if len < self.offset {
            self.damaged = true;
            out.damaged = true;
            return out;
        }
        if len == self.offset {
            return out;
        }
        let want =
            usize::try_from((len - self.offset).min(POLL_BYTES as u64)).unwrap_or(POLL_BYTES);
        let mut buf = vec![0u8; want];
        let file = std::fs::File::from(fd);
        let Ok(got) = file.read_at(&mut buf, self.offset) else {
            return out;
        };
        buf.truncate(got);
        self.offset += got as u64;
        for chunk in buf.split_inclusive(|b| *b == b'\n') {
            let complete = chunk.ends_with(b"\n");
            if self.skipping {
                if complete {
                    self.skipping = false;
                }
                continue;
            }
            self.partial.extend_from_slice(chunk);
            if self.partial.len() > MAX_LINE {
                self.partial.clear();
                out.bad_lines += 1;
                self.skipping = !complete;
                continue;
            }
            if !complete {
                continue;
            }
            let line = std::mem::take(&mut self.partial);
            let text = String::from_utf8_lossy(&line);
            if text.trim().is_empty() {
                continue;
            }
            match parse_line(&text) {
                Some(l) => out.lines.push(l),
                None => out.bad_lines += 1,
            }
        }
        out
    }
}
