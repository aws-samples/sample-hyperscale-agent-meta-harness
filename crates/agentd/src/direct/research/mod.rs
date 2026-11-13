//! Module `research`: the engine researches with Tavily and
//! downloads repositories for context, and the VM firewall stays as it is.
//!
//! The Tavily key is in the stage account, never in the VM. agentd offers the engine a small HTTP API on `127.0.0.1` ([`api`]); each call
//! goes to a route in the stage account over the private path the VM already has ([`route`]: SigV4 with the run's session credentials);
//! the route does the search or the download and stores the result where the worker role can read and cannot write. agentd reads it back,
//! checks every hash against the route's manifest and places it read-only in the context directory outside the work tree
//! ([`place`], [`context`], [`extract`]), with an index of origin, date, licence and SHA-256 ([`index`]). The `ham-research` command of the
//! worker image (`images/worker-python/ham-research`) is the engine-neutral way to call the API.
//!
//! Hooks it uses:
//! - `on_startup`: asks the route whether research is on for the run (`limits`; a run with research off, or a stage without the route,
//!   gets no server and no environment variable and nothing is logged for a missing route), makes the context directory, starts the
//!   server and puts `HAMH_RESEARCH_URL`, `HAMH_RESEARCH_WAIT_S` and `HAMH_CONTEXT_DIR` in the engine's environment. Whatever goes wrong
//!   here is logged and the run goes on without research: this module never ends a run.
//! - `on_shutdown`: stops the server, keeps a snapshot of the counters (counts only, never text), logs and records them, and reads the route's own count
//!   for the run once more (at most three seconds; a failed read loses only that part).
//! - `on_summary`: puts the snapshot in `summary.json` as `DirectSummary.research` ([`DirectResearchNote`]). The member is there when research was
//!   decided for the run: `enabled` true when the lane started (also when the engine never called it), `enabled` false when the user switched it off, the
//!   route said it is off or the lane could not start; it is left out on a stage without the research route. Telemetry only: no label, outcome or delivery
//!   depends on it.
//!
//! Rules: this directory, its tests (`crates/agentd/tests/direct_research*.rs`, `research_support/`) and its section of
//! `src/direct/AGENTS.md` are the module's. Settings are read from `AGENTD_RESEARCH_*` environment variables ([`settings`]).

pub mod api;
pub mod client;
pub mod context;
pub mod extract;
pub mod index;
pub mod place;
pub mod route;
pub mod settings;
pub mod store;

use crate::BoxFuture;
use crate::direct::compat;
use crate::direct::context::ContextDir;
use crate::direct::hooks::{DirectHook, HookEnv, Shutdown, StartupCtx, SummaryCtx};
use crate::direct::research::api::{Server, Shared, Stats};
use crate::direct::research::client::Client;
use crate::direct::research::place::Placer;
use crate::direct::research::route::{FailureKind, HttpRoute, Route};
use crate::direct::research::settings::Settings;
use crate::direct::research::store::{ObjectStore, S3Store};
use crate::s3::S3Objects;
use harness_protocol::direct::{DIRECT_TURN, s3};
use harness_protocol::envelope::EventType;
use harness_protocol::research::{
    DirectResearchCounts, DirectResearchNote, ResearchErrorCode, ResearchOptions, ResearchProvider,
    ThirdPartyMode,
};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long the question "is research on for this run?" may take before the module starts without an answer.
const PROBE_BUDGET: Duration = Duration::from_secs(8);
/// How long the read of the route's own count may take when the run ends. The hook has 15 seconds in all; a read that does not answer in time
/// loses the `usage` of the note and nothing else.
const FINAL_BUDGET: Duration = Duration::from_secs(3);
/// How long hashing the placed files and uploading the listing may take when the run ends (after the read of the route's count, inside the
/// hook's 15 seconds). Whatever is not done by then is a log line: the listing is evidence, never part of the run.
const LISTING_BUDGET: Duration = Duration::from_secs(8);

