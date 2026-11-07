//! Module `skills`. A run may carry skills:
//! folders of files the engine loads when the model needs them, which keeps the always-on prompt small. `ham` sends them in the `skills`
//! member of the start request (`harness_protocol::direct::Skill`, one validator for the three ends); this module puts them where the engine
//! reads them and proves, before any model call, that it will.
//!
//! What `on_startup` does, in order (an `Err` ends the run `stopped_setup_failed`, "did not start", before any model call):
//!
//! 1. The engine's HOME holds no skills folder ([`home::clear_home_skills`]); the session directory is the repository's git top level
//!    ([`cwd::top_level`]), where the engine reads the repository's own instruction files. Both happen in every run.
//! 2. A request with no skills stops here: nothing is placed, no listing runs, the run is as it was.
//! 3. The request's skills pass the shared validator (`check_skills`), and each SKILL.md reads the way the engine reads it
//!    ([`place::skill_document`]).
//! 4. What an earlier attempt of this run placed (folders that hold the marker file) is retired ([`place::retire_placed`]); then the engine's
//!    own listing, taken before anything is placed, shows the repository's skills and those of other modules in the skills directory: a
//!    repository skill, or another module's, with the name of a requested one stops the run, because the engine would take the requested one
//!    silently. A skill from anywhere
//!    else outside the repository stops it too.
//! 5. The skills are written under `<config dir>/skills/<name>/...`, root-owned and read-only, outside the repository, each folder with its
//!    marker file ([`place::place_skills`]); whatever else is in that directory is left as it is.
//! 6. The listing again: every placed skill is listed once, at its own file, reading the way the file reads
//!    ([`preflight::check_placed_listing_in`]).
//! 7. One `turn.progress` event, kind `skills`, names each skill with its digest and file count ([`skills_event`]); never its text.
//!
//! The module can be switched off with `AGENTD_SKILLS_ENABLED=0`; a request that carries skills is then refused, never ignored silently.
//! `AGENTD_SKILLS_LISTING_TIMEOUT_S` bounds one listing (default 20 s, at most 25 s: two listings run inside the 60 s of the startup hook).
//!
//! `summary.json` lists the skills the session was given (`DirectSummary.skills`: name, digest, file count and bytes, never the text) through
//! the `on_summary` hook; the same facts are in the event of step 7.

pub mod cwd;
pub mod home;
pub mod place;
pub mod preflight;

use crate::BoxFuture;
use crate::direct::hooks::{DirectHook, HookEnv, StartupCtx, SummaryCtx};
use crate::direct::request::check_skills_member;
use crate::direct::skills::place::PlacedSkill;
use crate::engine::direct::{skills_listing_argv, user_skills_dir};
use harness_protocol::direct::{DIRECT_TURN, DirectSkillRef};
use harness_protocol::envelope::{EventType, MAX_INLINE_PAYLOAD_BYTES};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The default bound of one listing, in seconds.
const DEFAULT_LISTING_S: u64 = 20;
/// The largest bound of one listing: two listings and the placement have to fit in the startup hook's 60 s.
const MAX_LISTING_S: u64 = 25;

/// The settings of the module, from `AGENTD_SKILLS_*` environment variables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `AGENTD_SKILLS_ENABLED`: on unless it reads `0`, `false`, `off` or `no` (any case).
    pub enabled: bool,
    /// `AGENTD_SKILLS_LISTING_TIMEOUT_S`: one listing may take this long. 1 to 25; anything else is 20.
    pub listing_timeout: Duration,
}

