//! `ham watch` without a run id and `ham update` ask for no more pages of the run list than they need.
//!
//! `ham watch` without a run id follows the newest run that is no probe. The list comes newest first, so the read stops at the first
//! page that holds such a run and goes on only while a page holds probe runs alone. `ham update` reads the list in four places: twice
//! for the version header that every answer of the backend carries (one page is one answer), and twice for the caller's active runs,
//! which can stand on any page (every page, as before).
//!
//! The lists: the stand-in stage with its store set to pages of 100 (`Mem::runs_page`) and, for a list that holds probe runs, the list
//! server of this file, which pages the same way (the service marks no run as a probe, so the stand-in stage cannot hold one). The
//! list calls are counted where they arrive: at the stand-in stage, at the list server, or at the plane a test puts in front of either.
#![allow(clippy::unwrap_used)]

mod support;

use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use ham::api::{Api, Retry};
use ham::backends::{Backends, BoxBuild, BuildFailure};
use ham::http::{ControlPlane, Response, TransportError};
use ham::https::HttpsPlane;
use ham::output::Output;
use ham::sigv4::Signer;
use ham::update::{DrainEnd, DrainOptions, Git, backend_after, drain};
use ham_core::backends::BackendDef;
use harness_protocol::goal::GoalStatus;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{AKID, BackendHeader, Fake, REGION, SECRET, TOKEN, creds};

/// The runs of one page of the list, on the stand-in stage and on the list server.
const PAGE: usize = 100;

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// The version after this build's (`5.14` gives `5.15`).
fn next_version() -> String {
    let (major, minor) = ham::version::CLIENT.split_once('.').unwrap();
    format!("{major}.{:02}", minor.parse::<u32>().unwrap() + 1)
}

// ---- the stand-in stage with a list in pages --------------------------------------------------------------------------------------------

/// The stand-in stage with its list in pages of 100. With `autopilot` it finishes each run it starts, so a watch of one ends.
async fn stage(autopilot: bool) -> Arc<Fake> {
    let fake = Fake::start_with(autopilot).await;
    fake.mem.runs_page.store(PAGE, Ordering::SeqCst);
    fake
}

fn plain_api(endpoint: &str) -> Api<HttpsPlane> {
    Api::new(Arc::new(
        HttpsPlane::new(endpoint, Signer::new(REGION, creds())).unwrap(),
    ))
    .with_retry(Retry {
        attempts: 2,
        base: Duration::from_millis(10),
    })
}

async fn new_run(fake: &Fake, prompt: &str) -> String {
    let req = serde_json::from_value::<harness_protocol::api::v1::StartDirectRunRequest>(
        json!({"prompt": prompt, "target": {"mode": "s3-only"}, "model": support::MODEL}),
    )
    .unwrap();
    plain_api(&fake.endpoint)
        .start_direct_run(&req, &format!("k-{}", prompt.replace(' ', "-")))
        .await
        .unwrap()
        .run_id
}

/// `n` more rows in the store of the stand-in stage, each like the run `like` and none of them active. The list is sorted by id, newest
/// first: with `older` the ids sort behind every id the stage makes, else in front of them.
fn rows_like(fake: &Fake, like: &str, n: usize, older: bool) {
    let mut g = fake.mem.inner.lock().unwrap();
    let row = g.runs.get(like).unwrap().clone();
    for k in 0..n {
        let id = if older {
            format!("01A{k:023}")
        } else {
            format!("7Z{k:024}")
        };
        let mut r = row.clone();
        r.run_id.clone_from(&id);
        r.status = GoalStatus::Complete;
        assert!(g.runs.insert(id, r).is_none(), "a new id");
    }
}

/// The stand-in stage with `total` runs in pages of 100 whose newest is a run the stage started and finished: a watch can follow it to
/// its end. The other runs are rows like it with older ids. Returns the stage and the newest run.
async fn newest_is_a_finished_run(total: usize) -> (Arc<Fake>, String) {
    let fake = stage(true).await;
    let newest = new_run(&fake, "the newest run").await;
    rows_like(&fake, &newest, total - 1, true);
    let ids = fake.mem.run_ids();
    assert_eq!(
        ids.len(),
        total,
        "TRIGGER ABSENT: the store holds {total} runs"
    );
    assert_eq!(
        ids.iter().max(),
        Some(&newest),
        "TRIGGER ABSENT: the run the stage started is the newest"
    );
    (fake, newest)
}

/// The stand-in stage with `total` runs in pages of 100, none of them active, whose oldest is a run the stage started. The other runs
/// are rows like it with newer ids. Returns the stage and the oldest run, which stands on the last page.
async fn oldest_is_a_started_run(total: usize) -> (Arc<Fake>, String) {
    let fake = stage(false).await;
    let oldest = new_run(&fake, "the oldest run").await;
    fake.mem.finish_run(&oldest);
    rows_like(&fake, &oldest, total - 1, false);
    let ids = fake.mem.run_ids();
    assert_eq!(
        ids.len(),
        total,
        "TRIGGER ABSENT: the store holds {total} runs"
    );
    assert_eq!(
        ids.iter().min(),
        Some(&oldest),
        "TRIGGER ABSENT: the run the stage started is the oldest"
    );
    (fake, oldest)
}

/// The queries of the list calls the stand-in stage has had, in order: empty for a first page, `nextToken=<n>` for a later one.
fn list_calls(fake: &Fake) -> Vec<String> {
    fake.mem
        .seen()
        .into_iter()
        .filter(|s| s.method == "GET" && s.path == "/v1/runs")
        .map(|s| s.query)
        .collect()
}

// ---- a list server for lists that hold probe runs ---------------------------------------------------------------------------------------

