//! In-memory stand-ins for the ports (feature `testing`): the same rules as the real adapters, so the route's tests and the offline
//! end-to-end simulation run without AWS or a network. A fake enforces what the real service enforces: counters are conditional, an
//! object is never replaced, a job moves only from the states it is in.

use crate::ports::{
    Blobs, Clock, Delta, Fail, IdKind, Idem, Invoker, JobEvent, JobRecord, PutOutcome, Table,
};
use harness_protocol::research::{
    ItemRef, JobState, PolicyItem, ResearchError, ResearchErrorCode, ResearchLimits, ResearchUsage,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct TableState {
    policies: BTreeMap<String, PolicyItem>,
    usage: BTreeMap<String, ResearchUsage>,
    rates: BTreeMap<(String, u64), u32>,
    stage_rates: BTreeMap<u64, u32>,
    item_seq: BTreeMap<String, u32>,
    search_seq: BTreeMap<String, u32>,
    idem: BTreeMap<(String, String), (String, String)>,
    seen: BTreeMap<String, BTreeSet<String>>,
    jobs: BTreeMap<(String, String), JobRecord>,
}

/// The research table in memory. A clone shares the same table, so the route and the job runner can run over one world.
#[derive(Clone, Debug, Default)]
pub struct MemTable {
    state: Arc<Mutex<TableState>>,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes a run's policy row, as `run-session` does at start.
    pub fn put_policy(&self, item: PolicyItem) {
        let run = item.pk.trim_start_matches("run#").to_string();
        self.lock().policies.insert(run, item);
    }

    /// The run's policy row as stored.
    pub fn policy(&self, run: &str) -> Option<PolicyItem> {
        self.lock().policies.get(run).cloned()
    }

    /// Switches research off for a run (what the owner's kill switch does mid-run).
    pub fn switch_off(&self, run: &str) {
        if let Some(p) = self.lock().policies.get_mut(run) {
            p.enabled = false;
        }
    }

    /// Every job of a run.
    pub fn jobs(&self, run: &str) -> Vec<JobRecord> {
        self.lock()
            .jobs
            .iter()
            .filter(|((r, _), _)| r == run)
            .map(|(_, j)| j.clone())
            .collect()
    }

    /// Rewrites a job in place (a test moves a job's clock).
    pub fn set_job(&self, job: JobRecord) {
        self.lock()
            .jobs
            .insert((job.run.clone(), job.job_id.clone()), job);
    }

    /// The answer stored under an idempotency key, as the table holds it (a test reads what the route kept).
    pub fn idem_answer(&self, run: &str, key: &str) -> Option<String> {
        self.lock()
            .idem
            .get(&(run.to_string(), key.to_string()))
            .map(|(_, answer)| answer.clone())
    }

    /// The number of URL hashes the run has seen.
    pub fn seen_count(&self, run: &str) -> usize {
        self.lock().seen.get(run).map_or(0, BTreeSet::len)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TableState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn over(code: ResearchErrorCode, what: &str) -> Fail {
    Fail::new(code, format!("{what} is used up for this run"))
}

impl Table for MemTable {
    async fn get_policy(&self, run: &str) -> Result<Option<PolicyItem>, Fail> {
        Ok(self.lock().policies.get(run).cloned())
    }

    async fn reserve(
        &self,
        run: &str,
        d: &Delta,
        limits: &ResearchLimits,
    ) -> Result<ResearchUsage, Fail> {
        let mut st = self.lock();
        let u = st.usage.entry(run.to_string()).or_default();
        let next = ResearchUsage {
            searches: u.searches.saturating_add(d.searches),
            fetched_urls: u.fetched_urls.saturating_add(d.fetched_urls),
            repos: u.repos.saturating_add(d.repos),
            outbound_bytes: u.outbound_bytes.saturating_add(d.outbound_bytes),
            context_bytes: u.context_bytes.saturating_add(d.context_bytes),
        };
        if d.searches > 0 && next.searches > limits.searches {
            return Err(over(ResearchErrorCode::LimitCalls, "the search count"));
        }
        if d.fetched_urls > 0 && next.fetched_urls > limits.fetched_urls {
            return Err(over(ResearchErrorCode::LimitCalls, "the fetched-URL count"));
        }
        if d.repos > 0 && next.repos > limits.repos {
            return Err(over(ResearchErrorCode::LimitCalls, "the repository count"));
        }
        if d.outbound_bytes > 0 && next.outbound_bytes > limits.outbound_bytes {
            return Err(over(
                ResearchErrorCode::LimitBytes,
                "the outbound text budget",
            ));
        }
        if d.context_bytes > 0 && next.context_bytes > limits.context_bytes {
            return Err(over(
                ResearchErrorCode::LimitBytes,
                "the research storage budget",
            ));
        }
        *u = next;
        Ok(next)
    }

    async fn rate_tick(&self, run: &str, minute: u64, cap: u32) -> Result<(), Fail> {
        let mut st = self.lock();
        let n = st.rates.entry((run.to_string(), minute)).or_default();
        if *n >= cap {
            return Err(Fail::new(
                ResearchErrorCode::LimitRate,
                format!("{cap} calls a minute"),
            )
            .with_retry_after(60));
        }
        *n += 1;
        Ok(())
    }

    async fn stage_tick(&self, minute: u64, cap: u32) -> Result<(), Fail> {
        let mut st = self.lock();
        let n = st.stage_rates.entry(minute).or_default();
        if *n >= cap {
            return Err(Fail::new(
                ResearchErrorCode::LimitRate,
                "the stage is at its search rate",
            )
            .with_retry_after(30));
        }
        *n += 1;
        Ok(())
    }

    async fn usage(&self, run: &str) -> Result<ResearchUsage, Fail> {
        Ok(self.lock().usage.get(run).copied().unwrap_or_default())
    }

    async fn next_id(&self, run: &str, kind: IdKind) -> Result<u32, Fail> {
        let mut st = self.lock();
        let seq = match kind {
            IdKind::Item => &mut st.item_seq,
            IdKind::Search => &mut st.search_seq,
        };
        let n = seq.entry(run.to_string()).or_default();
        *n += 1;
        Ok(*n)
    }

    async fn idem_get(&self, run: &str, key: &str, req_hash: &str) -> Result<Idem, Fail> {
        Ok(
            match self.lock().idem.get(&(run.to_string(), key.to_string())) {
                None => Idem::Fresh,
                Some((h, resp)) if h == req_hash => Idem::Replay(resp.clone()),
                Some(_) => Idem::Conflict,
            },
        )
    }

    async fn idem_put(
        &self,
        run: &str,
        key: &str,
        req_hash: &str,
        response: &str,
        _expires_s: u64,
    ) -> Result<(), Fail> {
        self.lock()
            .idem
            .entry((run.to_string(), key.to_string()))
            .or_insert_with(|| (req_hash.to_string(), response.to_string()));
        Ok(())
    }

    async fn seen_put(&self, run: &str, hashes: &[String], _expires_s: u64) -> Result<(), Fail> {
        self.lock()
            .seen
            .entry(run.to_string())
            .or_default()
            .extend(hashes.iter().cloned());
        Ok(())
    }

    async fn seen_get(&self, run: &str, hashes: &[String]) -> Result<Vec<String>, Fail> {
        let st = self.lock();
        let seen = st.seen.get(run);
        Ok(hashes
            .iter()
            .filter(|h| seen.is_some_and(|s| s.contains(*h)))
            .cloned()
            .collect())
    }

    async fn job_create(&self, job: &JobRecord) -> Result<bool, Fail> {
        let mut st = self.lock();
        let key = (job.run.clone(), job.job_id.clone());
        if st.jobs.contains_key(&key) {
            return Ok(false);
        }
        st.jobs.insert(key, job.clone());
        Ok(true)
    }

    async fn job_get(&self, run: &str, job_id: &str) -> Result<Option<JobRecord>, Fail> {
        Ok(self
            .lock()
            .jobs
            .get(&(run.to_string(), job_id.to_string()))
            .cloned())
    }

    async fn job_move(
        &self,
        run: &str,
        job_id: &str,
        from: &[JobState],
        to: JobState,
        now_s: u64,
        item: Option<&ItemRef>,
        error: Option<&ResearchError>,
    ) -> Result<bool, Fail> {
        let mut st = self.lock();
        let Some(job) = st.jobs.get_mut(&(run.to_string(), job_id.to_string())) else {
            return Ok(false);
        };
        if !from.contains(&job.state) {
            return Ok(false);
        }
        job.state = to;
        job.updated_s = now_s;
        job.item = item.cloned();
        job.error = error.cloned();
        Ok(true)
    }
}

/// Object bytes and content type by key.
type Objects = BTreeMap<String, (Vec<u8>, String)>;

/// The bucket in memory: an object is never replaced. A clone shares the same bucket.
#[derive(Clone, Debug, Default)]
pub struct MemBlobs {
    objects: Arc<Mutex<Objects>>,
    order: Arc<Mutex<Vec<String>>>,
}

impl MemBlobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every key, sorted.
    pub fn keys(&self) -> Vec<String> {
        self.lock().keys().cloned().collect()
    }

    /// Every key in the order it was written (a manifest comes after its body).
    pub fn order(&self) -> Vec<String> {
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The bytes at a key.
    pub fn bytes(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().get(key).map(|(b, _)| b.clone())
    }

    /// Replaces an object with no check (a test plays Cline overwriting its own prefix; the real bucket policy forbids it).
    pub fn force(&self, key: &str, body: Vec<u8>) {
        self.lock()
            .insert(key.to_string(), (body, "application/octet-stream".into()));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Objects> {
        self.objects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Blobs for MemBlobs {
    async fn put_new(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<PutOutcome, Fail> {
        let mut m = self.lock();
        if m.contains_key(key) {
            return Ok(PutOutcome::Exists);
        }
        m.insert(key.to_string(), (body, content_type.to_string()));
        self.order
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(key.to_string());
        Ok(PutOutcome::Created)
    }

    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, Fail> {
        Ok(self.bytes(key))
    }
}

/// Records the jobs it was asked to start; it can be told to refuse.
#[derive(Debug, Default)]
pub struct RecordingInvoker {
    events: Mutex<Vec<JobEvent>>,
    refuse: Mutex<bool>,
}

impl RecordingInvoker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn refusing() -> Self {
        let s = Self::default();
        *s.refuse
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        s
    }

    pub fn events(&self) -> Vec<JobEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Invoker for RecordingInvoker {
    async fn start_job(&self, event: &JobEvent) -> Result<(), Fail> {
        if *self
            .refuse
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            return Err(Fail::internal("the job function refused the start"));
        }
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
        Ok(())
    }
}

/// A clock a test moves.
#[derive(Debug)]
pub struct FixedClock(AtomicU64);

impl FixedClock {
    pub fn at_ms(ms: u64) -> Self {
        Self(AtomicU64::new(ms))
    }

    pub fn advance_s(&self, seconds: u64) {
        self.0.fetch_add(seconds * 1000, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

// ---- a scripted search provider and archive source ----------------------------------------------------------------------------------------

use crate::ports::{
    ArchiveSource, Downloaded, ExtractOutcome, GithubRepo, Provider, SearchOutcome,
};
use harness_protocol::research::{ResearchProvider, SearchRequest};
use std::collections::VecDeque;

/// A provider that answers from a script, one answer per call, and remembers what it was asked.
#[derive(Debug)]
pub struct ScriptedProvider {
    id: ResearchProvider,
    searches: Mutex<VecDeque<Result<SearchOutcome, Fail>>>,
    extracts: Mutex<VecDeque<Result<ExtractOutcome, Fail>>>,
    asked: Mutex<Vec<String>>,
}

impl ScriptedProvider {
    pub fn new(id: ResearchProvider) -> Self {
        Self {
            id,
            searches: Mutex::default(),
            extracts: Mutex::default(),
            asked: Mutex::default(),
        }
    }

    pub fn then_search(self, r: Result<SearchOutcome, Fail>) -> Self {
        self.push_search(r);
        self
    }

    pub fn then_extract(self, r: Result<ExtractOutcome, Fail>) -> Self {
        self.push_extract(r);
        self
    }

    /// Adds the answer of a later search call.
    pub fn push_search(&self, r: Result<SearchOutcome, Fail>) {
        self.searches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(r);
    }

    /// Adds the answer of a later extract call.
    pub fn push_extract(&self, r: Result<ExtractOutcome, Fail>) {
        self.extracts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push_back(r);
    }

    /// `search:<query>` and `extract:<url,url>` for every call made.
    pub fn asked(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Provider for ScriptedProvider {
    fn id(&self) -> ResearchProvider {
        self.id
    }

    async fn search(&self, req: &SearchRequest) -> Result<SearchOutcome, Fail> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("search:{}", req.query));
        self.searches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| Err(Fail::upstream("the script has no more search answers")))
    }

    async fn extract(&self, urls: &[String], _query: Option<&str>) -> Result<ExtractOutcome, Fail> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("extract:{}", urls.join(",")));
        self.extracts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .unwrap_or_else(|| Err(Fail::upstream("the script has no more extract answers")))
    }
}

/// An archive source that serves the bytes it was given for every request, and remembers the requests.
#[derive(Debug)]
pub struct FixedArchive {
    answer: Mutex<Option<Result<Downloaded, Fail>>>,
    asked: Mutex<Vec<String>>,
}

impl FixedArchive {
    pub fn serving(bytes: Vec<u8>) -> Self {
        Self {
            answer: Mutex::new(Some(Ok(Downloaded {
                bytes,
                final_host: "codeload.github.com".into(),
            }))),
            asked: Mutex::default(),
        }
    }

    pub fn failing(f: Fail) -> Self {
        Self {
            answer: Mutex::new(Some(Err(f))),
            asked: Mutex::default(),
        }
    }

    /// `<owner>/<repo>@<ref or HEAD>` for every download.
    pub fn asked(&self) -> Vec<String> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl ArchiveSource for FixedArchive {
    async fn download(
        &self,
        repo: &GithubRepo,
        git_ref: Option<&str>,
        max_bytes: u64,
    ) -> Result<Downloaded, Fail> {
        self.asked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!(
                "{}/{}@{}",
                repo.owner,
                repo.repo,
                git_ref.unwrap_or("HEAD")
            ));
        let answer = self
            .answer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match answer {
            Some(Ok(d)) if d.bytes.len() as u64 > max_bytes => Err(Fail::new(
                ResearchErrorCode::TooLarge,
                "the archive is over the download cap",
            )),
            Some(r) => r,
            None => Err(Fail::upstream("no archive was scripted")),
        }
    }
}
