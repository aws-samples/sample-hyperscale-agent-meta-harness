//! The services a repository declares and the local servers the report starts in their place.
//!
//! The report runs the repository's own test commands in a clean checkout. Many repositories keep a Compose file at the root that declares the database
//! or the broker their integration and end-to-end tests talk to; the worker has no Docker daemon, so the Compose container cannot run. The image
//! ships `ham-services`, which starts a native PostgreSQL or RabbitMQ on 127.0.0.1 with the settings the sample prompts give their Compose services.
//! The report starts that server for each declared service it knows (`postgres` and `rabbitmq` images, by name; any other image has no stand-in and is
//! left alone), as a setup step whose output says which Compose service the local server stands in for and that the Compose container did not run.
//! A server that cannot be started is a step marked not run with the reason. The steps get `DATABASE_URL`, `BROKER_URL` and `BROKER_PASSWORD` from
//! `ham-services env`, and only the ones the repository's own files read by name. After the last step the servers are stopped, whatever the steps did.
//!
//! The report never says a step passed or failed because of a server: a step keeps its exit code, its counts and its output. When a step exits
//! with a code other than 0, its output says it could not connect to a declared service's port on this machine, and the server was not reachable
//! (it did not start, or it no longer answered after the step), the step gets a `NEEDS:` head line ([`harness_protocol::direct::NEEDS_PREFIX`]).

use crate::direct::report::plan::PlannedStep;
use crate::direct::report::run::{
    RunEnv, block_that_fits, capture, mark_not_run, run_step_scanned, runtime_block_span,
};
use harness_core::refusal::redact;
use harness_protocol::direct::{
    NOT_RUN_PREFIX, PLAN_NOTE_PREFIX, SERVICE_COMMAND_PREFIX, STAND_IN_PREFIX, TestCommandSource,
    TestStep, TestStepRole,
};
use std::collections::BTreeMap;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// The Compose files read, in this order; the first that exists is the one.
pub const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yml",
    "docker-compose.yaml",
];

/// The longest Compose file read.
const MAX_COMPOSE_BYTES: u64 = 256 * 1024;
/// The most lines of a Compose file read.
const MAX_COMPOSE_LINES: usize = 4_000;
/// The time `ham-services <name>` may take: RabbitMQ waits up to 120 seconds for its port.
const START_LIMIT: Duration = Duration::from_secs(150);
/// The time `ham-services env` and `ham-services stop` may take.
const HELPER_LIMIT: Duration = Duration::from_secs(20);
/// The longest file the check for a variable name reads, the most files, and the most bytes in all.
const READ_FILE_MAX: u64 = 256 * 1024;
const READ_FILES_MAX: usize = 3_000;
const READ_BYTES_MAX: u64 = 24 * 1024 * 1024;

/// A service the report knows a local server for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Postgres,
    Rabbitmq,
}

impl Kind {
    /// The word `ham-services` takes and the report uses for the service.
    pub fn name(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Rabbitmq => "rabbitmq",
        }
    }

    /// The port of the local server.
    pub fn port(self) -> u16 {
        match self {
            Self::Postgres => 5432,
            Self::Rabbitmq => 5672,
        }
    }

    fn product(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Rabbitmq => "RabbitMQ",
        }
    }

    fn settings(self) -> &'static str {
        match self {
            Self::Postgres => "user app, database app, trust authentication",
            Self::Rabbitmq => "user app, a password the report made for this run",
        }
    }

    /// The variables the repository may read for this service.
    fn variables(self) -> &'static [&'static str] {
        match self {
            Self::Postgres => &["DATABASE_URL"],
            Self::Rabbitmq => &["BROKER_URL", "BROKER_PASSWORD"],
        }
    }
}

/// The variables the report may hand to the steps, and no others.
pub const VARIABLES: [&str; 3] = ["DATABASE_URL", "BROKER_URL", "BROKER_PASSWORD"];

/// A service of the repository's Compose file that has a local server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declared {
    /// The key of the service in the Compose file (`db`).
    pub service: String,
    /// The image it names (`postgres:16`).
    pub image: String,
    /// The Compose file it comes from.
    pub file: String,
    pub kind: Kind,
}