/// `GET /live/v1/runs` gives `rows`, newest first, in pages of 100 with the number of rows given so far as the page token, as the
/// stand-in stage pages. Every answer carries `version` as the backend's version. Every other request is answered 404. The server
/// keeps what it was asked.
struct ListServer {
    rows: Vec<Value>,
    /// A list call with this page token (empty: the first page) is answered 400 `bad-request`, as the service answers a token it cannot read.
    refused: Option<String>,
    version: String,
    asked: Mutex<Vec<String>>,
    /// `http://127.0.0.1:<port>/live`.
    endpoint: String,
}

impl ListServer {
    async fn start(rows: Vec<Value>, refused: Option<&str>) -> Arc<ListServer> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Arc::new(ListServer {
            rows,
            refused: refused.map(str::to_string),
            version: next_version(),
            asked: Mutex::default(),
            endpoint: format!("http://{addr}/live"),
        });
        let state = server.clone();
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri| {
            let state = state.clone();
            async move { state.serve(&method, &uri) }
        });
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        server
    }

    fn serve(&self, method: &Method, uri: &Uri) -> axum::response::Response {
        let path = uri.path().strip_prefix("/live").unwrap_or(uri.path());
        let query = uri.query().unwrap_or_default();
        self.asked.lock().unwrap().push(if query.is_empty() {
            format!("{method} {path}")
        } else {
            format!("{method} {path}?{query}")
        });
        let headers = [
            ("content-type", "application/json"),
            (ham::version::BACKEND_HEADER, self.version.as_str()),
        ];
        if method != Method::GET || path != "/v1/runs" {
            let body = json!({"code": "not-found", "message": "no such run"});
            return (StatusCode::NOT_FOUND, headers, body.to_string()).into_response();
        }
        let token = query.strip_prefix("nextToken=").unwrap_or_default();
        if self.refused.as_deref() == Some(token) {
            let body = json!({"code": "bad-request", "message": "nextToken is not valid"});
            return (StatusCode::BAD_REQUEST, headers, body.to_string()).into_response();
        }
        let start = token.parse::<usize>().unwrap_or(0);
        let body = page_from(&self.rows, start);
        (StatusCode::OK, headers, body.to_string()).into_response()
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }

    /// The list calls among what the server was asked, in order.
    fn list_calls(&self) -> Vec<String> {
        self.asked()
            .into_iter()
            .filter(|a| a == "GET /v1/runs" || a.starts_with("GET /v1/runs?"))
            .collect()
    }
}

/// `total` rows of the list, newest first, none of them active: the newest `probes` of them are probe runs.
fn rows(total: usize, probes: usize) -> Vec<Value> {
    rows_of("01M9", "2026-11-25T08:00:00Z", total, probes)
}

/// [`rows`] with ids that begin `id` and with `created` as the time every row was created: the lists of several backends are merged by
/// that time, newest first.
fn rows_of(id: &str, created: &str, total: usize, probes: usize) -> Vec<Value> {
    (0..total)
        .map(|k| {
            let mut r = json!({"runId": format!("{id}{:022}", total - k), "status": "complete", "phase": "planning",
                "target": {"mode": "s3-only"}, "createdAt": created, "ownerPrincipalArn": support::ME});
            if k < probes {
                r["kind"] = json!("probe");
            }
            r
        })
        .collect()
}

fn id_of(row: &Value) -> String {
    row["runId"].as_str().unwrap().to_string()
}

/// The answer of the list route for the page of `rows` that begins at `start`: 100 rows, with the number of rows given so far as the
/// next token while more follow.
fn page_from(rows: &[Value], start: usize) -> Value {
    let page: Vec<&Value> = rows.iter().skip(start).take(PAGE).collect();
    let mut body = json!({"runs": page});
    if start + PAGE < rows.len() {
        body["nextToken"] = json!((start + PAGE).to_string());
    }
    body
}

// ---- the compiled client ----------------------------------------------------------------------------------------------------------------

/// Fresh folders for one client, a config that names `endpoint` as the stage and a `credential_process` profile.
fn home(endpoint: &str, label: &str) -> PathBuf {
    let root = support::scratch(label);
    for d in ["config/ham", "state", "cache", "downloads"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(
        root.join("config/ham/config.toml"),
        format!("apiEndpoint = \"{endpoint}\"\nregion = \"{REGION}\"\nawsProfile = \"ham-test\"\n"),
    )
    .unwrap();
    let script = root.join("creds.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho '{{\"Version\": 1, \"AccessKeyId\": \"{AKID}\", \"SecretAccessKey\": \"{SECRET}\", \"SessionToken\": \"{TOKEN}\", \"Expiration\": \"2099-01-01T00:00:00Z\"}}'\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {REGION}\n",
            script.display()
        ),
    )
    .unwrap();
    std::fs::write(root.join("aws-credentials"), "").unwrap();
    root
}

/// `ham watch` with no run id, as a person types it, in the clean environment of `home`. A watch that has not ended after two minutes
/// fails the test.
async fn ham_watch(home: &Path) -> std::process::Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd.arg("watch")
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("HAMH_DOWNLOAD_DIR", home.join("downloads"))
        .env("AWS_CONFIG_FILE", home.join("aws-config"))
        .env("AWS_SHARED_CREDENTIALS_FILE", home.join("aws-credentials"))
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .expect("ham watch ended within two minutes")
        .unwrap()
}

/// What `ham watch` did against the list server: its exit code, its standard error, the list calls and the requests that followed them.
struct Watched {
    code: Option<i32>,
    err: String,
    lists: Vec<String>,
    after: Vec<String>,
}

