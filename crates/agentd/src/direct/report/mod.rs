//! Module `report`. After the engine has ended and the delivered head is
//! bundled, it clones that head into a fresh directory of the same microVM, runs the repository's own test commands there as the
//! unprivileged test user and uploads a `test-report.json` (`harness_protocol::direct::TestReport`) next to the run's other worker
//! objects. run-session reads it, checks it and puts it in the zip's `evidence/` and a one-sentence summary in EVIDENCE.md and the pull
//! request.
//!
//! An ordinary report result or failure never changes the label or outcome and never blocks delivery (root invariant 1: the harness
//! records how the engine stopped and never calls a run verified). The one hard failure is a create-only upload that finds
//! `test-report.json` already present: the module records one `upload-conflict` event and signals the driver to end `stopped_other`.
//! Every other failure of its own ends as a report that says `not_run` or as no report at all, the hook is bounded by the driver like
//! every module, and a run that ended by the user's stop, by its deadline or before it started makes no report.
//!
//! Hooks it uses: `on_startup` (keeps what the steps need: the test user, the package-source variables, the engine's home) and
//! `on_report` (does the work: the driver calls it once the head is committed, bundled and uploaded and `summary.json` is in the bucket,
//! while the package proxy, the container runtime and the heartbeat still run, before `turn.ended` and `turn-complete`; the driver bounds
//! it by `Limits::report`, 480 s, and the module budgets itself by the run's deadline: [`report_budget`]). The 15-second `on_finalize` hook is not used: a report
//! step takes minutes.
//!
//! Settings: `AGENTD_REPORT_TESTS` (`0`, `off`, `false`, `no`: the module is off for the stage), `AGENTD_REPORT_BUDGET_S` and
//! `AGENTD_REPORT_STEP_S`. The request's `reportTests: false` switches it off for one run, and its `reportStepS`
//! replaces the stage's step limit for one run: [`Settings::with_step_s`] lengthens the whole budget by the same
//! time and the module asks the driver for a matching ceiling ([`DirectHook::report_time`]). While it works the module writes `turn.progress`
//! events of kind `report` ([`progress`]) so the screen can say what the harness is doing and the report records the limit each command was given.

pub mod command;
pub mod parse;
pub mod plan;
pub mod progress;
pub mod run;
pub mod services;

use crate::BoxFuture;
use crate::direct::TEST_REPORT_OBJECT;
use crate::direct::classify::{
    not_replaced_text, scrub_detail, upload_conflict_payload, upload_conflict_text,
};
use crate::direct::hooks::{DirectHook, HookEnv, Report, StartupCtx};
use crate::s3::{PutNew, S3Objects};
use crate::time::{format_rfc3339, now_unix};
use harness_core::refusal::redact;
use harness_protocol::direct::{
    DEFAULT_REPORT_STEP_S, DEFAULT_REPORT_TAIL_BYTES, DirectOutcome, MAX_REPORT_BYTES,
    MAX_REPORT_STEP_S, MAX_REPORT_TAIL_BYTES, NEEDS_PREFIX, REPORT_SKIPPED_NO_TIME, TestReport,
    TestReportStatus, TestStep, TestStepRole, report_step_s_ok, s3,
};
use harness_protocol::envelope::EventType;
use harness_protocol::output_pins::OutputPin;
use plan::{Install, Plan, PlannedStep};
use progress::Progress;
use run::{RunEnv, SKIPPED_FOR_DISK, checkout, ran_out_of_disk, run_step_scanned};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// What the run's deadline keeps out of the report's budget: the report may use what is left of the deadline less this.
pub const DEADLINE_MARGIN: Duration = Duration::from_secs(180);
/// Less than this left after the margin and the report is skipped: a `not_run` report says so.
pub const MIN_BUDGET: Duration = Duration::from_secs(60);
/// The part of the overall budget kept for the upload of the report: the checkout and the steps end this much before it.
const UPLOAD_RESERVE: Duration = Duration::from_secs(15);
/// The report object is written with three tries (the first and two more), whether it is the create-only write or the write over a
/// foreign object.
const REPORT_PUT_RETRIES: u32 = 2;

