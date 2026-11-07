//! Module `goal`: the engine's own goal loop runs by default, and agentd
//! supervises it with brakes that are stalls, never a cap on rounds or tokens.
//!
//! How it works. The engine ships a goal plugin: it claims a goal for the session at the start of its first run, and when a run ends it
//! queues a follow-up prompt ("Are you sure you've completed the goal ...") until the model calls the tool that completes the goal.
//! Those follow-up runs have no `session/prompt` request, so the driver sees one answer and then silence. This module therefore:
//!
//! - places the plugin and the observer under the engine's read-only configuration directory (`on_startup`, after checking every file
//!   against the image's lock), fixes the plugin mode through the engine's environment, writes the pending goal to the state file and
//!   takes a fingerprint of the tree;
//! - listens to the wire (`on_wire`): the model's call of the completion tool is a done claim, the only claim the module believes;
//! - at the first turn end checks that the engine claimed the goal (an engine that did not runs on as a plain run), then votes
//!   [`TurnVote::Wait`] while the loop runs, which makes the driver hold the run open and read the pipe (`driver::hold`);
//! - reads the observer's rounds log and the state file at every vote and feeds the pure rules of [`machine`]: a done claim counts when
//!   the round that made it ended with a message and a quiet moment passed; `BLOCKED:` twice, a text-only stall and rounds that change
//!   no file end the run; a loop that stopped gets one "continue" ([`TurnVote::Prompt`], sent through the driver's one sender, so it
//!   never overlaps a user's message) and the run ends when continues change nothing;
//! - ends a run by clearing the state file, letting the round in flight finish for a bounded time and voting [`TurnVote::Stop`].
//!   `session/cancel` is not used for this: the plugin's follow-up rounds ignore it.
//!
//! - watches the engine's tool calls and the tree for the brake inside a round ([`watch`]), also when it is told that a command of the
//!   engine's user is running ([`command`]: the polls of a long test run are waiting, not polling, up to a grace), and, when that brake
//!   fires, gives the engine one last turn to write its final message before the run ends.
//!
//! It never says that work is good: the harness records the engine's own stop, and a harness brake says so in words that begin "The harness
//! ended the run:" (root invariant 1). Settings are `AGENTD_GOAL_*` ([`settings`]); a start request's `goal` member replaces some of them
//! for one run.

pub mod busy;
pub mod command;
pub mod files;
pub mod machine;
pub mod rounds;
pub mod settings;
pub mod state;
pub mod tree;
pub mod watch;

use crate::BoxFuture;
use crate::acp::{AcpError, Dir, Watched};
use crate::direct::classify::scrub_detail;
use crate::direct::hooks::{
    DirectHook, DriverCtl, Event, HookEnv, Shutdown, StartupCtx, TurnEnd, TurnVote, Wire,
};
use crate::engine::goal::{GoalEngine, goal_engine};
use crate::time::{format_rfc3339, now_unix};
use busy::{BusyProbe, ProcBusy};
use command::{CommandProbe, ProcProbe};
use files::place_plugins;
use harness_protocol::direct::{DIRECT_TURN, check_goal};
use harness_protocol::envelope::EventType;
use machine::{Brake, Decision, Fp, Limits, Machine, Obs, Rule};
use rounds::{RoundLine, RoundsReader};
use serde_json::{Value, json};
use settings::GoalSettings;
use state::{GoalState, cleared_state, pending_state, read_state, write_state};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use tree::TreeFp;
use watch::{Command, HoldNote, Watch, span};

/// The sentence a dead loop is given (sent as a prompt in the same session through the driver, so it is never sent beside a user's message).
pub const KICK_PROMPT: &str = "Continue working toward the goal. Run the checks of the goal card again and answer from their output. If you cannot make progress, end your final message with a line that starts with BLOCKED: and says what is missing. Work outside this checkout is not a blocker: report it as NOT DONE lines and finish the rest.";

/// [`KICK_PROMPT`] with the sentence about work outside the checkout when `outside_note` is true, without it when the user turned it
/// off.
pub fn kick_prompt(outside_note: bool) -> String {
    if outside_note {
        KICK_PROMPT.to_string()
    } else {
        settings::strip_outside_note(KICK_PROMPT)
    }
}

/// The one prompt of the last turn: sent in the same session, by the driver, after the brake inside a round fired and the engine is
/// idle. It asks for words and nothing else; whatever the engine answers is its final message, and the run still ends with the brake's words.
pub const FINAL_PROMPT: &str = "The harness is ending this run because no file has changed and no new text has appeared for too long. Do not start new work and do not run commands. Write your final message now: say what is finished, what is left, and what you could not check.";

/// The least time a run must have left for the last turn to be offered.
const FINAL_MIN_LEFT: Duration = Duration::from_secs(30);
/// What the last turn leaves of the run's deadline: it ends this long before.
const FINAL_MARGIN: Duration = Duration::from_secs(15);

/// What the module says when it has no rounds log to go on.
const FALLBACK_WORDS: &str = "Goal loop: the rounds log is missing or damaged; the harness relies on the engine's stream, the silence limit and the deadline.";

/// The longest summary of a done claim kept.
const SUMMARY_CHARS: usize = 300;
/// How long the rounds log may stay missing, with the loop claimed, before the module says it works without it.
const OBSERVER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The module, or `None` when it is off for this run: switched off, an engine without a goal plugin, or no plugin folder in the image.
/// `Hooks::standard` calls it once per run.
pub fn hook(env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    hook_with(env, GoalSettings::from_env())
}

