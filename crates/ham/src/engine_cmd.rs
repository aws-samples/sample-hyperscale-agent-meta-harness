//! `ham engine <name> init|show|check|edit|set|unset|import`: the settings file of one
//! engine, `~/.config/ham/engines/<name>.toml`. None of the verbs needs the network, the config file or AWS credentials.
//!
//! The engine is an argument: this module names no engine. It asks `ham_core::engines` whether the build has a key table for the word (the others
//! answer "no settings schema in this build yet") and `ham_core::engine_file` for the reading (one buffer, the shared check, a line for every
//! problem). A change is made as text by the comment-keeping patcher (`ham_core::config_edit`) and written by the same writer as `config.toml`
//! ([`crate::config_cmd::commit_file`]: a temp file in the same folder, synced, a re-read and SHA-256 compare just before the rename, a link followed
//! and kept, the mode kept and a new file private). A new text is read again with the shared check before it is written: a change that would leave
//! the file refused writes nothing. `unset` is the one change that may leave a problem behind, because it is how a bad key is removed; it says what
//! is still wrong.
//!
//! `import <file>` validates a file the same way and copies its bytes, comments and all; it refuses to replace an existing file without `--replace`.
//! `init` writes the template once and never changes a file that is there. `edit` opens `$VISUAL`, `$EDITOR`, else `vi`, then checks the file.

use crate::config_cmd::commit_file;
use ham_core::command::{EngineAction, EngineCommand};
use ham_core::config_edit::{self, Edit, EditValue, Outcome};
use ham_core::engine_file::{self, Disk, EngineFile, Reading, Source, shown_path};
use ham_core::engines;
use ham_core::exit::{ERROR, USAGE};
use ham_core::sanitize::sanitize_line;
use harness_protocol::direct::{EngineConfig, KeyKind, KeyRow, check_engine_config, keys_for};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// What a write that lost the race says (the file's name follows `engines/`).
fn changed_words(engine: &str) -> String {
    format!(
        "{} changed on disk; reloaded; nothing written",
        shown_path(engine)
    )
}

fn fail(err: &mut dyn Write, msg: impl std::fmt::Display) -> i32 {
    let _ = writeln!(err, "ham: {msg}");
    ERROR
}

/// A refusal: the usage exit code, the reason on standard error.
fn refuse(err: &mut dyn Write, msg: impl std::fmt::Display) -> i32 {
    let _ = writeln!(err, "ham: {msg}");
    USAGE
}

/// The engine has a key table in this build, or why it has none.
pub fn engine_ready(engine: &str) -> Result<(), String> {
    let Some(e) = engines::find(engine) else {
        let ids: Vec<&str> = engines::roadmap().iter().map(|e| e.id).collect();
        return Err(format!(
            "unknown engine {engine:?}; the engines are {}",
            ids.join(", ")
        ));
    };
    if engines::has_settings(engine) {
        Ok(())
    } else {
        Err(format!("{}: {}", e.name, engines::NO_SETTINGS_SCHEMA))
    }
}

/// Reads the engine file once (an absent file is an empty one).
pub fn load(env: Env<'_>, engine: &str) -> Result<EngineFile, String> {
    let path = engine_file::engine_path(env, engine)
        .ok_or_else(|| "no config location: set XDG_CONFIG_HOME or HOME".to_string())?;
    EngineFile::load(&path, &Disk)
}

// ---- the plan of a change: text in, text out ----------------------------------------------------------------------------------------

/// What setting or clearing one key comes to, before anything is written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The dotted name of the key.
    pub key: String,
    /// The value after the change as the card words it (`None` after an unset).
    pub stored: Option<String>,
    /// The new text of the file; `None` for a change that changes nothing (nothing is written, the modification time stays).
    pub text: Option<String>,
    /// What the new text still has wrong, one line each (only an unset may leave something).
    pub still_wrong: Vec<String>,
}

fn rank(path: &[String]) -> usize {
    if path == ["schema"] { 0 } else { usize::MAX }
}

/// True when a problem means the text cannot be used as TOML at all, so no change can be made to it.
fn unreadable(code: &str) -> bool {
    matches!(
        code,
        "syntax" | "nul" | "bom" | "not_utf8" | "too_large" | "too_deep"
    )
}

/// A name of a key that is short plain text: parts of `[a-z0-9_-]` joined by dots, at most 40 bytes (the rule that keeps a name out of a message
/// when it could carry an escape sequence).
pub fn plain_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 40
        && key.split('.').all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
        })
}

