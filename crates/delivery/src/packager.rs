//! `packager`. From the
//! verified bundle it builds `runs/<run>/deliverable/<repo>-<runId>-<sha7>.zip`:
//!
//! ```text
//! <repo>-<runId>-<sha7>/
//!   src/              git archive of the head, verbatim: in-tree export and conversion attributes are
//!                     ignored (bytes equal the committed blobs) and a symbolic link is a link entry
//!                     (Unix mode 0120777) holding its target, as `git archive` wrote it
//!   repo.bundle       full history of refs/heads/harness/<runId>, with HEAD
//!   EVIDENCE.md       what Cline reported and what the harness did (`PackageRequest.direct`)
//!   evidence/         every object under runs/<run>/evidence/, among them the session summary
//!   MANIFEST.sha256   `sha256sum -c` lines for every other file, sorted by path
//! ```
//!
//! EVIDENCE.md shows what Cline reported and what the harness did: the harness checked nothing about the work and says
//! so. Every package also writes the evidence archive (`<top>-evidence/`: EVIDENCE.md, `evidence/`, MANIFEST.sha256;
//! `s3::evidence_archive_key`), each with its `ART#` item, so `ham pull --artifact evidence` finds it.
//!
//! Entries carry the head's commit time, so the same inputs give the same bytes; the artifact id
//! derives from the zip SHA-256, so a retried invocation rewrites the same object and item. The
//! object goes up with bucket-default SSE-KMS and its SHA-256 in `x-amz-meta-sha256`; the
//! `ART#<artifactId>` item records it; the return value is the `delivery.ready` payload.

use crate::bundle::load_verified;
use crate::checks::{check_head_sha, harness_ref};
use crate::credential::civil;
use crate::error::DeliveryError;
use crate::files::{scratch, sha256_file, sha256_hex};
use crate::git::Git;
use crate::ports::{Artifacts, Blobs, PutMeta};
use harness_protocol::common::Usage;
use harness_protocol::direct::{DirectEvidence, HeadSource, TestReportStatus};
use harness_protocol::orchestration::DeliveryReadyPayload;
use harness_protocol::store::{ArtifactItem, ArtifactKind, s3};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PackageRequest {
    pub run_id: String,
    /// The `<repo>` of the zip name: the CodeCommit name, or the name `--s3-only` chose.
    pub repo: String,
    #[serde(default)]
    pub base_sha: Option<String>,
    pub head_sha: String,
    pub objective: String,
    #[serde(default)]
    pub pr_url: Option<String>,
    /// What Cline said about its own stop: the outcome label, Cline's last
    /// message, the model, the rules, the wall time and the tokens.
    pub direct: DirectEvidence,
}

#[derive(Debug)]
pub struct Packager<B, T> {
    pub blobs: B,
    pub artifacts: T,
    pub git: Git,
    pub bucket: String,
    pub work_root: PathBuf,
}

/// `<repo>-<runId>-<sha7>`.
pub fn top_dir(repo: &str, run_id: &str, head_sha: &str) -> String {
    format!("{repo}-{run_id}-{}", &head_sha[..7.min(head_sha.len())])
}

/// Content-derived, so a retry names the same artifact.
pub fn artifact_id(sha256: &str) -> String {
    format!("zip-{}", &sha256[..20.min(sha256.len())])
}

fn zip_err(e: zip::result::ZipError) -> DeliveryError {
    DeliveryError::unavailable(format!("zip: {e}"))
}

/// A Markdown table cell: pipes escaped, line breaks folded.
fn cell(s: &str) -> String {
    let one_line: Vec<&str> = s.split(['\n', '\r']).filter(|l| !l.is_empty()).collect();
    one_line.join(" ").replace('|', "\\|").trim().to_string()
}

/// `3731` as `1h 2m 11s`; the units that are zero at the front are left out.
pub fn wall_time(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
}

/// The `Tokens` row: the total, then the four kinds of token it adds up, each by name. `input` counts only the tokens the model read
/// without a cache, so a run that mostly re-read its prompt cache shows a small input and a large `cache read`. The total is the sum the run
/// card prints (control-api `total_tokens`) with the saturating addition agentd uses for the same counters. Plain digits: the live gate reads the
/// row by its labels.
fn tokens_row(u: &Usage) -> String {
    let total = u
        .input_tokens
        .saturating_add(u.output_tokens)
        .saturating_add(u.cache_read_input_tokens)
        .saturating_add(u.cache_write_input_tokens);
    format!(
        "| Tokens | total {total} = input {} (not cached) + output {} + cache read {} + cache write {} |",
        u.input_tokens, u.output_tokens, u.cache_read_input_tokens, u.cache_write_input_tokens
    )
}

