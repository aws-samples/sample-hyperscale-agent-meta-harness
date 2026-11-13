//! The offline simulation of the research route (the `DIRECT-RESEARCH` row, offline mode): the route under test, driven the way the VM
//! drives it, over in-memory stand-ins, with the fixture search provider and a generated is-plain-obj archive. Nothing here touches the
//! network or AWS.
//!
//! `cargo run -p research-api --example research_sim -- <out-dir>` writes
//!
//! - `<out-dir>/s3/runs/<run>/agents/direct/research/...`: every object the route stored for the ON run, exactly as the bucket would hold it
//!   (audit objects, item manifests and bodies, search records);
//! - `<out-dir>/sim.json`: the calls made with their HTTP status and error word, the final `limits` answer, the job id, the two runs and the
//!   OFF run's answer. The end-to-end script composes the record directory (the delivered notes, the index, the tool log) from this and judges it.
//!
//! The runs: an ON run (research enabled by its policy row) searches for the package, fetches the first result, downloads the repository
//! at its pinned commit, reads the job's status, then provokes the four ordinary refusals `ham-research selftest` shows (a query over 400
//! characters, a URL that is not https, a URL never seen, a repository on another host). An OFF run (research switched off in its row)
//! asks once and is answered `disabled`.

#[path = "../tests/job_support/mod.rs"]
mod job_support;

use harness_protocol::research::{PolicyItem, ResearchLimits, ResearchProvider, route};
use research_api::archive::filter_archive;
use research_api::fakes::{FixedArchive, FixedClock, MemBlobs, MemTable, RecordingInvoker};
use research_api::handler::{Config, Deps, Request, handle};
use research_api::jobs::{JobDeps, JobEnd, run_job};
use research_api::ports::StageKind;
use research_api::providers::FixtureProvider;
use serde_json::{Value, json};
use std::path::PathBuf;

const RUN_ON: &str = "01M4XAB0CDEFGHJKMNPQRSTVWX";
const RUN_OFF: &str = "01M4XAB0CDEFGHJKMNPQRSTVWY";
const ACCOUNT: &str = "111122223333";
const ROLE: &str = "hamh-direct-harness-worker-session";
/// 2026-10-16T14:29:40Z
const START_MS: u64 = 1_792_160_980_000;
const COMMIT: &str = job_support::COMMIT;
const REPO: &str = "https://github.com/sindresorhus/is-plain-obj";
const MIT: &str = "MIT License\n\nCopyright (c) Sindre Sorhus\n\nPermission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the \"Software\"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions:\n\nThe above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.\n\nTHE SOFTWARE IS PROVIDED \"AS IS\", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.\n";

type SimDeps = Deps<MemTable, MemBlobs, FixtureProvider, RecordingInvoker, FixedClock>;

struct Sim {
    deps: SimDeps,
    calls: Vec<Value>,
    n: u32,
}

impl Sim {
    fn caller(run: &str) -> String {
        format!("arn:aws:sts::{ACCOUNT}:assumed-role/{ROLE}/{run}.direct")
    }

    /// One call to a route as the run's worker session; the answer's status and body.
    async fn call(&mut self, run: &str, path: &str, body: Value, label: &str) -> (u16, Value) {
        self.n += 1;
        self.deps.clock.advance_s(2);
        let req = Request {
            method: "POST".into(),
            path: path.into(),
            caller_arn: Some(Self::caller(run)),
            account_id: Some(ACCOUNT.into()),
            access_key: Some("ASIA-test-session-key-id".into()),
            request_id: format!("req-sim-{:03}", self.n),
            body: body.to_string(),
        };
        let resp = handle(&self.deps, &req).await;
        let v: Value = serde_json::from_str(&resp.body).unwrap_or(Value::Null);
        self.calls.push(json!({
            "label": label, "path": path, "run": run, "status": resp.status, "code": resp.log.code,
            "requestId": req.request_id,
        }));
        (resp.status, v)
    }
}

fn repo_files() -> Vec<(&'static str, Vec<u8>)> {
    let index_js = "export default function isPlainObject(value) {\n  if (typeof value !== 'object' || value === null) { return false; }\n  const prototype = Object.getPrototypeOf(value);\n  return (prototype === null || prototype === Object.prototype);\n}\n".repeat(6);
    vec![
        ("index.js", index_js.into_bytes()),
        ("license", MIT.as_bytes().to_vec()),
        (
            "package.json",
            br#"{"name": "is-plain-obj", "version": "4.1.0", "license": "MIT"}"#.to_vec(),
        ),
        (
            "readme.md",
            b"# is-plain-obj\n\nCheck if a value is a plain object.\n".to_vec(),
        ),
        (
            ".github/workflows/main.yml",
            b"name: CI\non: push\n".to_vec(),
        ),
    ]
}

