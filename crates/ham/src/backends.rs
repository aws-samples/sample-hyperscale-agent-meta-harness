//! Several backends behind one [`ControlPlane`]. Each backend is one full stage in one AWS account with its own profile; the client
//! asks them itself, no service stands in between and no backend learns about another.
//!
//! - [`Backends::connect`] builds the planes. With ONE backend a failure ends the start as it always did, every call is forwarded as it came
//!   and the set has no view, so no screen shows a banner, a column or a count. With two or more, a backend that cannot be built is a `Down`
//!   entry the others do not wait for.
//! - A call carries its target: [`ControlPlane::call_on`] with an alias goes to that backend and to no other. Without one, a call about a
//!   run goes to the backend the run map names; an id the map does not know is asked of every backend at once ([`ham_core::backends::decide`]),
//!   and "no such run" is said only when every backend answered; a start without a target goes to `default_backend` or is refused.
//! - Whatever answers teaches the run map (the list, a start, a retry, a resume), and what a backend answered last is its health: the banners
//!   read it. A login that lapsed pauses the timer reads of that backend; [`ControlPlane::retry_down`] (the manual refresh) lifts the pause.
//! - With two or more backends a call that changes something is refused for a backend below the minimum version, one that reports no version
//!   at all, or one of an unknown major. A start that continues a run goes to the backend that holds that run.
//! - The read and the switch of an account's Bedrock data retention mode name their backend ([`ControlPlane::start_backend`] says which one a
//!   start goes to); with several backends a read or a switch that names none is refused, so no account is picked by its place in the file.
//!
//! Nothing here is written to disk: the map, the health and the versions live in memory (root invariant 5).

use crate::api::Api;
use crate::http::{ControlPlane, Response, TransportError};
use crate::version::BackendCell;
use ham_core::backends::{
    Answer, BackendDef, BackendInfo, BackendView, Decision, Health, Local, Lookup, Probe, Route,
    RunMap, Support, changes_something, decide, judge_answer, learns_from, route_of, support,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Why a backend's plane could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildFailure {
    /// The words for a person: for a lone backend they are what the client has always printed.
    pub text: String,
    /// True when the failure is about credentials (no profile, no login): the backend is `SignedOut`, not `Down`.
    pub credentials: bool,
}

pub type BoxBuild<P> = Pin<Box<dyn Future<Output = Result<P, BuildFailure>> + Send>>;

/// How a plane is made from the config of one backend (the live one signs with the backend's profile).
pub trait PlaneFactory<P>: Send + Sync + 'static {
    fn build(&self, def: BackendDef) -> BoxBuild<P>;
}

impl<P, F> PlaneFactory<P> for F
where
    F: Fn(BackendDef) -> BoxBuild<P> + Send + Sync + 'static,
{
    fn build(&self, def: BackendDef) -> BoxBuild<P> {
        self(def)
    }
}

/// How long a backend that failed is left alone by the timer reads and by calls that name no backend.
pub const RETRY_AFTER: Duration = Duration::from_secs(60);

/// How long one backend has to say whether it holds a run.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(5);

