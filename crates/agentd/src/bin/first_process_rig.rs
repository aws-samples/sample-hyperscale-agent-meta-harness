//! `agentd-first-process-rig -- <program> [args]`: the loop of the first process around any program.
//!
//! The tests of the loop start it as process 1 of a process-id namespace with a small shell fixture as the worker
//! (`tests/b883_first_process.rs`). It is a program of the test rig: no image copies it, and the shipped `agentd` has no such entry.

use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    // nosemgrep: args -- The program name is skipped and the rest only names the program this test rig runs as the worker, so no security decision rests on it.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some("--"), Some(program)) = (args.first().map(String::as_str), args.get(1)) else {
        eprintln!("usage: agentd-first-process-rig -- <program> [args]");
        return ExitCode::from(2);
    };
    let mut worker = Command::new(program);
    worker.args(&args[2..]);
    ExitCode::from(agentd::first::split(worker))
}