/// The route and the bucket a module instance talks to. The product builds them from the run; tests give fakes.
#[derive(Clone)]
pub struct Parts {
    pub route: Arc<dyn Route>,
    pub store: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for Parts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parts").finish_non_exhaustive()
    }
}

struct Running {
    server: Server,
    shared: Arc<Shared>,
    /// The provider the route named when the module started, if it answered.
    provider: Option<ResearchProvider>,
}

/// What the first question to the route decided.
enum Probe {
    /// Research is on (or the route did not answer in time): start the lane.
    Start(Option<ResearchProvider>),
    /// The stage has no research route: nothing starts, nothing is said, no member is written.
    Absent,
    /// The route says research is off for the run.
    Off,
}

/// The module.
pub struct ResearchHook {
    settings: Settings,
    parts: Option<Parts>,
    running: Mutex<Option<Running>>,
    /// The `research` member for `summary.json`: set when research was decided, copied by `on_summary`.
    note: Mutex<Option<DirectResearchNote>>,
}

impl std::fmt::Debug for ResearchHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResearchHook").finish_non_exhaustive()
    }
}

/// The module, or `None` when `AGENTD_RESEARCH_MODE` switches it off. `Hooks::standard` calls it once per run.
pub fn hook(_env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    let settings = Settings::from_env();
    settings
        .enabled
        .then(|| Arc::new(ResearchHook::new(settings)) as Arc<dyn DirectHook>)
}

impl ResearchHook {
    /// The product's module: the route and the bucket are built from the run at startup.
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            parts: None,
            running: Mutex::new(None),
            note: Mutex::new(None),
        }
    }

    /// A module that talks to these instead (tests).
    pub fn with_parts(settings: Settings, parts: Parts) -> Self {
        Self {
            settings,
            parts: Some(parts),
            running: Mutex::new(None),
            note: Mutex::new(None),
        }
    }

    fn parts_for(&self, env: &HookEnv) -> Result<Parts, String> {
        if let Some(p) = &self.parts {
            return Ok(p.clone());
        }
        if env.bucket.is_empty() {
            return Err("the run names no bucket".to_string());
        }
        Ok(Parts {
            route: Arc::new(
                HttpRoute::new(
                    env.creds.clone(),
                    env.internal_api_url.clone(),
                    env.region.clone(),
                )
                .with_timeout(self.settings.call_timeout),
            ),
            store: Arc::new(S3Store::new(
                env.creds.clone(),
                env.region.clone(),
                env.s3_endpoint.clone(),
                env.bucket.clone(),
            )),
        })
    }

    fn set_note(&self, note: Option<DirectResearchNote>) {
        *self.note.lock().unwrap_or_else(|e| e.into_inner()) = note;
    }

    /// The note of a run whose research was off: the word and nothing else.
    fn off(&self) {
        self.set_note(Some(DirectResearchNote {
            enabled: false,
            provider: None,
            usage: None,
            counts: None,
        }));
    }

    /// Whether to start. A route that is not there (this stage has none) means "do not start" and says nothing; a route that says research is off for
    /// the run means "do not start" and is logged. A route that does not answer in time starts the module: calls will say what failed.
    async fn probe(&self, client: &Client, env: &HookEnv) -> Probe {
        match tokio::time::timeout(PROBE_BUDGET, client.limits_now()).await {
            Ok(Ok(l)) => {
                client.set_limits(l.limits);
                Probe::Start(l.provider)
            }
            Ok(Err(f)) if f.kind == FailureKind::Absent => Probe::Absent,
            Ok(Err(f)) if f.error.code == ResearchErrorCode::Disabled => {
                env.log
                    .line("direct research off: the run's policy has research switched off");
                Probe::Off
            }
            Ok(Err(f)) if f.kind == FailureKind::Final => {
                env.log.line(&format!(
                    "direct research off: the route answered {}",
                    f.error.code
                ));
                Probe::Off
            }
            Ok(Err(f)) => {
                env.log.line(&format!(
                    "direct research starting without an answer from the route ({})",
                    f.error.code
                ));
                Probe::Start(None)
            }
            Err(_) => {
                env.log.line(
                    "direct research starting without an answer from the route (no answer in time)",
                );
                Probe::Start(None)
            }
        }
    }

    async fn start(&self, ctx: &mut StartupCtx<'_>) -> Result<(), String> {
        let env = ctx.env;
        let parts = self.parts_for(env)?;
        let client = Client::new(parts.route.clone(), &self.settings);
        let provider = match self.probe(&client, env).await {
            Probe::Start(provider) => provider,
            Probe::Absent => return Ok(()),
            Probe::Off => {
                self.off();
                return Ok(());
            }
        };
        let context = ContextDir::create(&env.agent_data)?;
        // The research budgets of this run: the stage's, with the request's `limits` member laid over them.
        let run_settings = match &ctx.request.limits {
            Some(l) => self.settings.with_limits(l),
            None => self.settings.clone(),
        };
        let placer = Arc::new(Placer::open(&context, &env.run_id, run_settings.clone())?);
        let listener = bind(self.settings.port).await?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let shared = Shared::new(
            client,
            parts.store,
            placer,
            run_settings,
            ctx.repo.to_path_buf(),
            env.log.clone(),
        );
        // The user's per-run setting (`ham --allow-third-party`, request member `thirdParty`): the check answer names it.
        shared.set_third_party(ctx.request.third_party == Some(ThirdPartyMode::Allow));
        let server = api::start(listener, shared.clone());
        // COMPAT-CAH-DATA: each variable under both prefixes (rules the user edited before the rename name the earlier one).
        compat::set_both(ctx.engine_env, "RESEARCH_URL", format!("http://{addr}"));
        compat::set_both(
            ctx.engine_env,
            "RESEARCH_WAIT_S",
            self.settings.wait.as_secs().to_string(),
        );
        compat::set_both(
            ctx.engine_env,
            "CONTEXT_DIR",
            context.path().display().to_string(),
        );
        env.log
            .line(&format!("direct research loopback API on {addr}"));
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(Running {
            server,
            shared,
            provider,
        });
        Ok(())
    }
}