struct Entry<P> {
    def: BackendDef,
    plane: Option<Arc<P>>,
    health: Health,
    cell: BackendCell,
    stale: bool,
    failed_at: Option<Instant>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The backends of one client and the routing between them.
pub struct Backends<P: ControlPlane> {
    entries: Arc<Mutex<Vec<Entry<P>>>>,
    default_alias: Option<String>,
    map: Mutex<RunMap>,
    factory: Arc<dyn PlaneFactory<P>>,
    client: String,
    min: String,
}

impl<P: ControlPlane> Backends<P> {
    /// Builds the planes of `defs` (all at once). One backend that cannot be built is an error with the text of before; with several, the
    /// failures are `Down` or `SignedOut` entries.
    pub async fn connect(
        defs: Vec<BackendDef>,
        default_alias: Option<String>,
        factory: impl PlaneFactory<P>,
    ) -> Result<Self, String> {
        let factory: Arc<dyn PlaneFactory<P>> = Arc::new(factory);
        let mut set = tokio::task::JoinSet::new();
        for (i, def) in defs.iter().cloned().enumerate() {
            let built = factory.build(def);
            set.spawn(async move { (i, built.await) });
        }
        let mut results: Vec<Option<Result<P, BuildFailure>>> =
            (0..defs.len()).map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            let (i, r) = joined.map_err(|e| format!("building a backend: {e}"))?;
            results[i] = Some(r);
        }
        let single = defs.len() == 1;
        let mut entries = Vec::with_capacity(defs.len());
        for (def, r) in defs.into_iter().zip(results) {
            let (plane, health, failed_at) = match r {
                Some(Ok(p)) => (Some(Arc::new(p)), Health::Up, None),
                Some(Err(f)) if single => return Err(f.text),
                Some(Err(f)) => (
                    None,
                    if f.credentials {
                        Health::SignedOut { fix: f.text }
                    } else {
                        Health::Down(f.text)
                    },
                    Some(Instant::now()),
                ),
                None => return Err("a backend was not built".to_string()),
            };
            entries.push(Entry {
                def,
                plane,
                health,
                cell: BackendCell::new(),
                stale: false,
                failed_at,
            });
        }
        Ok(Self {
            entries: Arc::new(Mutex::new(entries)),
            default_alias,
            map: Mutex::new(RunMap::default()),
            factory,
            client: crate::version::CLIENT.to_string(),
            min: ham_core::backends::MIN_BACKEND.to_string(),
        })
    }

    fn multi(&self) -> bool {
        lock(&self.entries).len() >= 2
    }

    /// The aliases in config order.
    pub fn aliases(&self) -> Vec<String> {
        lock(&self.entries)
            .iter()
            .map(|e| e.def.alias.clone())
            .collect()
    }

    /// The config of one backend.
    pub fn def_of(&self, alias: &str) -> Option<BackendDef> {
        lock(&self.entries)
            .iter()
            .find(|e| e.def.alias == alias)
            .map(|e| e.def.clone())
    }

    /// The alias a command that names a backend (or none) is aimed at: `None` for a client with one backend, which needs no target. With
    /// several, the named alias, else `default_backend`; with neither the start is refused by [`Local::NoBackendChosen`].
    pub fn select(&self, named: Option<&str>) -> Result<Option<String>, Local> {
        let aliases = self.aliases();
        match named {
            Some(a) if aliases.iter().any(|x| x == a) => {
                Ok((aliases.len() >= 2).then(|| a.to_string()))
            }
            Some(a) => Err(Local::UnknownAlias {
                alias: a.to_string(),
                aliases,
            }),
            None if aliases.len() < 2 => Ok(None),
            None => match &self.default_alias {
                Some(d) if aliases.iter().any(|x| x == d) => Ok(Some(d.clone())),
                _ => Err(Local::NoBackendChosen { aliases }),
            },
        }
    }

    /// The reader of the title's version: the one version the backends agree on, `mixed` when they differ. The client installs it once, for
    /// a client with several backends.
    pub fn install_title_version(&self) {
        if !self.multi() {
            return;
        }
        let entries = Arc::clone(&self.entries);
        crate::version::set_backend_override(Some(Arc::new(move || {
            let infos: Vec<BackendInfo> = lock(&entries)
                .iter()
                .map(|e| BackendInfo {
                    alias: e.def.alias.clone(),
                    health: e.health.clone(),
                    version: e.cell.backend(),
                    stale: e.stale,
                    customer_content: e.def.customer_content,
                    profile: e.def.aws_profile.clone(),
                })
                .collect();
            match ham_core::backends::title_version(&infos) {
                ham_core::backends::TitleVersion::One(v) => Some(v),
                ham_core::backends::TitleVersion::Mixed => {
                    Some(ham_core::backends::MIXED_WORD.to_string())
                }
                ham_core::backends::TitleVersion::Unknown => None,
            }
        })));
    }

    fn default_target(&self) -> Option<String> {
        let entries = lock(&self.entries);
        if let Some(d) = &self.default_alias
            && entries.iter().any(|e| &e.def.alias == d)
        {
            return Some(d.clone());
        }
        entries
            .iter()
            .find(|e| e.plane.is_some())
            .or_else(|| entries.first())
            .map(|e| e.def.alias.clone())
    }

