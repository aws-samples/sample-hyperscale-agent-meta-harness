//! `ham-admin`: operator commands through `/v1/admin/*`, with
//! the `ham` configuration file and SigV4 client. `ham` itself never writes the allowlist.
//!
//! - `ham-admin repos allow <target>`
//! - `ham-admin repos deny <target>`
//! - `ham-admin repos list`
//! - `ham-admin --version` (`-V`) and `ham-admin --help` (`-h`): one line on stdout, exit 0, before
//!   any configuration is read or any call is made.
//!
//! Exit 0 when done; 1 when the API refused or failed (its exact reason on stderr); 2 on a usage
//! error, before any call.

use ham::api::ApiError;
use harness_protocol::api::v1::{RepoAdminAction, RepoAdminRequest, RepoAllowView};
use harness_protocol::target::RepoTarget;

const USAGE: &str = "usage: ham-admin repos allow <target> | deny <target> | list";

#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    Allow(RepoTarget),
    Deny(RepoTarget),
    List,
}

/// The answer to a lone `--version`/`-V` or `--help`/`-h`; `None` for any other arguments (a flag
/// followed by more arguments is a usage error like any unknown command).
fn info(args: &[String]) -> Option<String> {
    match args {
        [a] if a == "--version" || a == "-V" => {
            Some(format!("ham-admin {}", env!("CARGO_PKG_VERSION")))
        }
        [a] if a == "--help" || a == "-h" => Some(USAGE.to_string()),
        _ => None,
    }
}

fn target(s: &str) -> Result<RepoTarget, String> {
    s.parse().map_err(|e| format!("{e}"))
}

fn parse(args: &[String]) -> Result<Cmd, String> {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["repos", "allow", t] => Ok(Cmd::Allow(target(t)?)),
        ["repos", "deny", t] => Ok(Cmd::Deny(target(t)?)),
        ["repos", "list"] => Ok(Cmd::List),
        _ => Err("unknown or incomplete command".into()),
    }
}

fn line(e: &RepoAllowView) -> String {
    format!("{} added by {} at {}", e.target, e.added_by, e.added_at)
}

/// The API's own reason for a refusal; the error text otherwise.
fn reason(e: &ApiError) -> String {
    match e {
        ApiError::Http { error, .. } => error.message.clone(),
        other => other.to_string(),
    }
}

async fn run(args: &[String]) -> i32 {
    if let Some(answer) = info(args) {
        println!("{answer}");
        return 0;
    }
    let cmd = match parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ham-admin: {e}\n{USAGE}");
            return 2;
        }
    };
    let api = match ham::cli::configured_api().await {
        Ok(a) => a,
        Err(e) => {
            eprintln!("ham-admin: {e}");
            return 1;
        }
    };
    let (action, target) = match cmd {
        Cmd::List => {
            return match api.list_repos().await {
                Ok(entries) => {
                    for e in &entries {
                        println!("{}", line(e));
                    }
                    0
                }
                Err(e) => {
                    eprintln!("ham-admin: {}", reason(&e));
                    1
                }
            };
        }
        Cmd::Allow(t) => (RepoAdminAction::Allow, t),
        Cmd::Deny(t) => (RepoAdminAction::Deny, t),
    };
    let req = RepoAdminRequest { action, target };
    match api.admin_repos(&req).await {
        Ok(r) => {
            match (&r.action, &r.entry) {
                (RepoAdminAction::Allow, Some(e)) => println!(
                    "allowed {} (added by {} at {})",
                    r.target, e.added_by, e.added_at
                ),
                _ => println!("denied {}", r.target),
            }
            0
        }
        Err(e) => {
            eprintln!("ham-admin: {}", reason(&e));
            1
        }
    }
}

#[tokio::main]
async fn main() {
    // nosemgrep: args -- the first element, the program path, is skipped and the rest is the ham-admin subcommand and options, parsed as data by the run function and never used to decide who may do what
    let args: Vec<String> = std::env::args().skip(1).collect();
    // COMPAT-CAH: the operator tool reads the config folder and the variables of `ham`, so it makes the same notices of the window of the earlier
    // names, once, before it does anything else. It has no link under an earlier name.
    ham::cli::announce_earlier_names(false, &args);
    std::process::exit(run(&args).await);
}