/// `127.0.0.1:<port>`, else any free port on 127.0.0.1.
async fn bind(port: u16) -> Result<tokio::net::TcpListener, String> {
    let first = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
    match first {
        Ok(l) => Ok(l),
        Err(e) if port != 0 && e.kind() == std::io::ErrorKind::AddrInUse => {
            tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(|e| format!("research: {}", e.kind()))
        }
        Err(e) => Err(format!("research: {}", e.kind())),
    }
}

impl DirectHook for ResearchHook {
    fn name(&self) -> &'static str {
        "research"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if !self.settings.enabled {
                return Ok(());
            }
            if let Some(ResearchOptions { enabled: false }) = ctx.request.research {
                ctx.env
                    .log
                    .line("direct research off: the user switched it off for this run");
                // The card shows it: one event, the words the client draws as a plain line.
                ctx.env.rec.record(
                    EventType::TurnProgress,
                    json!({"turn": DIRECT_TURN, "kind": "research", "phase": "off", "text": "Research off: you switched it off for this run."}),
                );
                self.off();
                return Ok(());
            }
            if let Err(why) = self.start(ctx).await {
                ctx.env.log.line(&format!(
                    "direct research not started: {}",
                    crate::direct::classify::scrub_detail(&why)
                ));
                self.off();
            }
            Ok(())
        })
    }

    fn on_shutdown<'a>(&'a self, end: &'a Shutdown<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let running = self
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let Some(Running {
                server,
                shared,
                provider,
            }) = running
            else {
                return;
            };
            server.stop().await;
            let s = shared.stats();
            let counts = counts_of(s, shared.placer().used_bytes());
            // The counts first: whatever the read of the route below does, the note keeps them.
            self.set_note(Some(DirectResearchNote {
                enabled: true,
                provider,
                usage: None,
                counts: Some(counts),
            }));
            if s.searches + s.fetches + s.repos + s.waits > 0 {
                end.env.log.line(&format!(
                    "direct research stopped searches={} fetches={} repos={} placed={} refused={} errors={} placed_bytes={}",
                    s.searches, s.fetches, s.repos, s.placed, s.refused, s.errors, counts.placed_bytes
                ));
                end.env.rec.record(
                    EventType::TurnProgress,
                    json!({
                        "kind": "research",
                        "searches": counts.searches,
                        "fetches": counts.fetches,
                        "repos": counts.repos,
                        "placed": counts.placed,
                        "refused": counts.refused,
                        "errors": counts.errors,
                        "placedBytes": counts.placed_bytes,
                    }),
                );
            }
            // The route's own count for the run, once more. It is the number the route's caps were applied to; a failure or a silence loses it
            // and the note says nothing about it.
            if let Ok(Ok(l)) = tokio::time::timeout(FINAL_BUDGET, shared.limits_now()).await {
                self.set_note(Some(DirectResearchNote {
                    enabled: true,
                    provider: l.provider.or(provider),
                    usage: Some(l.usage),
                    counts: Some(counts),
                }));
            }
            // What agentd placed, hashed from the disk: the placement evidence of the research checks. The server is stopped, so
            // nothing is being placed. A failure or a late finish is a log line.
            let placer = shared.placer().clone();
            if tokio::time::timeout(LISTING_BUDGET, upload_listing(end.env, placer))
                .await
                .is_err()
            {
                end.env
                    .log
                    .line("direct research listing not uploaded: out of time");
            }
        })
    }

    fn on_summary<'a>(&'a self, ctx: SummaryCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            ctx.summary.research = self.note.lock().unwrap_or_else(|e| e.into_inner()).clone();
        })
    }
}

