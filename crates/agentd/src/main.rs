//! `agentd serve [--listen <addr>]`: the runtime contract server (settings in `agentd::config`). As process 1 of its machine it starts
//! itself once more as the worker, with the argument `--behind-first-process` that only process 1 gives, and keeps the first-process
//! role.
//! `agentd creds --socket <path>`: the engine's `credential_process`; prints credentials JSON or
//! exits 1 (fail closed). `agentd sandbox-exec --policy-json <json> -- <program> [args]`: the engine's launcher in the
//! `landlock` mode. `agentd --version`.

use agentd::config::{Config, open_files_cap};
use agentd::creds_socket::fetch_credentials;
use agentd::first;
use agentd::logging::StderrLog;
use agentd::nofile::{KernelLimits, apply};
use agentd::sandbox::{
    ENV_VAR, Mode, Setting, main_sandbox_exec, note_process_mode, set_non_dumpable,
};
use agentd::server::{App, StopSignals, end_on_signals, serve};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: agentd serve [--listen <addr>] | agentd creds --socket <path> | agentd sandbox-exec --policy-json <json> -- <program> [args] | agentd --version"
    );
    ExitCode::from(2)
}

fn runtime(multi: bool) -> Option<tokio::runtime::Runtime> {
    let mut b = if multi {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    b.enable_all().build().ok()
}

fn creds(args: &[String]) -> ExitCode {
    let Some(socket) = flag(args, "--socket") else {
        return usage();
    };
    let Some(rt) = runtime(false) else {
        return ExitCode::from(1);
    };
    match rt.block_on(fetch_credentials(&PathBuf::from(socket))) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("agentd creds: {e}");
            ExitCode::from(1)
        }
    }
}

/// The first thing `agentd serve` does, before the settings are read or a secret is loaded: in the `landlock` mode it makes itself non-dumpable
/// (a process of the same user then cannot read its environment or memory through `/proc`) and says so to the helpers that give files to root.
/// A mode that is not a mode is not acted on here; the run that would start an engine ends with the reason.
fn harden() -> Result<(), ExitCode> {
    match Setting::from_value(std::env::var(ENV_VAR).ok().as_deref()) {
        Setting::Mode(Mode::Landlock) => {
            if let Err(e) = set_non_dumpable() {
                eprintln!("agentd: {e}");
                return Err(ExitCode::from(1));
            }
            note_process_mode(Mode::Landlock);
        }
        Setting::Mode(Mode::Uid) => note_process_mode(Mode::Uid),
        Setting::Refused(_) => {}
    }
    Ok(())
}

fn serve_cmd(args: &[String]) -> ExitCode {
    if let Err(code) = harden() {
        return code;
    }
    // As process 1 of its machine, which is what a session's microVM runs, `agentd serve` starts the worker as its one child and
    // only collects ended processes and passes the stop signals on (`agentd::first`). Nothing else is read or started here.
    if first::takes_the_role(std::process::id(), args) {
        return match first::worker_command(args) {
            Ok(worker) => ExitCode::from(first::split(worker)),
            Err(e) => {
                first::say(&format!(
                    "the worker could not be started ({e}); process 1 ends"
                ));
                ExitCode::from(1)
            }
        };
    }
    if first::is_behind_first(args, first::parent_pid()) {
        first::note_behind_first();
    }
    // Before the settings are used and before anything is started: every process agentd starts inherits the open-files limit it has now, and a
    // limit of hundreds of millions makes programs that close every descriptor number take minutes per start (`agentd::nofile`). It logs one line
    // and never ends the start.
    apply(
        &open_files_cap(&|k| std::env::var(k).ok()),
        &KernelLimits,
        &StderrLog,
    );
    let mut cfg = Config::from_env();
    if let Some(listen) = flag(args, "--listen") {
        match listen.parse() {
            Ok(addr) => cfg.listen = addr,
            Err(_) => return usage(),
        }
    }
    let Some(rt) = runtime(true) else {
        return ExitCode::from(1);
    };
    rt.block_on(async move {
        // The signals that ask agentd to end are watched before the port is bound. The worker behind the first process
        // then tells process 1 that it is ready for them; a signal that came earlier is held there and passed on now.
        let stops = StopSignals::watch(&StderrLog);
        if stops.is_some() {
            first::say_ready(&StderrLog);
        }
        let listener = match tokio::net::TcpListener::bind(cfg.listen).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("agentd: bind {}: {e}", cfg.listen);
                return ExitCode::from(1);
            }
        };
        eprintln!("agentd: listening on {}", cfg.listen);
        let app = App::new(cfg, Arc::new(StderrLog));
        // A signal that asks agentd to end is answered with a copy of the work first.
        tokio::spawn(end_on_signals(app.clone(), stops));
        match serve(app, listener).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agentd: {e}");
                ExitCode::from(1)
            }
        }
    })
}

fn main() -> ExitCode {
    // nosemgrep: args -- The program name is skipped and the rest only picks the agentd subcommand, serve, creds or the version flag, so no security decision rests on it.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => serve_cmd(&args),
        Some("creds") => creds(&args),
        Some("sandbox-exec") => main_sandbox_exec(&args[1..]),
        Some("--version") => {
            println!("agentd {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}