/// The words for a key that is not in the engine's table: the check's own, so `set aws.profile x` is refused like a file that holds it.
fn unknown_key_words(engine: &str, key: &str) -> String {
    if !plain_key(key) {
        return "that is not the name of an engine setting".to_string();
    }
    let mut value = Value::String("x".to_string());
    for part in key.split('.').rev() {
        let mut m = Map::new();
        m.insert(part.to_string(), value);
        value = Value::Object(m);
    }
    let Value::Object(settings) = value else {
        return "that is not the name of an engine setting".to_string();
    };
    let cfg = EngineConfig {
        engine: engine.to_string(),
        schema: 1,
        settings,
    };
    match check_engine_config(&cfg) {
        Err(p) => engine_file::describe(&p),
        Ok(()) => format!("key \"{key}\" is not an engine setting"),
    }
}

fn edit_value(row: &KeyRow, value: &str) -> Result<EditValue, String> {
    let bad = || format!("{} takes {}", row.path, row.kind.describe());
    match row.kind {
        KeyKind::Choice(_) => Ok(EditValue::Str(value.to_string())),
        KeyKind::Int { .. } => value
            .trim()
            .parse::<i64>()
            .map(EditValue::Int)
            .map_err(|_| bad()),
        KeyKind::Float { .. } => value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|x| x.is_finite())
            .map(EditValue::Float)
            .ok_or_else(bad),
        KeyKind::Set(_) => Ok(EditValue::StrList(
            value
                .split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect(),
        )),
    }
}

fn refusal_words(r: &config_edit::Refusal, engine: &str) -> String {
    let shown = shown_path(engine);
    match r {
        config_edit::Refusal::NotToml { line, column } => format!(
            "{shown} line {line} column {column}: the file is not valid TOML, so it is not changed; fix it with `ham engine {engine} edit`"
        ),
        config_edit::Refusal::Unsupported { line, reason } => format!(
            "{shown} line {line}: {reason}; edit the file by hand with `ham engine {engine} edit`"
        ),
        config_edit::Refusal::NotProvable => format!(
            "{shown}: the edit could not be shown to change only that key, so nothing was written; edit the file by hand"
        ),
    }
}

/// A file that cannot be changed as it is: not usable as TOML, or written for a newer `ham`. The words, or `None`.
fn cannot_change(reading: &Reading, engine: &str) -> Option<String> {
    let shown = shown_path(engine);
    reading
        .problems
        .iter()
        .find(|p| unreadable(&p.code) || p.code == "newer_schema")
        .map(|p| {
            let what = if p.code == "syntax" {
                // The parser's own short message, with the line and column, never the line of the file.
                let at = match (p.line, p.column) {
                    (Some(l), Some(c)) => format!(" line {l} column {c}"),
                    _ => String::new(),
                };
                format!(
                    "{shown}{at}: the file is not valid TOML ({})",
                    sanitize_line(&p.message)
                )
            } else {
                p.render(&shown)
            };
            format!("{what}, so it is not changed; fix it with `ham engine {engine} edit`")
        })
}

/// What `ham engine <name> set <key> <value>` does to `text`: nothing when the key already holds the value; else the new text, read again with
/// the shared check and refused (`Err`, nothing to write) if it has any problem.
pub fn plan_set(engine: &str, text: &str, key: &str, value: &str) -> Result<Plan, String> {
    let shown = shown_path(engine);
    let rows = keys_for(engine).ok_or_else(|| engines::NO_SETTINGS_SCHEMA.to_string())?;
    let Some(row) = rows.iter().find(|r| r.path == key) else {
        return Err(unknown_key_words(engine, key));
    };
    let value = edit_value(row, value)?;
    if let Some(why) = cannot_change(&engine_file::read_text(text, engine), engine) {
        return Err(why);
    }
    let edits = [
        Edit::Set {
            path: vec!["schema".to_string()],
            value: EditValue::Int(1),
        },
        Edit::Set {
            path: key.split('.').map(str::to_string).collect(),
            value,
        },
    ];
    match config_edit::apply(text, &edits, &rank) {
        Err(r) => Err(refusal_words(&r, engine)),
        Ok(Outcome::Unchanged) => Ok(Plan {
            key: key.to_string(),
            stored: engine_file::read_text(text, engine)
                .config
                .and_then(|c| c.get(key).map(engine_file::value_text)),
            text: None,
            still_wrong: Vec::new(),
        }),
        Ok(Outcome::Changed(new)) => {
            let reading = engine_file::read_text(&new, engine);
            if !reading.ok() {
                return Err(format!(
                    "{}\nNothing was written.",
                    reading.render(&shown).join("\n")
                ));
            }
            let stored = reading
                .config
                .as_ref()
                .and_then(|c| c.get(key).map(engine_file::value_text));
            Ok(Plan {
                key: key.to_string(),
                stored,
                text: Some(new),
                still_wrong: Vec::new(),
            })
        }
    }
}