/// As [`hook`] with these settings: a test names the plugin folder, the product reads them from the environment.
pub fn hook_with(env: &HookEnv, settings: GoalSettings) -> Option<Arc<dyn DirectHook>> {
    GoalHook::for_engine(&env.engine, settings).map(|h| h as Arc<dyn DirectHook>)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The monotonic time of tokio's clock (it can be paused in tests) as a std instant.
fn now() -> Instant {
    tokio::time::Instant::now().into_std()
}

/// The id the engine gave a tool call: a string, or a number written as one.
/// The sentence every stop of the brake inside a round ends with: what the user can run next. The detail of a run is cut at 300
/// characters, so the sentence is short.
fn next_step_words(run_id: &str) -> String {
    format!("Next: ham pull {run_id} or ham continue {run_id} '<prompt>'.")
}

#[derive(Clone, Debug)]
struct Paths {
    state: PathBuf,
}

#[derive(Debug)]
enum Phase {
    /// Not started, or off for this run.
    Off,
    /// The files are placed; the engine has not been seen to claim the goal.
    Armed,
    /// The loop runs.
    Running,
    /// The loop was told to stop and the round in flight may finish.
    Braking {
        brake: Brake,
        since: Instant,
        deadline: Instant,
    },
    /// The brake inside a round fired and the engine is idle: the last turn was voted. The next turn end ends the run with the brake's
    /// words, whatever the turn did.
    Final {
        brake: Brake,
    },
    Finished,
}

/// How the drain after a brake came out.
enum Drained {
    /// The module was not braking.
    NotBraking,
    /// The drain is over: the round in flight ended and the engine is quiet (`idle`), or the drain's time ran out.
    Over { brake: Brake, idle: bool },
}

#[derive(Debug)]
struct Core {
    phase: Phase,
    machine: Machine,
    paths: Option<Paths>,
    owner: Option<(u32, u32)>,
    armed_at: Option<Instant>,
    /// When the first turn end found the goal not (yet) claimed.
    unclaimed_since: Option<Instant>,
    last_round_event: Option<Instant>,
    noted_ends: u64,
    fallback_noted: bool,
}

#[derive(Debug, Default)]
struct WireState {
    last_line: Option<Instant>,
    /// Ids of the calls of the completion tool.
    tool_ids: BTreeSet<String>,
    /// The model's accepted call of the completion tool, with its summary, until the supervisor takes it.
    candidate: Option<Option<String>>,
}

/// What the brake inside a round shares between the wire, the looks at the tree and the turn end.
struct RoundWatch {
    watch: Mutex<Watch>,
    /// The brake the monitor raised, until the turn end takes it.
    brake: Mutex<Option<Brake>>,
    /// The monitor stops: it raised a brake, the loop is over or the engine is gone.
    stop: AtomicBool,
    /// Where the evidence that a command is running comes from; none when the engine has no user of its own or the setting is off.
    probe: Option<Arc<dyn CommandProbe>>,
    /// Where the CPU of the session outside agentd and the engine is read; none when the signal is off or the engine has no user of its own.
    busy: Option<Arc<dyn BusyProbe>>,
    /// The seconds of the window that were held back for a running command when the brake fired; the brake's event says so.
    held_s: AtomicU64,
}

/// Looks at the tree while tool calls come and raises the brake of [`watch`] when the calls make no progress. When a prompt is open (the
/// one long prompt that never ends) it is cancelled, once, through the driver; when the engine's own follow-up round is the
/// one that polls, no prompt is open and the next turn-end vote takes the brake. Every `every` while a call is pending or a command was
/// seen running it also samples the evidence that a command of the engine's user is running (a few files of `/proc`, on a thread that may
/// block) and tells the watch before the tree is looked at.
async fn monitor(
    rw: Arc<RoundWatch>,
    tree: Arc<TreeFp>,
    ctl: DriverCtl,
    env: HookEnv,
    poll: std::time::Duration,
    every: std::time::Duration,
) {
    let mut sampled: Option<Instant> = None;
    let mut cpu_read: Option<Instant> = None;
    loop {
        tokio::time::sleep(poll).await;
        if rw.stop.load(Ordering::Relaxed) {
            return;
        }
        // The CPU of the session outside agentd and the engine is read on the same beat, whether or not a call is waiting: the brake
        // judges a stretch from two readings, and the first has to be there when the stretch begins.
        if let Some(busy) = &rw.busy
            && cpu_read.is_none_or(|at| now().saturating_duration_since(at) >= every)
        {
            let busy = busy.clone();
            let at = now();
            let ticks = tokio::task::spawn_blocking(move || busy.read())
                .await
                .ok()
                .flatten();
            lock(&rw.watch).cpu(at, ticks);
            cpu_read = Some(at);
        }
        let exposed = lock(&rw.watch).exposed();
        if let Some(probe) = &rw.probe
            && exposed
            && sampled.is_none_or(|at| now().saturating_duration_since(at) >= every)
        {
            let probe = probe.clone();
            let at = now();
            let evidence = tokio::task::spawn_blocking(move || probe.sample(at))
                .await
                .unwrap_or(Command::Unknown);
            // The evidence is told at the instant it was asked for: the ages in it are those of that moment.
            let note = {
                let mut w = lock(&rw.watch);
                w.command(at, evidence);
                w.take_note()
            };
            if let Some(note) = note {
                note_hold(&env, note);
            }
            sampled = Some(at);
        }
        if !lock(&rw.watch).wants_look(now(), every) {
            continue;
        }
        let fp = tree.fingerprint().await;
        let (brake, held, note) = {
            let mut w = lock(&rw.watch);
            let brake = w.look(now(), fp);
            (brake, w.protected(), w.take_note())
        };
        // A stretch of the unchanged tree that the session's CPU excused is told to the run's events.
        if let Some(note) = note {
            note_hold(&env, note);
        }
        if let Some(brake) = brake {
            rw.held_s.store(held.as_secs(), Ordering::Relaxed);
            *lock(&rw.brake) = Some(brake);
            rw.stop.store(true, Ordering::Relaxed);
            if ctl.prompt_open() {
                let _ = ctl.interrupt();
            }
            return;
        }
    }
}

/// The run's event for a change of the hold on the brake: when it began (what holds it back and how old that command is) and when it
/// ended. The words say what the harness did, never that the command's work is good or will finish.
fn note_hold(env: &HookEnv, note: HoldNote) {
    match note {
        HoldNote::Held { age } => note_event(
            env,
            "hold",
            &format!(
                "Goal loop: the stall brake is held back: a command that began {} ago has used the CPU lately, so the calls that wait for it are not counted.",
                span(age)
            ),
            json!({"ageS": age.as_secs()}),
        ),
        HoldNote::Busy { pct, span: stretch } => note_event(
            env,
            "hold",
            &format!(
                "Goal loop: the stall brake is held back: the session CPU outside agentd and the engine was busy at {pct} percent over the last {}, so the calls that waited are not counted.",
                span(stretch)
            ),
            json!({"busyPct": pct, "spanS": stretch.as_secs()}),
        ),
        HoldNote::Released { held } => note_event(
            env,
            "release",
            &format!(
                "Goal loop: the stall brake counts calls again: the command has not used the CPU for a while. It was held back for {} in all.",
                span(held)
            ),
            json!({"heldS": held.as_secs()}),
        ),
    }
}

/// One line of the run's log and one `turn.progress` event of kind `goal-loop` for the terminal app.
fn note_event(env: &HookEnv, phase: &str, text: &str, fields: Value) {
    // The run's secret values first (what the engine printed in a goal summary may be one it was given), then the key ids and the line shape.
    let text = scrub_detail(&env.secrets.redact(text));
    let fields = env.secrets.redact_json(&fields);
    env.log
        .line(&format!("direct turn goal loop {phase}: {text}"));
    let mut payload =
        json!({"turn": DIRECT_TURN, "kind": "goal-loop", "phase": phase, "text": text});
    if let (Some(into), Some(from)) = (payload.as_object_mut(), fields.as_object()) {
        for (k, v) in from {
            into.insert(k.clone(), v.clone());
        }
    }
    env.rec.record(EventType::TurnProgress, payload);
}

/// Which processes the evidence of a running command and of the session's CPU looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    /// Every process of the engine's user (the microVM: the engine has a user of its own).
    User(u32),
    /// The processes below the engine's: in a Landlock sandbox the engine shares agentd's user, which cannot tell them apart.
    Below,
    /// No evidence: the engine shares agentd's user and nothing else marks its processes (a developer machine, a test).
    Off,
}