/// The overall budget of one report given `time_left` before the run's deadline: the module's budget, or what is left of the deadline after
/// [`DEADLINE_MARGIN`] when that is less; `None` when what is left is under [`MIN_BUDGET`] (the report is skipped). `time_left` is the
/// clock: the driver hands it over as the run's deadline minus now.
pub fn report_budget(settings: &Settings, time_left: Duration) -> Option<Duration> {
    let remaining = time_left.saturating_sub(DEADLINE_MARGIN);
    (remaining >= MIN_BUDGET).then(|| settings.budget.min(remaining))
}

/// What the driver's ceiling for `on_report` allows beyond the module's own budget when the request set the step limit: the time the upload and
/// its two retries may take after the last command, so the driver does not cut the hook off in the middle of the upload.
pub const CEILING_GRACE: Duration = Duration::from_secs(30);

/// The ceiling the module asks the driver for (and gets while the run's deadline allows it): its budget and [`CEILING_GRACE`].
pub fn ceiling_for(settings: &Settings) -> Duration {
    settings.budget + CEILING_GRACE
}

/// The settings of the module, from `AGENTD_REPORT_*` environment variables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `AGENTD_REPORT_TESTS`: `0`, `off`, `false` or `no` switch the module off for the stage; anything else leaves it on.
    pub enabled: bool,
    /// `AGENTD_REPORT_BUDGET_S` (default 480, 1 to 27,900): the overall time of a report (checkout, steps and upload), less when the run's deadline
    /// leaves less ([`report_budget`]); the driver's ceiling for the step is 480 s, so a larger value does not lengthen it, unless the run's
    /// request sets a step limit that differs from the stage's ([`ceiling_for`]).
    pub budget: Duration,
    /// `AGENTD_REPORT_STEP_S` (default 300, 1 to 27,900): the time one step may take.
    pub step: Duration,
    /// `AGENTD_REPORT_TAIL_KIB` (default 256, 4 to 4096): the bytes of the end of a step's output that the report keeps, as bytes.
    pub tail: usize,
}

impl Settings {
    pub fn from_vars(get: &impl Fn(&str) -> Option<String>) -> Self {
        let secs = |key: &str, default: u64| {
            Duration::from_secs(
                get(key)
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .filter(|v| (1..=u64::from(MAX_REPORT_STEP_S)).contains(v))
                    .unwrap_or(default),
            )
        };
        let off = get("AGENTD_REPORT_TESTS").is_some_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "off" | "false" | "no"
            )
        });
        Self {
            enabled: !off,
            budget: secs("AGENTD_REPORT_BUDGET_S", 480),
            step: secs("AGENTD_REPORT_STEP_S", u64::from(DEFAULT_REPORT_STEP_S)),
            tail: get("AGENTD_REPORT_TAIL_KIB")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|k| (4..=MAX_REPORT_TAIL_BYTES / 1024).contains(k))
                .map_or(DEFAULT_REPORT_TAIL_BYTES, |k| k * 1024),
        }
    }

    /// The settings of one run whose request carries `step_s` (`StartDirectRunRequest.reportStepS`, protocol 1.41.0): the step limit becomes
    /// `step_s`, and a limit longer than the stage's lengthens the whole budget by the same time, so the one long command has room to run
    /// (the headroom the stage keeps over its own step limit, 180 s with the defaults, stays). A shorter limit keeps the stage's budget. A value
    /// outside `1..=MAX_REPORT_STEP_S`, or none, leaves the settings as they are. The deadline rule ([`report_budget`]) still bounds the result.
    pub fn with_step_s(self, step_s: Option<u32>) -> Self {
        let Some(s) = step_s.filter(|s| report_step_s_ok(*s)) else {
            return self;
        };
        let step = Duration::from_secs(u64::from(s));
        let headroom = self.budget.saturating_sub(self.step);
        Self {
            step,
            budget: self.budget.max(step + headroom),
            ..self
        }
    }
}

/// What `on_startup` keeps for the work at the end.
#[derive(Clone, Debug)]
struct Startup {
    report_tests: bool,
    /// The stage's settings with the step limit of the request applied ([`Settings::with_step_s`]).
    settings: Settings,
    engine_uid: Option<(u32, u32)>,
    /// The engine's Landlock sandbox; the report's steps run in one of their own folder ([`Sandbox::rebased`]).
    sandbox: Option<crate::sandbox::Sandbox>,
    engine_env: BTreeMap<String, String>,
    engine_home: PathBuf,
    /// The cargo config the package proxy gave the run (`StartupCtx::cargo_config`).
    cargo_config: Option<String>,
}

