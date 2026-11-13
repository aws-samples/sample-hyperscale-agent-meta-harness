//! The fake engine with the measured behaviour of the pinned Cline (3.0.71) on the points message control depends on.
//! `fake-acp-engine` runs it when `FAKE_ACP_CLINE=1`.
//!
//! It is a small ACP agent, not a replay. What it copies from the live measurements, and why a test wants it:
//!
//! - **An internal queue.** A second `session/prompt` while one is open is queued; its request is answered at once with `end_turn` and
//!   every usage 0; its text runs later as a turn of its own, with no open prompt, no stop reason and no usage; a `session/cancel` after
//!   that does nothing. A harness that sent two prompts at once would see exactly this, so the fake logs `overlap` and a test can fail on it.
//! - **Cancel.** `session/cancel` of a single open prompt answers `cancelled` in about 0.2 s and kills the shell tool (the tool's own process
//!   only: a process it started and left behind is not touched, which is how the pinned engine leaves orphans).
//! - **Tool processes outlive the engine.** A tool runs as its own process (a `sleep N`) in a process group of its own, so a
//!   kill of the engine's group does not reach it.
//! - **Faults.** An engine can hang (answer no cancel), die in the middle of a turn, or answer a provider's 503.
//!
//! A prompt's text may hold one `@fake{...}` JSON object that scripts the turn. All members are optional:
//!
//! | member | effect |
//! |---|---|
//! | `work_ms` | how long the model streams before it ends (default 40, or `FAKE_ACP_CLINE_WORK_MS`) |
//! | `say` | the final message (default `OK`); put `DONE` in it for the harness's done marker |
//! | `write` | `{"path", "text"}`: writes a file of the workspace at the start of the turn |
//! | `tool` | `{"secs", "pidfile", "orphan", "survive_cancel"}`: runs a shell tool for `secs`; the tool's pid (and the orphan's) is appended to `pidfile` |
//! | `stream` | `{"chunks", "step_ms"}`: after the model work, streams `chunks` message chunks (`stream-1`, `stream-2` ...), one per `step_ms`; a cancel stops it at once |
//! | `edit` | `{"path", "parts", "step_ms"}`: a streamed file edit: the file is emptied, then one part is appended per `step_ms`; a cancel stops after the parts written and reports the edit aborted |
//! | `ignore_cancel` | the engine answers no cancel for this turn |
//! | `queued_reply` | this prompt gets the queue treatment although none is open: an immediate `end_turn` with usage 0, then the turn runs unattached |
//! | `die_ms` | the process exits (code 3) this long into the turn, without a response |
//! | `fail_times` | the first n prompts with this exact text are answered with the provider's 503 |
//! | `gen` | the directives apply to the nth engine process of the workspace only; a restarted engine ignores them |
//! | `cancel_reply_ms` | how long a cancel takes to answer (default 150) |
//!
//! Each prompt's open and end, each overlap and each cancel is a line in `FAKE_ACP_LOG` (`prompt-open`, `prompt-end`, `overlap`,
//! `cancel ... acted=`), next to every line in and out.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The scripted part of a prompt's text.
#[derive(Clone, Debug, Default)]
struct Directives {
    work_ms: u64,
    say: Option<String>,
    write: Option<(String, String)>,
    tool: Option<Tool>,
    stream: Option<(u64, u64)>,
    edit: Option<Edit>,
    ignore_cancel: bool,
    queued_reply: bool,
    die_ms: Option<u64>,
    fail_times: u32,
    cancel_reply_ms: u64,
}

#[derive(Clone, Debug)]
struct Edit {
    path: String,
    parts: Vec<String>,
    step_ms: u64,
}

#[derive(Clone, Debug)]
struct Tool {
    secs: u64,
    pidfile: Option<String>,
    orphan: bool,
    survive_cancel: bool,
}

