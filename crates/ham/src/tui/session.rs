//! The terminal session guard. One object puts the terminal into the modes the TUI needs and takes every one of
//! them out again on a normal exit, Ctrl-C, SIGTERM (and SIGINT and SIGHUP) and a panic. ratatui's own restore undoes only raw mode and
//! the alternate screen, so a TUI that also turns on bracketed paste, focus events or mouse reports must undo those itself: a terminal
//! left in mouse mode prints escape sequences for every click, and one left in bracketed paste wraps the next program's input.
//!
//! The sequences are pure functions over a writer ([`write_enter`], [`write_restore`]), tested without a terminal; [`TerminalSession`] is the
//! thin part that owns the process-wide state (raw mode, the stdout writes, the panic hook). The modes that are on are recorded in one
//! atomic, so the panic hook, the signal path and `Drop` can each call the restore and only the first one writes.

use ham_core::config::MouseMode;
use std::cell::Cell;
use std::io::{self, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicU8, Ordering};

/// The terminal modes of a TUI session. `raw` is the tty's line discipline; the others are DEC private modes written as escape
/// sequences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Modes {
    pub raw: bool,
    pub alt_screen: bool,
    pub paste: bool,
    pub focus: bool,
    pub mouse: bool,
}

const RAW: u8 = 1;
const ALT_SCREEN: u8 = 2;
const PASTE: u8 = 4;
const FOCUS: u8 = 8;
const MOUSE: u8 = 16;

impl Modes {
    /// What the TUI uses: raw input, the alternate screen, bracketed paste and focus events, and mouse reports when `mouse`.
    pub const fn tui(mouse: bool) -> Self {
        Self {
            raw: true,
            alt_screen: true,
            paste: true,
            focus: true,
            mouse,
        }
    }

    /// The modes of a session from the environment and the config's `mouse` setting (see [`wants_mouse`]).
    pub fn for_session(env: &dyn Fn(&str) -> Option<String>, setting: Option<MouseMode>) -> Self {
        Self::tui(wants_mouse(env, setting))
    }

    fn bits(self) -> u8 {
        [
            (self.raw, RAW),
            (self.alt_screen, ALT_SCREEN),
            (self.paste, PASTE),
            (self.focus, FOCUS),
            (self.mouse, MOUSE),
        ]
        .into_iter()
        .filter(|(on, _)| *on)
        .fold(0, |acc, (_, bit)| acc | bit)
    }

    fn from_bits(bits: u8) -> Self {
        Self {
            raw: bits & RAW != 0,
            alt_screen: bits & ALT_SCREEN != 0,
            paste: bits & PASTE != 0,
            focus: bits & FOCUS != 0,
            mouse: bits & MOUSE != 0,
        }
    }
}

/// Terminals (`TERM_PROGRAM` values) for which `mouse = auto` turns mouse reports on. A name stays on this list only
/// while a PTY scenario, or the Terminal.app row of the Mac lane, shows that it reports clicks the way the TUI reads them;
/// `Apple_Terminal` and a terminal with another name are off under `auto`. The kitty and xterm families are matched by `TERM` when
/// no program name is set (an ssh session forwards `TERM`, not `TERM_PROGRAM`).
const MOUSE_AUTO_TERMINALS: &[&str] = &["iTerm.app", "ghostty", "WezTerm", "vscode"];