    /// The plane of `alias`, built again when it could not be built and the pause is over.
    async fn plane_of(&self, alias: &str) -> Result<Arc<P>, TransportError> {
        let (def, rebuild) = {
            let entries = lock(&self.entries);
            let Some(e) = entries.iter().find(|e| e.def.alias == alias) else {
                return Err(TransportError::Local(Local::UnknownAlias {
                    alias: alias.to_string(),
                    aliases: entries.iter().map(|e| e.def.alias.clone()).collect(),
                }));
            };
            if let Some(p) = &e.plane {
                return Ok(Arc::clone(p));
            }
            let paused = e.failed_at.is_some_and(|t| t.elapsed() < RETRY_AFTER);
            if paused {
                let reason = match &e.health {
                    Health::Down(r) | Health::Refused(r) => r.clone(),
                    Health::SignedOut { fix } => fix.clone(),
                    Health::Slow | Health::Up => "not built".to_string(),
                };
                return Err(TransportError::Local(Local::BackendDown {
                    alias: alias.to_string(),
                    reason,
                }));
            }
            (e.def.clone(), true)
        };
        debug_assert!(rebuild);
        let built = self.factory.build(def).await;
        let mut entries = lock(&self.entries);
        let Some(e) = entries.iter_mut().find(|e| e.def.alias == alias) else {
            return Err(TransportError::Local(Local::UnknownAlias {
                alias: alias.to_string(),
                aliases: Vec::new(),
            }));
        };
        match built {
            Ok(p) => {
                let p = Arc::new(p);
                e.plane = Some(Arc::clone(&p));
                e.health = Health::Up;
                e.failed_at = None;
                Ok(p)
            }
            Err(f) => {
                e.health = if f.credentials {
                    Health::SignedOut {
                        fix: f.text.clone(),
                    }
                } else {
                    Health::Down(f.text.clone())
                };
                e.failed_at = Some(Instant::now());
                Err(TransportError::Local(Local::BackendDown {
                    alias: alias.to_string(),
                    reason: f.text,
                }))
            }
        }
    }

    /// Records what an answer says about the backend: its version, whether it is up, whether its login lapsed.
    fn note(&self, alias: &str, result: &Result<Response, TransportError>) {
        let mut entries = lock(&self.entries);
        let Some(e) = entries.iter_mut().find(|e| e.def.alias == alias) else {
            return;
        };
        match result {
            Ok(r) => {
                e.cell.note_answer(r.status, &r.headers);
                // The last answer is the health: a failure of the gateway or of the service shows as a banner, a plain answer clears it.
                let judged =
                    judge_answer(r.status, crate::version::carries_tag(&r.headers), &r.body);
                e.failed_at = Some(Instant::now());
                match judged {
                    Answer::Fine => {
                        e.health = Health::Up;
                        e.failed_at = None;
                    }
                    Answer::SignInLapsed => {
                        e.health = Health::SignedOut {
                            fix: sign_in_words(e.def.aws_profile.as_deref()),
                        };
                    }
                    Answer::Slow => e.health = Health::Slow,
                    Answer::Refused(why) => e.health = Health::Refused(why),
                    Answer::Failed(why) => e.health = Health::Down(why),
                }
            }
            Err(TransportError::Credentials(m) | TransportError::Sign(m)) => {
                e.health = Health::SignedOut { fix: m.clone() };
                e.failed_at = Some(Instant::now());
            }
            Err(TransportError::Timeout(_)) => {
                e.health = Health::Slow;
                e.failed_at = Some(Instant::now());
            }
            Err(TransportError::Local(_)) => {}
            Err(other) => {
                e.health = Health::Down(other.to_string());
                e.failed_at = Some(Instant::now());
            }
        }
    }

    fn learn_from(&self, alias: &str, route: Route<'_>, method: &str, r: &Response) {
        if !(200..300).contains(&r.status) || !learns_from(route, method) {
            return;
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&r.body) else {
            return;
        };
        let mut map = lock(&self.map);
        match route {
            Route::List => {
                for row in v
                    .get("runs")
                    .and_then(|x| x.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = row.get("runId").and_then(|x| x.as_str()) {
                        map.learn(alias, id);
                    }
                }
            }
            Route::Start | Route::Run(_) if method == "POST" => {
                if let Some(id) = v.get("runId").and_then(|x| x.as_str()) {
                    map.learn(alias, id);
                }
            }
            _ => {}
        }
    }

