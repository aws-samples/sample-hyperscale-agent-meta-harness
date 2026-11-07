//! The text a command writes, as data: the same functions serve the shell and the chat box.
//!
//! An [`Output`] is where a command writes. In the shell it is [`Output::live`]: every piece goes to standard output or standard
//! error at once, so a long command still shows its notices while it works. For the chat box it is [`Output::capture`]: the
//! pieces are kept, in order, with the stream they belong to, and [`Output::finish`] turns them and the exit code into a
//! [`CmdOutput`]. A piece is the exact text that was written, so the standard output of a captured command is, byte for byte, what
//! the shell prints.
//!
//! A line that carries a bearer link (the presigned URL of `ham link`) is written with [`Output::out_bearer_line`]: the shell
//! prints the URL line, a captured output records a line without it and keeps the URL apart in [`CmdOutput::bearer`], so the panel
//! never draws, logs or stores the URL and the box can hand it to the clipboard or the opener only on the user's own key press.
//!
//! Nothing here touches the terminal modes. A command that needs the terminal (`rules edit`, the git prompts of `push`) must be
//! run by a caller that has handed the terminal over.

use std::fmt::Display;
use std::io::Write;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Standard output or standard error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Out,
    Err,
}

/// One piece of output: the exact text written to one stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    pub stream: Stream,
    pub text: String,
}

/// Where a command writes. See the module documentation.
#[derive(Debug)]
pub struct Output {
    chunks: Mutex<Vec<Chunk>>,
    bearer: Mutex<Option<String>>,
    live: bool,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Output {
    /// Keeps everything written, in order, for the caller to show.
    pub fn capture() -> Self {
        Self {
            chunks: Mutex::new(Vec::new()),
            bearer: Mutex::new(None),
            live: false,
        }
    }

    /// Prints everything at once on the real standard output and standard error (the shell). Nothing is kept.
    pub fn live() -> Self {
        Self {
            live: true,
            ..Self::capture()
        }
    }

    fn put(&self, stream: Stream, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.live {
            // A closed pipe is the reader's choice; the exit code of the command does not change.
            match stream {
                Stream::Out => {
                    let mut o = std::io::stdout();
                    let _ = o.write_all(text.as_bytes());
                    let _ = o.flush();
                }
                Stream::Err => {
                    let mut e = std::io::stderr();
                    let _ = e.write_all(text.as_bytes());
                    let _ = e.flush();
                }
            }
        } else {
            locked(&self.chunks).push(Chunk {
                stream,
                text: text.to_string(),
            });
        }
    }

    /// Text exactly as given, on standard output.
    pub fn out(&self, text: &str) {
        self.put(Stream::Out, text);
    }

    /// Text exactly as given, on standard error.
    pub fn err(&self, text: &str) {
        self.put(Stream::Err, text);
    }

    /// `text` and a line break, on standard output.
    pub fn out_line(&self, text: impl Display) {
        self.put(Stream::Out, &format!("{text}\n"));
    }

    /// `text` and a line break, on standard error.
    pub fn err_line(&self, text: impl Display) {
        self.put(Stream::Err, &format!("{text}\n"));
    }

    /// A line on standard output that carries the bearer link `secret`: the shell prints `shell_text`; a captured output records
    /// `panel_text` (which must not contain the link) and keeps `secret` for [`CmdOutput::bearer`].
    pub fn out_bearer_line(&self, shell_text: &str, panel_text: &str, secret: &str) {
        if self.live {
            self.out_line(shell_text);
        } else {
            self.out_line(panel_text);
            *locked(&self.bearer) = Some(secret.to_string());
        }
    }

    /// A writer onto one stream, for the functions that take a `std::io::Write`.
    pub fn writer(&self, stream: Stream) -> StreamWriter<'_> {
        StreamWriter { out: self, stream }
    }

    /// The pieces and the exit code.
    pub fn finish(self, code: i32) -> CmdOutput {
        CmdOutput {
            chunks: self
                .chunks
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
            code,
            bearer: self
                .bearer
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner),
        }
    }
}

/// A [`std::io::Write`] onto one stream of an [`Output`].
#[derive(Debug)]
pub struct StreamWriter<'a> {
    out: &'a Output,
    stream: Stream,
}

impl Write for StreamWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.out.put(self.stream, &String::from_utf8_lossy(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a command wrote and how it ended.
#[derive(Clone, PartialEq, Eq)]
pub struct CmdOutput {
    /// The pieces in the order they were written.
    pub chunks: Vec<Chunk>,
    /// The exit code the shell would have ended with.
    pub code: i32,
    /// A bearer link written with [`Output::out_bearer_line`]; never part of the text.
    pub bearer: Option<String>,
}

/// A dump of the output shows that a bearer link exists and never the link, so a failed assertion, a log line or a debug print of an
/// app or a message cannot carry it.
impl std::fmt::Debug for CmdOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CmdOutput")
            .field("chunks", &self.chunks)
            .field("code", &self.code)
            .field("bearer", &self.bearer.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

impl CmdOutput {
    fn text_of(&self, stream: Stream) -> String {
        self.chunks
            .iter()
            .filter(|c| c.stream == stream)
            .map(|c| c.text.as_str())
            .collect()
    }

    /// What the shell prints on standard output.
    pub fn stdout_text(&self) -> String {
        self.text_of(Stream::Out)
    }

    /// What the shell prints on standard error.
    pub fn stderr_text(&self) -> String {
        self.text_of(Stream::Err)
    }

    /// The lines of both streams in the order they were written, for a result panel.
    pub fn lines(&self) -> Vec<String> {
        let all: String = self.chunks.iter().map(|c| c.text.as_str()).collect();
        let mut lines: Vec<String> = all.split('\n').map(str::to_string).collect();
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }
}