/// The module.
#[derive(Debug)]
pub struct ReportHook {
    settings: Settings,
    startup: Mutex<Option<Startup>>,
    /// What the module asks the driver for in `on_report`: set at startup, and only when the request carried a valid step limit, so a stage that
    /// sets `AGENTD_REPORT_BUDGET_S` above the driver's 480 s sees no change.
    ceiling: Mutex<Option<Duration>>,
}

impl ReportHook {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            startup: Mutex::new(None),
            ceiling: Mutex::new(None),
        }
    }
}

/// The module, or `None` while it is switched off for the stage. `Hooks::standard` calls it once per run.
pub fn hook(_env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    let settings = Settings::from_vars(&|k| std::env::var(k).ok());
    settings
        .enabled
        .then(|| Arc::new(ReportHook::new(settings)) as Arc<dyn DirectHook>)
}

fn skips(outcome: DirectOutcome) -> bool {
    matches!(
        outcome,
        DirectOutcome::StoppedSetupFailed
            | DirectOutcome::StoppedByUser
            | DirectOutcome::StoppedTimeout
    )
}

impl DirectHook for ReportHook {
    fn name(&self) -> &'static str {
        "report"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        let step_s = ctx.request.report_step_s;
        if step_s.is_some_and(|s| !report_step_s_ok(s)) {
            ctx.env.log.line(&format!(
                "direct report: reportStepS must be 1 to {MAX_REPORT_STEP_S}; the stage's limit is used"
            ));
        }
        let settings = self.settings.with_step_s(step_s);
        if let Ok(mut slot) = self.ceiling.lock() {
            *slot = (settings != self.settings).then(|| ceiling_for(&settings));
        }
        let kept = Startup {
            report_tests: ctx.request.report_tests != Some(false),
            settings,
            engine_uid: ctx.engine_uid,
            sandbox: ctx.sandbox.clone(),
            engine_env: ctx
                .engine_env
                .iter()
                .filter(|(k, _)| run::passes_to_steps(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            engine_home: ctx.home.to_path_buf(),
            cargo_config: ctx.cargo_config.map(str::to_string),
        };
        if let Ok(mut slot) = self.startup.lock() {
            *slot = Some(kept);
        }
        Box::pin(async { Ok(()) })
    }

    fn report_time(&self) -> Option<Duration> {
        self.ceiling.lock().ok().and_then(|c| *c)
    }

    fn on_report<'a>(&'a self, r: &'a Report<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let kept = self.startup.lock().ok().and_then(|mut s| s.take());
            let Some(kept) = kept else {
                return;
            };
            if !self.settings.enabled || !kept.report_tests || skips(r.classified.outcome) {
                return;
            }
            let env = r.env;
            let Some(budget) = report_budget(&kept.settings, r.time_left) else {
                // Too little time before the run's deadline: a report that says so, and nothing runs.
                let mut skipped = TestReport::without_steps(
                    &env.run_id,
                    r.head_sha,
                    TestReportStatus::NotRun,
                    Some(REPORT_SKIPPED_NO_TIME.to_string()),
                );
                skipped.started_at = Some(format_rfc3339(now_unix()));
                let up = upload(env, skipped, r.head_sha).await;
                if let Some(pin) = up.pin {
                    r.stored(pin);
                }
                if let Some(object) = up.conflict {
                    r.upload_conflict(object);
                }
                return;
            };
            // The overall budget covers the checkout, the steps and the upload: the checkout and the steps end `UPLOAD_RESERVE` (or a
            // quarter of a short budget) before it. Each step is bounded by what is left of that, and by the step limit.
            let reserve = UPLOAD_RESERVE.min(budget / 4);
            let steps_settings = Settings {
                budget: budget.saturating_sub(reserve),
                ..kept.settings
            };
            let progress = Progress::new(env.rec.clone());
            progress.checkout();
            let report = make_report(
                &steps_settings,
                &kept,
                &env.run_id,
                r.head_sha,
                r.bundle,
                &env.agent_data,
                &env.cfg.engine_path,
                &progress,
            )
            .await;
            progress.ended(report.status);
            let up = upload(env, report, r.head_sha).await;
            if let Some(pin) = up.pin {
                r.stored(pin);
            }
            if let Some(object) = up.conflict {
                r.upload_conflict(object);
            }
        })
    }
}