/// What `ham engine <name> unset <key>` does to `text`: nothing when the file does not have the key; else the new text. The key may be any plain
/// name the file has, so a key the check refuses can be removed; whatever the new text still has wrong is in `still_wrong`.
pub fn plan_unset(engine: &str, text: &str, key: &str) -> Result<Plan, String> {
    let shown = shown_path(engine);
    if !plain_key(key) {
        return Err("that is not the name of an engine setting".to_string());
    }
    if let Some(why) = cannot_change(&engine_file::read_text(text, engine), engine) {
        return Err(why);
    }
    let edits = [Edit::Clear {
        path: key.split('.').map(str::to_string).collect(),
    }];
    match config_edit::apply(text, &edits, &rank) {
        Err(r) => Err(refusal_words(&r, engine)),
        Ok(Outcome::Unchanged) => Ok(Plan {
            key: key.to_string(),
            stored: None,
            text: None,
            still_wrong: Vec::new(),
        }),
        Ok(Outcome::Changed(new)) => {
            let still_wrong = engine_file::read_text(&new, engine).render(&shown);
            Ok(Plan {
                key: key.to_string(),
                stored: None,
                text: Some(new),
                still_wrong,
            })
        }
    }
}

// ---- writing --------------------------------------------------------------------------------------------------------------------------

/// What a `set` or `unset` did.
#[derive(Clone, Debug)]
pub struct Applied {
    pub path: PathBuf,
    pub key: String,
    /// The value after the change (`None` after an unset).
    pub value: Option<String>,
    /// False for a change that changed nothing: the file was not written.
    pub wrote: bool,
    pub still_wrong: Vec<String>,
}

fn write_plan(file: &EngineFile, engine: &str, plan: Plan) -> Result<Applied, String> {
    let wrote = match &plan.text {
        Some(text) => {
            commit_file(file.path(), file.digest(), text, &changed_words(engine))?;
            true
        }
        None => false,
    };
    Ok(Applied {
        path: file.path().to_path_buf(),
        key: plan.key,
        value: plan.stored,
        wrote,
        still_wrong: plan.still_wrong,
    })
}

/// `ham engine <name> set <key> <value>` on the file `env` points at.
pub fn apply_set(env: Env<'_>, engine: &str, key: &str, value: &str) -> Result<Applied, String> {
    engine_ready(engine)?;
    if let Some(text) = ham_core::config_dir::write_refusal(env) {
        return Err(text); // COMPAT-CAH: nothing is written while only the folder of the earlier command name holds the files
    }
    let file = load(env, engine)?;
    let plan = plan_set(engine, file.text(), key, value)?;
    write_plan(&file, engine, plan)
}

/// `ham engine <name> unset <key>` on the file `env` points at.
pub fn apply_unset(env: Env<'_>, engine: &str, key: &str) -> Result<Applied, String> {
    engine_ready(engine)?;
    if let Some(text) = ham_core::config_dir::write_refusal(env) {
        return Err(text); // COMPAT-CAH: nothing is written while only the folder of the earlier command name holds the files
    }
    let file = load(env, engine)?;
    let plan = plan_unset(engine, file.text(), key)?;
    write_plan(&file, engine, plan)
}

/// Creates `path` with `text` unless it exists; true when it wrote. The folder is made if it is not there; a new file is private.
fn create_once(path: &Path, text: &str) -> Result<bool, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut f) => f
            .write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map(|()| true)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

// ---- the verbs ------------------------------------------------------------------------------------------------------------------------

fn problem_json(p: &engine_file::Problem) -> Value {
    json!({"code": p.code, "line": p.line, "column": p.column, "key": p.key, "message": sanitize_line(&p.message)})
}

fn run_init(
    env: Env<'_>,
    engine: &str,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let file = match load(env, engine) {
        Ok(f) => f,
        Err(e) => return fail(err, e),
    };
    let shown = shown_path(engine);
    let Some(template) = engines::settings_template(engine) else {
        return refuse(err, engines::NO_SETTINGS_SCHEMA);
    };
    let wrote = if file.exists() {
        false
    } else {
        match create_once(file.path(), &template) {
            Ok(w) => w,
            Err(e) => return fail(err, e),
        }
    };
    let path = file.path().display().to_string();
    if json_out {
        let _ = writeln!(out, "{}", json!({"path": path, "created": wrote}));
    } else if wrote {
        let _ = writeln!(
            out,
            "wrote {shown} ({} bytes): every setting is commented out, so a run still uses the engine's own; remove the # of a line to use it",
            template.len()
        );
    } else {
        let _ = writeln!(out, "{shown} already exists; it was left as it is");
    }
    0
}

