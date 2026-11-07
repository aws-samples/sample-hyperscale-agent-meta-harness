//! From text the user chose to the clipboard. The module draws nothing and reads no event; the screen calls it
//! from the three things a user does to copy: the `y` key, the `/copy` command and the release of a mouse selection.
//!
//! * **Prepare.** [`prepare`] is the only way to make the value the writers accept. It cleans the text (no escape sequence, control
//!   character or 8-bit control survives; bidirectional, zero-width and tag characters are shown as `<U+XXXX>` markers, so the copy equals
//!   what is visible), masks presigned links and token shapes (a heuristic second line behind the worker's redaction, not a guarantee), and
//!   refuses an empty text or one over [`COPY_LIMIT`] bytes. It never copies a prefix.
//! * **Route.** [`plan`] lists the routes for the machine. Over ssh (`SSH_TTY` or `SSH_CONNECTION` set) the only route is OSC 52, because a
//!   tool would fill the clipboard of the machine at the far end. Otherwise a local tool runs as an argument vector with the text on stdin
//!   and never through a shell (`pbcopy` on macOS; `wl-copy` under Wayland, `xclip` then `xsel` under X11), then OSC 52 when no tool took it.
//!   The sequence is sent plain, also inside tmux (it works with `set-clipboard on`; the wrapped form needs `allow-passthrough`, off by
//!   default since tmux 3.3). A terminal may ignore OSC 52, so the notice says the terminal decides.
//! * **Only here.** The bytes of the clipboard sequence are made in this file, from a [`Prepared`] text, and nowhere else; a test scans the
//!   crate for the sequence. Transcript text reaches the sequence only as the text the user selected, after cleaning.
//! * **Which text.** [`last_answer`] is the last engine message as it stands and [`block_text`] a block's head and body.

use ham_core::chat::{Block, BlockKind, ChatModel, Role};
use ham_core::redact::redact_tokens;
use ham_core::sanitize::display_text;
use std::fmt;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The most bytes one copy carries. Longer text is refused; `/export` is the way to keep it.
pub const COPY_LIMIT: usize = 64 * 1024;

/// Text that passed [`prepare`]: clean, masked and within the limit. Only this file can make one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    text: String,
    masked: bool,
}

impl Prepared {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn bytes(&self) -> usize {
        self.text.len()
    }

    /// A secret-shaped value was hidden in the copy.
    pub fn masked(&self) -> bool {
        self.masked
    }
}

/// Why a copy did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyError {
    /// Nothing visible to copy.
    Empty,
    /// More than [`COPY_LIMIT`] bytes.
    TooLarge { bytes: usize, limit: usize },
    /// The plan had no route that took the text.
    NoRoute,
    /// The terminal could not be written to.
    Write(String),
}

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyError::Empty => write!(f, "Nothing to copy."),
            CopyError::TooLarge { bytes, limit } => write!(
                f,
                "Too large to copy: {} is over the limit of {} KiB. Use /export to keep long text.",
                size_text(*bytes),
                limit / 1024
            ),
            CopyError::NoRoute => write!(f, "No clipboard route took the text."),
            CopyError::Write(why) => write!(f, "Could not write to the terminal: {why}"),
        }
    }
}

impl std::error::Error for CopyError {}

/// `12 bytes`, `1 byte`, `1.5 KiB`, `3.2 MiB`.
pub fn size_text(bytes: usize) -> String {
    match bytes {
        1 => "1 byte".to_string(),
        0..=1023 => format!("{bytes} bytes"),
        1024..=1_048_575 => format!("{:.1} KiB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MiB", bytes as f64 / 1_048_576.0),
    }
}

/// Cleans, masks and checks `text` for a copy.
pub fn prepare(text: &str) -> Result<Prepared, CopyError> {
    let clean = display_text(text);
    if clean.trim().is_empty() {
        return Err(CopyError::Empty);
    }
    // Masking only shortens a long run of text, so a text far over the limit is refused before the scan.
    if clean.len() > COPY_LIMIT * 4 {
        return Err(CopyError::TooLarge {
            bytes: clean.len(),
            limit: COPY_LIMIT,
        });
    }
    let shown = redact_tokens(&clean);
    if shown.len() > COPY_LIMIT {
        return Err(CopyError::TooLarge {
            bytes: shown.len(),
            limit: COPY_LIMIT,
        });
    }
    Ok(Prepared {
        masked: shown != clean,
        text: shown,
    })
}

/// A local clipboard program and its fixed arguments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tool {
    pub program: &'static str,
    args: &'static [&'static str],
}

impl Tool {
    /// The program followed by its arguments: the whole command, which never goes through a shell.
    pub fn argv(&self) -> Vec<&'static str> {
        std::iter::once(self.program)
            .chain(self.args.iter().copied())
            .collect()
    }
}

const PBCOPY: Tool = Tool {
    program: "pbcopy",
    args: &[],
};
const WL_COPY: Tool = Tool {
    program: "wl-copy",
    args: &[],
};
const XCLIP: Tool = Tool {
    program: "xclip",
    args: &["-selection", "clipboard"],
};
const XSEL: Tool = Tool {
    program: "xsel",
    args: &["--clipboard", "--input"],
};

/// One way to reach the clipboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    Tool(Tool),
    /// The terminal's own clipboard sequence, written to the terminal.
    Osc52,
}

impl Route {
    /// The name the notice shows.
    pub fn label(&self) -> String {
        match self {
            Route::Tool(t) => t.program.to_string(),
            Route::Osc52 => "OSC 52".to_string(),
        }
    }
}