async fn watch_at(server: &ListServer, label: &str) -> Watched {
    let h = home(&server.endpoint, label);
    let o = ham_watch(&h).await;
    let lists = server.list_calls();
    let after = server.asked().split_off(lists.len());
    assert!(
        server.asked()[..lists.len()] == lists[..],
        "the list calls come first: {:?}",
        server.asked()
    );
    Watched {
        code: o.status.code(),
        err: text(&o.stderr),
        lists,
        after,
    }
}

/// The request with which a watch of `run` starts.
fn first_of_a_watch(run: &str) -> String {
    format!("GET /v1/runs/{run}")
}

// ---- `ham update` in this process -------------------------------------------------------------------------------------------------------

/// A plane in front of a stage that keeps the list calls (`GET /v1/runs`, with or without a query) and can leave them without an answer.
struct Lists {
    inner: HttpsPlane,
    /// The path and query of every list call, in order.
    calls: Mutex<Vec<String>>,
    /// While set, no list call gets an answer.
    all_fail: AtomicBool,
    /// While set, a list call that carries a page token gets no answer.
    later_pages_fail: AtomicBool,
    /// The list calls without a page token so far: each begins a read of the list.
    reads: AtomicUsize,
    /// When the read with this number begins, the stand-in stage starts to report this version.
    reports_from: Mutex<Option<(usize, Arc<Fake>, String)>>,
}

impl ControlPlane for Lists {
    async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        if method == "GET" && (path == "/v1/runs" || path.starts_with("/v1/runs?")) {
            self.calls.lock().unwrap().push(path.to_string());
            let later = path.contains("nextToken=");
            if !later {
                let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
                let from = self.reports_from.lock().unwrap().clone();
                if let Some((at, fake, version)) = from
                    && read == at
                {
                    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(version);
                }
            }
            if self.all_fail.load(Ordering::SeqCst)
                || (later && self.later_pages_fail.load(Ordering::SeqCst))
            {
                return Err(TransportError::Network("the stage went away".into()));
            }
        }
        self.inner.call(method, path, headers, body).await
    }

    async fn fetch(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        self.inner.fetch(url, range, if_match).await
    }
}

impl Lists {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

/// An API on the stage at `endpoint` behind a [`Lists`] plane; a call is made once.
fn counted(endpoint: &str) -> (Api<Lists>, Arc<Lists>) {
    let plane = Arc::new(Lists {
        inner: HttpsPlane::new(endpoint, Signer::new(REGION, creds())).unwrap(),
        calls: Mutex::default(),
        all_fail: AtomicBool::new(false),
        later_pages_fail: AtomicBool::new(false),
        reads: AtomicUsize::new(0),
        reports_from: Mutex::new(None),
    });
    let api = Api::new(plane.clone()).with_retry(Retry {
        attempts: 1,
        base: Duration::from_millis(1),
    });
    (api, plane)
}

/// The backend's version is kept in one place of the process: a test that reads it lets no other test of this file change it meanwhile.
static BACKEND_CELL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn git(dir: &Path, args: &[&str]) {
    let o = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// The stand-in of `setup.sh --update`. Asked for its plan (`--dry-run`) it changes nothing and prints the plan of an update that
/// deploys. Otherwise it keeps the installed client as `ham.prev` and installs one that prints `ham v<the newest version>`, the way
/// the real setup installs one (a new file, then one rename).
const SETUP: &str = r#"#!/bin/sh
for arg in "$@"; do
  [ "$arg" = --dry-run ] || continue
  printf '  update:        the stage keeps its resources and gets the new revision; the runner is installed again for the build and removed afterwards\n'
  printf '  guard:         the update takes the deploy lock of the stage, so the stage starts no new runs, and waits for the active runs to end (--wait-for-runs). No run is cancelled.\n'
  printf 'Nothing was created. Run again without --dry-run to do it.\n'
  exit 0
done
prefix=""
while [ $# -gt 0 ]; do
  case $1 in --prefix) prefix=$2; shift ;; esac
  shift
done
bin=$prefix/bin
ln "$bin/ham" "$bin/ham.prev" 2>/dev/null || cp -p "$bin/ham" "$bin/ham.prev"
printf '#!/bin/sh\necho "ham vNEWEST"\n' > "$bin/.ham.setup-new"
chmod 755 "$bin/.ham.setup-new"
mv -f "$bin/.ham.setup-new" "$bin/ham"
exit 0
"#;

/// [`SETUP`] with the plan of an update that deploys nothing on the stage, in the words the real setup prints (the stand-in setup of
/// `b804_update.rs` prints the same two lines).
fn setup_that_deploys_nothing() -> String {
    const DEPLOYS: [&str; 2] = [
        "the stage keeps its resources and gets the new revision; the runner is installed again for the build and removed afterwards",
        "the update takes the deploy lock of the stage, so the stage starts no new runs, and waits for the active runs to end (--wait-for-runs). No run is cancelled.",
    ];
    const NOTHING: [&str; 2] = [
        "nothing is deployed; the runner is installed again, builds only the client and is removed afterwards",
        "this update changes nothing on the stage, so it waits for no run (--wait-for-runs) and the stage keeps starting runs.",
    ];
    let mut script = SETUP.to_string();
    for (deploys, nothing) in DEPLOYS.into_iter().zip(NOTHING) {
        assert_eq!(
            script.matches(deploys).count(),
            1,
            "TRIGGER ABSENT: the stand-in setup prints this plan line once: {deploys}"
        );
        script = script.replace(deploys, nothing);
    }
    assert!(
        ham_core::update::plan_deploys_nothing(&format!(
            "  update:        {}\n  guard:         {}\n",
            NOTHING[0], NOTHING[1]
        )),
        "TRIGGER ABSENT: the client reads these two lines as the plan of an update that deploys nothing"
    );
    script
}

/// Which `ham update` a test runs.
#[derive(Clone, Copy, Default)]
struct How {
    check: bool,
    yes: bool,
    no_wait: bool,
}

/// One `ham update` in this process: a source repository whose main holds the version after this build's with the stand-in setup, the
/// installed client in `<root>/bin`, and what `ham::update::run` needs.
struct Update {
    downloads: PathBuf,
    bin: PathBuf,
    source: String,
    newest: String,
}

impl Update {
    fn new(label: &str) -> Update {
        Update::with_setup(label, SETUP)
    }

    /// As [`Update::new`], with `script` as the stand-in `setup.sh` of the source repository.
    fn with_setup(label: &str, script: &str) -> Update {
        let newest = next_version();
        let root = support::scratch(label);
        let repo = root.join("source");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("VERSION"), format!("{newest}\n")).unwrap();
        std::fs::write(
            repo.join("CHANGELOG.md"),
            format!(
                "# Changelog\n\n## [Unreleased]\n\n## [{newest}] - 2026-11-20\n\n### Added\n\n- The newest change.\n\n## [{}] - 2026-11-10\n\n- An older change.\n",
                ham::version::CLIENT
            ),
        )
        .unwrap();
        let setup = repo.join("setup.sh");
        std::fs::write(&setup, script.replace("NEWEST", &newest)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&setup, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "commit",
                "-q",
                "-m",
                "release",
            ],
        );
        let downloads = root.join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        // This build's `ham` in `<root>/bin`, the layout setup installs: a hard link when the scratch folder is on the same volume, else a copy.
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let bin = bin.join("ham");
        if std::fs::hard_link(env!("CARGO_BIN_EXE_ham"), &bin).is_err() {
            std::fs::copy(env!("CARGO_BIN_EXE_ham"), &bin).unwrap();
        }
        Update {
            source: format!("file://{}", repo.display()),
            downloads,
            bin,
            newest,
        }
    }