fn run_show(
    env: Env<'_>,
    engine: &str,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let file = match load(env, engine) {
        Ok(f) => f,
        Err(e) => return fail(err, e),
    };
    let shown = shown_path(engine);
    let reading = file.read(engine);
    let settings_hash = reading.config.as_ref().map(EngineConfig::canonical_sha256);
    let keys = reading.set_keys();
    if json_out {
        let _ = writeln!(
            out,
            "{}",
            json!({
                "engine": engine, "path": file.path().display().to_string(), "exists": file.exists(),
                "bytes": file.file_len(), "fileSha256": file.file_sha256(), "settingsSha256": settings_hash,
                "keys": keys.iter().map(|k| json!({"key": k.path, "value": k.value, "line": k.line})).collect::<Vec<_>>(),
                "problems": reading.problems.iter().map(problem_json).collect::<Vec<_>>(),
            })
        );
        return if reading.ok() { 0 } else { USAGE };
    }
    let _ = writeln!(
        out,
        "{}",
        sanitize_line(&format!("engine settings file: {}", file.path().display()))
    );
    if !file.exists() {
        let _ = writeln!(
            out,
            "  none yet: a run uses the engine's own settings; `ham engine {engine} init` writes a template"
        );
        return 0;
    }
    let _ = writeln!(out, "  size: {} bytes", file.file_len());
    if let Some(sha) = file.file_sha256() {
        let _ = writeln!(out, "  file sha256: {sha}");
    }
    if !reading.ok() {
        for p in &reading.problems {
            let _ = writeln!(err, "ham: {}", p.render(&shown));
        }
        let _ = writeln!(
            err,
            "ham: {shown} is refused, so a run would send none of it; `ham engine {engine} edit` opens the file"
        );
        return USAGE;
    }
    match settings_hash {
        Some(hash) => {
            let _ = writeln!(
                out,
                "  settings hash: {hash}   (the run card shows the first 12 digits)"
            );
            let _ = writeln!(out, "  the next run sends {} setting(s):", keys.len());
            for k in &keys {
                let at = k
                    .line
                    .map_or_else(String::new, |l| format!("   (line {l})"));
                let _ = writeln!(
                    out,
                    "    {} = {}{at}",
                    k.path,
                    engines::shown_value(k.path, &k.value)
                );
            }
        }
        None => {
            let _ = writeln!(
                out,
                "  no setting is set: a run uses the engine's own settings"
            );
        }
    }
    0
}

fn run_check(
    env: Env<'_>,
    engine: &str,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let file = match load(env, engine) {
        Ok(f) => f,
        Err(e) => return fail(err, e),
    };
    let shown = shown_path(engine);
    let reading = file.read(engine);
    let path = file.path().display().to_string();
    let ok = reading.ok();
    if json_out {
        let _ = writeln!(
            out,
            "{}",
            json!({"engine": engine, "path": path, "exists": file.exists(), "ok": ok,
                   "keys": reading.set_keys().len(),
                   "problems": reading.problems.iter().map(problem_json).collect::<Vec<_>>()})
        );
        return if ok { 0 } else { USAGE };
    }
    if !file.exists() {
        let _ = writeln!(
            out,
            "no engine file at {path}; every setting is the engine's own"
        );
        return 0;
    }
    if ok {
        let n = reading.set_keys().len();
        let _ = writeln!(out, "{shown}: ok ({n} setting(s) set)");
        return 0;
    }
    for line in reading.render(&shown) {
        let _ = writeln!(out, "error: {line}");
    }
    let _ = writeln!(out, "{shown}: refused; a run would send none of it");
    USAGE
}

fn run_set(
    env: Env<'_>,
    engine: &str,
    key: &str,
    value: &str,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match apply_set(env, engine, key, value) {
        Err(e) => refuse(err, e),
        Ok(a) => {
            let stored = a.value.clone().unwrap_or_default();
            if json_out {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"key": a.key, "value": stored, "path": a.path.display().to_string(), "wrote": a.wrote})
                );
            } else if a.wrote {
                let _ = writeln!(out, "{} = {stored}", a.key);
            } else {
                let _ = writeln!(
                    out,
                    "{} is already {stored}; the file was not written",
                    a.key
                );
            }
            0
        }
    }
}