/// The system the TUI runs on, as far as the clipboard cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
    Other,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(target_os = "linux") {
            Os::Linux
        } else {
            Os::Other
        }
    }
}

/// The routes to try, in order. `env` reads an environment variable; `has_program` says whether a program is installed.
pub fn plan(
    os: Os,
    env: &dyn Fn(&str) -> Option<String>,
    has_program: &dyn Fn(&str) -> bool,
) -> Vec<Route> {
    let set = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    if set("SSH_TTY") || set("SSH_CONNECTION") {
        return vec![Route::Osc52];
    }
    let mut routes = Vec::new();
    let mut add = |t: Tool| {
        if has_program(t.program) {
            routes.push(Route::Tool(t));
        }
    };
    match os {
        Os::MacOs => add(PBCOPY),
        Os::Linux => {
            if set("WAYLAND_DISPLAY") {
                add(WL_COPY);
            }
            if set("DISPLAY") {
                add(XCLIP);
                add(XSEL);
            }
        }
        Os::Other => {}
    }
    routes.push(Route::Osc52);
    routes
}

/// Whether `name` is an executable file in one of the directories of `path` (the value of `PATH`).
pub fn on_path(name: &str, path: Option<&str>) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if name.is_empty() || name.contains('/') {
        return false;
    }
    path.is_some_and(|p| {
        std::env::split_paths(p).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

/// [`plan`] for this machine: the real environment, `PATH` and operating system.
pub fn plan_here() -> Vec<Route> {
    let path = std::env::var("PATH").ok();
    plan(Os::current(), &|k| std::env::var(k).ok(), &|p| {
        on_path(p, path.as_deref())
    })
}

/// Runs a clipboard program. The real one is [`SystemRunner`]; tests use a script.
pub trait Runner {
    /// Runs `argv` (program first) with `input` on its stdin. `Ok(true)` when it exited with status 0.
    fn run(&mut self, argv: &[&str], input: &[u8]) -> io::Result<bool>;
}

/// Runs the program with `std::process::Command`: no shell, stdin piped, output dropped, killed at the deadline.
#[derive(Clone, Copy, Debug)]
pub struct SystemRunner {
    pub deadline: Duration,
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(2),
        }
    }
}

impl Runner for SystemRunner {
    fn run(&mut self, argv: &[&str], input: &[u8]) -> io::Result<bool> {
        let (program, args) = argv
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no command"))?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        // The text goes in on its own thread: a tool that never reads must not hold this one.
        let writer = child.stdin.take().map(|mut stdin| {
            let data = input.to_vec();
            std::thread::spawn(move || {
                let _ = stdin.write_all(&data);
            })
        });
        let started = Instant::now();
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.success()),
                Ok(None) if started.elapsed() >= self.deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{program} did not finish in {:?}", self.deadline),
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => break Err(e),
            }
        };
        if let Some(w) = writer {
            let _ = w.join();
        }
        result
    }
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding (RFC 4648).
pub fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(BASE64[((n >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 sequence that sets the clipboard to `p`: ESC, `]52;c;`, the text in base64, BEL. This is the only place the sequence is made.
pub fn osc52(p: &Prepared) -> Vec<u8> {
    let mut out = Vec::with_capacity(p.text.len() * 4 / 3 + 16);
    out.extend_from_slice(b"\x1b]52;c;");
    out.extend_from_slice(base64(p.text.as_bytes()).as_bytes());
    out.push(0x07);
    out
}

/// What a copy did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub route: Route,
    pub bytes: usize,
    pub masked: bool,
}

impl Outcome {
    /// The notice line: the size and the path used.
    pub fn notice(&self) -> String {
        let size = size_text(self.bytes);
        let mut s = match self.route {
            Route::Tool(t) => format!("Copied {size} with {}.", t.program),
            Route::Osc52 => {
                format!("Copied {size} with OSC 52; the terminal decides whether it takes it.")
            }
        };
        if self.masked {
            s.push_str(" A secret-shaped value was hidden.");
        }
        s
    }
}

/// Puts `p` on the clipboard by the first route that works: a tool that exits 0, or OSC 52 written to `terminal`. A tool that fails or is
/// missing hands over to the next route.
pub fn copy(
    p: &Prepared,
    routes: &[Route],
    runner: &mut dyn Runner,
    terminal: &mut dyn Write,
) -> Result<Outcome, CopyError> {
    for route in routes {
        let done = match route {
            Route::Tool(t) => matches!(runner.run(&t.argv(), p.text.as_bytes()), Ok(true)),
            Route::Osc52 => {
                terminal
                    .write_all(&osc52(p))
                    .and_then(|()| terminal.flush())
                    .map_err(|e| CopyError::Write(e.to_string()))?;
                true
            }
        };
        if done {
            return Ok(Outcome {
                route: *route,
                bytes: p.bytes(),
                masked: p.masked,
            });
        }
    }
    Err(CopyError::NoRoute)
}

/// The last answer: the last message the engine wrote, as it stands now.
pub fn last_answer(model: &ChatModel) -> Option<&Block> {
    model
        .blocks
        .iter()
        .rev()
        .find(|b| b.kind == BlockKind::Message && b.effective_role() == Role::Engine)
}

/// A block as text: its head line and its body.
pub fn block_text(b: &Block) -> String {
    let head = b.head.trim();
    match (head.is_empty(), b.body.is_empty()) {
        (true, _) => b.body.clone(),
        (false, true) => head.to_string(),
        (false, false) => format!("{head}\n{}", b.body),
    }
}
