//! A command that needs the terminal: `ham push`, whose git and ssh may ask for a password, a passphrase or a host
//! key, and `ham rules edit`, which opens the user's editor. The command runs as a child `ham` with the same arguments the shell form takes
//! (the in-process runner has no terminal to give), through [`reader::suspend_for`](super::reader::suspend_for): the reader parks, the TUI
//! modes go off, the child owns the terminal, and the modes come back. The child's standard output and error go to a private file, so the
//! result panel can show them after the terminal returns; the prompts of git and ssh are written to the terminal itself and are seen as
//! they would be in a shell.
//!
//! Both streams go to one file opened for appending, so a line written to standard error stays between the lines of standard output
//! that surround it. The panel keeps the last [`KEEP`] bytes: a push of a large history prints a lot, and its end is what says whether it
//! worked. The file lives in a private directory (mode 0700) that is removed when the call returns.

use crate::output::{Chunk, CmdOutput, Stream};
use std::fs::{DirBuilder, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

/// The most output the panel keeps: the last 256 KiB.
pub const KEEP: u64 = 256 * 1024;

/// The line put before the kept end when the start of the output was cut.
const CUT: &str = "(earlier output was cut)\n";

/// A private directory that is removed when it goes out of scope.
struct Scratch(PathBuf);

impl Scratch {
    fn create() -> io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        // nosemgrep: temp-dir -- the output is never written in the shared directory itself: the file is made inside a private 0700 directory under it
        let path = std::env::temp_dir().join(format!("ham-handoff-{}-{nanos}", std::process::id()));
        DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The output of a command that did not run: `text` on standard error and exit code [`ham_core::exit::ERROR`].
pub fn failed(text: String) -> CmdOutput {
    CmdOutput {
        chunks: vec![Chunk {
            stream: Stream::Err,
            text,
        }],
        code: ham_core::exit::ERROR,
        bearer: None,
    }
}

/// The last [`KEEP`] bytes of `file`, cut at a line start, as text; the notice that the start was cut comes first when it was.
fn kept_end(file: &mut File) -> io::Result<String> {
    let len = file.metadata()?.len();
    let start = len.saturating_sub(KEEP);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        // The cut may fall in the middle of a line: drop the part line.
        if let Some(i) = text.find('\n') {
            text.drain(..=i);
        }
        text.insert_str(0, CUT);
    }
    Ok(text)
}

/// Runs `program argv...` in `dir` with the terminal handed over by `run`, and returns what it wrote and how it ended.
///
/// `run` receives the command with its output already sent to the private file and only has to run it (the TUI passes
/// [`reader::suspend_for`](super::reader::suspend_for), the tests `Command::status`). A child that cannot start, or a private file that cannot be made, gives
/// exit code [`ham_core::exit::ERROR`] and a line that says why; a child that ends on a signal gives the same code.
pub fn run_child_with<F>(program: &Path, argv: &[String], dir: &Path, run: F) -> CmdOutput
where
    F: FnOnce(&mut Command) -> io::Result<ExitStatus>,
{
    let scratch = match Scratch::create() {
        Ok(s) => s,
        Err(e) => {
            return failed(format!(
                "could not make a private directory for the output of {}: {e}\n",
                program.display()
            ));
        }
    };
    let file_path = scratch.0.join("output.txt");
    let mut file = match OpenOptions::new()
        .create_new(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .open(&file_path)
    {
        Ok(f) => f,
        Err(e) => return failed(format!("could not make the output file: {e}\n")),
    };
    let (out, err) = match (file.try_clone(), file.try_clone()) {
        (Ok(out), Ok(err)) => (out, err),
        (Err(e), _) | (_, Err(e)) => {
            return failed(format!("could not share the output file: {e}\n"));
        }
    };
    let mut cmd = Command::new(program);
    // COMPAT-CAH: the notices of the window of the earlier names were made once, when this program started; the child makes none of its own.
    cmd.args(argv)
        .env(ham_core::envnames::NO_NOTICES, "1")
        .current_dir(dir)
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    let status = match run(&mut cmd) {
        Ok(s) => s,
        Err(e) => return failed(format!("could not start {}: {e}\n", program.display())),
    };
    let text = match kept_end(&mut file) {
        Ok(t) => t,
        Err(e) => format!("(the output could not be read: {e})\n"),
    };
    let mut chunks = Vec::new();
    if !text.is_empty() {
        chunks.push(Chunk {
            stream: Stream::Out,
            text,
        });
    }
    CmdOutput {
        chunks,
        code: status.code().unwrap_or(ham_core::exit::ERROR),
        bearer: None,
    }
}