/// `text` as a Markdown block quote: every line, an empty one as a bare `>`, so nothing in it starts a heading or a
/// table of its own.
fn quote(text: &str) -> String {
    text.trim_end_matches(['\n', '\r'])
        .lines()
        .map(|l| {
            let l = l.trim_end_matches('\r');
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// EVIDENCE.md of a direct run, from the request alone (deterministic). It states what Cline reported and what the harness
/// did, and claims nothing about the work: no word of a check the harness does not make
/// (`harness_protocol::direct::banned_claim_word`).
pub fn render_evidence(r: &PackageRequest) -> String {
    let d = &r.direct;
    // The engine that ran the session: every line that says who reported, left, stopped or worked takes its name from here.
    let name = d.engine_or_default().display_name().to_string();
    let mut md = String::new();
    let mut line = |s: String| {
        md.push_str(&s);
        md.push('\n');
    };
    line(format!("# EVIDENCE: {} run {}", cell(&r.repo), r.run_id));
    line(String::new());
    line("| Field | Value |".into());
    line("| --- | --- |".into());
    line(format!("| Run | `{}` |", r.run_id));
    line(format!("| Repository | `{}` |", cell(&r.repo)));
    line(match &r.base_sha {
        Some(b) => format!("| Base | `{b}` |"),
        None => "| Base | empty |".into(),
    });
    line(format!("| Head | `{}` |", r.head_sha));
    line(format!(
        "| Pull request | {} |",
        r.pr_url.as_deref().map_or("none".to_string(), cell)
    ));
    line(format!("| Outcome | {} |", cell(&d.label)));
    if let Some(detail) = &d.detail {
        line(format!("| Detail | {} |", cell(detail)));
    }
    if let Some(reason) = &d.stop_reason {
        line(format!("| {name} stop reason | `{}` |", cell(reason)));
    }
    line(format!("| Model | {} |", cell(&d.model_id)));
    // A run that switched route or model: the Model row stays the model the user chose.
    if let Some(final_model) = &d.final_model_id {
        line(format!(
            "| Ran on | {} after {} {} |",
            cell(final_model),
            d.model_switches,
            if d.model_switches == 1 {
                "switch"
            } else {
                "switches"
            }
        ));
    }
    if let Some(v) = &d.engine_version {
        line(format!("| {name} version | {} |", cell(v)));
    }
    line(format!(
        "| Rules | {} |",
        if d.rules.is_empty() {
            "none".to_string()
        } else {
            d.rules
                .iter()
                .map(|x| {
                    format!(
                        "{} ({})",
                        cell(&x.name),
                        &x.sha256[..12.min(x.sha256.len())]
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        }
    ));
    if let Some(w) = d.wall_s {
        line(format!("| Wall time | {} |", wall_time(w)));
    }
    if let Some(u) = &d.usage {
        line(tokens_row(u));
    }
    // The flow the run declared: a fact read from the final commit message, beside the others and never in the label.
    if let Some(flow) = d.flow {
        line(format!("| Flow | {} |", flow.word()));
    }
    line(format!(
        "| Delivered from | {} |",
        match d.head_source {
            HeadSource::Final => "final bundle",
            HeadSource::Snapshot => "last snapshot",
        }
    ));
    line(String::new());
    line("## Objective".into());
    line(String::new());
    line(quote(&r.objective));
    line(String::new());
    line(format!("## {name}'s last message"));
    line(String::new());
    match d.cline_message.as_deref().filter(|m| !m.trim().is_empty()) {
        Some(m) => line(quote(m)),
        None => line(format!("{name} left no final message.")),
    }
    line(String::new());
    // The clean-checkout test report: one sentence from the summary, which holds the one allowed wording. It sits beside the label
    // and never in it.
    let report = d.test_report.as_ref();
    if let Some(s) = report {
        line("## Clean-checkout test report".into());
        line(String::new());
        line(s.sentence().replace(['\n', '\r'], " "));
        line(String::new());
        // A command the harness chose from the manifest of a Cargo workspace, and one stopped at its limit with the settings that change it.
        if let Some(note) = s.plan_note() {
            line(note.replace(['\n', '\r'], " "));
            line(String::new());
        }
        if let Some(note) = s.limit_note() {
            line(note.replace(['\n', '\r'], " "));
            line(String::new());
        }
        // What each service step and each test step did and needed: the lines the report derived from its steps, with the exit codes and
        // counts as the commands gave them.
        if !s.details.is_empty() {
            line("What the report did, step by step:".into());
            line(String::new());
            for d in &s.details {
                line(format!("- {}", d.replace(['\n', '\r'], " ")));
            }
            line(String::new());
        }
        if s.status != TestReportStatus::Off {
            line("It records what the repository's own test commands did in a clean checkout of this head, step by step, in `evidence/test-report.json`. It does not say the tests are meaningful.".into());
            line(String::new());
        }
    }
    // The review round: one sentence from the summary's record, which holds the allowed wording. It sits beside the label and never in it.
    if let Some(rv) = &d.review {
        line("## Review round".into());
        line(String::new());
        line(rv.sentence_for(&name).replace(['\n', '\r'], " "));
        line(String::new());
    }
    let ran_tests = report
        .is_some_and(|s| matches!(s.status, TestReportStatus::Ran | TestReportStatus::TimedOut));
    line("## What the harness did".into());
    line(String::new());
    if ran_tests {
        line(format!(
            "The harness hosted the session, committed what {name} left in the tree, scanned the commits for secrets and packaged this zip. It made no judgment on the work. It also ran the repository's own test commands in a clean checkout of the delivered head and recorded what they did in the section above."
        ));
    } else {
        line(format!(
            "The harness hosted the session, committed what {name} left in the tree, scanned the commits for secrets and packaged this zip. It ran no test and made no judgment on the work."
        ));
    }
    line(String::new());
    line("## Contents".into());
    line(String::new());
    line(format!(
        "- `src/`: `git archive` of `{}`, verbatim (in-tree export attributes are ignored)",
        r.head_sha
    ));
    line(format!(
        "- `repo.bundle`: full history of `refs/heads/harness/{}`",
        r.run_id
    ));
    line("- `evidence/`: the run's evidence objects, among them the session summary".into());
    if report.is_some_and(|s| s.status != TestReportStatus::Off) {
        line("- `evidence/test-report.json`: the clean-checkout test report, step by step".into());
    }
    line("- `MANIFEST.sha256`: SHA-256 of every other file, `sha256sum -c` format".into());
    md
}

/// An evidence object's path under `evidence/`, or `None` for a name that could escape or is a
/// directory marker.
fn object_rel(key: &str, prefix: &str) -> Option<String> {
    let rel = key.strip_prefix(prefix)?;
    let ok = !rel.is_empty()
        && !rel.ends_with('/')
        && rel
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('\\'));
    ok.then(|| format!("evidence/{rel}"))
}

fn zip_time(secs: u64) -> DateTime {
    let (y, mo, d, h, mi, s) = civil(secs);
    let n = |v: u64| u8::try_from(v).unwrap_or(0);
    u16::try_from(y)
        .ok()
        .and_then(|y| DateTime::from_date_and_time(y, n(mo), n(d), n(h), n(mi), n(s)).ok())
        .unwrap_or_default()
}

fn check_request(r: &PackageRequest) -> Result<(), DeliveryError> {
    harness_ref(&r.run_id)?;
    check_head_sha(&r.head_sha)?;
    if let Some(b) = &r.base_sha {
        check_head_sha(b)?;
    }
    let repo_ok = (1..=100).contains(&r.repo.len())
        && !r.repo.starts_with('.')
        && r.repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    if !repo_ok {
        return Err(DeliveryError::invalid(format!(
            "repo {:?} is not a repository name",
            r.repo
        )));
    }
    if let Some(u) = &r.pr_url
        && (!u.starts_with("https://") || u.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        return Err(DeliveryError::invalid(format!(
            "prUrl {u:?} is not an https URL"
        )));
    }
    Ok(())
}

/// A file entry to add after `src/`: bytes in memory or a file on disk.
enum Body {
    Bytes(Vec<u8>),
    File(PathBuf),
}

impl<B: Blobs, T: Artifacts> Packager<B, T> {
    pub async fn package(&self, r: &PackageRequest) -> Result<DeliveryReadyPayload, DeliveryError> {
        check_request(r)?;
        let work = scratch(&self.work_root, "package-")?;
        let loaded = load_verified(
            &self.git,
            &self.blobs,
            &self.bucket,
            work.path(),
            &r.run_id,
            &r.head_sha,
        )
        .await?
        .map_err(|p| DeliveryError::invalid(p.to_string()))?;
        let hr = harness_ref(&r.run_id)?;
        let repo = loaded.repo.as_path();
        let top = top_dir(&r.repo, &r.run_id, &r.head_sha);

        // src/: git archive of the head with export attributes neutralized.
        self.git.verbatim_archives(repo).await?;
        let src_zip = work.path().join("src.zip");
        self.git
            .archive_zip(repo, &r.head_sha, &format!("{top}/src/"), &src_zip)
            .await?;
        // repo.bundle: full history of the harness branch, cloning to the head.
        self.git.symbolic_head(repo, hr.as_str()).await?;
        let bundle = work.path().join("repo.bundle");
        self.git
            .bundle_create(repo, &bundle, &["HEAD", hr.as_str()])
            .await?;

        // Everything after src/, by path under the top directory.
        let mut rest: BTreeMap<String, Body> = BTreeMap::new();
        rest.insert("repo.bundle".into(), Body::File(bundle));
        rest.insert(
            "EVIDENCE.md".into(),
            Body::Bytes(render_evidence(r).into_bytes()),
        );
        let prefix = s3::evidence_prefix(&r.run_id);
        let mut keys = self.blobs.list(&prefix).await?;
        keys.sort();
        for (i, key) in keys.iter().enumerate() {
            let Some(rel) = object_rel(key, &prefix) else {
                continue;
            };
            let file = work.path().join(format!("evidence-{i}"));
            if self.blobs.get_to_file(key, &file).await?.is_some() {
                rest.insert(rel, Body::File(file));
            }
        }

        let when = zip_time(self.git.commit_time(repo, &r.head_sha).await?);
        let zip_path = work.path().join(format!("{top}.zip"));
        write_zip(&zip_path, Some(&src_zip), &top, &rest, when)?;

        let sha7 = &r.head_sha[..7];
        let key = s3::deliverable_key(&r.run_id, &r.repo, sha7);
        let (sha256, size_bytes) = sha256_file(&zip_path)?;
        let id = artifact_id(&sha256);
        self.publish(
            &r.run_id,
            &id,
            ArtifactKind::Zip,
            &key,
            &zip_path,
            "application/zip",
        )
        .await?;

        // The evidence tree alone: EVIDENCE.md, evidence/ and their manifest.
        let mut evidence = rest;
        evidence.remove("repo.bundle");
        let ev_top = format!("{top}-evidence");
        let ev_path = work.path().join(format!("{ev_top}.zip"));
        write_zip(&ev_path, None, &ev_top, &evidence, when)?;
        let (ev_sha, _) = sha256_file(&ev_path)?;
        self.publish(
            &r.run_id,
            &format!("evidence-{}", &ev_sha[..20]),
            ArtifactKind::Evidence,
            &s3::evidence_archive_key(&r.run_id, &r.repo, sha7),
            &ev_path,
            "application/zip",
        )
        .await?;

        Ok(DeliveryReadyPayload {
            artifact_id: id,
            sha256,
            size_bytes,
            pr_url: r.pr_url.clone(),
        })
    }

    /// Uploads `file` to `key` with its SHA-256 and records the `ART#<id>` item.
    async fn publish(
        &self,
        run_id: &str,
        id: &str,
        kind: ArtifactKind,
        key: &str,
        file: &Path,
        content_type: &'static str,
    ) -> Result<(), DeliveryError> {
        let (sha256, size_bytes) = sha256_file(file)?;
        self.blobs
            .put_file(
                key,
                file,
                &PutMeta {
                    content_type,
                    sha256_hex: sha256.clone(),
                },
            )
            .await?;
        self.artifacts
            .put_artifact(&ArtifactItem {
                run_id: run_id.to_string(),
                artifact_id: id.to_string(),
                kind,
                s3_uri: format!("s3://{}/{key}", self.bucket),
                sha256,
                size_bytes,
            })
            .await
    }
}

/// Writes the zip: git's `src/` entries copied raw (when given), then `rest` in path order, then
/// the manifest.
fn write_zip(
    out: &Path,
    src_zip: Option<&Path>,
    top: &str,
    rest: &BTreeMap<String, Body>,
    when: DateTime,
) -> Result<(), DeliveryError> {
    let mut zw = ZipWriter::new(BufWriter::new(File::create(out)?));
    let mut manifest: BTreeMap<String, String> = BTreeMap::new();
    let top_slash = format!("{top}/");

    if let Some(src_zip) = src_zip {
        copy_src(&mut zw, &mut manifest, src_zip, &top_slash)?;
    }
    write_rest(zw, manifest, &top_slash, rest, when)
}

/// git's `src/` entries into the zip and the manifest. Entries are copied raw, which keeps git's bytes, compression, time and Unix
/// attributes as git wrote them (zip 9 copies an entry's attributes unchanged): a plain file has none, so it has no mode and reads as
/// 100644, and an executable keeps its mode. A link entry is written again with `add_symlink` (same name, target and time), so a
/// link's mode (0120777) and target do not depend on the raw copy and `src/` is the head tree with its modes. The manifest line of a
/// link covers the link text, as the entry's content.
fn copy_src<W: Write + std::io::Seek>(
    zw: &mut ZipWriter<W>,
    manifest: &mut BTreeMap<String, String>,
    src_zip: &Path,
    top_slash: &str,
) -> Result<(), DeliveryError> {
    // `S_IFMT` and `S_IFLNK` of a Unix mode.
    const IFMT: u32 = 0o170_000;
    const IFLNK: u32 = 0o120_000;
    let mut src = ZipArchive::new(File::open(src_zip)?).map_err(zip_err)?;
    for i in 0..src.len() {
        let mut link = None;
        {
            let mut f = src.by_index(i).map_err(zip_err)?;
            if !f.is_dir() {
                let mut body = Vec::new();
                f.read_to_end(&mut body)?;
                let entry_name = f.name().map_err(zip_err)?.into_owned();
                let name = entry_name
                    .strip_prefix(top_slash)
                    .unwrap_or(entry_name.as_str());
                manifest.insert(name.to_string(), sha256_hex(&body));
                if f.unix_mode().is_some_and(|m| m & IFMT == IFLNK) {
                    let target = String::from_utf8(body).map_err(|_| {
                        DeliveryError::invalid(format!(
                            "the symbolic link {name} has a target that is not UTF-8, which a zip entry cannot hold verbatim"
                        ))
                    })?;
                    link = Some((entry_name.clone(), target, f.last_modified()));
                }
            }
        }
        match link {
            Some((name, target, when)) => {
                let opts =
                    SimpleFileOptions::default().last_modified_time(when.unwrap_or_default());
                zw.add_symlink(name, target, opts).map_err(zip_err)?;
            }
            None => {
                let raw = src.by_index_raw(i).map_err(zip_err)?;
                zw.raw_copy_file(raw).map_err(zip_err)?;
            }
        }
    }
    Ok(())
}

/// `rest` in path order, then `MANIFEST.sha256`.
fn write_rest<W: Write + std::io::Seek>(
    mut zw: ZipWriter<W>,
    mut manifest: BTreeMap<String, String>,
    top_slash: &str,
    rest: &BTreeMap<String, Body>,
    when: DateTime,
) -> Result<(), DeliveryError> {
    let deflated = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(when)
        .unix_permissions(0o644);
    for (rel, body) in rest {
        let name = format!("{top_slash}{rel}");
        match body {
            Body::Bytes(b) => {
                zw.start_file(name, deflated).map_err(zip_err)?;
                zw.write_all(b)?;
                manifest.insert(rel.clone(), sha256_hex(b));
            }
            Body::File(p) => {
                let (sha, size) = sha256_file(p)?;
                // A bundle is already compressed; evidence objects deflate.
                let opts = if rel == "repo.bundle" {
                    deflated.compression_method(CompressionMethod::Stored)
                } else {
                    deflated
                }
                .large_file(size >= u64::from(u32::MAX));
                zw.start_file(name, opts).map_err(zip_err)?;
                std::io::copy(&mut File::open(p)?, &mut zw)?;
                manifest.insert(rel.clone(), sha);
            }
        }
    }

    let text: String = manifest
        .iter()
        .map(|(p, h)| format!("{h}  {p}\n"))
        .collect();
    zw.start_file(format!("{top_slash}MANIFEST.sha256"), deflated)
        .map_err(zip_err)?;
    zw.write_all(text.as_bytes())?;
    let mut inner = zw.finish().map_err(zip_err)?;
    inner.flush()?;
    Ok(())
}
