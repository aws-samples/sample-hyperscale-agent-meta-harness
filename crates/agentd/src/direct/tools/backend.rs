//! The seam between the tool server and what a tool does. The server speaks the protocol, validates the input against the table
//! of `harness_protocol::tools`, limits the calls, cuts the answer to the cap and counts; a backend does the work and answers with a
//! [`ToolOutput`]. One backend per tool of the table; a later package adds a tool by writing a backend and one line in `backends.rs`.
//!
//! What a backend promises:
//! - It stops when its [`CancelToken`] fires: it ends its child processes (the whole group, see [`kill_process_group`]) and returns within the
//!   server's grace (500 ms). A backend that does not is aborted after the grace, so every process it starts must also die when its future is
//!   dropped (`kill_on_drop`, or a [`GroupGuard`] that is disarmed once the child has been waited for).
//! - Its failure text is a template: a fixed phrase or a name taken from the tool's own schema, never a value the caller sent, a path, a parser
//!   message, a panic payload or the text of another process. The server puts the failure word in front, and the text reaches the model.
//! - It does not log the input or the output. The server logs the tool, the result word, the time and the size.

use crate::BoxFuture;
use crate::direct::tools::stats::Counters;
use crate::engine::LaunchSpec;
use harness_protocol::tools::{CodeSearchInput, ToolInputError, ToolName};
use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::sync::watch;

// ---- cancel --------------------------------------------------------------------------------------------------------------------

/// A signal that a call is no longer wanted: the client sent `notifications/cancelled`, its connection closed, the server ended, or the call ran
/// past the server's backstop. Cloning gives another handle on the same signal.
#[derive(Clone, Debug)]
pub struct CancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub fn new() -> Self {
        let (tx, _) = watch::channel(false);
        Self { tx: Arc::new(tx) }
    }

    /// Fires the signal. Calling it again changes nothing.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Completes when the signal has fired (at once when it already has).
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        let _ = rx.wait_for(|fired| *fired).await;
    }
}

// ---- what a call answers --------------------------------------------------------------------------------------------------------

/// The words an answer to a failed call starts with after `error: `. The same six as `harness_protocol::tools::RESULT_FAILURE_WORDS`
/// (a test compares them), so that the table of the protocol and the code cannot drift apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureWord {
    Busy,
    Disabled,
    Refused,
    Timeout,
    Unavailable,
    Usage,
}

impl FailureWord {
    pub const ALL: [FailureWord; 6] = [
        Self::Busy,
        Self::Disabled,
        Self::Refused,
        Self::Timeout,
        Self::Unavailable,
        Self::Usage,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Disabled => "disabled",
            Self::Refused => "refused",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Usage => "usage",
        }
    }

    /// The call did not run (the summary counts it as `refused`); the other words mean it ran and failed (`failed`). `unavailable` after a
    /// fault of the backend may follow effects that already happened, so it never promises that a retry is safe.
    pub const fn is_refusal(self) -> bool {
        matches!(
            self,
            Self::Busy | Self::Disabled | Self::Refused | Self::Usage
        )
    }
}

/// What a backend answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolOutput {
    /// The answer. `truncated`: the backend itself limited it (match limits); the server's own cut is added to that.
    Text { text: String, truncated: bool },
    /// A failure. `detail` is a template (see the module text); the server cleans it to one short line.
    Failed {
        word: FailureWord,
        detail: Option<String>,
    },
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            truncated: false,
        }
    }

    /// An answer the backend has limited itself.
    pub fn shortened(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            truncated: true,
        }
    }

    pub fn failed(word: FailureWord, detail: impl Into<String>) -> Self {
        Self::Failed {
            word,
            detail: Some(detail.into()),
        }
    }

    pub fn failed_bare(word: FailureWord) -> Self {
        Self::Failed { word, detail: None }
    }
}

// ---- the context -----------------------------------------------------------------------------------------------------------------

/// Builds the launch spec of a command the backend runs as the engine's user: `(argv, stderr file name) -> LaunchSpec`. The hook of a run gives
/// one that carries the scrubbed environment (no `AWS_*`), the engine's search path, the work tree as the working directory and the engine's
/// uid; a test gives its own.
pub type SpecFn = Arc<dyn Fn(Vec<String>, &str) -> LaunchSpec + Send + Sync>;