/// The evidence a run gets for its engine user and its confinement. The user wins when there is one.
pub fn evidence_choice(engine_uid: Option<(u32, u32)>, sandboxed: bool) -> Evidence {
    match (engine_uid, sandboxed) {
        (Some((uid, _)), _) => Evidence::User(uid),
        (None, true) => Evidence::Below,
        (None, false) => Evidence::Off,
    }
}

/// Where the evidence that a command is running comes from.
enum ProbeSource {
    /// The product: the processes of the run's engine user in the real `/proc`.
    Engine,
    /// A test names the probe (they run as one user, so there is no engine user to look at).
    Given(Arc<dyn CommandProbe>),
    /// A test names the engine user and a `/proc` tree of its own; the probe is chosen as the product chooses it.
    Proc {
        uid: Option<(u32, u32)>,
        root: PathBuf,
    },
    /// No evidence of a running command (a test that is about the session's CPU alone).
    Off,
}

/// Where the CPU of the session outside agentd and the engine is read.
enum BusySource {
    /// The product: `/proc` of the machine, when the engine has a user of its own.
    Engine,
    /// A test names the reading.
    Given(Arc<dyn BusyProbe>),
    /// A test names a `/proc` tree; read as the product reads it when the engine user is named (`for_engine_over_proc`).
    Proc(PathBuf),
    /// No reading (the tests that name a command probe: they run as one user and need a brake that does not move with the machine's load).
    None,
}

/// The goal module of one run.
pub struct GoalHook {
    settings: GoalSettings,
    engine: &'static GoalEngine,
    env: OnceLock<HookEnv>,
    core: Mutex<Core>,
    wire: Mutex<WireState>,
    reader: Mutex<Option<RoundsReader>>,
    tree: OnceLock<Arc<TreeFp>>,
    /// The brake inside a round; present when the tree could be read.
    round: OnceLock<Arc<RoundWatch>>,
    /// The settings of this run: the stage's, with the start request's `goal` member laid over them. Set by `arm`.
    effective: OnceLock<GoalSettings>,
    /// Where the evidence that a command is running comes from; the product builds it from the engine's user in `arm`.
    source: ProbeSource,
    /// Where the session's CPU is read.
    busy: BusySource,
    /// The last turn is open: the prompt of [`FINAL_PROMPT`] was sent and not yet answered.
    final_open: Arc<AtomicBool>,
    /// The number of that prompt.
    final_no: AtomicU32,
    /// How long the last turn may take, set when it is voted.
    final_budget: Mutex<Duration>,
}

impl std::fmt::Debug for GoalHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoalHook")
            .field("core", &*lock(&self.core))
            .finish_non_exhaustive()
    }
}

impl GoalHook {
    /// The module for the engine profile `engine`, or `None` when it is off: switched off, an engine with no goal plugin, or no plugin
    /// folder (`lock.json`) in `settings.plugin_dir`.
    pub fn for_engine(engine: &str, settings: GoalSettings) -> Option<Arc<Self>> {
        if !settings.enabled {
            return None;
        }
        let engine = goal_engine(engine)?;
        settings
            .plugin_dir
            .join("lock.json")
            .is_file()
            .then(|| Self::new(settings, engine))
    }

    /// The module with these settings for this engine (tests register it through `Deps.hooks`).
    pub fn new(settings: GoalSettings, engine: &'static GoalEngine) -> Arc<Self> {
        Self::new_with(settings, engine, ProbeSource::Engine, BusySource::Engine)
    }

    /// As [`for_engine`](Self::for_engine) with the evidence that a command is running taken from `probe` instead of the process table of
    /// the engine's user (tests: they run as one user, so there is no engine user to look at).
    pub fn for_engine_with_probe(
        engine: &str,
        settings: GoalSettings,
        probe: Arc<dyn CommandProbe>,
    ) -> Option<Arc<Self>> {
        Self::for_engine_from(
            engine,
            settings,
            ProbeSource::Given(probe),
            BusySource::None,
        )
    }

    /// As [`for_engine`](Self::for_engine) with the CPU of the session taken from `busy` (tests: they run as one user, so the real reading is off).
    pub fn for_engine_with_busy(
        engine: &str,
        settings: GoalSettings,
        busy: Arc<dyn BusyProbe>,
    ) -> Option<Arc<Self>> {
        Self::for_engine_from(engine, settings, ProbeSource::Off, BusySource::Given(busy))
    }

    /// As [`for_engine`](Self::for_engine) for a run whose engine user is `uid` (as `StartupCtx.engine_uid` names it) and whose process table
    /// is the `/proc` tree at `root`: the evidence is chosen, logged and read exactly as in the product, over files a test wrote. (A real
    /// engine user needs a root agentd; tests run as one user.)
    pub fn for_engine_over_proc(
        engine: &str,
        settings: GoalSettings,
        uid: Option<(u32, u32)>,
        root: PathBuf,
    ) -> Option<Arc<Self>> {
        Self::for_engine_from(
            engine,
            settings,
            ProbeSource::Proc {
                uid,
                root: root.clone(),
            },
            BusySource::Proc(root),
        )
    }

    fn for_engine_from(
        engine: &str,
        settings: GoalSettings,
        source: ProbeSource,
        busy: BusySource,
    ) -> Option<Arc<Self>> {
        if !settings.enabled {
            return None;
        }
        let engine = goal_engine(engine)?;
        settings
            .plugin_dir
            .join("lock.json")
            .is_file()
            .then(|| Self::new_with(settings, engine, source, busy))
    }