/// A switch variable that is on: `1`, `true`, `yes` or `on`, in any case.
fn switch_on(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// `mouse = auto`: on for a terminal that is known to report the mouse the way the TUI reads it.
fn auto_terminal(env: &dyn Fn(&str) -> Option<String>) -> bool {
    let program = env("TERM_PROGRAM").unwrap_or_default();
    if MOUSE_AUTO_TERMINALS.contains(&program.as_str()) {
        return true;
    }
    program.is_empty() && env("TERM").is_some_and(|t| t.starts_with("xterm") || t.contains("kitty"))
}

/// `HAMH_NO_MOUSE=1` (or `true`, `yes`, `on`): the mouse stays off for this run of `ham`, whatever the config or `/mouse` says.
pub fn mouse_forced_off(env: &dyn Fn(&str) -> Option<String>) -> bool {
    env("HAMH_NO_MOUSE").is_some_and(|v| switch_on(&v))
}

/// Whether the session asks the terminal for mouse reports.
pub fn wants_mouse(env: &dyn Fn(&str) -> Option<String>, setting: Option<MouseMode>) -> bool {
    if mouse_forced_off(env) {
        return false;
    }
    match setting {
        Some(MouseMode::Off) => false,
        Some(MouseMode::On) | None => true,
        Some(MouseMode::Auto) => auto_terminal(env),
    }
}

/// Mouse reports as press and release (1000), drag (1002) and SGR coordinates (1006). crossterm's own capture also writes all-motion
/// (1003) and the RXVT encoding (1015), which the TUI has no use for.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// Writes the sequences that turn the mouse reports on or off: the same three modes the session starts with,
/// and the same three turned off, so a terminal left in any of them is repaired.
pub fn write_mouse<W: Write>(w: &mut W, on: bool) -> io::Result<()> {
    w.write_all(if on { MOUSE_ON } else { MOUSE_OFF })
}

/// The one-line hint a session that asks for the mouse starts with: the mouse stops the terminal's own selection, and the key that
/// brings it back is Fn in Terminal.app, Option in iTerm2 and Shift in most other terminals.
pub fn selection_hint(env: &dyn Fn(&str) -> Option<String>) -> String {
    let key = match env("TERM_PROGRAM").as_deref() {
        Some("Apple_Terminal") => "Fn",
        Some("iTerm.app") => "Option",
        _ => "Shift",
    };
    format!("mouse is on: hold {key} to select text with the terminal · /mouse off")
}

/// Writes the sequences that turn on every escape-sequence mode of `m` (raw mode is not a sequence).
pub fn write_enter<W: Write>(w: &mut W, m: &Modes) -> io::Result<()> {
    if m.alt_screen {
        w.write_all(b"\x1b[?1049h")?;
    }
    if m.paste {
        w.write_all(b"\x1b[?2004h")?;
    }
    if m.focus {
        w.write_all(b"\x1b[?1004h")?;
    }
    if m.mouse {
        w.write_all(MOUSE_ON)?;
    }
    Ok(())
}

/// Every private mode the TUI turns on or that a ham of an earlier version could have left on, turned off: all five mouse modes (the TUI
/// writes 1000, 1002 and 1006; crossterm's own capture also wrote 1003 and 1015), focus reports and bracketed paste. The alternate
/// screen is not in the list: leaving or entering it would move what the person sees, and the session enters it next.
const PRE_RESET: &[u8] =
    b"\x1b[?1015l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1004l\x1b[?2004l";

/// The start of a session: the pre-reset, then [`write_enter`]. A ham that was killed with SIGKILL cannot restore the terminal, so the
/// window it ran in may still report the mouse and wrap pastes; the next start repairs that before it asks for its own modes.
pub fn write_start<W: Write>(w: &mut W, m: &Modes) -> io::Result<()> {
    w.write_all(PRE_RESET)?;
    write_enter(w, m)
}

/// Writes the sequences that turn off every mode of `m`, in the reverse order of [`write_enter`], then reset the text style and show
/// the cursor. It stands alone: a panic hook has no memory of what was written.
pub fn write_restore<W: Write>(w: &mut W, m: &Modes) -> io::Result<()> {
    if m.mouse {
        w.write_all(MOUSE_OFF)?;
    }
    if m.focus {
        w.write_all(b"\x1b[?1004l")?;
    }
    if m.paste {
        w.write_all(b"\x1b[?2004l")?;
    }
    if m.alt_screen {
        w.write_all(b"\x1b[?1049l")?;
    }
    w.write_all(b"\x1b[0m\x1b[?25h")
}

/// The modes that are on now, as bits; the first restore takes them (swaps in 0), so the others find nothing to do.
static ACTIVE: AtomicU8 = AtomicU8::new(0);

thread_local! {
    /// True on the thread that entered the session. A panic on another thread (a spawned effect) does not end the TUI, so it must not
    /// take the terminal out of the modes the still-running loop draws into.
    static ON_TUI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Turns the mouse reports on or off for the running session (`/mouse on|off`). The modes that are on follow, so the restore on a normal exit,
/// Ctrl-C, a signal or a panic writes the right sequences. Without an active session nothing is written.
pub(crate) fn set_mouse(on: bool) -> io::Result<()> {
    if ACTIVE.load(Ordering::SeqCst) == 0 {
        return Ok(());
    }
    if on {
        ACTIVE.fetch_or(MOUSE, Ordering::SeqCst);
    } else {
        ACTIVE.fetch_and(!MOUSE, Ordering::SeqCst);
    }
    let mut out = io::stdout().lock();
    write_mouse(&mut out, on).and_then(|()| out.flush())
}

/// Takes the terminal out of every mode that is on. Safe to call from the panic hook, a signal path and `Drop`: only the first call
/// finds modes to undo.
fn restore_now() -> io::Result<()> {
    let bits = ACTIVE.swap(0, Ordering::SeqCst);
    if bits == 0 {
        return Ok(());
    }
    let modes = Modes::from_bits(bits);
    let mut out = io::stdout().lock();
    let written = write_restore(&mut out, &modes).and_then(|()| out.flush());
    if modes.raw {
        // Raw mode is undone even when the write failed (the terminal may be gone).
        let _ = crossterm::terminal::disable_raw_mode();
    }
    written
}

/// Takes the terminal out of the TUI modes for a child that needs it (an editor): what was on is turned off as [`restore_now`] does, and
/// returned so that [`resume`] can turn it on again. `None` when no session is active.
pub(crate) fn suspend() -> io::Result<Option<Modes>> {
    let bits = ACTIVE.swap(0, Ordering::SeqCst);
    if bits == 0 {
        return Ok(None);
    }
    let modes = Modes::from_bits(bits);
    let mut out = io::stdout().lock();
    let written = write_restore(&mut out, &modes).and_then(|()| out.flush());
    if modes.raw {
        // Raw mode is undone even when the write failed (the terminal may be gone).
        let _ = crossterm::terminal::disable_raw_mode();
    }
    written.map(|()| Some(modes))
}

/// Turns the modes back on after a child (the modes [`suspend`] returned), as [`TerminalSession::enter`] does.
pub(crate) fn resume(modes: Modes) -> io::Result<()> {
    if modes.raw {
        crossterm::terminal::enable_raw_mode()?;
    }
    ACTIVE.store(modes.bits(), Ordering::SeqCst);
    let mut out = io::stdout().lock();
    if let Err(e) = write_start(&mut out, &modes).and_then(|()| out.flush()) {
        drop(out);
        let _ = restore_now();
        return Err(e);
    }
    Ok(())
}

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // The terminal comes back first, so the message prints on the normal screen and stays readable.
            if ON_TUI_THREAD.with(Cell::get) {
                let _ = restore_now();
            }
            previous(info);
        }));
    });
}

