//! `ham lifecycle init <dir> [--kiro]`: the lifecycle pack copied into a repository, for runs that target
//! one. `AGENTS.md` (the contract) and `.agents/skills/lifecycle-*/` land in `<dir>` (made when it is missing); with `--kiro` the link
//! `.kiro/skills` points at `.agents/skills`. A file that is already there is kept, never overwritten, and a repository's own `AGENTS.md` stays
//! its own: the report says the contract is not in it. A link where a pack file belongs is an error and nothing is written through it.
//! No network and no credentials.

use ham_core::command::LifecycleCommand;
use ham_core::exit::ERROR;
use ham_core::pack::{self, Installed, KiroLink};
use serde_json::json;
use std::io::Write;
use std::path::Path;

fn action(how: &Installed) -> &'static str {
    match how {
        Installed::Created => "created",
        Installed::Same => "same",
        Installed::KeptEdited => "kept",
        // `install_repo` never replaces a file of a repository; the word is here so that the match is whole.
        Installed::Updated { .. } => "updated",
    }
}

/// Runs `ham lifecycle init`; returns the exit code. Output goes to `out`, errors to `err`.
pub fn run(
    cmd: &LifecycleCommand,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let LifecycleCommand::Init { dir, kiro } = cmd;
    let target = Path::new(dir);
    if target.exists() && !target.is_dir() {
        let _ = writeln!(err, "ham: {dir} is not a directory");
        return ERROR;
    }
    let done = match pack::install_repo(target, *kiro) {
        Ok(d) => d,
        Err(e) => {
            let _ = writeln!(err, "ham: {e}");
            return ERROR;
        }
    };
    if json_out {
        let record = json!({
            "pack": pack::NAME,
            "version": pack::version(),
            "digest": pack::digest(),
            "dir": dir,
            "files": done.files.iter().map(|(path, how)| json!({"path": path, "action": action(how)})).collect::<Vec<_>>(),
            "kiro": done.kiro.as_ref().map(|k| match k {
                KiroLink::Created => "created".to_string(),
                KiroLink::Same => "same".to_string(),
                KiroLink::Kept(why) => format!("kept: {why}"),
            }),
        });
        let _ = writeln!(out, "{record}");
        return 0;
    }
    let digest = pack::digest();
    let _ = writeln!(
        out,
        "lifecycle pack {} ({}) in {dir}",
        pack::version(),
        &digest[..12]
    );
    for (path, how) in &done.files {
        match how {
            Installed::Created => {
                let _ = writeln!(out, "  created {path}");
            }
            Installed::Same => {
                let _ = writeln!(out, "  kept {path} (already there; not changed)");
            }
            Installed::KeptEdited if path == "AGENTS.md" => {
                let _ = writeln!(
                    out,
                    "  kept {path} (it is your own; the lifecycle contract is not in it. The pack's text is lifecycle/pack/AGENTS.md of this release, or `ham rules init` for a rule file)"
                );
            }
            Installed::KeptEdited => {
                let _ = writeln!(
                    out,
                    "  kept {path} (different from the pack's text; not changed)"
                );
            }
            Installed::Updated { was } => {
                let _ = writeln!(out, "  updated {path} (was pack {was})");
            }
        }
    }
    match &done.kiro {
        Some(KiroLink::Created) => {
            let _ = writeln!(out, "  created .kiro/skills (a link to ../.agents/skills)");
        }
        Some(KiroLink::Same) => {
            let _ = writeln!(
                out,
                "  kept .kiro/skills (already linked to ../.agents/skills)"
            );
        }
        Some(KiroLink::Kept(why)) => {
            let _ = writeln!(out, "  kept .kiro/skills: {why}");
        }
        None => {}
    }
    0
}