/// The report of the delivered head: checkout, plan, steps. Every failure becomes a report that says why.
#[allow(clippy::too_many_arguments)]
async fn make_report(
    settings: &Settings,
    kept: &Startup,
    run_id: &str,
    head: &str,
    bundle: &Path,
    agent_data: &Path,
    path: &[PathBuf],
    progress: &Progress,
) -> TestReport {
    let started = Instant::now();
    let started_at = format_rfc3339(now_unix());
    let not_run = |why: String| {
        let mut r = TestReport::without_steps(
            run_id,
            head,
            TestReportStatus::NotRun,
            Some(scrub_detail(&why)),
        );
        r.started_at = Some(started_at.clone());
        r
    };
    let work = agent_data.join(format!("report-{}", crate::time::new_ulid()));
    let checkout_dir = work.join("checkout");
    if let Err(e) = std::fs::create_dir_all(&work) {
        return not_run(format!("the report directory could not be made: {e}"));
    }
    if let Err(e) = checkout(bundle, head, &checkout_dir).await {
        return not_run(format!("the clean checkout failed: {e}"));
    }
    if let Some(owner) = kept.engine_uid {
        // The test user owns the checkout it runs in; the report directory itself stays root's.
        crate::engine::chown_tree(&[&checkout_dir], owner).await;
    }
    let steps = match plan::plan(&checkout_dir) {
        Plan::Steps(s) => s,
        Plan::NoCommand(why) => {
            let mut r =
                TestReport::without_steps(run_id, head, TestReportStatus::NoCommand, Some(why));
            r.started_at = Some(started_at);
            r.duration_ms = elapsed_ms(started);
            return r;
        }
    };
    let mut env = RunEnv {
        work: work.clone(),
        run_as: kept.engine_uid,
        sandbox: kept.sandbox.as_ref().map(|s| s.rebased(&[&work])),
        path: path.to_vec(),
        engine_env: kept.engine_env.clone(),
        engine_home: Some(kept.engine_home.clone()),
        cargo_config: kept.cargo_config.clone(),
    };
    // The services the repository declares and the report has a local server for are started before the steps and stopped after the last one,
    // whatever the steps did.
    let declared = services::declared(&checkout_dir);
    let mut service_steps: Vec<TestStep> = Vec::new();
    let mut live: Vec<services::Live> = Vec::new();
    if !declared.is_empty() {
        let s = services::start(
            &env,
            &checkout_dir,
            &declared,
            started,
            settings.budget,
            settings.tail,
        )
        .await;
        for (k, v) in &s.vars {
            env.engine_env.insert(k.clone(), v.clone());
        }
        service_steps = s.steps;
        live = s.live;
    }
    let secrets = services::secrets(&env.engine_env);
    for step in &mut service_steps {
        services::scrub(step, &secrets);
    }
    let (mut done, status, reason) = run_steps(
        &env,
        &steps,
        &checkout_dir,
        settings,
        started,
        progress,
        &live,
        &secrets,
    )
    .await;
    if !declared.is_empty() {
        services::stop(&env, &checkout_dir).await;
        service_steps.append(&mut done);
        done = service_steps;
    }
    TestReport {
        schema_version: harness_protocol::direct::TEST_REPORT_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        head_sha: head.to_string(),
        status,
        reason,
        started_at: Some(started_at),
        duration_ms: elapsed_ms(started),
        steps: done,
        summary: None,
    }
}