    fn version_of(&self, alias: &str) -> Option<String> {
        lock(&self.entries)
            .iter()
            .find(|e| e.def.alias == alias)
            .and_then(|e| e.cell.backend())
    }

    /// One call to one backend, with the bookkeeping, and nothing else.
    async fn forward(
        &self,
        alias: &str,
        route: Route<'_>,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let plane = self.plane_of(alias).await?;
        let r = plane.call(method, path, headers, body).await;
        self.note(alias, &r);
        if let Ok(resp) = &r {
            self.learn_from(alias, route, method, resp);
        }
        r
    }

    /// Refuses a call that changes something on a backend below the minimum version or of an unknown major (with two backends or more). A
    /// version not known yet is learnt with one read first. A backend whose service answers that read without any version header is a stage that
    /// predates the tag, older than the minimum. A read that fails, or that only the gateway answers, tells nothing, and the change goes on.
    async fn gate(&self, alias: &str, method: &str, path: &str) -> Result<(), TransportError> {
        if !changes_something(method, path) {
            return Ok(());
        }
        let mut found = support(&self.client, &self.min, self.version_of(alias).as_deref());
        if found == Support::Unknown {
            let probe = self
                .forward(alias, Route::Other, "GET", "/v1/whoami", &[], Vec::new())
                .await;
            found = support(&self.client, &self.min, self.version_of(alias).as_deref());
            if found == Support::Unknown
                && let Ok(r) = &probe
                && (200..300).contains(&r.status)
                && !crate::version::carries_tag(&r.headers)
            {
                return Err(TransportError::Local(Local::Untagged {
                    alias: alias.to_string(),
                    min: self.min.clone(),
                }));
            }
        }
        match found {
            Support::ReadOnly { version, min } => Err(TransportError::Local(Local::ReadOnly {
                alias: alias.to_string(),
                version,
                min,
            })),
            Support::UnknownMajor { version } => Err(TransportError::Local(Local::UnknownMajor {
                alias: alias.to_string(),
                version,
            })),
            Support::Full | Support::Unknown => Ok(()),
        }
    }