/// Hashes what the placer placed, writes the listing to `<agent data>/context-listing.json` and uploads it to
/// `runs/<run>/agents/direct/context-listing.json` (three tries, as the clean-checkout report). Nothing here can change the run: every failure
/// is a log line.
async fn upload_listing(env: &HookEnv, placer: Arc<Placer>) {
    let text = match tokio::task::spawn_blocking(move || placer.listing()).await {
        Ok(t) => t,
        Err(_) => {
            env.log
                .line("direct research listing not written: the hashing stopped");
            return;
        }
    };
    let file = env.agent_data.join("context-listing.json");
    if let Err(e) = std::fs::write(&file, text.as_bytes()) {
        env.log
            .line(&format!("direct research listing not written: {e}"));
        return;
    }
    let creds = match env.creds.current().await {
        Ok(c) => c,
        Err(e) => {
            env.log.line(&format!(
                "direct research listing not uploaded: no credentials ({})",
                crate::direct::classify::scrub_detail(&e.to_string())
            ));
            return;
        }
    };
    let objects = S3Objects::new(env.region.clone(), env.s3_endpoint.clone());
    let uri = s3::uri(&env.bucket, &s3::context_listing_key(&env.run_id));
    let mut wait = env.cfg.direct.upload_retry_pause;
    let mut last = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(wait).await;
            wait *= 2;
        }
        match objects.put_file(&uri, &creds, &file).await {
            Ok(()) => {
                env.log.line(
                    "direct research listing uploaded: what agentd placed, hashed from the disk",
                );
                return;
            }
            Err(e) => last = e,
        }
    }
    env.log.line(&format!(
        "direct research listing not uploaded: {}",
        crate::direct::classify::scrub_detail(&last)
    ));
}

/// The counts of the note: agentd's own counters and the bytes placed so far.
pub fn counts_of(s: Stats, placed_bytes: u64) -> DirectResearchCounts {
    DirectResearchCounts {
        searches: s.searches,
        fetches: s.fetches,
        repos: s.repos,
        waits: s.waits,
        placed: s.placed,
        refused: s.refused,
        errors: s.errors,
        placed_bytes,
    }
}
