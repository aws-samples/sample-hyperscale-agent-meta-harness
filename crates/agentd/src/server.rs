//! The AgentCore Runtime contract on port 8080: `GET /ping`, `POST /invocations`.
//!
//! `/invocations` takes a `kind: "direct"` body, validates it, claims the session's one turn slot (409 while a turn runs),
//! sets `/ping` to `HealthyBusy`, answers `InvocationAccepted` at once and runs the direct driver in the background:
//! AgentCore cuts synchronous requests at 15 minutes.

use crate::config::Config;
use crate::creds::{CredStore, HttpRefresher};
use crate::health::Health;
use crate::logging::LogSink;
use crate::time::SystemClock;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use harness_protocol::runtime::{InvocationAccepted, turn_result_path};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// The end of the session as agentd learns of it: a termination signal to `agentd serve` ([`end_on_signals`]). The copy task of a
/// running turn watches it and saves a copy of the work at once; who asked waits until that copy is saved, within a limit.
#[derive(Debug)]
pub struct Ending {
    /// The number of the last request to end; 0 before the first.
    asked: watch::Sender<u64>,
    /// The number of the last request whose copy is saved (or could not be).
    saved: watch::Sender<u64>,
    /// How many copy tasks watch.
    watching: AtomicUsize,
}

impl Default for Ending {
    fn default() -> Self {
        Self {
            asked: watch::Sender::new(0),
            saved: watch::Sender::new(0),
            watching: AtomicUsize::new(0),
        }
    }
}

impl Ending {
    /// agentd is asked to end: every copy task that watches saves a copy now. Returns the number of this request.
    pub fn ask(&self) -> u64 {
        let mut n = 0;
        self.asked.send_modify(|asked| {
            *asked += 1;
            n = *asked;
        });
        n
    }

    /// Whether a copy task watches: a turn is in its prompt phase and saves copies.
    pub fn watched(&self) -> bool {
        self.watching.load(Ordering::SeqCst) > 0
    }

    /// The watch of one copy task.
    pub fn watch(self: &Arc<Self>) -> EndingWatch {
        self.watching.fetch_add(1, Ordering::SeqCst);
        EndingWatch {
            asked: self.asked.subscribe(),
            ending: self.clone(),
        }
    }

    /// Waits until the copy of request `n` is saved or no copy task watches any more, at most `limit`. `false` when the limit came first.
    pub async fn saved(&self, n: u64, limit: Duration) -> bool {
        let mut saved = self.saved.subscribe();
        tokio::time::timeout(limit, async {
            loop {
                if *saved.borrow_and_update() >= n || !self.watched() {
                    return;
                }
                if saved.changed().await.is_err() {
                    return;
                }
            }
        })
        .await
        .is_ok()
    }
}

/// What a copy task holds of [`Ending`]; dropping it (the prompt phase is over) wakes who waits for a copy.
#[derive(Debug)]
pub struct EndingWatch {
    asked: watch::Receiver<u64>,
    ending: Arc<Ending>,
}

impl EndingWatch {
    /// Waits for the next request to end and returns its number.
    pub async fn asked(&mut self) -> u64 {
        if self.asked.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        *self.asked.borrow_and_update()
    }

    /// The copy of request `n` is saved, or could not be.
    pub fn saved(&self, n: u64) {
        self.ending
            .saved
            .send_modify(|saved| *saved = (*saved).max(n));
    }
}

impl Drop for EndingWatch {
    fn drop(&mut self) {
        self.ending.watching.fetch_sub(1, Ordering::SeqCst);
        // Who waits looks again and finds no watcher.
        self.ending.saved.send_modify(|_| {});
    }
}

pub struct App {
    pub cfg: Config,
    pub health: Arc<Health>,
    pub log: Arc<dyn LogSink>,
    pub creds: Arc<CredStore>,
    /// The end of the session, as a termination signal announces it.
    pub ending: Arc<Ending>,
    /// One turn at a time: an AgentCore session belongs to one agent.
    pub(crate) turn_lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("cfg", &self.cfg)
            .field("health", &self.health)
            .finish_non_exhaustive()
    }
}

impl App {
    pub fn new(cfg: Config, log: Arc<dyn LogSink>) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            health: Health::new(),
            log,
            creds: CredStore::new(Arc::new(HttpRefresher::new()), Arc::new(SystemClock)),
            ending: Arc::new(Ending::default()),
            turn_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }
}

async fn ping(State(app): State<Arc<App>>) -> Response {
    axum::Json(app.health.ping()).into_response()
}

fn bad_request(message: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({ "error": message })),
    )
        .into_response()
}