    /// `ham update` on a terminal whose question `ask` answers; the wait reads the runs every 50 ms for at most five seconds. Returns the
    /// exit code, standard output and standard error. The caller holds [`BACKEND_CELL`].
    async fn run<P: ControlPlane>(
        &self,
        api: &Api<P>,
        how: How,
        ask: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    ) -> (i32, String, String) {
        let ctx = ham::update::Context {
            source: self.source.clone(),
            downloads: self.downloads.clone(),
            git: Git::system(),
            exe: self.bin.canonicalize().unwrap(),
            terminal: true,
            ask,
            backend_recheck: Duration::ZERO,
        };
        let req = ham::update::Request {
            check: how.check,
            yes: how.yes,
            json: false,
            drain: DrainOptions {
                poll: Duration::from_millis(50),
                limit: Duration::from_secs(5),
                no_wait: how.no_wait,
            },
        };
        let out = Output::capture();
        let code = ham::update::run(api, &req, &ctx, &out).await;
        let done = out.finish(code);
        (code, done.stdout_text(), done.stderr_text())
    }

    /// The line a finished update ends with.
    fn updated(&self) -> String {
        format!("Updated: ham v{0} | backend v{0}", self.newest)
    }
}

fn never_asked(_: &str) -> Option<String> {
    panic!("--yes and --check ask nothing")
}

/// The stand-in stage of [`oldest_is_a_started_run`] with 250 runs on three pages, two of them active: the newest run, the first row
/// of the first page (one of the rows `rows_like` adds), and the oldest, the last row of the last page. Returns the stage and both
/// ids in the order of the list, newest first.
async fn active_on_the_first_and_on_the_last_page() -> (Arc<Fake>, String, String) {
    let (fake, oldest) = oldest_is_a_started_run(250).await;
    let newest = fake.mem.run_ids().into_iter().max().unwrap();
    {
        let mut g = fake.mem.inner.lock().unwrap();
        for id in [&newest, &oldest] {
            g.runs.get_mut(id).unwrap().status = GoalStatus::Active;
        }
    }
    let listed = plain_api(&fake.endpoint).list_runs().await.unwrap();
    let at: Vec<usize> = listed
        .iter()
        .enumerate()
        .filter(|(_, r)| r.status == GoalStatus::Active)
        .map(|(k, _)| k)
        .collect();
    assert_eq!(
        (
            listed.len(),
            at,
            listed.first().map(|r| &r.run_id),
            listed.last().map(|r| &r.run_id)
        ),
        (250, vec![0, 249], Some(&newest), Some(&oldest)),
        "TRIGGER ABSENT: of the 250 runs the stage lists in pages of {PAGE}, the first and the last are active and no other"
    );
    (fake, newest, oldest)
}

/// The ids of `runs` that `said` does not hold: what a failure names as missing.
fn missing(said: &str, runs: &[&String]) -> Vec<String> {
    runs.iter()
        .filter(|id| !said.contains(id.as_str()))
        .map(|id| (*id).clone())
        .collect()
}

/// One turn of the wait of `ham update` that stops at once when a run is active.
async fn one_turn_of_the_wait<P: ControlPlane>(api: &Api<P>) -> DrainEnd {
    let opts = DrainOptions {
        poll: Duration::from_millis(50),
        limit: Duration::from_secs(5),
        no_wait: true,
    };
    drain(api, &opts, &Output::capture()).await.unwrap()
}

const FIRST: &str = "/v1/runs";
const SECOND: &str = "/v1/runs?nextToken=100";
const THIRD: &str = "/v1/runs?nextToken=200";

// ---- `ham watch` without a run id -------------------------------------------------------------------------------------------------------