impl Settings {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let enabled = get("AGENTD_SKILLS_ENABLED").is_none_or(|v| {
            !matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            )
        });
        let secs = get("AGENTD_SKILLS_LISTING_TIMEOUT_S")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map_or(DEFAULT_LISTING_S, |s| s.min(MAX_LISTING_S));
        Self {
            enabled,
            listing_timeout: Duration::from_secs(secs),
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// What `summary.json` lists for the placed skills: name, digest, file count and total bytes. Never content.
pub fn skill_refs(placed: &[PlacedSkill]) -> Vec<DirectSkillRef> {
    placed
        .iter()
        .map(|p| DirectSkillRef {
            name: p.name.clone(),
            sha256: p.digest.clone(),
            files: u32::try_from(p.files.len()).unwrap_or(u32::MAX),
            bytes: p.files.iter().map(|f| f.bytes).sum(),
        })
        .collect()
}

/// The `turn.progress` event of kind `skills`: each placed skill with its digest and file count, `total` (how many skills were placed),
/// and whether (and how long) the preflight ran. It holds no instruction text and no file content.
///
/// An inline event is at most [`MAX_INLINE_PAYLOAD_BYTES`] and a run may carry 32 skills, whose entries do not all fit when their
/// names are long, so the event lists the first skills that fit and `total` says how many there are; the card on the run screen counts the total
/// and says "and N more".
pub fn skills_event(placed: &[PlacedSkill], preflight_ms: Option<u64>) -> Value {
    let rows: Vec<Value> = placed
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "digest": p.digest,
                "files": p.files.len(),
                "bytes": p.files.iter().map(|f| f.bytes).sum::<u64>(),
            })
        })
        .collect();
    let build = |listed: usize| {
        json!({
            "turn": DIRECT_TURN,
            "kind": "skills",
            "skills": &rows[..listed],
            "total": rows.len(),
            "preflight": { "ran": preflight_ms.is_some(), "ms": preflight_ms },
        })
    };
    let mut listed = rows.len();
    let mut event = build(listed);
    while listed > 0 && serde_json::to_vec(&event).map_or(0, |b| b.len()) > MAX_INLINE_PAYLOAD_BYTES
    {
        listed -= 1;
        event = build(listed);
    }
    event
}

/// The module.
#[derive(Debug)]
pub struct Skills {
    settings: Settings,
    refs: Mutex<Vec<DirectSkillRef>>,
}

impl Skills {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            refs: Mutex::new(Vec::new()),
        }
    }

    /// The skills this run was given, as `summary.json` lists them; empty until `on_startup` placed them.
    pub fn refs(&self) -> Vec<DirectSkillRef> {
        self.refs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn startup(&self, ctx: &mut StartupCtx<'_>) -> Result<(), String> {
        let skills = &ctx.request.skills;
        if !self.settings.enabled {
            return if skills.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "skills: the run carries {} skill(s) and the skills module is switched off on this agentd (AGENTD_SKILLS_ENABLED)",
                    skills.len()
                ))
            };
        }
        // 1. HOME and the session directory, in every run.
        home::clear_home_skills(ctx.home)?;
        // A module that ran before this one and named another session directory keeps its choice.
        if ctx.session_cwd.is_none() {
            *ctx.session_cwd = Some(cwd::top_level(ctx.repo).await?);
        }
        // 2. no skills, nothing more
        if skills.is_empty() {
            return Ok(());
        }
        // 3. the shared validator
        check_skills_member(skills)?;
        let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();

        // 4. what an earlier attempt placed goes first, then the listing before anything is placed
        let skills_dir = user_skills_dir(ctx.config_dir);
        place::retire_placed(&skills_dir)?;
        let program = ctx.engine_argv().first().cloned().unwrap_or_default();
        let listing = skills_listing_argv(&ctx.env.engine, &program, ctx.config_dir)
            .map(|argv| ctx.launch_spec(argv, "skills-listing-stderr.log"));
        let limit = self.settings.listing_timeout;
        let mut preflight_ms: Option<u64> = None;
        if let Some(spec) = &listing {
            let t = Instant::now();
            let rows = preflight::run_skills_listing(spec, limit).await?;
            preflight::check_repo_skills_in(&rows, ctx.repo, &names, Some(&skills_dir))?;
            preflight_ms = Some(u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
        }

        // 5. place
        let placed = place::place_skills(&skills_dir, skills)?;

        // 6. the listing again
        if let Some(spec) = &listing {
            let t = Instant::now();
            let rows = preflight::run_skills_listing(spec, limit).await?;
            preflight::check_placed_listing_in(&rows, &placed, ctx.repo, Some(&skills_dir))?;
            let again = u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX);
            preflight_ms = Some(preflight_ms.unwrap_or(0).saturating_add(again));
        }

        // 7. the event, and the refs for summary.json
        ctx.env
            .rec
            .record(EventType::TurnProgress, skills_event(&placed, preflight_ms));
        *self
            .refs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = skill_refs(&placed);
        Ok(())
    }
}

impl DirectHook for Skills {
    fn name(&self) -> &'static str {
        "skills"
    }

    fn on_startup<'a, 'b: 'a>(
        &'a self,
        ctx: &'a mut StartupCtx<'b>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.startup(ctx))
    }

    fn on_summary<'a>(&'a self, ctx: SummaryCtx<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            ctx.summary.skills = self.refs();
        })
    }
}

/// The module. `Hooks::standard` calls it once per run.
pub fn hook(_env: &HookEnv) -> Option<Arc<dyn DirectHook>> {
    Some(Arc::new(Skills::new(Settings::from_env())))
}
