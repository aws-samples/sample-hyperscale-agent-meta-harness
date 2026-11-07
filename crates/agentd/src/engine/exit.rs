//! How an engine process ended, in words for the failed turn's error text and the runtime log.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;

/// What agentd saw of the engine process when it reaped it after the engine's output closed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The process exited with this code.
    Code(i32),
    /// The kernel ended the process with this signal (an abort, a segfault, the out-of-memory killer's KILL).
    Signal(i32),
    /// The process was still running `after_s` seconds after its output closed, and agentd stopped its process group:
    /// the KILL that follows is agentd's, not the engine's own end.
    StoppedByAgentd { after_s: u64 },
    /// The wait itself failed.
    Unknown(String),
}

/// The [`Exit`] of a reaped process.
pub fn exit_of(status: ExitStatus) -> Exit {
    match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::Unknown("neither an exit code nor a signal".into()),
    }
}

/// The name of a signal that ends a process often (Linux numbering, which is the microVM's and the build hosts').
fn signal_name(signal: i32) -> Option<&'static str> {
    Some(match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        3 => "SIGQUIT",
        4 => "SIGILL",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return None,
    })
}

/// `exit status 3`, `killed by signal 9 (SIGKILL)`, and the two cases agentd saw no exit of.
pub fn describe_exit(exit: &Exit) -> String {
    match exit {
        Exit::Code(code) => format!("exit status {code}"),
        Exit::Signal(signal) => match signal_name(*signal) {
            Some(name) => format!("killed by signal {signal} ({name})"),
            None => format!("killed by signal {signal}"),
        },
        Exit::StoppedByAgentd { after_s } => {
            format!("still running {after_s} s after its output closed; agentd stopped it")
        }
        Exit::Unknown(why) => format!("exit status unknown ({why})"),
    }
}