/// The newest page holds a run that is no probe: the watch follows that run and has asked for that page alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b962_ham_watch_asks_for_one_page_when_the_newest_page_holds_a_run_that_is_no_probe() {
    let (fake, newest) = newest_is_a_finished_run(250).await;
    let h = home(&fake.endpoint, "b962-watch-one");
    let before = list_calls(&fake).len();
    let o = ham_watch(&h).await;
    let calls = list_calls(&fake).split_off(before);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o.stderr));
    assert!(
        text(&o.stdout).contains(&format!("Run       {newest} complete")),
        "the watch follows the newest run:\n{}",
        text(&o.stdout)
    );
    assert_eq!(
        calls,
        [""],
        "the newest of 250 runs is no probe: the first page is the only one asked for"
    );
}

/// The newest 120 of 250 runs are probes: the first page holds probes alone, so the watch reads on, and it stops at the second page,
/// which holds the newest run that is no probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b962_ham_watch_reads_on_while_a_page_holds_only_probes_and_stops_at_the_page_with_a_run() {
    let list = rows(250, 120);
    let want = id_of(&list[120]);
    let server = ListServer::start(list, None).await;
    let w = watch_at(&server, "b962-watch-probes").await;
    assert_eq!(
        w.after.first(),
        Some(&first_of_a_watch(&want)),
        "the watch follows the newest run that is no probe: {:?}\n{}",
        w.after,
        w.err
    );
    assert!(
        w.after
            .iter()
            .all(|a| a.starts_with(&first_of_a_watch(&want))),
        "no other run is asked for: {:?}",
        w.after
    );
    assert_eq!(
        w.lists,
        ["GET /v1/runs", "GET /v1/runs?nextToken=100"],
        "the second page holds the run: the third is not asked for"
    );
}

/// A list of probes only is read to its end and has no run to watch, in the words it always had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b962_a_list_of_probes_only_is_read_to_its_end_and_has_no_run_to_watch() {
    let server = ListServer::start(rows(250, 250), None).await;
    let w = watch_at(&server, "b962-watch-all-probes").await;
    assert_eq!(w.code, Some(1), "{}", w.err);
    assert_eq!(w.err.trim(), "ham: no runs to watch");
    assert_eq!(
        w.lists,
        [
            "GET /v1/runs",
            "GET /v1/runs?nextToken=100",
            "GET /v1/runs?nextToken=200"
        ],
        "every page is read before the command says that there is no run"
    );
    assert!(w.after.is_empty(), "no run is asked for: {:?}", w.after);
}

/// An empty list has no run to watch after its one page, and a list of one page gives its newest run that is no probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b962_an_empty_list_and_a_list_of_one_page_answer_as_before() {
    let fake = stage(false).await;
    let h = home(&fake.endpoint, "b962-watch-empty");
    let o = ham_watch(&h).await;
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert_eq!(text(&o.stderr).trim(), "ham: no runs to watch");
    assert_eq!(list_calls(&fake), [""], "an empty list is one page");

    // Three runs, the newest a probe: one page without a token.
    let list = rows(3, 1);
    let want = id_of(&list[1]);
    let server = ListServer::start(list, None).await;
    let w = watch_at(&server, "b962-watch-one-page").await;
    assert_eq!(w.lists, ["GET /v1/runs"]);
    assert_eq!(
        w.after.first(),
        Some(&first_of_a_watch(&want)),
        "{:?}\n{}",
        w.after,
        w.err
    );
}

/// A page that cannot be read ends the command with that failure: no run is watched, and the command does not say that there are no
/// runs, because it has not read them all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b962_a_page_that_fails_gives_the_failure_and_neither_a_run_nor_no_runs() {
    // The first page fails.
    let server = ListServer::start(rows(250, 250), Some("")).await;
    let w = watch_at(&server, "b962-watch-first-fails").await;
    assert_eq!(w.code, Some(1), "{}", w.err);
    assert_eq!(
        w.err.trim(),
        "ham: HTTP 400: nextToken is not valid (bad-request)"
    );
    assert_eq!(w.lists, ["GET /v1/runs"]);
    assert!(w.after.is_empty(), "no run is asked for: {:?}", w.after);

    // The first page holds probes alone and the second fails.
    let server = ListServer::start(rows(250, 250), Some("100")).await;
    let w = watch_at(&server, "b962-watch-second-fails").await;
    assert_eq!(w.code, Some(1), "{}", w.err);
    assert_eq!(
        w.err.trim(),
        "ham: HTTP 400: nextToken is not valid (bad-request)"
    );
    assert_eq!(w.lists, ["GET /v1/runs", "GET /v1/runs?nextToken=100"]);
    assert!(w.after.is_empty(), "no run is asked for: {:?}", w.after);
}

// ---- `ham update` ------------------------------------------------------------------------------------------------------------------------

/// `ham update --check` needs the backend's version, which the first answer carries: one page.
#[tokio::test]
async fn b962_ham_update_check_asks_for_one_page() {
    let _cell = BACKEND_CELL.lock().await;
    let (fake, _) = oldest_is_a_started_run(250).await;
    let u = Update::new("b962-check");
    let (api, plane) = counted(&fake.endpoint);
    let (code, out, err) = u
        .run(
            &api,
            How {
                check: true,
                ..How::default()
            },
            &never_asked,
        )
        .await;
    assert_eq!(code, ham::update::exit::DONE, "{out}\n{err}");
    for part in [
        format!("ham v{}", ham::version::CLIENT),
        format!("backend v{}", ham::version::CLIENT),
        format!("v{}", u.newest),
    ] {
        assert!(out.contains(&part), "{part:?} missing from:\n{out}");
    }
    assert_eq!(
        plane.calls(),
        [FIRST],
        "the version is in the header of the first answer"
    );
}