    fn new_with(
        settings: GoalSettings,
        engine: &'static GoalEngine,
        source: ProbeSource,
        busy: BusySource,
    ) -> Arc<Self> {
        let machine = Machine::new(Limits {
            who: engine.label,
            ..settings.limits()
        });
        Arc::new(Self {
            settings,
            engine,
            env: OnceLock::new(),
            core: Mutex::new(Core {
                phase: Phase::Off,
                machine,
                paths: None,
                owner: None,
                armed_at: None,
                unclaimed_since: None,
                last_round_event: None,
                noted_ends: 0,
                fallback_noted: false,
            }),
            wire: Mutex::new(WireState::default()),
            reader: Mutex::new(None),
            tree: OnceLock::new(),
            round: OnceLock::new(),
            effective: OnceLock::new(),
            source,
            busy,
            final_open: Arc::new(AtomicBool::new(false)),
            final_no: AtomicU32::new(0),
            final_budget: Mutex::new(Duration::ZERO),
        })
    }

    /// The settings of this run: the request's `goal` member over the stage's, once `arm` has read the request.
    fn cfg(&self) -> &GoalSettings {
        self.effective.get().unwrap_or(&self.settings)
    }

    fn limits(&self) -> Limits {
        Limits {
            who: self.engine.label,
            ..self.settings.limits()
        }
    }

    fn env(&self) -> Option<&HookEnv> {
        self.env.get()
    }

    /// One line of the run's log and one `turn.progress` event of kind `goal-loop` for the terminal app.
    fn note(&self, phase: &str, text: &str) {
        self.note_with(phase, text, json!({}));
    }

    /// A `goal-loop` progress event: the phase and the sentence, and the numbers and words a screen would otherwise read out of the
    /// sentence (`rounds` and `status` of a round, `rule` of a brake, `summary` of a done claim).
    fn note_with(&self, phase: &str, text: &str, fields: Value) {
        let Some(env) = self.env() else { return };
        note_event(env, phase, text, fields);
    }

    /// Switches the module off for the rest of the run (the run goes on as a plain run) and makes sure no goal is left to claim.
    fn go_off(&self, why: &str) {
        let (paths, owner) = {
            let mut c = lock(&self.core);
            c.phase = Phase::Off;
            (c.paths.clone(), c.owner)
        };
        if let Some(p) = paths {
            let _ = write_state(&p.state, &cleared_state(), owner);
        }
        self.note("off", &format!("Goal loop off: {why}"));
    }

    async fn arm(&self, ctx: &mut StartupCtx<'_>) -> Result<(), String> {
        let _ = self.env.set(ctx.env.clone());
        // The request's `goal` member (checked in `on_startup`) is laid over the stage's settings for this run.
        let eff = match &ctx.request.goal {
            Some(g) => self.settings.with_request(g),
            None => self.settings.clone(),
        };
        // The default goal text names the records folder of the run (COMPAT-CAH-DATA).
        let mut eff = eff;
        if let Ok(folder) = crate::direct::records::choose(ctx.repo) {
            eff.goal_text = settings::goal_text_in(folder, &eff.goal_text);
        }
        let _ = self.effective.set(eff);
        let cfg = self.cfg();
        place_plugins(
            &self.settings.plugin_dir,
            ctx.config_dir,
            self.engine.plugins,
        )?;
        let state = ctx.data_dir.join(self.engine.state_file);
        let rounds = ctx.data_dir.join(self.engine.rounds_file);
        for file in [&state, &rounds] {
            let dir = file.parent().ok_or("a goal file has no directory")?;
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            // The engine user rewrites the state file and appends to the rounds log, so both directories are its own.
            if let Some((uid, gid)) = ctx.engine_uid {
                let mut d = Some(dir);
                while let Some(p) = d.filter(|p| *p != ctx.data_dir) {
                    std::os::unix::fs::chown(p, Some(uid), Some(gid))
                        .map_err(|e| format!("{}: {e}", p.display()))?;
                    d = p.parent();
                }
            }
        }
        for (k, v) in self.engine.env {
            ctx.engine_env.insert((*k).to_string(), (*v).to_string());
        }
        // The fingerprint of the tree as the run begins: the first round is compared with it.
        let hidden = crate::direct::goal::hidden_paths(&ctx.env.cfg.profiles_dir, &ctx.env.engine);
        let base = match TreeFp::open(&ctx.env.agent_data, ctx.repo, &hidden).await {
            Ok(t) => {
                let t = Arc::new(t);
                let base = t.fingerprint().await;
                let _ = self.tree.set(t.clone());
                // The brake inside a round: it watches the engine's tool calls and the tree from here on.
                let probe = self.command_probe(ctx);
                let busy = self.busy_probe(ctx);
                let rw = Arc::new(RoundWatch {
                    watch: Mutex::new(
                        Watch::new(cfg.watch_limits(self.engine.label), now(), base.clone())
                            .with_command_grace(cfg.command_grace)
                            .with_release_after(cfg.look_every * 6)
                            .with_busy_pct(if busy.is_some() { cfg.busy_pct } else { 0 })
                            .with_new_calls(cfg.new_calls)
                            .with_next_step(next_step_words(&ctx.env.run_id))
                            // a window of 20 minutes is judged over 30 seconds at least; a short window (a test's) over a quarter of it
                            .with_busy_min_span(watch::BUSY_MIN_SPAN.min(cfg.round_window / 4)),
                    ),
                    brake: Mutex::new(None),
                    stop: AtomicBool::new(false),
                    probe,
                    busy,
                    held_s: AtomicU64::new(0),
                });
                if self.round.set(rw.clone()).is_ok() {
                    tokio::spawn(monitor(
                        rw,
                        t,
                        ctx.env.ctl.clone(),
                        ctx.env.clone(),
                        cfg.poll,
                        cfg.look_every,
                    ));
                }
                base
            }
            Err(e) => {
                ctx.env.log.line(&format!(
                    "direct turn goal loop: no tree fingerprint, rounds count as progress: {e}"
                ));
                None
            }
        };
        let created = format_rfc3339(now_unix());
        write_state(
            &state,
            &pending_state(&self.cfg().goal_text, &created),
            ctx.engine_uid,
        )
        .map_err(|e| format!("{}: {e}", state.display()))?;
        *lock(&self.reader) = Some(RoundsReader::new(rounds.clone()));
        let mut c = lock(&self.core);
        c.machine = Machine::new(self.limits()).with_base(base);
        c.paths = Some(Paths { state });
        c.owner = ctx.engine_uid;
        c.armed_at = Some(now());
        c.phase = Phase::Armed;
        drop(c);
        ctx.env.log.line(&format!(
            "direct turn goal loop armed: plugins placed, goal text {} bytes",
            self.cfg().goal_text.len()
        ));
        Ok(())
    }