/// Finds `@fake{...}` in `text` (the first balanced object) and reads it.
fn directives(text: &str, generation: u32) -> Directives {
    let mut d = Directives {
        work_ms: std::env::var("FAKE_ACP_CLINE_WORK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(40),
        cancel_reply_ms: 150,
        ..Directives::default()
    };
    let Some(at) = text.find("@fake{") else {
        return d;
    };
    let body = &text[at + 5..];
    let mut depth = 0usize;
    let mut end = None;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in body.char_indices() {
        if in_str {
            if esc {
                esc = false;
            } else if c == '\\' {
                esc = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(v) = end.and_then(|e| serde_json::from_str::<Value>(&body[..e]).ok()) else {
        return d;
    };
    // `gen`: the directives are for the nth process of the workspace only. A fresh session's prompt quotes the task, directives included,
    // and a restarted engine must not repeat the fault of the first.
    if v.get("gen")
        .and_then(Value::as_u64)
        .is_some_and(|g| g != u64::from(generation))
    {
        return d;
    }
    let num = |k: &str| v.get(k).and_then(Value::as_u64);
    d.work_ms = num("work_ms").unwrap_or(d.work_ms);
    d.say = v.get("say").and_then(Value::as_str).map(String::from);
    d.write = v.get("write").and_then(|w| {
        Some((
            w.get("path")?.as_str()?.to_string(),
            w.get("text")?.as_str()?.to_string(),
        ))
    });
    d.tool = v.get("tool").map(|t| Tool {
        secs: t.get("secs").and_then(Value::as_u64).unwrap_or(30),
        pidfile: t.get("pidfile").and_then(Value::as_str).map(String::from),
        orphan: t.get("orphan").and_then(Value::as_bool).unwrap_or(false),
        survive_cancel: t
            .get("survive_cancel")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    });
    d.stream = v.get("stream").map(|t| {
        (
            t.get("chunks").and_then(Value::as_u64).unwrap_or(10),
            t.get("step_ms").and_then(Value::as_u64).unwrap_or(50),
        )
    });
    d.edit = v.get("edit").and_then(|e| {
        Some(Edit {
            path: e.get("path")?.as_str()?.to_string(),
            parts: e
                .get("parts")?
                .as_array()?
                .iter()
                .filter_map(|p| p.as_str().map(String::from))
                .collect(),
            step_ms: e.get("step_ms").and_then(Value::as_u64).unwrap_or(100),
        })
    });
    d.ignore_cancel = v
        .get("ignore_cancel")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    d.queued_reply = v
        .get("queued_reply")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    d.die_ms = num("die_ms");
    d.fail_times = num("fail_times").map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX));
    d.cancel_reply_ms = num("cancel_reply_ms").unwrap_or(d.cancel_reply_ms);
    d
}

struct Session {
    id: String,
    /// A turn is executing, attached to a prompt or drained from the queue.
    busy: AtomicBool,
    /// The executing turn answers a prompt (a cancel can reach it).
    open: AtomicBool,
    /// The executing turn ignores a cancel.
    deaf: AtomicBool,
    cancel: AtomicBool,
    queue_used: AtomicBool,
    queued: Mutex<Vec<String>>,
}

struct Shared {
    out: Mutex<Box<dyn Write + Send>>,
    /// What `die_ms` does: the process exits with code 3 (in a test, a flag is set instead).
    die: Box<dyn Fn() + Send + Sync>,
    log: Mutex<Option<std::fs::File>>,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    workspace: PathBuf,
    generation: u32,
    model: Option<String>,
    fails: Mutex<HashMap<String, u32>>,
    calls: AtomicU32,
}

impl Shared {
    fn log(&self, line: &str) {
        if let Some(f) = self.log.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
            let _ = writeln!(f, "{line}");
        }
    }

    fn emit(&self, v: &Value) {
        self.log(&format!("out {v}"));
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    fn update(&self, sid: &str, u: Value) {
        self.emit(&json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}}));
    }

    fn chunk(&self, sid: &str, text: &str) {
        self.update(
            sid,
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}),
        );
    }

    fn respond(&self, id: &Value, result: Value) {
        self.emit(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: &Value, code: i64, message: &str, data: Option<Value>) {
        let mut e = json!({"code": code, "message": message});
        if let Some(d) = data {
            e["data"] = d;
        }
        self.emit(&json!({"jsonrpc": "2.0", "id": id, "error": e}));
    }

    fn path(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.workspace.join(p)
        }
    }
}