/// After setup the client reads the backend's version again until it is the new one. Each turn of that wait is one page: here the stage
/// reports the new version from the second turn on.
#[tokio::test]
async fn b962_the_version_read_after_setup_asks_for_one_page_a_turn() {
    let _cell = BACKEND_CELL.lock().await;
    let (fake, _) = oldest_is_a_started_run(250).await;
    let newest = next_version();
    let (api, plane) = counted(&fake.endpoint);
    *plane.reports_from.lock().unwrap() = Some((2, fake.clone(), newest.clone()));
    ham::version::set_backend_for_test(Some(ham::version::CLIENT));
    let now = backend_after(
        &api,
        &newest,
        Duration::from_secs(30),
        Duration::from_millis(1),
    )
    .await
    .unwrap();
    assert_eq!(now.as_deref(), Some(newest.as_str()));
    assert_eq!(
        plane.calls(),
        [FIRST, FIRST],
        "two turns: the first answer of each carries the version"
    );
}

/// A whole update with no active run: one page for the version, every page for the caller's active runs, one page after setup. The
/// update ends as it always did.
#[tokio::test]
async fn b962_a_whole_update_asks_one_page_for_the_version_every_page_for_the_wait_and_one_page_after_setup()
 {
    let _cell = BACKEND_CELL.lock().await;
    let (fake, _) = oldest_is_a_started_run(250).await;
    let u = Update::new("b962-whole");
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(u.newest.clone());
    let (api, plane) = counted(&fake.endpoint);
    let (code, out, err) = u
        .run(
            &api,
            How {
                yes: true,
                ..How::default()
            },
            &never_asked,
        )
        .await;
    assert_eq!(code, ham::update::exit::DONE, "{out}\n{err}");
    assert!(out.contains(&u.updated()), "{out}\n{err}");
    assert_eq!(
        plane.calls(),
        [FIRST, FIRST, SECOND, THIRD, FIRST],
        "the version before the question, the caller's runs before setup, the version after setup"
    );
}

/// The read for the caller's active runs stays whole, and every active run is named wherever it stands: with the newest run active
/// on the first page of the list and the oldest on the last, one turn of the wait ends with both ids, and an update under
/// `--no-wait` stops with both, in the order of the list, and setup does not run.
#[tokio::test]
async fn b962_the_wait_finds_an_active_run_on_the_last_page() {
    let _cell = BACKEND_CELL.lock().await;
    let (fake, newest, oldest) = active_on_the_first_and_on_the_last_page().await;

    // The wait itself, one turn.
    let (api, plane) = counted(&fake.endpoint);
    let end = one_turn_of_the_wait(&api).await;
    assert_eq!(
        end,
        DrainEnd::NotWaiting(vec![newest.clone(), oldest.clone()]),
        "the end of the wait does not name every active run: it lacks {:?}",
        missing(&format!("{end:?}"), &[&newest, &oldest])
    );
    assert_eq!(plane.calls(), [FIRST, SECOND, THIRD]);

    // The update.
    let u = Update::new("b962-active-last");
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(u.newest.clone());
    let (api, plane) = counted(&fake.endpoint);
    let before = std::fs::read(&u.bin).unwrap();
    let (code, out, err) = u
        .run(
            &api,
            How {
                yes: true,
                no_wait: true,
                ..How::default()
            },
            &never_asked,
        )
        .await;
    assert_eq!(code, ham::update::exit::REFUSED, "{out}\n{err}");
    let want = format!("2 run(s) are active: {newest}, {oldest}. --no-wait stops here.");
    assert!(
        err.contains(&want),
        "the update does not say `{want}`: of the active runs its words lack {:?}:\n{err}",
        missing(&err, &[&newest, &oldest])
    );
    assert_eq!(std::fs::read(&u.bin).unwrap(), before, "setup did not run");
    let calls = plane.calls();
    assert_eq!(
        calls[calls.len() - 3..],
        [FIRST, SECOND, THIRD],
        "the wait read every page: {calls:?}"
    );
}

/// The read before setup stays whole too, and the sentence that is its own shows it: when setup's plan says that the update deploys
/// nothing on the stage, `ham update` waits for no run and names every active run of the caller. With the newest run active on the
/// first page of the list and the oldest on the last, that sentence holds both ids in the order of the list, the update goes on to
/// its end, and no turn of the wait reads the list: one page for the version, every page for the caller's runs, one page after setup.
#[tokio::test]
async fn b962_the_read_before_setup_names_the_active_runs_of_every_page() {
    let _cell = BACKEND_CELL.lock().await;
    let (fake, newest, oldest) = active_on_the_first_and_on_the_last_page().await;
    let u = Update::with_setup("b962-active-nothing", &setup_that_deploys_nothing());
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(u.newest.clone());
    let (api, plane) = counted(&fake.endpoint);
    let (code, out, err) = u
        .run(
            &api,
            How {
                yes: true,
                ..How::default()
            },
            &never_asked,
        )
        .await;
    let want =
        format!("does not wait for your 2 active runs: {newest}, {oldest}. No run is cancelled.");
    assert!(
        err.contains(&want),
        "the update does not say `{want}`: of the active runs its words lack {:?}:\n{err}",
        missing(&err, &[&newest, &oldest])
    );
    assert_eq!(code, ham::update::exit::DONE, "{out}\n{err}");
    assert!(out.contains(&u.updated()), "{out}\n{err}");
    assert_eq!(
        plane.calls(),
        [FIRST, FIRST, SECOND, THIRD, FIRST],
        "the version before the question, the caller's runs before setup, the version after setup"
    );
}

