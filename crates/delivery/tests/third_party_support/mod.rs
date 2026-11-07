//! Shared fixtures of the third-party copy tests: stored research items in an in-memory bucket and a world of one real git
//! repository with a bundle validator over it.
#![allow(dead_code, clippy::unwrap_used)]

use super::common::*;
use delivery::git::Git;
use delivery::validator::{
    Gitleaks, Limits, Rule, ThirdPartyMode, ValidateOutput, ValidateRequest, Validator,
};
use flate2::Compression;
use flate2::write::GzEncoder;
use harness_protocol::research::{
    BodyRef, ItemKind, ItemManifest, LicenceEvidence, ManifestFile, ResearchProvider, s3 as rs3,
};
use harness_protocol::store::s3::verified_bundle_key;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::PathBuf;

pub const COMMIT: &str = "666df7c10035f7e26f27ec214fe5ae3173435f34";
pub const ORIGIN: &str = "https://github.com/acme/widgets";

pub fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

/// Source-like text of about `n` bytes, whole lines only.
pub fn text(n: usize, tag: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while out.len() < n {
        out.push_str(&format!("// {tag} line {i:04}\n"));
        i += 1;
    }
    out
}

pub fn targz(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (p, body) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_cksum();
        b.append_data(&mut h, p, *body).unwrap();
    }
    let tar = b.into_inner().unwrap();
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&tar).unwrap();
    enc.finish().unwrap()
}

pub fn manifest(id: &str, kind: ItemKind, body_name: &str, body: &[u8]) -> ItemManifest {
    ItemManifest {
        schema: "hamh-research-item/1".into(),
        item_id: id.into(),
        kind,
        run: RUN.into(),
        agent: "direct".into(),
        provider: if kind == ItemKind::Repo {
            ResearchProvider::GithubArchive
        } else {
            ResearchProvider::Tavily
        },
        request_id: "req-1".into(),
        origin: if kind == ItemKind::Repo {
            ORIGIN.into()
        } else {
            "https://docs.example.org/guide".into()
        },
        commit: (kind == ItemKind::Repo).then(|| COMMIT.to_string()),
        git_ref: None,
        retrieved_at: "2026-10-16T14:30:02Z".into(),
        licence: LicenceEvidence {
            spdx: "MIT".into(),
            source: "file:license".into(),
            evidence_sha256: None,
        },
        body: BodyRef {
            key: format!("{}items/{id}/{body_name}", rs3::prefix(RUN)),
            sha256: sha(body),
            bytes: body.len() as u64,
        },
        source_sha256: None,
        files: Vec::new(),
        dropped: Vec::new(),
        caps: None,
        truncated: false,
    }
}

/// Stores a repository item in the bucket the validator reads.
pub fn store_repo<S: AsRef<str>>(blobs: &MemBlobs, id: &str, files: &[(&str, S)]) {
    let bodies: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (*p, b.as_ref().as_bytes()))
        .collect();
    let archive = targz(&bodies);
    let mut m = manifest(id, ItemKind::Repo, "body.tar.gz", &archive);
    m.files = bodies
        .iter()
        .map(|(p, b)| ManifestFile {
            path: (*p).to_string(),
            sha256: sha(b),
            bytes: b.len() as u64,
        })
        .collect();
    blobs.insert(&rs3::manifest_key(RUN, id), m.canonical_bytes());
    blobs.insert(&m.body.key, archive);
}

pub fn store_page(blobs: &MemBlobs, id: &str, body: &str) {
    let m = manifest(id, ItemKind::Page, "body.txt", body.as_bytes());
    blobs.insert(&rs3::manifest_key(RUN, id), m.canonical_bytes());
    blobs.insert(&m.body.key, body.as_bytes().to_vec());
}

pub struct World {
    pub _tmp: tempfile::TempDir,
    pub root: PathBuf,
    pub work: PathBuf,
    pub base: String,
    pub v: Validator<MemBlobs, Gitleaks>,
}

pub fn world_with(base_files: &[(&str, &str)]) -> World {
    let tmp = tmp();
    let root = tmp.path().to_path_buf();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut files = vec![("src/calc.py", "def add(a, b):\n    return a + b\n")];
    files.extend_from_slice(base_files);
    let (work, shas) = work_repo(&root, "work", &[&files]);
    let v = Validator {
        blobs: MemBlobs::default(),
        scanner: Gitleaks {
            bin: std::env::var_os("GITLEAKS_BIN")
                .map_or_else(|| PathBuf::from("gitleaks"), PathBuf::from),
            home: home.clone(),
        },
        git: Git::new("git", &home),
        bucket: BUCKET.into(),
        work_root: root.join("scratch"),
    };
    World {
        _tmp: tmp,
        root,
        work,
        base: shas[0].clone(),
        v,
    }
}

pub fn world() -> World {
    world_with(&[])
}

impl World {
    pub fn stage(&self) -> String {
        let f = self.root.join("verified.bundle");
        let head = verified_bundle(&self.work, RUN, &f);
        self.v
            .blobs
            .insert(&verified_bundle_key(RUN), std::fs::read(&f).unwrap());
        head
    }

    pub fn turn(&self, files: &[(&str, Option<&str>)]) -> String {
        commit(&self.work, files, "agent turn");
        self.stage()
    }

    pub fn req(&self, head: &str, mode: ThirdPartyMode) -> ValidateRequest {
        ValidateRequest {
            run_id: RUN.into(),
            base_sha: Some(self.base.clone()),
            head_sha: head.into(),
            allowed_globs: vec!["**".into(), "!.git/**".into()],
            limits: Limits::default(),
            source_bundle_uri: None,
            engine_config: Default::default(),
            third_party: mode,
        }
    }

    pub async fn validate(&self, head: &str) -> ValidateOutput {
        self.v
            .validate(&self.req(head, ThirdPartyMode::Block))
            .await
            .unwrap()
    }

    pub fn research_reads(&self) -> usize {
        self.v
            .blobs
            .reads
            .lock()
            .unwrap()
            .iter()
            .filter(|k| k.contains("/research/"))
            .count()
    }
}

pub fn copies(out: &ValidateOutput) -> Vec<(Rule, Option<String>)> {
    out.violations
        .iter()
        .filter(|v| v.rule == Rule::ThirdPartyCopy)
        .map(|v| (v.rule, v.path.clone()))
        .collect()
}