/// The kind of server an image is: its name without registry, tag and digest must be `postgres` or `rabbitmq`.
pub fn kind_of_image(image: &str) -> Option<Kind> {
    let base = image.rsplit('/').next()?;
    match base.split(['@', ':']).next()? {
        "postgres" => Some(Kind::Postgres),
        "rabbitmq" => Some(Kind::Rabbitmq),
        _ => None,
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// The text of a YAML scalar after its key: quotes and a trailing comment removed.
fn scalar(value: &str) -> String {
    let v = value.trim();
    if let Some(rest) = v.strip_prefix('"') {
        return rest.split('"').next().unwrap_or("").to_string();
    }
    if let Some(rest) = v.strip_prefix('\'') {
        return rest.split('\'').next().unwrap_or("").to_string();
    }
    v.split(" #").next().unwrap_or("").trim().to_string()
}

/// The `(service, image)` pairs of the `services:` block of a Compose file, in order. A reading of the shape every Compose file has (a `services:` key
/// at the left edge, one key for each service one level in, an `image:` line inside it); anything else (flow style, an anchor, an include, a tab)
/// gives no pair, so the report never makes up a service.
pub fn parse_compose(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut in_services = false;
    let mut service_indent: Option<usize> = None;
    let mut current: Option<String> = None;
    for raw in text.lines().take(MAX_COMPOSE_LINES) {
        let line = raw.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with('\t') {
            return Vec::new();
        }
        let ind = indent(line);
        if ind == 0 {
            in_services = trimmed == "services:";
            service_indent = None;
            current = None;
            continue;
        }
        if !in_services {
            continue;
        }
        let level = *service_indent.get_or_insert(ind);
        if ind == level {
            current = trimmed
                .strip_suffix(':')
                .map(|k| k.trim_matches(['"', '\'']).to_string())
                .filter(|k| !k.is_empty() && !k.contains(' '));
        } else if ind > level
            && let (Some(service), Some(value)) = (&current, trimmed.strip_prefix("image:"))
        {
            let image = scalar(value);
            if !image.is_empty() && !image.contains(char::is_whitespace) {
                out.push((service.clone(), image));
                current = None;
            }
        }
    }
    out
}

/// The services the repository's Compose file declares that have a local server, the first of each kind.
pub fn declared(root: &Path) -> Vec<Declared> {
    let Ok(root) = root.canonicalize() else {
        return Vec::new();
    };
    for file in COMPOSE_FILES {
        let Ok(real) = root.join(file).canonicalize() else {
            continue;
        };
        if !real.starts_with(&root) || !real.is_file() {
            continue;
        }
        let mut buf = Vec::new();
        if std::fs::File::open(&real)
            .and_then(|f| f.take(MAX_COMPOSE_BYTES).read_to_end(&mut buf))
            .is_err()
        {
            return Vec::new();
        }
        let mut out: Vec<Declared> = Vec::new();
        for (service, image) in parse_compose(&String::from_utf8_lossy(&buf)) {
            if let Some(kind) = kind_of_image(&image)
                && !out.iter().any(|d| d.kind == kind)
            {
                out.push(Declared {
                    service,
                    image,
                    file: file.to_string(),
                    kind,
                });
            }
        }
        return out;
    }
    Vec::new()
}

/// Whether a text file of the tree holds `name` as a word: the repository's own code reads that variable. Bounded in files and bytes; a link that
/// leaves the tree is not followed, and `.git`, `.ham` (the harness records, which quote the commands of the session), `node_modules` and the like are skipped.
pub fn repo_reads(root: &Path, name: &str) -> bool {
    let mut stack = vec![root.to_path_buf()];
    let (mut files, mut bytes) = (0usize, 0u64);
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.filter_map(Result::ok) {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let file_name = entry.file_name();
            let n = file_name.to_string_lossy();
            if kind.is_dir() {
                if !matches!(
                    n.as_ref(),
                    ".git"
                        | ".ham"
                        | "node_modules"
                        | "target"
                        | ".venv"
                        | "venv"
                        | "__pycache__"
                        | "vendor"
                ) {
                    stack.push(entry.path());
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            files += 1;
            let Ok(meta) = entry.metadata() else { continue };
            if files > READ_FILES_MAX || bytes > READ_BYTES_MAX {
                return false;
            }
            if meta.len() > READ_FILE_MAX {
                continue;
            }
            bytes += meta.len();
            let mut buf = Vec::new();
            if std::fs::File::open(entry.path())
                .and_then(|f| f.take(READ_FILE_MAX).read_to_end(&mut buf))
                .is_ok()
                && String::from_utf8_lossy(&buf).contains(name)
            {
                return true;
            }
        }
    }
    false
}

/// The `export NAME='value'` lines of `ham-services env`, read as text and never run: only the names of [`VARIABLES`], values with no control
/// character, and `${BROKER_PASSWORD}` in a value replaced by the password the same output gave. A value that still holds a `$` or a backtick is dropped.
pub fn parse_exports(text: &str) -> BTreeMap<String, String> {
    let mut raw: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("export ") else {
            continue;
        };
        let Some((name, value)) = rest.split_once('=') else {
            continue;
        };
        if !VARIABLES.contains(&name) {
            continue;
        }
        let value = value.trim();
        let inner = value
            .strip_prefix('\'')
            .and_then(|v| v.strip_suffix('\''))
            .or_else(|| value.strip_prefix('"').and_then(|v| v.strip_suffix('"')));
        if let Some(inner) = inner
            && !inner.chars().any(char::is_control)
        {
            raw.insert(name.to_string(), inner.to_string());
        }
    }
    let password = raw.get("BROKER_PASSWORD").cloned();
    let mut out = BTreeMap::new();
    for (name, mut value) in raw {
        if let Some(pw) = &password {
            value = value.replace("${BROKER_PASSWORD}", pw);
        }
        if !value.is_empty() && !value.contains(['$', '`']) {
            out.insert(name, value);
        }
    }
    out
}

/// Whether something accepts a connection on 127.0.0.1 at `port`.
pub fn reachable(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_secs(1),
    )
    .is_ok()
}

/// A server the report tried to start.
#[derive(Clone, Debug)]
pub struct Live {
    pub kind: Kind,
    /// Whether `ham-services` ended with 0.
    pub started: bool,
    /// Why it did not start.
    pub reason: Option<String>,
}

impl Live {
    /// Why a step could not use this server, or `None` when it answers: `up` says whether something accepts a connection on a port of this machine
    /// ([`reachable`] in the report; a test names its own).
    pub fn unavailable_with(&self, up: &dyn Fn(u16) -> bool) -> Option<String> {
        if !self.started {
            return Some(format!(
                "it could not be started ({})",
                self.reason
                    .as_deref()
                    .unwrap_or("ham-services did not start it")
            ));
        }
        (!up(self.kind.port())).then(|| "it stopped answering during the step".to_string())
    }
}

/// What starting the declared services gave.
#[derive(Debug, Default)]
pub struct Started {
    /// One setup step for each declared service, in order.
    pub steps: Vec<TestStep>,
    pub live: Vec<Live>,
    /// The variables the steps get.
    pub vars: BTreeMap<String, String>,
}

/// The sentence at the head of the output of a service step: which Compose service the local server stands in for, and what the report did.
pub fn stand_in_sentence(d: &Declared, started: bool, why: Option<&str>) -> String {
    let k = d.kind;
    let who = format!("compose service {} ({} in {})", d.service, d.image, d.file);
    if started {
        format!(
            "{who} has a local {} server in its place, on 127.0.0.1:{} with {}; the Compose container did not run",
            k.product(),
            k.port(),
            k.settings()
        )
    } else {
        format!(
            "{who} has no server in this report: the local {} server that would stand in for it could not be started ({}); the Compose container does not run",
            k.product(),
            why.unwrap_or("ham-services did not start it")
        )
    }
}

/// Puts `line` at the head of the output of `step`, after the `NOT RUN:` line and the plan note when it has them. The runtime block of a step not
/// run for want of a runtime stays whole and goes behind the head lines, because the readers of the head lines look right after the
/// reason. A line that starts with `prefix` and sits there already (a command printed it) is removed first, so only agentd's line is ever read.
pub fn put_head_line(step: &mut TestStep, prefix: &str, line: &str, cap: usize) {
    let (skip, block_lines) = runtime_block_span(step);
    let tail = std::mem::take(&mut step.output_tail);
    let mut lines: Vec<&str> = tail.lines().collect();
    let mut head: Vec<String> = Vec::new();
    if step.is_not_run() && lines.first().is_some_and(|l| l.starts_with(NOT_RUN_PREFIX)) {
        head.push(lines.remove(0).to_string());
    }
    // The block leaves the lines first, so that the loops below see the lines that follow the head.
    let block_at = skip.min(lines.len());
    let block: String = lines
        .drain(block_at..(block_at + block_lines).min(lines.len()))
        .map(|l| format!("{l}\n"))
        .collect();
    while lines
        .first()
        .is_some_and(|l| l.starts_with(PLAN_NOTE_PREFIX))
    {
        head.push(lines.remove(0).to_string());
    }
    while lines.first().is_some_and(|l| l.starts_with(prefix)) {
        lines.remove(0);
    }
    head.push(line.to_string());
    let rest = lines.join("\n");
    // Every head line counts with the newline behind it, the last one with the newline that separates it from what follows.
    let other: usize = head.iter().map(|h| h.len() + 1).sum();
    let shown = block_that_fits(&block, other, rest.len(), cap);
    let head = head
        .iter()
        .map(String::as_str)
        .chain(shown.lines())
        .collect::<Vec<_>>()
        .join("\n");
    let room = cap.saturating_sub(head.len() + 1);
    let (kept, cut) = crate::direct::report::parse::last_bytes(&rest, room);
    step.output_tail = if kept.is_empty() {
        head
    } else {
        format!("{head}\n{kept}")
    };
    step.output_truncated |= cut || shown.len() < block.len();
}

/// Starts the local server for each declared service as a setup step, within `budget` counted from `started`, and reads the variables the repository
/// reads. A step that does not end with 0 is marked not run with the reason; the report goes on with the steps that follow.
pub async fn start(
    env: &RunEnv,
    cwd: &Path,
    declared: &[Declared],
    started: Instant,
    budget: Duration,
    tail: usize,
) -> Started {
    let mut out = Started::default();
    for d in declared {
        let left = budget.saturating_sub(started.elapsed());
        let command = format!("{SERVICE_COMMAND_PREFIX}{}", d.kind.name());
        let planned = PlannedStep {
            role: TestStepRole::Setup,
            source: TestCommandSource::Detected,
            command: command.clone(),
            logged: None,
        };
        let mut step = if left < Duration::from_secs(10) {
            TestStep::not_run(
                planned.role,
                planned.source,
                &command,
                "the report time limit left no time to start it",
                None,
            )
        } else {
            let (mut step, _) =
                run_step_scanned(env, &planned, cwd, START_LIMIT.min(left), tail).await;
            if !step.is_not_run() && (step.timed_out || step.exit_code != Some(0)) {
                let why = match (step.timed_out, step.exit_code) {
                    (true, _) => "it did not start within its time".to_string(),
                    (false, code) => {
                        let first = step
                            .output_tail
                            .lines()
                            .map(str::trim)
                            .rfind(|l| !l.is_empty())
                            .unwrap_or("");
                        let first: String = redact(first).chars().take(100).collect();
                        match code {
                            Some(c) if first.is_empty() => format!("it exited {c}"),
                            Some(c) => format!("it exited {c}: {first}"),
                            None => "it did not end".to_string(),
                        }
                    }
                };
                step.timed_out = false;
                mark_not_run(&mut step, &why, tail);
            }
            step
        };
        let ok = !step.is_not_run() && step.exit_code == Some(0);
        let why = step.not_run_reason().map(str::to_string);
        let sentence = stand_in_sentence(d, ok, why.as_deref());
        put_head_line(
            &mut step,
            STAND_IN_PREFIX,
            &format!("{STAND_IN_PREFIX}{sentence}"),
            tail,
        );
        out.live.push(Live {
            kind: d.kind,
            started: ok,
            reason: why,
        });
        out.steps.push(step);
    }
    if out.live.iter().any(|l| l.started)
        && let Some(text) = capture(
            env,
            &format!("{SERVICE_COMMAND_PREFIX}env"),
            cwd,
            HELPER_LIMIT,
        )
        .await
    {
        let exports = parse_exports(&text);
        for (name, value) in exports {
            let wanted = out
                .live
                .iter()
                .filter(|l| l.started)
                .any(|l| l.kind.variables().contains(&name.as_str()));
            // BROKER_URL is only useful with its password, and the repository may read the URL alone.
            if wanted && repo_reads(cwd, &name) {
                out.vars.insert(name, value);
            }
        }
    }
    out
}

/// Stops the servers `start` started. Best effort: the answer is not looked at.
pub async fn stop(env: &RunEnv, cwd: &Path) {
    let _ = capture(
        env,
        &format!("{SERVICE_COMMAND_PREFIX}stop"),
        cwd,
        HELPER_LIMIT,
    )
    .await;
}

/// The password a step's output must not show: every value of a secret variable the report handed over is replaced in the command and the output of
/// the steps.
pub fn secrets(vars: &BTreeMap<String, String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(pw) = vars.get("BROKER_PASSWORD").filter(|p| p.len() >= 8) {
        out.push(pw.clone());
    }
    out
}

/// Replaces each of `secrets` in the command and the output of `step`.
pub fn scrub(step: &mut TestStep, secrets: &[String]) {
    for s in secrets {
        step.command = step.command.replace(s.as_str(), "***");
        step.output_tail = step.output_tail.replace(s.as_str(), "***");
    }
}