/// What a backend may use besides its input.
pub struct ToolContext {
    /// The repository of the run.
    pub work_tree: PathBuf,
    /// The context directory outside the work tree (`HAMH_CONTEXT_DIR`), when the run has one.
    pub context_dir: Option<PathBuf>,
    /// The counters of the server (a backend that counts something of its own reads them; it never writes the server's).
    pub counters: Arc<Counters>,
    spec: SpecFn,
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("work_tree", &self.work_tree)
            .field("context_dir", &self.context_dir)
            .finish_non_exhaustive()
    }
}

impl ToolContext {
    pub fn new(
        work_tree: PathBuf,
        context_dir: Option<PathBuf>,
        spec: SpecFn,
        counters: Arc<Counters>,
    ) -> Self {
        Self {
            work_tree,
            context_dir,
            counters,
            spec,
        }
    }

    /// A context for a directory, with no change of user and the process's own search path: for tests and for the conformance example.
    pub fn for_directory(work_tree: PathBuf, counters: Arc<Counters>) -> Self {
        let cwd = work_tree.clone();
        let path: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_else(|| vec!["/usr/local/bin".into(), "/usr/bin".into(), "/bin".into()]);
        let spec: SpecFn = Arc::new(move |argv, stderr| LaunchSpec {
            argv,
            env: Default::default(),
            cwd: cwd.clone(),
            path: path.clone(),
            // nosemgrep: temp-dir -- for_directory builds the context of tests and of the conformance example only, the production context comes from ToolContext::new with the run's own directories
            stderr_log: std::env::temp_dir().join(stderr),
            run_as: None,
            sandbox: None,
        });
        Self::new(work_tree, None, spec, counters)
    }

    /// The launch spec of a command run as the engine's user; `stderr_name` is a file name, not a path.
    pub fn launch_spec(&self, argv: Vec<String>, stderr_name: &str) -> LaunchSpec {
        (self.spec)(argv, stderr_name)
    }
}

/// One tool.
pub trait ToolBackend: Send + Sync {
    /// The tool of the table this backend serves.
    fn name(&self) -> ToolName;

    /// Runs one call. `input` has passed [`validate_input`]. The future is dropped when the call is aborted.
    fn call<'a>(
        &'a self,
        ctx: &'a ToolContext,
        input: Value,
        cancel: CancelToken,
    ) -> BoxFuture<'a, ToolOutput>;
}

/// Checks the arguments of a call against the table. Every tool of the table has its parser here, and no wildcard, so a tool that joins the
/// table is a compile error until it has one.
pub fn validate_input(tool: ToolName, arguments: &Value) -> Result<(), ToolInputError> {
    match tool {
        ToolName::CodeSearch => CodeSearchInput::from_arguments(arguments).map(|_| ()),
    }
}

// ---- processes -------------------------------------------------------------------------------------------------------------------

/// A process group leader's pid is a real one: the numbers 0 and 1 would name this process's own group and init.
fn group_leader(pid: u32) -> bool {
    pid > 1 && i32::try_from(pid).is_ok()
}

fn kill_group_args(pid: u32) -> [String; 3] {
    ["-KILL".to_string(), "--".to_string(), format!("-{pid}")]
}

/// Ends every process of the group `pid` leads (`kill -KILL -- -pid`; the child was started with `process_group(0)`, so its pid is its group's).
/// Call it while the child has not yet been waited for: a reaped pid may name another process.
pub async fn kill_process_group(pid: u32) {
    if !group_leader(pid) {
        return;
    }
    let _ = tokio::process::Command::new("kill")
        .args(kill_group_args(pid))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Ends the group of `pid` when it is dropped, unless it was disarmed. A backend holds one for a child it started in its own group, so that an
/// aborted future (a cancel the backend did not heed, a client that hung up) does not leave the child running; it disarms the guard once the
/// child has been waited for, because after that the number may belong to another process.
#[derive(Debug)]
pub struct GroupGuard {
    pid: Option<u32>,
}

impl GroupGuard {
    pub fn new(pid: u32) -> Self {
        Self { pid: Some(pid) }
    }

    pub fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take().filter(|p| group_leader(*p)) {
            // A blocking call of a few milliseconds: a drop cannot await.
            let _ = std::process::Command::new("kill")
                .args(kill_group_args(pid))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