    /// The backend of a run, asking every backend when the map does not know it. `Ok(Err(response))` is the answer of the backend that holds the
    /// run to the `GET /v1/runs/<id>` that found it, or the 404 of the first backend when none holds it.
    async fn locate(&self, run_id: &str) -> Result<Located, TransportError> {
        match lock(&self.map).lookup(run_id) {
            Lookup::One(a) => return Ok(Located::Alias(a)),
            Lookup::Ambiguous(aliases) => {
                return Err(TransportError::Local(Local::Ambiguous {
                    run_id: run_id.to_string(),
                    aliases,
                }));
            }
            Lookup::Unknown => {}
        }
        let path = format!("/v1/runs/{run_id}");
        let planes: Vec<(String, Option<Arc<P>>, String)> = lock(&self.entries)
            .iter()
            .map(|e| {
                let why = match &e.health {
                    Health::Down(r) => r.clone(),
                    Health::SignedOut { fix } => fix.clone(),
                    _ => "not built".to_string(),
                };
                (e.def.alias.clone(), e.plane.clone(), why)
            })
            .collect();
        let mut set = tokio::task::JoinSet::new();
        for (i, (alias, plane, why)) in planes.iter().cloned().enumerate() {
            let path = path.clone();
            set.spawn(async move {
                let Some(plane) = plane else {
                    return (i, alias, Err(TransportError::Network(why)));
                };
                let r = match tokio::time::timeout(
                    PROBE_DEADLINE,
                    plane.call("GET", &path, &[], Vec::new()),
                )
                .await
                {
                    Ok(r) => r,
                    Err(_) => Err(TransportError::Timeout(path.clone())),
                };
                (i, alias, r)
            });
        }
        let mut answers: Vec<Option<(String, Result<Response, TransportError>)>> =
            (0..planes.len()).map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            if let Ok((i, alias, r)) = joined {
                if planes[i].1.is_some() {
                    self.note(&alias, &r);
                }
                answers[i] = Some((alias, r));
            }
        }
        let mut outcomes = Vec::new();
        let mut winner: Option<(String, Response)> = None;
        let mut first_404: Option<Response> = None;
        for (alias, r) in answers.into_iter().flatten() {
            let probe = match &r {
                Ok(resp) if (200..300).contains(&resp.status) => {
                    if winner.is_none() {
                        winner = Some((alias.clone(), resp.clone()));
                    }
                    Probe::Has
                }
                Ok(resp) if resp.status == 404 => {
                    first_404.get_or_insert_with(|| resp.clone());
                    Probe::NotHere
                }
                Ok(resp) => Probe::CannotTell(format!("HTTP {}", resp.status)),
                Err(TransportError::Credentials(m) | TransportError::Sign(m)) => {
                    Probe::CannotTell(m.clone())
                }
                Err(TransportError::Timeout(_)) => {
                    Probe::CannotTell("did not answer in time".to_string())
                }
                Err(other) => Probe::CannotTell(other.to_string()),
            };
            outcomes.push((alias, probe));
        }
        match decide(&outcomes) {
            Decision::Owner(a) => {
                lock(&self.map).learn(&a, run_id);
                let answer = winner.filter(|(w, _)| *w == a).map(|(_, r)| r);
                Ok(Located::Found(a, answer))
            }
            Decision::Ambiguous(aliases) => {
                let mut map = lock(&self.map);
                for a in &aliases {
                    map.learn(a, run_id);
                }
                Err(TransportError::Local(Local::Ambiguous {
                    run_id: run_id.to_string(),
                    aliases,
                }))
            }
            Decision::NotFound => Ok(Located::Missing(first_404.unwrap_or_else(|| Response {
                status: 404,
                body: br#"{"code":"not-found","message":"no such run"}"#.to_vec(),
                ..Response::default()
            }))),
            Decision::CannotTell {
                checked,
                unresolved,
            } => Err(TransportError::Local(Local::CannotTell {
                run_id: run_id.to_string(),
                checked,
                unresolved,
            })),
        }
    }

    /// The run ids a tag edit names, wherever the body holds a `runId` or a list `runIds`.
    fn ids_in(body: &[u8]) -> Vec<String> {
        fn walk(v: &serde_json::Value, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(m) => {
                    for (k, x) in m {
                        match (k.as_str(), x) {
                            ("runId", serde_json::Value::String(s)) => out.push(s.clone()),
                            ("runIds", serde_json::Value::Array(a)) => {
                                out.extend(a.iter().filter_map(|y| y.as_str().map(str::to_string)));
                            }
                            _ => walk(x, out),
                        }
                    }
                }
                serde_json::Value::Array(a) => a.iter().for_each(|y| walk(y, out)),
                _ => {}
            }
        }
        let mut out = Vec::new();
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) {
            walk(&v, &mut out);
        }
        out.sort();
        out.dedup();
        out
    }

    /// The run a start continues: `continueFrom.runId` in the body of `POST /v1/direct/runs`. `None` for a body that continues nothing (or that
    /// is no JSON, which the backend refuses by itself).
    fn continues(body: &[u8]) -> Option<String> {
        #[derive(serde::Deserialize)]
        struct Reference {
            #[serde(rename = "runId")]
            run_id: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Start {
            #[serde(rename = "continueFrom")]
            continue_from: Option<Reference>,
        }
        serde_json::from_slice::<Start>(body)
            .ok()?
            .continue_from?
            .run_id
            .filter(|id| !id.is_empty())
    }

    /// The backend a start goes to, with two backends or more: the one `target` names, else the one that holds the run it continues, else
    /// `default_backend`; with neither the start is refused, no backend is picked for it.
    ///
    /// A follow-up starts where its run is. A backend named for it must be that one: `sure` false checks it against what the client already
    /// knows (the map), as the start itself always did; `sure` true asks every backend when the map does not know the run, and refuses a run that
    /// no backend holds. The check of provider data sharing asks with `sure`, because it changes an account before the start is sent.
    async fn start_alias(
        &self,
        target: Option<&str>,
        continues: Option<&str>,
        sure: bool,
    ) -> Result<String, TransportError> {
        let aliases = self.aliases();
        match (target, continues) {
            (Some(t), Some(id)) => {
                let known = lock(&self.map).lookup(id);
                let owner = match known {
                    Lookup::One(owner) => Some(owner),
                    _ if sure => Some(self.owner(id).await?),
                    _ => None,
                };
                match owner {
                    Some(owner) if owner != t => {
                        Err(TransportError::Local(Local::ContinueElsewhere {
                            run_id: id.to_string(),
                            owner,
                            named: t.to_string(),
                        }))
                    }
                    _ => Ok(t.to_string()),
                }
            }
            (Some(t), None) => Ok(t.to_string()),
            // A start that continues a run goes to the backend that holds it: the map, else every backend is asked. The answer of the probe is
            // not the answer of the start. A run that no backend holds, or that a backend could not be asked about, starts nothing.
            (None, Some(from)) => self.owner(from).await,
            (None, None) => match &self.default_alias {
                Some(d) if aliases.iter().any(|a| a == d) => Ok(d.clone()),
                _ => Err(TransportError::Local(Local::NoBackendChosen { aliases })),
            },
        }
    }

    /// The backend a read or a switch of the account's data retention mode goes to: the one named; with one backend, that one. With several
    /// and none named the call is refused: no account is picked by its place in the file.
    fn retention_alias(&self, target: Option<&str>) -> Result<String, TransportError> {
        let aliases = self.aliases();
        match target {
            Some(t) => Ok(t.to_string()),
            None if aliases.len() >= 2 => {
                Err(TransportError::Local(Local::NoBackendChosen { aliases }))
            }
            None => Ok(aliases.into_iter().next().unwrap_or_default()),
        }
    }

    async fn dispatch(
        &self,
        target: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        let route = route_of(method, path);
        if !self.multi() {
            let only = self.aliases().into_iter().next().unwrap_or_default();
            if let Some(t) = target
                && t != only
            {
                return Err(TransportError::Local(Local::UnknownAlias {
                    alias: t.to_string(),
                    aliases: vec![only],
                }));
            }
            let plane = self.plane_of(&only).await?;
            return plane.call(method, path, headers, body).await;
        }
        let aliases = self.aliases();
        if let Some(t) = target
            && !aliases.iter().any(|a| a == t)
        {
            return Err(TransportError::Local(Local::UnknownAlias {
                alias: t.to_string(),
                aliases,
            }));
        }
        let continues = if route == Route::Start {
            Self::continues(&body)
        } else {
            None
        };
        let alias = match (target, route) {
            (_, Route::Start) => {
                self.start_alias(target, continues.as_deref(), false)
                    .await?
            }
            (Some(t), _) => t.to_string(),
            (None, Route::Run(id)) => match self.locate(id).await? {
                Located::Alias(a) => a,
                Located::Found(a, answer) => {
                    if method == "GET"
                        && !path.contains('?')
                        && path == format!("/v1/runs/{id}")
                        && let Some(r) = answer
                    {
                        return Ok(r);
                    }
                    a
                }
                Located::Missing(r) => return Ok(r),
            },
            // A change that names no run (the index backfill, repository admin) goes to `default_backend` or is refused: no arbitrary backend.
            (None, Route::Other) if changes_something(method, path) => match &self.default_alias {
                Some(d) if aliases.iter().any(|a| a == d) => d.clone(),
                _ => return Err(TransportError::Local(Local::NoBackendChosen { aliases })),
            },
            (None, Route::Tags) => {
                let mut owners: Vec<String> = Vec::new();
                for id in Self::ids_in(&body) {
                    let a = match self.locate(&id).await? {
                        Located::Alias(a) | Located::Found(a, _) => a,
                        Located::Missing(r) => return Ok(r),
                    };
                    if !owners.contains(&a) {
                        owners.push(a);
                    }
                }
                match owners.as_slice() {
                    [one] => one.clone(),
                    [] => self.default_target().unwrap_or_default(),
                    _ => {
                        return Err(TransportError::Local(Local::MixedBackends {
                            aliases: owners,
                        }));
                    }
                }
            }
            (None, _) => self.default_target().unwrap_or_default(),
        };
        self.gate(&alias, method, path).await?;
        self.forward(&alias, route, method, path, headers, body)
            .await
    }
}

