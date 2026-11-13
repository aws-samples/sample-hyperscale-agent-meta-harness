//! `agentd serve [--listen <addr>]`: the runtime contract server (settings in `agentd::config`).
//! `agentd creds --socket <path>`: the engine's `credential_process`; prints credentials JSON or
//! exits 1 (fail closed). `agentd sandbox-exec --policy-json <json> -- <program> [args]`: the engine's launcher in the
//! `landlock` mode. `agentd --version`.

use agentd::config::{Config, open_files_cap};
use agentd::creds_socket::fetch_credentials;
use agentd::logging::StderrLog;
use agentd::nofile::{KernelLimits, apply};
use agentd::sandbox::{
    ENV_VAR, Mode, Setting, main_sandbox_exec, note_process_mode, set_non_dumpable,
};
use agentd::server::{App, serve};
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
        let listener = match tokio::net::TcpListener::bind(cfg.listen).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("agentd: bind {}: {e}", cfg.listen);
                return ExitCode::from(1);
            }
        };
        eprintln!("agentd: listening on {}", cfg.listen);
        match serve(App::new(cfg, Arc::new(StderrLog)), listener).await {
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