fn head(text: &str) -> String {
    text.chars().take(40).collect::<String>().replace('\n', " ")
}

fn kill_pid(pid: u32) {
    let _ = Command::new("kill")
        .args(["-KILL", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The usage a model turn reports; a queued request reports zeros.
fn usage(n: u64) -> Value {
    json!({"inputTokens": 100 + n, "outputTokens": 20 + n, "totalTokens": 120 + 2 * n})
}

/// Why a wait ended.
enum Waited {
    Done,
    Cancelled,
    /// `die_ms` passed: the engine is gone (the process has exited, or the test's flag is set).
    Died,
}

/// Sleeps `ms` in slices of 20 ms; a cancel the engine acts on ends it early; `die_ms` ends the process.
fn wait_ms(sh: &Shared, s: &Session, d: &Directives, started: Instant, ms: u64) -> Waited {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        if let Some(die) = d.die_ms
            && started.elapsed() >= Duration::from_millis(die)
        {
            sh.log("exiting 3 in the middle of a turn");
            (sh.die)();
            return Waited::Died;
        }
        if s.cancel.load(Ordering::SeqCst) {
            return Waited::Cancelled;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Waited::Done
}

/// Appends one pid to the pid file a scenario named (a test reads it to find the processes the tool started).
fn record_pid(pidfile: Option<&Path>, pid: u32) {
    use std::io::Write;
    if let Some(p) = pidfile
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
    {
        let _ = writeln!(f, "{pid}");
    }
}

/// Leaves a `sleep` to init, the way a tool command that forks and exits does: a short-lived shell starts it in the background and
/// ends at once. The shell text holds only the command, its duration and a `&`; no expansion is involved. The sleep's duration is unique
/// (it carries the clock's nanoseconds), so its pid is found in the process list by its command line and recorded in `pidfile`.
fn leave_orphan(sh: &Shared, secs: u64, pidfile: Option<&Path>) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let length = format!("{}.{nanos:09}", secs + 60);
    let started = Command::new("sh")
        .arg("-c")
        .arg(format!("sleep {length} &"))
        .current_dir(&sh.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .status()
        .is_ok_and(|s| s.success());
    if !started {
        return;
    }
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if let Some(pid) = pid_of_sleep(&length) {
            record_pid(pidfile, pid);
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The pid of the process whose command line is `sleep <length>`.
fn pid_of_sleep(length: &str) -> Option<u32> {
    let want = [b"sleep".as_slice(), length.as_bytes()];
    for entry in std::fs::read_dir("/proc").ok()?.filter_map(Result::ok) {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let words: Vec<&[u8]> = cmdline
            .split(|b| *b == 0)
            .filter(|w| !w.is_empty())
            .collect();
        if words == want {
            return Some(pid);
        }
    }
    None
}

/// Starts the shell tool: a `sleep` of its own in a group of its own; its pid goes to `pidfile` from here. With `orphan` a second
/// `sleep` is left to init first (its pid is recorded before the tool's).
fn start_tool(sh: &Shared, t: &Tool) -> Option<std::process::Child> {
    let pidfile = t.pidfile.as_deref().map(|p| sh.path(p));
    if t.orphan {
        leave_orphan(sh, t.secs, pidfile.as_deref());
    }
    let child = Command::new("sleep")
        .arg(t.secs.to_string())
        .current_dir(&sh.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    record_pid(pidfile.as_deref(), child.id());
    Some(child)
}

/// One turn. `req` is the id of the prompt it answers; `None` for a turn drained from the queue (no response is ever sent).
fn run_turn(sh: &Arc<Shared>, s: &Arc<Session>, req: Option<Value>, text: &str) {
    let d = directives(text, sh.generation);
    let started = Instant::now();
    let drained = req.is_none();
    s.cancel.store(false, Ordering::SeqCst);
    s.deaf.store(d.ignore_cancel, Ordering::SeqCst);
    s.open.store(!drained, Ordering::SeqCst);
    if !drained {
        // a prompt that starts with nothing queued behind it is a plain prompt again: its cancel works
        s.queue_used.store(false, Ordering::SeqCst);
    }
    sh.log(&format!(
        "{} {} {}",
        if drained {
            "drained-open"
        } else {
            "prompt-open"
        },
        s.id,
        head(text)
    ));
    let finish = |stop: &str, n: u64, drained_turn: bool, req: &Option<Value>| {
        if let Some(id) = req {
            let result = if n == 0 && stop == "end_turn" {
                json!({"stopReason": stop})
            } else {
                json!({"stopReason": stop, "_meta": {"usage": usage(n)}})
            };
            sh.respond(id, result);
        }
        sh.log(&format!(
            "{} {} stop={stop}",
            if drained_turn {
                "drained-end"
            } else {
                "prompt-end"
            },
            s.id
        ));
    };

    // A provider failure comes before any update.
    if d.fail_times > 0 {
        let mut f = sh.fails.lock().unwrap_or_else(|p| p.into_inner());
        let n = f.entry(text.to_string()).or_insert(0);
        if *n < d.fail_times {
            *n += 1;
            drop(f);
            if let Some(id) = &req {
                sh.error(
                    id,
                    -32603,
                    "Internal error: Service unavailable",
                    Some(json!({"errorName": "ServiceUnavailableException", "isRetryable": true, "statusCode": 503})),
                );
            }
            sh.log(&format!("prompt-end {} error=503", s.id));
            s.open.store(false, Ordering::SeqCst);
            s.busy.store(false, Ordering::SeqCst);
            return;
        }
    }
    if let Some((path, body)) = &d.write {
        let dest = sh.path(path);
        if let Some(p) = dest.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::write(&dest, body);
        sh.log(&format!("write {path}"));
    }
    let cancelled = |sh: &Shared, tool: Option<&mut std::process::Child>, d: &Directives| {
        if let Some(t) = tool {
            if !d.tool.as_ref().is_some_and(|t| t.survive_cancel) {
                kill_pid(t.id());
                let _ = t.wait();
            }
            sh.update(
                &s.id,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed", "rawOutput": {"error": "Command was aborted"}}),
            );
        }
        std::thread::sleep(Duration::from_millis(d.cancel_reply_ms));
    };

    sh.chunk(&s.id, "working");
    let mut stop = "end_turn";
    let worked = wait_ms(sh, s, &d, started, d.work_ms);
    if let Waited::Died = worked {
        return;
    }
    if let Waited::Cancelled = worked {
        cancelled(sh, None, &d);
        stop = "cancelled";
    }
    if stop == "end_turn"
        && let Some((chunks, step_ms)) = d.stream
    {
        for i in 1..=chunks {
            match wait_ms(sh, s, &d, started, step_ms) {
                Waited::Cancelled => {
                    cancelled(sh, None, &d);
                    stop = "cancelled";
                    break;
                }
                Waited::Died => return,
                Waited::Done => sh.chunk(&s.id, &format!("stream-{i}")),
            }
        }
    }
    if stop == "end_turn"
        && let Some(e) = &d.edit
    {
        sh.update(
            &s.id,
            json!({"sessionUpdate": "tool_call", "toolCallId": "call-edit", "title": format!("Edit {}", e.path), "kind": "edit", "status": "pending"}),
        );
        sh.update(
            &s.id,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-edit", "status": "in_progress"}),
        );
        let dest = sh.path(&e.path);
        if let Some(p) = dest.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let _ = std::fs::write(&dest, "");
        for part in &e.parts {
            match wait_ms(sh, s, &d, started, e.step_ms) {
                Waited::Cancelled => {
                    sh.update(
                        &s.id,
                        json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-edit", "status": "completed", "rawOutput": {"error": "Edit was aborted"}}),
                    );
                    cancelled(sh, None, &d);
                    stop = "cancelled";
                    break;
                }
                Waited::Died => return,
                Waited::Done => {}
            }
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&dest) {
                let _ = f.write_all(part.as_bytes());
            }
            sh.log(&format!("edit-part {}", e.path));
        }
        if stop == "end_turn" {
            sh.update(
                &s.id,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-edit", "status": "completed"}),
            );
        }
    }
    if stop == "end_turn"
        && let Some(t) = &d.tool
    {
        sh.update(
            &s.id,
            json!({"sessionUpdate": "tool_call", "toolCallId": "call-1", "title": format!("Run sleep {}", t.secs), "kind": "execute", "status": "pending"}),
        );
        sh.update(
            &s.id,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "in_progress"}),
        );
        let mut child = start_tool(sh, t);
        let end = Instant::now() + Duration::from_secs(t.secs);
        let mut ended_by_cancel = false;
        while Instant::now() < end {
            match wait_ms(sh, s, &d, started, 40) {
                Waited::Cancelled => {
                    ended_by_cancel = true;
                    break;
                }
                Waited::Died => return,
                Waited::Done => {}
            }
            if let Some(c) = child.as_mut()
                && c.try_wait().ok().flatten().is_some()
            {
                break;
            }
        }
        if ended_by_cancel {
            cancelled(sh, child.as_mut(), &d);
            stop = "cancelled";
        } else {
            sh.update(
                &s.id,
                json!({"sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed"}),
            );
            if let Some(mut c) = child {
                let _ = c.wait();
            }
        }
    }
    if stop == "end_turn" {
        sh.chunk(&s.id, d.say.as_deref().unwrap_or("OK"));
    }
    let n = u64::from(sh.calls.fetch_add(1, Ordering::SeqCst));
    finish(stop, n + 1, drained, &req);
    s.open.store(false, Ordering::SeqCst);
    // what the queue holds runs next, unattached to any prompt, until it is empty
    loop {
        let next = {
            let mut q = s.queued.lock().unwrap_or_else(|p| p.into_inner());
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        };
        match next {
            Some(t) => run_turn(sh, s, None, &t),
            None => break,
        }
    }
    s.busy.store(false, Ordering::SeqCst);
}

fn generation(state: &Path) -> u32 {
    let _ = std::fs::create_dir_all(state);
    let f = state.join("generation");
    let n = std::fs::read_to_string(&f)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(0)
        + 1;
    let _ = std::fs::write(&f, n.to_string());
    n
}

/// What the fake engine needs to start: the workspace it writes into, the file it logs to and the model it reports.
#[derive(Clone, Debug)]
pub struct Config {
    pub workspace: PathBuf,
    pub log: Option<PathBuf>,
    pub model: Option<String>,
}

/// Runs the fake engine on the process's stdin and stdout until stdin closes. `die_ms` ends the process with code 3.
pub fn run(args: &[String]) -> ExitCode {
    let get = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let Some(workspace) = get("--workspace") else {
        eprintln!(
            "usage: fake-acp-engine --transcripts <dir> --workspace <dir> (FAKE_ACP_CLINE=1)"
        );
        return ExitCode::from(2);
    };
    let cfg = Config {
        workspace: PathBuf::from(workspace),
        log: std::env::var_os("FAKE_ACP_LOG").map(PathBuf::from),
        model: std::env::var("FAKE_ACP_MODEL")
            .ok()
            .filter(|m| !m.is_empty()),
    };
    serve(
        cfg,
        std::io::stdin().lock(),
        Box::new(std::io::stdout()),
        Box::new(|| std::process::exit(3)),
    )
}

/// The engine itself, on any reader and writer (a test serves it in process, so its lines are measured). `die` is what `die_ms` does.
pub fn serve(
    cfg: Config,
    input: impl BufRead,
    out: Box<dyn Write + Send>,
    die: Box<dyn Fn() + Send + Sync>,
) -> ExitCode {
    let workspace = cfg.workspace;
    let log = cfg.log.and_then(|p| {
        p.parent().map(std::fs::create_dir_all);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .ok()
    });
    let sh = Arc::new(Shared {
        out: Mutex::new(out),
        die,
        log: Mutex::new(log),
        sessions: Mutex::new(HashMap::new()),
        generation: generation(&workspace.join(".fake-acp")),
        workspace,
        model: cfg.model,
        fails: Mutex::new(HashMap::new()),
        calls: AtomicU32::new(0),
    });
    let mut sessions = 0u32;
    for line in input.lines().map_while(Result::ok) {
        if line.trim().is_empty() {
            continue;
        }
        sh.log(&format!("in {line}"));
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = req["method"].as_str().unwrap_or_default().to_string();
        let id = req.get("id").cloned();
        match (method.as_str(), id) {
            ("initialize", Some(id)) => sh.respond(
                &id,
                json!({"agentCapabilities": {"loadSession": true, "promptCapabilities": {"image": false}}, "agentInfo": {"name": "fake-cline", "version": "3.0.71"}, "authMethods": [], "protocolVersion": 1}),
            ),
            ("session/new", Some(id)) => {
                sessions += 1;
                let sid = format!("cline-{}-{sessions}", sh.generation);
                sh.sessions.lock().unwrap_or_else(|p| p.into_inner()).insert(
                    sid.clone(),
                    Arc::new(Session {
                        id: sid.clone(),
                        busy: AtomicBool::new(false),
                        open: AtomicBool::new(false),
                        deaf: AtomicBool::new(false),
                        cancel: AtomicBool::new(false),
                        queue_used: AtomicBool::new(false),
                        queued: Mutex::new(Vec::new()),
                    }),
                );
                sh.log(&format!("sessions: new-session {sid} generation={}", sh.generation));
                let mut result = json!({"sessionId": sid});
                if let Some(m) = &sh.model {
                    result["configOptions"] = json!([{"id": "model", "category": "model", "currentValue": m, "options": []}]);
                }
                sh.respond(&id, result);
            }
            ("session/set_config_option", Some(id)) => {
                let value = req
                    .pointer("/params/value")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                sh.respond(
                    &id,
                    json!({"configOptions": [{"id": "model", "category": "model", "currentValue": value, "options": []}]}),
                );
            }
            ("session/prompt", Some(id)) => {
                let sid = req
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let text = req
                    .pointer("/params/prompt/0/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let session = sh
                    .sessions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&sid)
                    .cloned();
                let Some(s) = session else {
                    sh.error(&id, -32602, &format!("unknown session {sid}"), None);
                    continue;
                };
                let queued_reply = directives(&text, sh.generation).queued_reply;
                if s.busy.load(Ordering::SeqCst) || queued_reply {
                    // The pinned engine's internal queue: answered at once, no model work, usage 0; the text runs later, unattached.
                    sh.log(&format!("overlap {sid} {}", head(&text)));
                    s.queue_used.store(true, Ordering::SeqCst);
                    let start_drain = !s.busy.load(Ordering::SeqCst);
                    s.queued
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(text);
                    std::thread::sleep(Duration::from_millis(5));
                    sh.respond(
                        &id,
                        json!({"stopReason": "end_turn", "_meta": {"usage": {"inputTokens": 0, "outputTokens": 0, "totalTokens": 0}}}),
                    );
                    if start_drain {
                        // nothing was running: the queue drains at once (the case a test makes with `queued_reply`)
                        s.busy.store(true, Ordering::SeqCst);
                        let (sh2, s2) = (sh.clone(), s.clone());
                        std::thread::spawn(move || {
                            let next = {
                                let mut q = s2.queued.lock().unwrap_or_else(|p| p.into_inner());
                                if q.is_empty() { None } else { Some(q.remove(0)) }
                            };
                            if let Some(t) = next {
                                run_turn(&sh2, &s2, None, &t);
                            } else {
                                s2.busy.store(false, Ordering::SeqCst);
                            }
                        });
                    }
                    continue;
                }
                s.busy.store(true, Ordering::SeqCst);
                let (sh2, s2) = (sh.clone(), s.clone());
                std::thread::spawn(move || run_turn(&sh2, &s2, Some(id), &text));
            }
            ("session/cancel", None) => {
                let sid = req
                    .pointer("/params/sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let session = sh
                    .sessions
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&sid)
                    .cloned();
                let acted = session.as_ref().is_some_and(|s| {
                    s.open.load(Ordering::SeqCst)
                        && !s.queue_used.load(Ordering::SeqCst)
                        && !s.deaf.load(Ordering::SeqCst)
                });
                sh.log(&format!("cancel {sid} acted={acted}"));
                if acted && let Some(s) = session {
                    s.cancel.store(true, Ordering::SeqCst);
                }
            }
            (_, Some(id)) => sh.error(&id, -32601, &format!("unknown method {method}"), None),
            _ => {}
        }
    }
    ExitCode::SUCCESS
}