enum Located {
    /// The map knew the run.
    Alias(String),
    /// Asked and found: the backend, and its answer to `GET /v1/runs/<id>`.
    Found(String, Option<Response>),
    /// Every backend answered and none holds the run: the first 404.
    Missing(Response),
}

/// The words that tell a person with a lapsed login how to sign in again.
fn sign_in_words(profile: Option<&str>) -> String {
    match profile.filter(|p| !p.is_empty()) {
        Some(p) => format!("the sign-in has lapsed; run: aws sso login --profile {p}"),
        None => "the sign-in has lapsed; sign in again".to_string(),
    }
}

impl<P: ControlPlane> ControlPlane for Backends<P> {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.dispatch(None, method, path, headers, body).await
    }

    async fn call_on(
        &self,
        target: Option<&str>,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.dispatch(target, method, path, headers, body).await
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        // A presigned address needs no identity: any backend's plane fetches it.
        let plane = lock(&self.entries).iter().find_map(|e| e.plane.clone());
        match plane {
            Some(p) => p.fetch(url, range, if_match).await,
            None => Err(TransportError::Local(Local::BackendDown {
                alias: self.aliases().join(", "),
                reason: "no backend is available to fetch from".to_string(),
            })),
        }
    }

    async fn data_retention(
        &self,
        target: Option<&str>,
    ) -> Option<Result<crate::http::RetentionAnswer, TransportError>> {
        let alias = match self.retention_alias(target) {
            Ok(a) => a,
            Err(e) => return Some(Err(e)),
        };
        match self.plane_of(&alias).await {
            Ok(plane) => plane.data_retention(None).await,
            Err(e) => Some(Err(e)),
        }
    }

    async fn put_data_retention(
        &self,
        target: Option<&str>,
        mode: &str,
    ) -> Option<Result<crate::http::RetentionAnswer, TransportError>> {
        let alias = match self.retention_alias(target) {
            Ok(a) => a,
            Err(e) => return Some(Err(e)),
        };
        match self.plane_of(&alias).await {
            Ok(plane) => plane.put_data_retention(None, mode).await,
            Err(e) => Some(Err(e)),
        }
    }

    async fn start_backend(
        &self,
        target: Option<&str>,
        source: Option<&str>,
    ) -> Result<Option<String>, TransportError> {
        if !self.multi() {
            return Ok(None);
        }
        if let Some(t) = target
            && !self.aliases().iter().any(|a| a == t)
        {
            return Err(TransportError::Local(Local::UnknownAlias {
                alias: t.to_string(),
                aliases: self.aliases(),
            }));
        }
        self.start_alias(target, source, true).await.map(Some)
    }

    fn backend_view(&self) -> Option<BackendView> {
        let entries = lock(&self.entries);
        (entries.len() >= 2).then(|| BackendView {
            backends: entries
                .iter()
                .map(|e| BackendInfo {
                    alias: e.def.alias.clone(),
                    health: e.health.clone(),
                    version: e.cell.backend(),
                    stale: e.stale,
                    customer_content: e.def.customer_content,
                    profile: e.def.aws_profile.clone(),
                })
                .collect(),
        })
    }

    fn declared_safe(&self) -> Vec<(String, bool)> {
        lock(&self.entries)
            .iter()
            .map(|e| (e.def.alias.clone(), e.def.customer_content))
            .collect()
    }

    fn backend_of(&self, run_id: &str) -> Option<String> {
        lock(&self.map).backend_of(run_id).map(str::to_string)
    }

    fn retry_down(&self) {
        for e in lock(&self.entries).iter_mut() {
            e.failed_at = None;
        }
    }

    fn paused(&self, alias: &str) -> bool {
        lock(&self.entries).iter().any(|e| {
            e.def.alias == alias
                && matches!(e.health, Health::SignedOut { .. } | Health::Refused(_))
                && e.failed_at.is_some_and(|t| t.elapsed() < RETRY_AFTER)
        })
    }

    fn note_slow(&self, alias: &str) {
        if let Some(e) = lock(&self.entries)
            .iter_mut()
            .find(|e| e.def.alias == alias)
        {
            e.health = Health::Slow;
            e.failed_at = Some(Instant::now());
        }
    }

    fn set_stale(&self, alias: &str, stale: bool) {
        if let Some(e) = lock(&self.entries)
            .iter_mut()
            .find(|e| e.def.alias == alias)
        {
            e.stale = stale;
        }
    }
}