fn run_unset(
    env: Env<'_>,
    engine: &str,
    key: &str,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match apply_unset(env, engine, key) {
        Err(e) => refuse(err, e),
        Ok(a) => {
            if json_out {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"key": a.key, "value": null, "wrote": a.wrote})
                );
            } else if a.wrote {
                let _ = writeln!(out, "{} = (not set)", a.key);
            } else {
                let _ = writeln!(out, "{} is not set; the file was not written", a.key);
            }
            if !a.still_wrong.is_empty() {
                for line in &a.still_wrong {
                    let _ = writeln!(err, "ham: warning: {}", sanitize_line(line));
                }
                let _ = writeln!(
                    err,
                    "ham: warning: {} still has a problem; `ham engine {engine} check` lists it and `ham engine {engine} edit` opens the file",
                    shown_path(engine)
                );
            }
            0
        }
    }
}

fn run_import(
    env: Env<'_>,
    engine: &str,
    source: &str,
    replace: bool,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let shown = shown_path(engine);
    let from = Path::new(source);
    let (bytes, len) = match Disk.read(from) {
        Ok(Some(x)) => x,
        Ok(None) => return refuse(err, format!("{source}: no such file")),
        Err(e) => return refuse(err, e),
    };
    let reading = engine_file::read_bytes(&bytes, len, engine);
    if !reading.ok() {
        for line in reading.render(&sanitize_line(source)) {
            let _ = writeln!(err, "ham: {line}");
        }
        let _ = writeln!(err, "ham: nothing was imported; {shown} is as it was");
        return USAGE;
    }
    let file = match load(env, engine) {
        Ok(f) => f,
        Err(e) => return fail(err, e),
    };
    if file.exists() && file.bytes() == bytes.as_slice() {
        let _ = writeln!(out, "{shown} already holds this file; nothing was written");
        return 0;
    }
    if file.exists() && !replace {
        return refuse(
            err,
            format!(
                "{shown} already exists; `--replace` replaces it with {}",
                sanitize_line(source)
            ),
        );
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return refuse(err, format!("{source}: the file is not UTF-8 text"));
    };
    if let Err(e) = commit_file(file.path(), file.digest(), &text, &changed_words(engine)) {
        return fail(err, e);
    }
    let n = reading.set_keys().len();
    let hash = reading.config.as_ref().map(EngineConfig::canonical_sha256);
    if json_out {
        let _ = writeln!(
            out,
            "{}",
            json!({"path": file.path().display().to_string(), "keys": n, "settingsSha256": hash, "replaced": file.exists()})
        );
    } else {
        let _ = writeln!(
            out,
            "imported {} as {shown} ({n} setting(s); sha256 {})",
            sanitize_line(source),
            hex(&Sha256::digest(text.as_bytes()))
        );
    }
    0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn run_edit(env: Env<'_>, engine: &str, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let file = match load(env, engine) {
        Ok(f) => f,
        Err(e) => return fail(err, e),
    };
    // The editor gets the file a link points to, because many editors replace the file they save, and the replacement of a link would lose the link.
    let target = match std::fs::symlink_metadata(file.path()) {
        Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(file.path()) {
            Ok(t) => t,
            Err(e) => return fail(err, format!("{}: {e}", file.path().display())),
        },
        _ => file.path().to_path_buf(),
    };
    if !file.exists()
        && let Some(template) = engines::settings_template(engine)
        && let Err(e) = create_once(&target, &template)
    {
        return fail(err, e);
    }
    if let Err(e) = crate::rules_cmd::run_editor(&crate::rules_cmd::editor(env), &target) {
        return fail(err, e);
    }
    let code = run_check(env, engine, false, out, err);
    if code != 0 {
        let _ = writeln!(err, "ham: fix the file with: ham engine {engine} edit");
    }
    code
}

/// `ham engine <name> ...` against the file `env` points at. Exit 0 when it did what it was asked, 2 when it refused (a file the check refuses,
/// an engine with no key table, a value out of range), 1 for a file that could not be read or written.
pub fn run(
    cmd: &EngineCommand,
    env: Env<'_>,
    json_out: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let engine = cmd.engine.as_str();
    if let Err(e) = engine_ready(engine) {
        return refuse(err, e);
    }
    match &cmd.action {
        EngineAction::Init => run_init(env, engine, json_out, out, err),
        EngineAction::Show => run_show(env, engine, json_out, out, err),
        EngineAction::Check => run_check(env, engine, json_out, out, err),
        EngineAction::Edit => run_edit(env, engine, out, err),
        EngineAction::Set { key, value } => run_set(env, engine, key, value, json_out, out, err),
        EngineAction::Unset { key } => run_unset(env, engine, key, json_out, out, err),
        EngineAction::Import { file, replace } => {
            run_import(env, engine, file, *replace, json_out, out, err)
        }
    }
}