    /// Where the evidence that a command is running comes from: the probe a test named, else the process table of the engine's user, else
    /// none (switched off, no engine user of its own, or `/proc` of agentd's own user: no evidence, and the brake counts every call as it did).
    fn command_probe(&self, ctx: &StartupCtx<'_>) -> Option<Arc<dyn CommandProbe>> {
        let cfg = self.cfg();
        let log = |text: &str| {
            ctx.env
                .log
                .line(&format!("direct turn goal loop: command evidence {text}"));
        };
        if !cfg.looks_for_commands() {
            log("off: switched off");
            return None;
        }
        let (engine_uid, root) = match &self.source {
            ProbeSource::Given(p) => {
                log("on: a probe was given");
                return Some(p.clone());
            }
            ProbeSource::Engine => (ctx.engine_uid, PathBuf::from("/proc")),
            ProbeSource::Proc { uid, root } => (*uid, root.clone()),
            ProbeSource::Off => return None,
        };
        let uid = match evidence_choice(engine_uid, ctx.sandbox.is_some()) {
            Evidence::User(uid) => uid,
            Evidence::Below => {
                log(
                    "on: the processes below the engine's (it runs in a sandbox, in agentd's own user)",
                );
                return Some(Arc::new(
                    ProcProbe::for_engine_family_in(
                        &root,
                        ctx.env.ctl.engine_pid_cell(),
                        cfg.command_cpu_pct,
                    )
                    .with_window(cfg.command_window()),
                ));
            }
            Evidence::Off => {
                log("off: the engine has no user of its own");
                return None;
            }
        };
        match ProcProbe::for_engine_user_in(&root, uid, cfg.command_cpu_pct) {
            Some(p) => {
                log(&format!("on: the processes of user {uid}"));
                Some(Arc::new(p.with_window(cfg.command_window())))
            }
            None => {
                log("off: the engine user cannot be told from agentd's");
                None
            }
        }
    }

    /// Where the CPU of the session outside agentd and the engine is read: the reading a test named, else `/proc` of the machine when the
    /// engine has a user of its own (on a machine with no such user the load is not the run's), else none. Off when `AGENTD_STALL_BUSY_PCT` is 0,
    /// the stage's `AGENTD_GOAL_COMMAND` is off or the run's grace of a running command is 0.
    fn busy_probe(&self, ctx: &StartupCtx<'_>) -> Option<Arc<dyn BusyProbe>> {
        let cfg = self.cfg();
        let log = |text: &str| {
            ctx.env
                .log
                .line(&format!("direct turn goal loop: cpu evidence {text}"));
        };
        let root = match &self.busy {
            BusySource::None => return None,
            _ if !cfg.busy_signal() => {
                log("off: switched off");
                return None;
            }
            BusySource::Given(p) => {
                log("on: a reading was given");
                return Some(p.clone());
            }
            BusySource::Proc(root) => root.clone(),
            BusySource::Engine => PathBuf::from("/proc"),
        };
        let engine_user = match &self.source {
            ProbeSource::Proc { uid, .. } => *uid,
            _ => ctx.engine_uid,
        };
        if evidence_choice(engine_user, ctx.sandbox.is_some()) == Evidence::Off {
            log("off: the engine has no user of its own");
            return None;
        }
        log("on: the busy time of /proc/stat less agentd and the engine");
        Some(Arc::new(ProcBusy::new(
            root,
            std::process::id(),
            ctx.env.ctl.engine_pid_cell(),
        )))
    }

    /// A fresh session replaced the one that held the goal: the engine claims goals per session, so the goal is written again.
    fn rearm(&self) {
        let (paths, owner) = {
            let c = lock(&self.core);
            if matches!(c.phase, Phase::Off | Phase::Finished | Phase::Final { .. }) {
                return;
            }
            (c.paths.clone(), c.owner)
        };
        let Some(p) = paths else { return };
        let created = format_rfc3339(now_unix());
        if write_state(
            &p.state,
            &pending_state(&self.cfg().goal_text, &created),
            owner,
        )
        .is_err()
        {
            self.go_off("the goal could not be written for the new session");
            return;
        }
        {
            let mut c = lock(&self.core);
            c.machine = Machine::new(self.limits());
            c.phase = Phase::Armed;
            c.noted_ends = 0;
        }
        *lock(&self.wire) = WireState::default();
        if let Some(rw) = self.round.get() {
            lock(&rw.watch).boundary(now());
        }
        self.note(
            "rearm",
            "Goal loop: the goal is set again for the new session.",
        );
    }

    /// The user's cancel, the deadline or a stall: the engine's loop is told to stop, because its follow-up rounds ignore `session/cancel`.
    fn stop_loop(&self) {
        let (paths, owner) = {
            let mut c = lock(&self.core);
            if matches!(c.phase, Phase::Off) {
                return;
            }
            c.phase = Phase::Finished;
            (c.paths.clone(), c.owner)
        };
        self.watch_done();
        if let Some(p) = paths {
            let _ = write_state(&p.state, &cleared_state(), owner);
        }
    }

    /// The loop is over: the monitor of the brake inside a round has nothing more to watch.
    fn watch_done(&self) {
        if let Some(rw) = self.round.get() {
            rw.stop.store(true, Ordering::Relaxed);
        }
    }