impl<P: ControlPlane> std::fmt::Debug for Backends<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Backends")
            .field("aliases", &self.aliases())
            .field("default", &self.default_alias)
            .finish_non_exhaustive()
    }
}

impl<P: ControlPlane> Backends<P> {
    /// The backend that holds a run: from the map, else by asking every backend. Used where a command continues a run and must start the new
    /// run where the old one is.
    pub async fn owner(&self, run_id: &str) -> Result<String, TransportError> {
        match self.locate(run_id).await? {
            Located::Alias(a) | Located::Found(a, _) => Ok(a),
            Located::Missing(_) => Err(TransportError::Local(Local::CannotTell {
                run_id: run_id.to_string(),
                checked: self.aliases(),
                unresolved: Vec::new(),
            })),
        }
    }
}

/// What a command does with the backend it is aimed at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aim<'a> {
    /// A new run: it needs a backend chosen by `--backend` or `default_backend`.
    Start,
    /// A new run that continues the run with this id: it starts on the backend that holds that run.
    Continue(&'a str),
    /// Anything else: a call about a run finds its backend by itself.
    Other,
}

/// The `api` aimed for one command: the backend `--backend` names, else for a start `default_backend` (a start with two backends and no
/// choice is refused), else for a continue the backend of the source run. A client with one backend is never aimed, so what it sends is what it
/// always sent. A start marked as customer content on a backend not declared safe is refused here, before any call.
pub async fn aim<P: ControlPlane>(
    api: &Api<Backends<P>>,
    named: Option<&str>,
    customer_content: bool,
    kind: Aim<'_>,
) -> Result<Api<Backends<P>>, String> {
    let backends = api.plane();
    let mut alias = match named {
        Some(n) => backends
            .select(Some(n))
            .map_err(|local| local.to_string())?,
        None => None,
    };
    if alias.is_none() && backends.aliases().len() >= 2 {
        alias = match kind {
            Aim::Start => backends.select(None).map_err(|local| local.to_string())?,
            Aim::Continue(from) => Some(backends.owner(from).await.map_err(|e| e.to_string())?),
            Aim::Other => None,
        };
    }
    if customer_content {
        if kind == Aim::Other {
            return Err(
                "--customer-content marks a run that you start; it has no meaning for this command"
                    .to_string(),
            );
        }
        let checked = alias
            .clone()
            .or_else(|| backends.aliases().into_iter().next())
            .unwrap_or_default();
        if let Some(def) = backends.def_of(&checked)
            && let Some(refusal) = ham_core::backends::customer_content_refusal(&def, true)
        {
            return Err(refusal.to_string());
        }
    }
    Ok(match alias {
        Some(a) => api.on(&a),
        None => api.clone(),
    })
}

/// The factory of the live client: a plane is a SigV4 signer from the backend's profile and Region, and an HTTPS connection to its endpoint. A
/// failure to sign reads as `credentials`; the text of a failure is what the client has always printed for it.
pub fn https_factory() -> impl PlaneFactory<crate::https::HttpsPlane> {
    |def: BackendDef| -> BoxBuild<crate::https::HttpsPlane> {
        Box::pin(async move {
            let endpoint = def.api_endpoint.clone().unwrap_or_default();
            let signer = crate::sigv4::Signer::from_profile(
                def.aws_profile.as_deref(),
                def.region.as_deref(),
            )
            .await
            .map_err(|e| BuildFailure {
                credentials: matches!(e, TransportError::Credentials(_) | TransportError::Sign(_)),
                text: e.to_string(),
            })?;
            crate::https::HttpsPlane::new(&endpoint, signer).map_err(|e| BuildFailure {
                text: format!("apiEndpoint: {e}"),
                credentials: false,
            })
        })
    }
}
