//! Prints the launch of an engine as `agentd` builds it, for a scratch folder: `agentd::engine::launch::standalone` as JSON. It holds no
//! launch logic of its own.
//!
//! ```text
//! engine_launch --engine <name> --profiles-dir <dir> --root <new dir> --model <catalog id> --region <region>
//!               [--command-timeout-seconds <n>] [--endpoint <url>] [--idle-timeout <text>]
//! ```
//!
//! Exit 0 with one JSON object `{"argv", "env", "cwd", "files", "readyLine"}`; exit 2 with a usage line on a bad argument; exit 1 with the error
//! text when the launch cannot be built.

use agentd::engine::launch::{Standalone, standalone};
use serde_json::json;
use std::path::Path;
use std::process::ExitCode;

const USAGE: &str = "usage: engine_launch --engine <name> --profiles-dir <dir> --root <new dir> --model <catalog id> --region <region> [--command-timeout-seconds <n>] [--endpoint <url>] [--idle-timeout <text>]";

fn main() -> ExitCode {
    let mut named: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    // nosemgrep: args -- the first element, the program path, is skipped and the rest is the example's own options, read as data
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let known = [
            "--engine",
            "--profiles-dir",
            "--root",
            "--model",
            "--region",
            "--command-timeout-seconds",
            "--endpoint",
            "--idle-timeout",
        ];
        let Some(value) = args.next().filter(|_| known.contains(&flag.as_str())) else {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        };
        named.insert(flag, value);
    }
    let need = |flag: &str| named.get(flag).map(String::as_str);
    let (Some(engine), Some(profiles), Some(root), Some(model), Some(region)) = (
        need("--engine"),
        need("--profiles-dir"),
        need("--root"),
        need("--model"),
        need("--region"),
    ) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let command_timeout_s = match need("--command-timeout-seconds").map(str::parse::<u32>) {
        Some(Ok(n)) => Some(n),
        Some(Err(_)) => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
        None => None,
    };
    let spec = Standalone {
        engine,
        profiles_dir: Path::new(profiles),
        root: Path::new(root),
        model,
        region,
        command_timeout_s,
        endpoint: need("--endpoint"),
        idle_timeout: need("--idle-timeout"),
    };
    match standalone(&spec) {
        Ok(launch) => {
            println!(
                "{}",
                json!({
                    "argv": launch.argv,
                    "env": launch.env,
                    "cwd": launch.cwd,
                    "files": launch.files,
                    "readyLine": launch.ready_line,
                })
            );
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("engine_launch: {why}");
            ExitCode::from(1)
        }
    }
}