/// Puts a `NEEDS:` line at the head of the output of a test step that ran, exited with a code other than 0 and said a connection to the port of a
/// declared service was refused while that service was not reachable: the server could not be started, or it no longer answered after the step. The
/// step keeps its exit code, its counts and its output. A step that passed, one that was stopped at its limit and one that was not run
/// get none, and a service that answers gets none whatever the output says: a test that fails with the server up is a failed test.
pub fn note_needs(
    step: &mut TestStep,
    role: TestStepRole,
    refused: &[u16],
    live: &[services::Live],
    up: &dyn Fn(u16) -> bool,
    tail: usize,
) {
    if role != TestStepRole::Test
        || step.is_not_run()
        || step.timed_out
        || step.exit_code.is_none_or(|c| c == 0)
    {
        return;
    }
    let lines: Vec<String> = live
        .iter()
        .filter(|l| refused.contains(&l.kind.port()))
        .filter_map(|l| {
            l.unavailable_with(up)
                .map(|why| format!("{NEEDS_PREFIX}{}: {why}", l.kind.name()))
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    // Written one after the other at the head, the last first: each insert goes in front of the earlier lines of its kind.
    services::put_head_line(step, NEEDS_PREFIX, &lines.join("\n"), tail);
}

fn elapsed_ms(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Runs the planned steps one after the other within the shared budget. A setup step that fails ends the run of steps, and the steps behind it
/// are listed as not run with the setup step that stopped them, so the report names the step and counts no test as failed for want of
/// it; a step that hits the time limit ends the run of steps too. An install the report added ([`Install`]) stops only the test steps that
/// need it: each is listed as not run with that install named, or, when it has the command the log wrote, runs that command instead.
#[allow(clippy::too_many_arguments)]
pub async fn run_steps(
    env: &RunEnv,
    steps: &[PlannedStep],
    cwd: &Path,
    settings: &Settings,
    started: Instant,
    progress: &Progress,
    live: &[services::Live],
    secrets: &[String],
) -> (Vec<TestStep>, TestReportStatus, Option<String>) {
    let mut done: Vec<TestStep> = Vec::new();
    // A `cargo test` the harness chose from the manifest of a workspace carries a line that says so and how many members it runs.
    let members = plan::detected_workspace_members(steps, cwd);
    // The installs of the report that failed, with the reason each gives the steps that need it.
    let mut failed_installs: Vec<(Install, String)> = Vec::new();
    for (i, planned) in steps.iter().enumerate() {
        let left = settings.budget.saturating_sub(started.elapsed());
        if left.is_zero() {
            return (
                done,
                TestReportStatus::TimedOut,
                Some("the report time limit ended before every command ran".into()),
            );
        }
        // A test step whose install failed runs the command the log wrote when it has one, else it is listed as not run.
        let as_written: PlannedStep;
        let failed = planned
            .install
            .and_then(|k| failed_installs.iter().find(|(f, _)| *f == k));
        let planned = match (failed, planned.role) {
            (Some((_, why)), TestStepRole::Test) => match &planned.fallback {
                Some(command) => {
                    as_written = PlannedStep {
                        command: command.clone(),
                        install: None,
                        fallback: None,
                        ..planned.clone()
                    };
                    &as_written
                }
                None => {
                    done.push(TestStep::not_run(
                        planned.role,
                        planned.source,
                        &redact(&planned.command),
                        why,
                        planned.logged,
                    ));
                    continue;
                }
            },
            _ => planned,
        };
        let limit = settings.step.min(left);
        // The screen hears which command starts and with how long to run; the report records the same limit on the command.
        let limit_s = progress.running(i + 1, steps.len(), &planned.command, limit);
        let (mut step, refused) = run_step_scanned(env, planned, cwd, limit, settings.tail).await;
        run::set_plan_note(&mut step, members, settings.tail);
        services::scrub(&mut step, secrets);
        note_needs(
            &mut step,
            planned.role,
            &refused,
            live,
            &services::reachable,
            settings.tail,
        );
        if !step.is_not_run() {
            step.limit_s = Some(limit_s);
        }
        let timed_out = step.timed_out;
        // A setup that was not run (a missing service) did not fail; one whose command could not be read stops the steps like a failed one.
        let failed_setup = run::setup_stops_the_run(&step).then(|| run::skipped_for_setup(&step));
        let disk_full = ran_out_of_disk(&step);
        done.push(step);
        if timed_out {
            return (
                done,
                TestReportStatus::TimedOut,
                Some("a command was stopped at the report time limit".into()),
            );
        }
        if disk_full {
            // The volume is still full, so what the later commands would print says nothing about the repository: each is listed as not run.
            done.extend(steps[i + 1..].iter().map(|later| {
                TestStep::not_run(
                    later.role,
                    later.source,
                    &redact(&later.command),
                    SKIPPED_FOR_DISK,
                    later.logged,
                )
            }));
            break;
        }
        if let Some(why) = failed_setup {
            if let (TestStepRole::Setup, Some(install)) = (planned.role, planned.install) {
                // An install the report added: only the steps that need it are left out, as they come.
                failed_installs.push((install, why));
                continue;
            }
            // The tests need what the setup did not give them, so none is started: a test that failed for want of an install is no result of
            // the repository's tests.
            done.extend(steps[i + 1..].iter().map(|later| {
                TestStep::not_run(
                    later.role,
                    later.source,
                    &redact(&later.command),
                    &why,
                    later.logged,
                )
            }));
            break;
        }
    }
    (done, TestReportStatus::Ran, None)
}

/// Checks the report, writes it under the run's data directory and uploads it. Ordinary failures are log lines. A refused create-only
/// write returns the end-object name after recording its single conflict event, so the driver makes it the hard final classification.
async fn upload(env: &HookEnv, report: TestReport, head: &str) -> Uploaded {
    let mut pin = None;
    let conflict = upload_into(env, report, head, &mut pin).await;
    Uploaded { conflict, pin }
}

/// What [`upload`] did: the conflict it signals to the driver, and the record of the object agentd stored.
struct Uploaded {
    conflict: Option<&'static str>,
    pin: Option<OutputPin>,
}

async fn upload_into(
    env: &HookEnv,
    report: TestReport,
    head: &str,
    stored: &mut Option<OutputPin>,
) -> Option<&'static str> {
    let mut report = report.bounded();
    report.reason = report.reason.take().map(|r| redact(&r));
    if let Err(why) = report.validate(&env.run_id, head) {
        env.log.line(&format!(
            "direct report not uploaded: it breaks its own bounds ({why})"
        ));
        return None;
    }
    let body = match serde_json::to_vec_pretty(&report) {
        Ok(b) if b.len() <= MAX_REPORT_BYTES => b,
        _ => {
            env.log.line("direct report not uploaded: it is too large");
            return None;
        }
    };
    let file = env.agent_data.join("test-report.json");
    if let Err(e) = std::fs::write(&file, &body) {
        env.log.line(&format!("direct report not written: {e}"));
        return None;
    }
    let creds = match env.creds.current().await {
        Ok(c) => c,
        Err(e) => {
            env.log.line(&format!(
                "direct report not uploaded: no credentials ({})",
                scrub_detail(&e.to_string())
            ));
            return None;
        }
    };
    let objects = S3Objects::new(env.region.clone(), env.s3_endpoint.clone());
    let uri = s3::uri(&env.bucket, &s3::test_report_key_t(&env.run_id, env.turn));
    let pause = env.cfg.direct.upload_retry_pause;
    // Create-only (`If-None-Match: *`), three tries. A 412 after a try whose answer was lost is checked against the checksum agentd sent:
    // agentd's own report counts as stored, another object is a conflict (`S3Objects::put_file_new_retrying`).
    match objects
        .put_file_new_retrying_receipt(&uri, &creds, &file, REPORT_PUT_RETRIES, pause)
        .await
    {
        Ok((PutNew::Stored, pin)) => {
            *stored = pin;
            env.log.line(&format!(
                "direct report uploaded: status={:?} steps={}",
                report.status,
                report.steps.len()
            ));
            None
        }
        Ok((PutNew::Exists, _)) => {
            env.log.line(&format!(
                "direct report not uploaded: {}",
                upload_conflict_text(TEST_REPORT_OBJECT)
            ));
            env.rec.record(
                EventType::TurnProgress,
                upload_conflict_payload(TEST_REPORT_OBJECT),
            );
            // The conflict is recorded; agentd's own report now replaces the foreign object (the versioned bucket keeps it as an
            // earlier version), so the report run-session reads is the one agentd made.
            match objects
                .put_file_retrying_receipt(&uri, &creds, &file, REPORT_PUT_RETRIES, pause)
                .await
            {
                Ok(pin) => {
                    *stored = Some(pin);
                    env.log.line(&format!(
                        "direct report replaced: agentd's own report was written over the object that was there: status={:?} steps={}",
                        report.status,
                        report.steps.len()
                    ));
                }
                Err(e) => env.log.line(&format!(
                    "direct report not replaced: {}",
                    not_replaced_text(&e)
                )),
            }
            Some(TEST_REPORT_OBJECT)
        }
        Err(e) => {
            env.log
                .line(&format!("direct report not uploaded: {}", scrub_detail(&e)));
            None
        }
    }
}