/// `POST /invocations` of `kind: "direct"`: parse, validate, take the turn slot, answer at once
/// and run the direct driver in the background. 400 for a body that does not parse or validate (a message with a rule or a
/// position, never the input), 409 while a turn runs.
async fn invocations(State(app): State<Arc<App>>, body: Bytes) -> Response {
    use harness_protocol::direct::{
        DIRECT_AGENT_ID, DirectInvocationRequest, MAX_DIRECT_BODY_BYTES, validate_direct_invocation,
    };
    if body.len() > MAX_DIRECT_BODY_BYTES {
        return bad_request(format!("body exceeds {MAX_DIRECT_BODY_BYTES} bytes"));
    }
    let req: DirectInvocationRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return bad_request(format!(
                "body is not a DirectInvocationRequest (line {}, column {})",
                e.line(),
                e.column()
            ));
        }
    };
    if let Err(e) = validate_direct_invocation(&req) {
        app.log.line(&format!("direct invocation refused: {e}"));
        return bad_request(e.to_string());
    }
    let Ok(slot) = app.turn_lock.clone().try_lock_owned() else {
        app.log
            .line("direct turn refused: a turn is already running");
        return (
            StatusCode::CONFLICT,
            axum::Json(json!({ "error": "a turn is already running in this session" })),
        )
            .into_response();
    };
    let busy = Health::busy(&app.health);
    // A rerun must never be answered by the earlier result.
    let stale = turn_result_path(&app.cfg.data_root, &req.run_id, DIRECT_AGENT_ID, req.turn);
    let _ = std::fs::remove_file(stale);
    let ack = InvocationAccepted {
        run_id: req.run_id.clone(),
        agent_id: DIRECT_AGENT_ID.to_string(),
        turn: req.turn,
        accepted: true,
    };
    let task = app.clone();
    tokio::spawn(async move {
        crate::direct::run(&task, req).await;
        // The result file is on disk before /ping reads Healthy and the slot frees.
        drop(busy);
        drop(slot);
    });
    axum::Json(ack).into_response()
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/ping", get(ping))
        .route("/invocations", post(invocations))
        .with_state(app)
}

pub async fn serve(app: Arc<App>, listener: tokio::net::TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(app)).await
}

/// The exit code of a process that a signal ended, as a shell reports it: 128 and the signal's number.
const EXIT_SIGTERM: i32 = 143;
const EXIT_SIGINT: i32 = 130;

/// Whether the agentd with process id `pid` ends once the copy that a termination signal asked for is saved: every process but
/// the first of its machine. Process 1 never got such a signal while agentd had no handler, so it runs on as before.
pub fn ends_after_the_copy(pid: u32) -> bool {
    pid != 1
}

/// Answers the signals that ask `agentd serve` to end, SIGTERM and SIGINT: the copy task of a running turn saves a copy of the
/// work at once, before anything else happens on this path, and this waits for it for the time the copy has
/// (`snapshot::STOP_COPY_BUDGET`) and two seconds.
///
/// What follows keeps what each signal did before agentd had a handler. As the first process of its machine (process 1, as in a session's
/// microVM) agentd ran on, because the kernel gives such a process no signal it has no handler for: it still runs on, the copy task keeps
/// its timer and cuts none of its looks short, and the platform ends the machine. Anywhere else the signal ended agentd: it still ends,
/// with the signal's exit code, once the copy is saved and its event had two event periods to reach the service.
///
/// A turn that is not in its prompt phase has no copy task (it is starting, or in its end sequence, which saves the copy of the end
/// itself): the signal then asks for no copy and is logged as that. The first process runs on through it; any other process ends at once,
/// in the middle of that turn, as it did before agentd had a handler.
pub async fn end_on_signals(app: Arc<App>) {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        app.log
            .line("termination signals cannot be watched: a signal ends agentd as before");
        return;
    };
    let first_process = !ends_after_the_copy(std::process::id());
    loop {
        let (name, code) = tokio::select! {
            _ = term.recv() => ("SIGTERM", EXIT_SIGTERM),
            _ = int.recv() => ("SIGINT", EXIT_SIGINT),
        };
        let working = app.ending.watched();
        app.log.line(&format!(
            "termination signal {name}: {}",
            if working {
                "a copy of the work is saved first"
            } else if app.health.is_busy() {
                "a turn runs outside its prompt phase, where no copy task works; the end sequence of a turn saves the copy of the end itself"
            } else {
                "no turn is working, so there is no work to copy"
            }
        ));
        let n = app.ending.ask();
        let budget = app
            .cfg
            .direct
            .stop_copy_budget
            .unwrap_or(crate::direct::snapshot::STOP_COPY_BUDGET);
        let saved = app.ending.saved(n, budget + Duration::from_secs(2)).await;
        if working && !saved {
            app.log.line(&format!(
                "termination signal {name}: the copy was not saved within {} s",
                budget.as_secs_f32()
            ));
        }
        if first_process {
            app.log.line(&format!(
                "termination signal {name}: agentd is the first process of its machine and runs on until the machine ends"
            ));
            continue;
        }
        if working {
            tokio::time::sleep(app.cfg.direct.events_every * 2).await;
        }
        app.log
            .line(&format!("termination signal {name}: agentd ends"));
        std::process::exit(code);
    }
}