    /// The brake the monitor raised, if one is waiting: the engine's own loop is told to stop, the module goes to braking and the vote is
    /// the drain's (the round in flight, or the prompt that was cancelled, may finish; then the run ends). Nothing happens when the loop is
    /// over or another brake is already draining.
    fn round_brake(&self, at: Instant, end: &TurnEnd<'_>) -> Option<TurnVote> {
        let interrupted = end.interrupted;
        let rw = self.round.get()?;
        let brake = lock(&rw.brake).take()?;
        let (paths, owner, over) = {
            let c = lock(&self.core);
            (
                c.paths.clone(),
                c.owner,
                matches!(
                    c.phase,
                    Phase::Finished | Phase::Braking { .. } | Phase::Final { .. }
                ),
            )
        };
        if over {
            return None;
        }
        let Some(paths) = paths else {
            return Some(TurnVote::Stop {
                detail: scrub_detail(&brake.detail),
            });
        };
        if let Err(e) = write_state(&paths.state, &cleared_state(), owner)
            && let Some(env) = self.env()
        {
            env.log.line(&format!(
                "direct turn goal loop: the state file could not be cleared: {e}"
            ));
        }
        self.note_with(
            "brake",
            &brake.detail,
            json!({"rule": brake.rule.word(), "heldS": rw.held_s.load(Ordering::Relaxed)}),
        );
        if interrupted {
            // The prompt that polled was cancelled by this module's own interrupt: no round is in flight, and the driver does not hold a
            // run after an interrupt, so the engine is idle (unless it ignored the cancel) and gets its last turn; then the run ends here
            // with the module's words.
            let answered = !matches!(end.raw.watched, Watched::Done(Err(AcpError::CancelIgnored)));
            if answered && self.final_wanted(&brake, end.time_left) {
                return Some(self.begin_final(brake, end.time_left));
            }
            lock(&self.core).phase = Phase::Finished;
            self.watch_done();
            return Some(TurnVote::Stop {
                detail: scrub_detail(&brake.detail),
            });
        }
        lock(&self.core).phase = Phase::Braking {
            since: at,
            deadline: at + self.cfg().drain,
            brake,
        };
        Some(self.drain(at, &paths, end.time_left))
    }

    /// Whether the brake gets the last turn: the user did not switch it off, the brake is the one raised in the middle of a round (the
    /// round rules end a round that has its message already), and enough of the run is left.
    fn final_wanted(&self, brake: &Brake, time_left: Duration) -> bool {
        self.cfg().final_message
            && brake.rule == Rule::RoundNoProgress
            && time_left >= FINAL_MIN_LEFT
    }

    /// The brake becomes the last turn: the engine is idle, the plugin's state file is cleared, and one prompt asks for the final
    /// message in the same session. The turn takes at most the final window, cut to fit the deadline; when it ends, however it ends, the run
    /// ends with the brake's words (`after_final`).
    fn begin_final(&self, brake: Brake, time_left: Duration) -> TurnVote {
        *lock(&self.final_budget) = self
            .cfg()
            .final_window
            .min(time_left.saturating_sub(FINAL_MARGIN));
        lock(&self.core).phase = Phase::Final { brake };
        self.watch_done();
        self.note(
            "final",
            &format!(
                "Goal loop: the harness asks {} for a final message before it ends the run.",
                self.engine.label
            ),
        );
        TurnVote::Prompt(FINAL_PROMPT.to_string())
    }

    /// The last turn ended (answered, cut short by its window or by the engine's grace): the run ends with the brake's words.
    fn after_final(&self) -> Option<TurnVote> {
        let mut c = lock(&self.core);
        let Phase::Final { brake } = &c.phase else {
            return None;
        };
        let detail = scrub_detail(&brake.detail);
        c.phase = Phase::Finished;
        Some(TurnVote::Stop { detail })
    }

    /// The last turn was sent: its window starts. When the window passes with the prompt still open it is cancelled, once, through the
    /// driver, and `after_final` ends the run.
    fn final_turn_sent(&self, no: u32) {
        self.final_no.store(no, Ordering::Relaxed);
        self.final_open.store(true, Ordering::Relaxed);
        let Some(env) = self.env() else { return };
        let window = *lock(&self.final_budget);
        let ctl = env.ctl.clone();
        let open = self.final_open.clone();
        tokio::spawn(async move {
            tokio::time::sleep(window).await;
            if open.swap(false, Ordering::Relaxed) {
                let _ = ctl.interrupt();
            }
        });
    }

    /// The tool calls and the text of the engine, for the brake inside a round. Counted whatever the phase of the loop: a run that polls
    /// is stuck with or without the goal loop.
    fn watch_wire(&self, msg: &Value) {
        let Some(rw) = self.round.get() else { return };
        if msg.get("method").and_then(Value::as_str) != Some("session/update") {
            return;
        }
        let Some(u) = msg.pointer("/params/update") else {
            return;
        };
        let tools = watch::WireTools {
            completion: self.engine.completion_tool,
            delegate: self.engine.delegate_tool,
        };
        watch::feed_update(&mut lock(&rw.watch), now(), u, tools);
    }

    /// The user's cancel (`ham stop`) is up.
    fn user_cancelled(&self) -> bool {
        self.env().is_some_and(|e| *e.controls.subscribe().borrow())
    }