/// A stage that gives no answer to the first page stops the update in the words it always had.
#[tokio::test]
async fn b962_a_first_page_that_fails_stops_the_update_as_before() {
    let _cell = BACKEND_CELL.lock().await;
    let (fake, _) = oldest_is_a_started_run(250).await;
    let u = Update::new("b962-first-fails");
    let (api, plane) = counted(&fake.endpoint);
    plane.all_fail.store(true, Ordering::SeqCst);
    let (code, out, err) = u
        .run(
            &api,
            How {
                yes: true,
                ..How::default()
            },
            &never_asked,
        )
        .await;
    assert_eq!(code, ham::update::exit::FAILED, "{out}\n{err}");
    assert!(
        err.contains("could not reach the backend") && err.contains("the stage went away"),
        "{err}"
    );
    assert_eq!(plane.calls(), [FIRST]);
}

/// A later page that fails while the update reads the caller's runs stops the update: what the pages before it held decides nothing, and
/// setup does not run. The pages start to fail when the question is answered, which is after the read for the version.
#[tokio::test]
async fn b962_a_later_page_that_fails_in_the_read_for_the_wait_stops_the_update_before_setup() {
    let _cell = BACKEND_CELL.lock().await;
    let (fake, _) = oldest_is_a_started_run(250).await;
    let u = Update::new("b962-later-fails");
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(u.newest.clone());
    let (api, plane) = counted(&fake.endpoint);
    let switch = plane.clone();
    let yes = move |_: &str| {
        switch.later_pages_fail.store(true, Ordering::SeqCst);
        Some("y\n".to_string())
    };
    let before = std::fs::read(&u.bin).unwrap();
    let (code, out, err) = u.run(&api, How::default(), &yes).await;
    assert!(
        plane.later_pages_fail.load(Ordering::SeqCst),
        "TRIGGER ABSENT: the question was asked\n{out}\n{err}"
    );
    assert_eq!(code, ham::update::exit::FAILED, "{out}\n{err}");
    assert!(
        err.contains("could not read your runs") && err.contains("the stage went away"),
        "{err}"
    );
    assert_eq!(std::fs::read(&u.bin).unwrap(), before, "setup did not run");
    let calls = plane.calls();
    assert_eq!(
        calls[calls.len() - 2..],
        [FIRST, SECOND],
        "the read stopped at the page that failed: {calls:?}"
    );
}

// ---- the counts --------------------------------------------------------------------------------------------------------------------------

/// The list calls of one list: `ham watch` without a run id, `ham update --check`, a whole `ham update` with no active run, one turn of
/// its wait for the caller's runs and one turn of its read of the backend's version after setup.
struct Counts {
    watch: usize,
    check: usize,
    update: usize,
    wait_turn: usize,
    version_turn: usize,
}

/// The four counts of `ham update` on the stage at `endpoint`, whose backend reports the version after this build's. Each answer is
/// asserted: the check reports, the update ends updated, the wait finds no active run, the version read gives the new version.
async fn update_counts(endpoint: &str, label: &str) -> (usize, usize, usize, usize) {
    let u = Update::new(&format!("{label}-check"));
    let (api, plane) = counted(endpoint);
    let (code, out, err) = u
        .run(
            &api,
            How {
                check: true,
                ..How::default()
            },
            &never_asked,
        )
        .await;
    assert_eq!(code, ham::update::exit::DONE, "{out}\n{err}");
    assert!(out.contains(&format!("backend v{}", u.newest)), "{out}");
    let check = plane.calls().len();

    let u = Update::new(&format!("{label}-update"));
    let (api, plane) = counted(endpoint);
    let (code, out, err) = u
        .run(
            &api,
            How {
                yes: true,
                ..How::default()
            },
            &never_asked,
        )
        .await;
    assert_eq!(code, ham::update::exit::DONE, "{out}\n{err}");
    assert!(out.contains(&u.updated()), "{out}\n{err}");
    let update = plane.calls().len();

    let (api, plane) = counted(endpoint);
    assert_eq!(one_turn_of_the_wait(&api).await, DrainEnd::Clear);
    let wait_turn = plane.calls().len();

    let (api, plane) = counted(endpoint);
    let now = backend_after(&api, &u.newest, Duration::ZERO, Duration::from_millis(1))
        .await
        .unwrap();
    assert_eq!(now.as_deref(), Some(u.newest.as_str()));
    let version_turn = plane.calls().len();
    (check, update, wait_turn, version_turn)
}

/// A list of `total` runs without a probe, on the stand-in stage in pages of 100.
async fn counts_without_probes(total: usize, label: &str) -> Counts {
    let (fake, newest) = newest_is_a_finished_run(total).await;
    let h = home(&fake.endpoint, &format!("{label}-watch"));
    let before = list_calls(&fake).len();
    let o = ham_watch(&h).await;
    let watch = list_calls(&fake).len() - before;
    assert!(
        text(&o.stdout).contains(&format!("Run       {newest} complete")),
        "{}\n{}",
        text(&o.stdout),
        text(&o.stderr)
    );

    let (fake, _) = oldest_is_a_started_run(total).await;
    *fake.faults.backend_header.lock().unwrap() = BackendHeader::Value(next_version());
    let (check, update, wait_turn, version_turn) = update_counts(&fake.endpoint, label).await;
    Counts {
        watch,
        check,
        update,
        wait_turn,
        version_turn,
    }
}

/// A list of `total` runs whose newest `probes` are probe runs, on the list server in pages of 100.
async fn counts_with_probes(total: usize, probes: usize, label: &str) -> Counts {
    let list = rows(total, probes);
    let want = id_of(&list[probes]);
    let server = ListServer::start(list.clone(), None).await;
    let w = watch_at(&server, &format!("{label}-watch")).await;
    assert_eq!(
        w.after.first(),
        Some(&first_of_a_watch(&want)),
        "{:?}\n{}",
        w.after,
        w.err
    );

    let server = ListServer::start(list, None).await;
    let (check, update, wait_turn, version_turn) = update_counts(&server.endpoint, label).await;
    Counts {
        watch: w.lists.len(),
        check,
        update,
        wait_turn,
        version_turn,
    }
}

