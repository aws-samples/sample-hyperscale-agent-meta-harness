//! The controlling terminal for a child that has to talk to the person (an editor) when this process's own output goes elsewhere.
//! `ham rules edit` and `ham continue` without a prompt open `$VISUAL` or `$EDITOR`. In the chat box they run as a child
//! `ham` whose standard output and error go to a file (the result panel shows that file when the child ends), and an editor that
//! inherited the file would draw its screen into it. [`editor_stdio`] gives the editor the terminal for each stream that is not one.

use std::fs::{File, OpenOptions};
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Stdio};

/// The path of the process's controlling terminal.
const TTY: &str = "/dev/tty";

/// Sends the output of `cmd` that would not reach a terminal to the controlling terminal instead; returns whether it did.
pub fn editor_stdio(cmd: &mut Command) -> bool {
    editor_stdio_at(
        cmd,
        Path::new(TTY),
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
    )
}

fn open_tty(tty: &Path) -> Option<File> {
    OpenOptions::new().read(true).write(true).open(tty).ok()
}

/// [`editor_stdio`] with the terminal's path and the state of the two streams given. A stream that is a terminal already, and both
/// streams when the terminal cannot be opened, stay as they are.
pub fn editor_stdio_at(
    cmd: &mut Command,
    tty: &Path,
    out_is_terminal: bool,
    err_is_terminal: bool,
) -> bool {
    if out_is_terminal && err_is_terminal {
        return false;
    }
    let Some(file) = open_tty(tty) else {
        return false;
    };
    if !out_is_terminal {
        match file.try_clone() {
            Ok(f) => {
                cmd.stdout(Stdio::from(f));
            }
            Err(_) => return false,
        }
    }
    if !err_is_terminal {
        cmd.stderr(Stdio::from(file));
    }
    true
}

/// Whether an editor can be shown to the person: input comes from a terminal, and output reaches one or can reach the controlling
/// terminal.
pub fn can_ask() -> bool {
    can_ask_at(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
        Path::new(TTY),
    )
}

/// [`can_ask`] with the state of standard input and output and the terminal's path given.
pub fn can_ask_at(in_is_terminal: bool, out_is_terminal: bool, tty: &Path) -> bool {
    in_is_terminal && (out_is_terminal || open_tty(tty).is_some())
}