    fn on_wire_line(&self, msg: &Value) {
        let mut w = lock(&self.wire);
        w.last_line = Some(now());
        if msg.get("method").and_then(Value::as_str) != Some("session/update") {
            return;
        }
        let Some(u) = msg.pointer("/params/update") else {
            return;
        };
        let id = match u.get("toolCallId") {
            Some(Value::String(s)) => Some(s.clone()),
            Some(Value::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let named = u
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with(self.engine.completion_tool));
        match (u.get("sessionUpdate").and_then(Value::as_str), id) {
            (Some("tool_call"), Some(id)) if named => {
                w.tool_ids.insert(id);
            }
            (Some("tool_call_update"), Some(id)) => {
                let ours = named || w.tool_ids.contains(&id);
                let completed = u.get("status").and_then(Value::as_str) == Some("completed");
                // Only an exact `true` counts: the plugin answers `completed:false` to a call that came too early.
                let accepted = u.pointer("/rawOutput/completed") == Some(&Value::Bool(true));
                if ours && completed && accepted && w.candidate.is_none() {
                    let summary = u
                        .pointer("/rawOutput/summary")
                        .and_then(Value::as_str)
                        .map(|s| s.chars().take(SUMMARY_CHARS).collect::<String>())
                        .filter(|s| !s.trim().is_empty());
                    w.candidate = Some(summary);
                }
            }
            _ => {}
        }
    }

    async fn step(&self, end: &TurnEnd<'_>) -> TurnVote {
        let at = now();
        // The last turn of a brake ended: the run ends with the brake's words, also when the window cut the turn short.
        if let Some(vote) = self.after_final() {
            return vote;
        }
        // A brake raised inside a round is answered first, also when it was this module's own interrupt that ended the prompt.
        if let Some(vote) = self.round_brake(at, end) {
            return vote;
        }
        // A module's interrupt (a user's message that cannot wait) is answered by that module; this one has no opinion.
        if end.interrupted {
            return TurnVote::Done;
        }
        enum Kind {
            Idle,
            Armed,
            Running,
            Braking,
        }
        let (kind, paths) = {
            let c = lock(&self.core);
            let kind = match c.phase {
                Phase::Off | Phase::Finished | Phase::Final { .. } => Kind::Idle,
                Phase::Armed => Kind::Armed,
                Phase::Running => Kind::Running,
                Phase::Braking { .. } => Kind::Braking,
            };
            (kind, c.paths.clone())
        };
        let Some(paths) = paths else {
            return TurnVote::Done;
        };
        match kind {
            Kind::Idle => TurnVote::Done,
            Kind::Braking => self.drain(at, &paths, end.time_left),
            Kind::Armed => self.advance(at, &paths, true).await,
            Kind::Running => self.advance(at, &paths, false).await,
        }
    }

    /// One look at the loop: the done claim, the rounds log, the tree, the state file; then the rules decide. `armed`: the engine has not
    /// been seen to claim the goal yet, and this look decides whether it did.
    async fn advance(&self, at: Instant, paths: &Paths, armed: bool) -> TurnVote {
        // The done claim is taken before the rounds log is read: the round that made the call started before the call, so its start line
        // is in the file whenever the claim is seen.
        let (claim, last_line) = {
            let mut w = lock(&self.wire);
            (w.candidate.take(), w.last_line)
        };
        let polled = lock(&self.reader)
            .as_mut()
            .map(RoundsReader::poll)
            .unwrap_or_default();
        let polled_present = polled.present;
        if !polled.lines.is_empty()
            && let Some(rw) = self.round.get()
        {
            lock(&rw.watch).boundary(at);
        }
        let ends = polled
            .lines
            .iter()
            .filter(|l| matches!(l, RoundLine::End(_)))
            .count();
        let fp = if ends > 0 {
            match self.tree.get() {
                Some(t) => t.fingerprint().await,
                None => None,
            }
        } else {
            None
        };
        let goal = read_state(&paths.state);
        let claim_seen = claim.is_some();
        let (decision, round_note, fallback_note, owner, promoted) = {
            let mut c = lock(&self.core);
            let mut seen = 0;
            for line in polled.lines {
                match line {
                    RoundLine::Start { n } => c.machine.observe(at, Obs::Start { n }),
                    RoundLine::End(end) => {
                        seen += 1;
                        // The tree is measured once for the rounds read together: the last of them carries it.
                        let fp = match (&fp, seen == ends) {
                            (_, false) => Fp::Same,
                            (Some(f), true) => Fp::Is(f.clone()),
                            (None, true) => Fp::Unknown,
                        };
                        c.machine.observe(at, Obs::End { end, fp });
                    }
                }
            }
            if let Some(l) = last_line {
                c.machine.observe(l, Obs::Line);
            }
            if let Some(summary) = claim {
                c.machine.observe(at, Obs::Candidate { summary });
            }
            if let GoalState::Seen { pending, active } = goal {
                c.machine.goal_seen(at, pending || active > 0);
            }
            // The goal was claimed when the state file shows an active goal, and also when the engine has gone on by itself: the loop can
            // finish between the first answer and this look (the goal then leaves the state file) and a read can meet the plugin's
            // half-written file.
            let evidence = matches!(goal, GoalState::Seen { active, .. } if active > 0)
                || claim_seen
                || c.machine.starts() >= 2;
            if armed && !evidence {
                let since = *c.unclaimed_since.get_or_insert(at);
                if at.saturating_duration_since(since) < self.settings.claim_wait {
                    return TurnVote::Wait {
                        until: self.settings.poll,
                    };
                }
                drop(c);
                self.go_off(&format!(
                    "the engine did not claim the goal ({goal:?}); the run goes on as a plain run"
                ));
                return TurnVote::Done;
            }
            let promoted = armed;
            if promoted {
                c.phase = Phase::Running;
            }
            let round_note = self.round_note(&mut c, at);
            let waiting = c
                .armed_at
                .is_some_and(|a| at.saturating_duration_since(a) >= OBSERVER_WAIT);
            let fallback_note = ((polled.damaged
                || (!polled.present && !c.machine.observed() && waiting))
                && !c.fallback_noted)
                .then(|| {
                    c.fallback_noted = true;
                    FALLBACK_WORDS
                });
            (
                c.machine.decide(at),
                round_note,
                fallback_note,
                c.owner,
                promoted,
            )
        };
        if promoted {
            self.note(
                "on",
                "Goal loop on: the engine checks the goal after each round.",
            );
            // The observer writes its first line when the first run starts, before that run's answer: no file then means no observer.
            if !polled_present {
                lock(&self.core).fallback_noted = true;
                self.note("fallback", FALLBACK_WORDS);
            }
        }
        if let Some(text) = fallback_note {
            self.note("fallback", text);
        }
        if let Some((text, rounds, status)) = round_note {
            self.note_with("round", &text, json!({"rounds": rounds, "status": status}));
        }
        match decision {
            Decision::Wait => TurnVote::Wait {
                until: self.settings.poll,
            },
            Decision::Done { summary } => {
                lock(&self.core).phase = Phase::Finished;
                self.watch_done();
                let fields = match &summary {
                    Some(s) => json!({"summary": s}),
                    None => json!({}),
                };
                let words = match &summary {
                    Some(s) => format!(
                        "Goal loop: {} called the goal-complete tool and its round ended with a message. Its summary: {s}",
                        self.engine.label
                    ),
                    None => format!(
                        "Goal loop: {} called the goal-complete tool and its round ended with a message.",
                        self.engine.label
                    ),
                };
                self.note_with("done", &words, fields);
                TurnVote::Done
            }
            Decision::Kick => {
                self.note("kick", "Goal loop: the loop stopped; one continue is sent.");
                TurnVote::Prompt(self.cfg().kick_prompt())
            }
            Decision::Brake(brake) => {
                // The engine's own loop is told to stop first; the round in flight may finish before the run ends.
                if let Err(e) = write_state(&paths.state, &cleared_state(), owner)
                    && let Some(env) = self.env()
                {
                    env.log.line(&format!(
                        "direct turn goal loop: the state file could not be cleared: {e}"
                    ));
                }
                self.note_with("brake", &brake.detail, json!({"rule": brake.rule.word()}));
                lock(&self.core).phase = Phase::Braking {
                    since: at,
                    deadline: at + self.settings.drain,
                    brake,
                };
                TurnVote::Wait {
                    until: self.settings.poll,
                }
            }
        }
    }

    /// The words of a round event, at most one per `events_every`.
    fn round_note(&self, c: &mut Core, at: Instant) -> Option<(String, u64, String)> {
        let ends = c.machine.ends();
        if ends == c.noted_ends {
            return None;
        }
        if c.last_round_event
            .is_some_and(|t| at.saturating_duration_since(t) < self.settings.events_every)
        {
            return None;
        }
        c.noted_ends = ends;
        c.last_round_event = Some(at);
        let status = c
            .machine
            .last_end()
            .map_or("unknown", |e| e.status.as_str())
            .to_string();
        Some((
            format!("Goal loop: {ends} rounds ended, the last one {status}."),
            ends,
            status,
        ))
    }

    /// After a brake: the round in flight may finish, then the run ends. The state file is cleared again if the goal came back.
    fn drain(&self, at: Instant, paths: &Paths, time_left: Duration) -> TurnVote {
        let polled = lock(&self.reader)
            .as_mut()
            .map(RoundsReader::poll)
            .unwrap_or_default();
        let goal = read_state(&paths.state);
        let (verdict, owner) = {
            let mut c = lock(&self.core);
            for line in polled.lines {
                match line {
                    RoundLine::Start { n } => c.machine.observe(at, Obs::Start { n }),
                    RoundLine::End(end) => c.machine.observe(at, Obs::End { end, fp: Fp::Same }),
                }
            }
            let owner = c.owner;
            let drained = match &c.phase {
                Phase::Braking {
                    brake,
                    since,
                    deadline,
                } => {
                    let quiet_since = c.machine.last_end_at().map_or(*since, |e| e.max(*since));
                    let idle = !c.machine.open_round()
                        && at.saturating_duration_since(quiet_since) >= c.machine.limits().settle;
                    (idle || at >= *deadline).then(|| Drained::Over {
                        brake: brake.clone(),
                        idle,
                    })
                }
                _ => Some(Drained::NotBraking),
            };
            if drained.is_some() {
                c.phase = Phase::Finished;
                if let Some(rw) = self.round.get() {
                    rw.stop.store(true, Ordering::Relaxed);
                }
            }
            (drained, owner)
        };
        match verdict {
            Some(Drained::NotBraking) => TurnVote::Done,
            // The round in flight ended by itself and the engine is idle: its last turn. A round still in flight at the end of the drain
            // would queue the prompt behind it, so that run ends at once.
            Some(Drained::Over { brake, idle: true }) if self.final_wanted(&brake, time_left) => {
                self.begin_final(brake, time_left)
            }
            Some(Drained::Over { brake, .. }) => TurnVote::Stop {
                detail: scrub_detail(&brake.detail),
            },
            None => {
                if matches!(goal, GoalState::Seen { pending, active } if pending || active > 0) {
                    let _ = write_state(&paths.state, &cleared_state(), owner);
                }
                TurnVote::Wait {
                    until: self.settings.poll,
                }
            }
        }
    }
}

/// The paths the engine's profile says agentd moves aside at launch (`dir/**` and plain relative paths).
pub fn hidden_paths(profiles_dir: &std::path::Path, engine: &str) -> Vec<String> {
    harness_core::engine_profile::load_profile(&profiles_dir.join(format!("{engine}.toml")))
        .map(|p| p.hidden_paths)
        .unwrap_or_default()
}

impl DirectHook for GoalHook {
    fn name(&self) -> &'static str {
        "goal"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // The `goal` member is checked again with the one check `ham` and control-api call: agentd trusts neither a stale client nor
            // a hand-made request. The words name the member and its range, never the value.
            if let Some(g) = &ctx.request.goal
                && let Err(e) = check_goal(g)
            {
                return Err(e.to_string());
            }
            // The user's switch: the run is the plain run it was before the goal plugin. Nothing
            // is placed, no goal is written and no plugin mode is set, so the engine does not loop; one `off` event tells the card.
            if ctx.request.goal.is_some_and(|g| !g.enabled) {
                let _ = self.env.set(ctx.env.clone());
                self.note(
                    "off",
                    "Goal loop off: you switched it off for this run, so the run goes on as a plain run.",
                );
                return Ok(());
            }
            // The goal loop is a default, not a requirement: a run that cannot have it goes on without it.
            match self.arm(ctx).await {
                // The engine's plugin sends the follow-up prompts: the harness's nudge on top of them would be a second loop.
                Ok(()) => ctx.nudge_off = true,
                Err(why) => self.go_off(&why),
            }
            Ok(())
        })
    }

    fn on_wire(&self, wire: &Wire<'_>) {
        if wire.stale || wire.dir != Dir::FromEngine {
            return;
        }
        self.watch_wire(wire.msg);
        if matches!(
            lock(&self.core).phase,
            Phase::Off | Phase::Finished | Phase::Final { .. }
        ) {
            return;
        }
        self.on_wire_line(wire.msg);
    }

    fn on_event(&self, event: &Event<'_>) {
        match event {
            // The user's cancel ends the run: the engine's loop is told to stop (its rounds ignore `session/cancel`). A cancel the driver
            // wrote for a stall or the deadline is not final (the ladder may resume once, the deadline ends the run by itself), so the loop
            // is left alone.
            Event::Cancel { .. } if self.user_cancelled() => self.stop_loop(),
            Event::PromptSent(p) if p.text == self.cfg().kick_prompt() => {
                lock(&self.core).machine.kicked(now());
            }
            Event::PromptSent(p) if p.text == FINAL_PROMPT => self.final_turn_sent(p.no),
            Event::PromptReplied(r) if r.no == self.final_no.load(Ordering::Relaxed) => {
                self.final_open.store(false, Ordering::Relaxed);
            }
            // A session began: what the engine's user runs now is the engine's own, not a command of the model's.
            Event::SessionStarted(s) => {
                if let Some(probe) = self.round.get().and_then(|rw| rw.probe.as_ref()) {
                    probe.rebaseline();
                }
                if s.number >= 2 {
                    self.rearm();
                }
            }
            _ => {}
        }
    }

    fn on_turn_end<'a>(&'a self, end: &'a TurnEnd<'a>) -> BoxFuture<'a, TurnVote> {
        Box::pin(async move { self.step(end).await })
    }

    fn on_shutdown<'a>(&'a self, _end: &'a Shutdown<'a>) -> BoxFuture<'a, ()> {
        self.watch_done();
        Box::pin(std::future::ready(()))
    }
}