/// Counts and prints the list calls of both commands on three lists in pages of 100 (lines that begin `B962-COUNT`). It asserts the
/// answers and no count: the same test gives the counts before and after a change of the reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b962_counts_the_list_calls_of_ham_watch_and_ham_update() {
    let _cell = BACKEND_CELL.lock().await;
    let lists = [
        (
            "250 runs, the newest no probe",
            counts_without_probes(250, "b962-count-a").await,
        ),
        (
            "250 runs, the newest 120 probes",
            counts_with_probes(250, 120, "b962-count-b").await,
        ),
        (
            "1,600 runs, the newest no probe",
            counts_without_probes(1600, "b962-count-c").await,
        ),
    ];
    for (list, c) in lists {
        println!(
            "B962-COUNT list calls | {list} | ham watch {} | ham update --check {} | ham update (no active run) {} | a turn of its wait for the runs {} | a turn of its version read {}",
            c.watch, c.check, c.update, c.wait_turn, c.version_turn
        );
    }
}

// ---- several backends -------------------------------------------------------------------------------------------------------------------

/// What one backend of several was asked: the path and query of every call, in order.
type Asked = Arc<Mutex<Vec<String>>>;

/// One backend of several: it answers the list route with its rows in pages of 100 and keeps what it was asked.
struct Backend {
    rows: Vec<Value>,
    asked: Asked,
}

impl ControlPlane for Backend {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _headers: &[(String, String)],
        _body: Vec<u8>,
    ) -> Result<Response, TransportError> {
        self.asked.lock().unwrap().push(path.to_string());
        let start = match path.strip_prefix("/v1/runs") {
            Some("") if method == "GET" => Some(0),
            Some(query) if method == "GET" => query
                .strip_prefix("?nextToken=")
                .and_then(|t| t.parse::<usize>().ok()),
            _ => None,
        };
        let (status, body) = match start {
            Some(start) => (200, page_from(&self.rows, start)),
            None => (
                404,
                json!({"code": "not-found", "message": "no such route here"}),
            ),
        };
        Ok(Response {
            status,
            headers: BTreeMap::new(),
            body: body.to_string().into_bytes(),
        })
    }

    async fn fetch(
        &self,
        _url: &str,
        _range: Option<(u64, u64)>,
        _if_match: Option<&str>,
    ) -> Result<Response, TransportError> {
        unreachable!("a read of the list fetches nothing")
    }
}

/// A client with two backends and no default, `work` and `spare`, that hold these lists. Returns the API and what each was asked.
async fn two_backends(
    work: Vec<Value>,
    spare: Vec<Value>,
) -> (Api<Backends<Backend>>, Asked, Asked) {
    let (asked_work, asked_spare) = (Asked::default(), Asked::default());
    let mut planes: BTreeMap<String, Backend> = BTreeMap::new();
    planes.insert(
        "work".into(),
        Backend {
            rows: work,
            asked: asked_work.clone(),
        },
    );
    planes.insert(
        "spare".into(),
        Backend {
            rows: spare,
            asked: asked_spare.clone(),
        },
    );
    let planes = Arc::new(Mutex::new(planes));
    let factory = move |d: BackendDef| -> BoxBuild<Backend> {
        let plane = planes
            .lock()
            .unwrap()
            .remove(&d.alias)
            .ok_or_else(|| BuildFailure {
                text: format!("no plane for {}", d.alias),
                credentials: false,
            });
        Box::pin(async move { plane })
    };
    let defs = ["work", "spare"]
        .map(|alias| BackendDef {
            alias: alias.into(),
            api_endpoint: Some(format!("https://{alias}.example/direct")),
            region: None,
            aws_profile: Some(alias.into()),
            customer_content: false,
        })
        .to_vec();
    let backends = Backends::connect(defs, None, factory).await.unwrap();
    (Api::new(Arc::new(backends)), asked_work, asked_spare)
}

/// With several backends both reads take the merged list of every backend whole, as the read of every page does: the newest run that
/// is no probe can stand on any backend, behind the probes of another, and the first page of one backend says nothing about the others.
#[tokio::test]
async fn b962_with_several_backends_both_reads_take_the_whole_list_of_every_backend() {
    // `work`: 130 probe runs in two pages, all of them newer than every run of `spare`. `spare`: a probe and two runs.
    let spare = rows_of("01M8", "2026-11-24T08:00:00Z", 3, 1);
    let want = id_of(&spare[1]);
    let work = rows_of("01M9", "2026-11-25T08:00:00Z", 130, 130);
    let (api, work, spare) = two_backends(work, spare).await;

    let newest = api.newest_run(|r| !r.is_probe()).await.unwrap();
    assert_eq!(
        newest.map(|r| r.run_id),
        Some(want),
        "the newest run that is no probe stands on the second backend"
    );
    assert_eq!(
        *work.lock().unwrap(),
        ["/v1/runs", "/v1/runs?nextToken=100"],
        "every page of the first backend"
    );
    assert_eq!(*spare.lock().unwrap(), ["/v1/runs"]);

    api.runs_answer().await.unwrap();
    assert_eq!(
        *work.lock().unwrap(),
        [
            "/v1/runs",
            "/v1/runs?nextToken=100",
            "/v1/runs",
            "/v1/runs?nextToken=100"
        ],
        "the read of one answer asks every backend, as the read of every page does"
    );
    assert_eq!(*spare.lock().unwrap(), ["/v1/runs", "/v1/runs"]);
}