/// The terminal in TUI modes for as long as the value lives. Dropping it, or calling [`TerminalSession::restore`], takes them all out.
#[derive(Debug)]
pub struct TerminalSession {
    _entered: (),
}

impl TerminalSession {
    /// Turns the modes on. If anything fails, the modes already on are turned off before the error is returned.
    pub fn enter(modes: Modes) -> io::Result<Self> {
        install_panic_hook();
        ON_TUI_THREAD.with(|c| c.set(true));
        if modes.raw {
            crossterm::terminal::enable_raw_mode()?;
        }
        // Recorded before the write: a write that fails half way is undone by the same restore.
        ACTIVE.store(modes.bits(), Ordering::SeqCst);
        let mut out = io::stdout().lock();
        if let Err(e) = write_start(&mut out, &modes).and_then(|()| out.flush()) {
            drop(out);
            let _ = restore_now();
            return Err(e);
        }
        Ok(Self { _entered: () })
    }

    /// Turns the modes off now and reports a write error; the drop that follows finds nothing left to do.
    pub fn restore(self) -> io::Result<()> {
        restore_now()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = restore_now();
    }
}

/// Resolves with the number of the first of SIGTERM (15), SIGINT (2) and SIGHUP (1) that the process receives. Raw mode turns off the
/// terminal's own Ctrl-C signal, so SIGINT here comes from `kill`, not from the keyboard (the keyboard's Ctrl-C is a key event).
#[cfg(unix)]
pub async fn wait_for_signal() -> i32 {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        // No handler could be installed: nothing will ever arrive through this path.
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => 15,
        _ = int.recv() => 2,
        _ = hup.recv() => 1,
    }
}

/// No signals are caught on other platforms.
#[cfg(not(unix))]
pub async fn wait_for_signal() -> i32 {
    std::future::pending().await
}

/// SIGCONT, which tokio has no name for. The numbers are those of the targets the client is built for; any other target gets no forwarder.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
const SIGCONT: Option<i32> = Some(18);
#[cfg(target_os = "macos")]
const SIGCONT: Option<i32> = Some(19);
#[cfg(not(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    target_os = "macos"
)))]
const SIGCONT: Option<i32> = None;

/// Sends `FocusGained` on `tx` each time the process is continued after a stop (job control: the shell stopped it, something else drew on the
/// terminal, the shell continued it). The program cannot know what the terminal shows after that, which is what the terminal's own focus
/// report tells it too, so the event loop repaints on either. `None` where the platform has no SIGCONT number here or the signal cannot be
/// watched. Call it inside the runtime. The sender is weak, so this never keeps the key channel open: the loop still ends when the key
/// reader ends, and the task ends with the channel.
pub fn forward_continue(
    tx: tokio::sync::mpsc::WeakUnboundedSender<(crossterm::event::Event, std::time::Instant)>,
) -> Option<tokio::task::JoinHandle<()>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut cont = signal(SignalKind::from_raw(SIGCONT?)).ok()?;
        Some(tokio::spawn(async move {
            while cont.recv().await.is_some() {
                let Some(strong) = tx.upgrade() else { break };
                if strong
                    .send((
                        crossterm::event::Event::FocusGained,
                        std::time::Instant::now(),
                    ))
                    .is_err()
                {
                    break;
                }
            }
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = (tx, SIGCONT);
        None
    }
}

/// The name of a signal number [`wait_for_signal`] returns.
pub fn signal_name(n: i32) -> &'static str {
    match n {
        1 => "SIGHUP",
        2 => "SIGINT",
        15 => "SIGTERM",
        _ => "a signal",
    }
}