fn write_tree(out: &std::path::Path, blobs: &MemBlobs) -> std::io::Result<usize> {
    let mut n = 0;
    for key in blobs.keys() {
        let path = out.join("s3").join(&key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, blobs.bytes(&key).unwrap_or_default())?;
        n += 1;
    }
    Ok(n)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // nosemgrep: args -- the one argument is the output directory the caller names; the simulator is a test tool and reads nothing else from it
    let first = std::env::args().nth(1);
    let out = PathBuf::from(first.ok_or("usage: research_sim <out-dir>")?);
    std::fs::create_dir_all(&out)?;

    let provider = FixtureProvider::new(StageKind::Test).map_err(|f| f.to_string())?;
    let deps: SimDeps = Deps {
        table: MemTable::new(),
        blobs: MemBlobs::new(),
        provider,
        invoker: RecordingInvoker::new(),
        clock: FixedClock::at_ms(START_MS),
        cfg: Config {
            worker_role_name: ROLE.into(),
            stage_rate_per_min: 80,
            stage_kind: StageKind::Test,
        },
    };
    let expires = START_MS / 1000 + 8 * 3600;
    deps.table.put_policy(PolicyItem::new(
        RUN_ON,
        true,
        ResearchProvider::Fixture,
        ResearchLimits::default(),
        expires,
    ));
    deps.table.put_policy(PolicyItem::new(
        RUN_OFF,
        false,
        ResearchProvider::Fixture,
        ResearchLimits::default(),
        expires,
    ));
    let mut sim = Sim {
        deps,
        calls: Vec::new(),
        n: 0,
    };

    // The ON run.
    let (_, limits0) = sim
        .call(RUN_ON, route::LIMITS, json!({}), "limits-before")
        .await;
    let query = "is-plain-obj npm package what counts as a plain object";
    let (_, search) = sim
        .call(
            RUN_ON,
            route::SEARCH,
            json!({"query": query, "idempotencyKey": "search-1"}),
            "search",
        )
        .await;
    let first = search["results"][0]["url"]
        .as_str()
        .ok_or("the search returned no result")?
        .to_string();
    let (_, fetch) = sim
        .call(
            RUN_ON,
            route::FETCH,
            json!({"urls": [first], "query": "what counts as plain"}),
            "fetch",
        )
        .await;
    let (_, repo) = sim
        .call(
            RUN_ON,
            route::REPO,
            json!({"url": REPO, "ref": COMMIT, "idempotencyKey": "repo-1"}),
            "repo-start",
        )
        .await;
    let job_id = repo["jobId"].as_str().ok_or("no job id")?.to_string();

    // research-job runs the job (here in process; in the stage it is the second Lambda, started asynchronously).
    let files = repo_files();
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, d)| (*n, d.as_slice())).collect();
    let archive = job_support::archive_of(&refs);
    let job_deps = JobDeps {
        table: sim.deps.table.clone(),
        blobs: sim.deps.blobs.clone(),
        source: FixedArchive::serving(archive),
        clock: FixedClock::at_ms(START_MS + 20_000),
        filter: filter_archive,
    };
    let event = sim
        .deps
        .invoker
        .events()
        .into_iter()
        .next()
        .ok_or("the job was not started")?;
    let ended = run_job(&job_deps, &event).await;
    let JobEnd::Succeeded(item) = ended else {
        return Err(format!("the job did not succeed: {ended:?}").into());
    };
    sim.deps.clock.advance_s(25);
    let (_, job) = sim
        .call(RUN_ON, route::JOB, json!({"jobId": job_id}), "job-status")
        .await;

    // The four ordinary refusals of `ham-research selftest`.
    let (s1, _) = sim
        .call(
            RUN_ON,
            route::SEARCH,
            json!({"query": "q".repeat(401)}),
            "selftest-usage",
        )
        .await;
    let (s2, _) = sim
        .call(
            RUN_ON,
            route::FETCH,
            json!({"urls": ["http://www.npmjs.com/package/is-plain-obj"]}),
            "selftest-refused-url",
        )
        .await;
    let (s3, _) = sim
        .call(
            RUN_ON,
            route::FETCH,
            json!({"urls": ["https://example.org/never-seen"]}),
            "selftest-refused-unseen-url",
        )
        .await;
    let (s4, _) = sim
        .call(
            RUN_ON,
            route::REPO,
            json!({"url": "https://gitlab.com/example/project", "idempotencyKey": "selftest-1"}),
            "selftest-refused-host",
        )
        .await;
    let (_, limits1) = sim
        .call(RUN_ON, route::LIMITS, json!({}), "limits-after")
        .await;

    // The OFF run: research is switched off in its policy row.
    let (off_status, off_body) = sim
        .call(
            RUN_OFF,
            route::SEARCH,
            json!({"query": query}),
            "off-search",
        )
        .await;

    let objects = write_tree(&out, &sim.deps.blobs)?;
    let summary = json!({
        "schema": "hamh-research-sim/1", "runOn": RUN_ON, "runOff": RUN_OFF, "startMs": START_MS,
        "search": {"id": search["id"], "firstUrl": first, "provider": search["provider"]},
        "fetch": fetch, "repoItem": item, "job": job, "jobId": job_id,
        "selftestStatuses": [s1, s2, s3, s4],
        "limitsBefore": limits0, "limitsAfter": limits1,
        "off": {"status": off_status, "code": off_body["code"]},
        "calls": sim.calls, "objects": objects,
    });
    std::fs::write(
        out.join("sim.json"),
        serde_json::to_string_pretty(&summary)?,
    )?;
    println!(
        "RESEARCH-SIM OK objects={objects} calls={}",
        sim.calls.len()
    );
    Ok(())
}
