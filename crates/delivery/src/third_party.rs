//! The third-party copy rule of the bundle validator.
//!
//! During the run, Cline may search the web and download repositories for context through the research route. The route stores each
//! item (a page, or a filtered repository archive) and its manifest under `runs/<run>/agents/direct/research/items/<item>/`, where the
//! worker's role can read and cannot write. The context directory the run reads them from is outside the work tree, so nothing in it
//! reaches the zip by itself. This rule closes the other way in: a file the run delivers, or a file anywhere in the history it delivers
//! (a blob that `base..head` adds, even one removed again), that equals a stored file of 1 KiB or more is a violation naming the file,
//! the origin and the licence. "Equals" and the names that are exempt are `harness_protocol::research_copy`, the code that
//! `ham-research check` runs in the session, so the run is shown before it finishes what the validator would block.
//!
//! The stored files are read from the bucket here, not taken from the session: a repository's archive is opened and each file is hashed
//! (raw, and with line endings and trailing blanks made alike), after the archive's own SHA-256 is checked against its manifest. A stored
//! item that cannot be read or does not match its manifest is a violation too: the rule could not be applied, and a run is not delivered
//! on a rule that could not be applied. A run that stored nothing, or added no file, is not asked anything.

use crate::error::DeliveryError;
use crate::files::sha256_file;
use crate::git::{AddedBlob, Git, TreeEntry};
use crate::ports::Blobs;
use crate::validator::{Rule, Violation};
use flate2::read::GzDecoder;
use harness_protocol::research::{ItemKind, ItemManifest, s3 as rs3};
use harness_protocol::research_copy::{
    COPY_FLOOR_BYTES, COPY_NORMALISED_FLOOR_BYTES, CopyIndex, MatchKind, StoredFile,
    is_exempt_path, sha256_hex,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::Path;

/// The most violations one validation reports for this rule; more are counted in the last one.
const MAX_REPORTED: usize = 25;
/// A stored file larger than this is not hashed (the route keeps files of at most 1 MiB).
const MAX_STORED_FILE: u64 = 8 * 1024 * 1024;
/// The most bytes a stored archive may decode to (the route keeps at most 200 MiB of files).
const MAX_DECODED_BYTES: u64 = 1024 * 1024 * 1024;
/// Delivered blobs are read in batches of at most this many bytes.
const BATCH_BYTES: u64 = 32 * 1024 * 1024;

fn problem(detail: String) -> Violation {
    Violation {
        rule: Rule::ThirdPartyCopy,
        path: None,
        detail,
    }
}

/// The ids of the stored items of a run, from the manifest keys under its research prefix, in order.
async fn item_ids<B: Blobs>(blobs: &B, run_id: &str) -> Result<Vec<String>, DeliveryError> {
    let prefix = format!("{}items/", rs3::prefix(run_id));
    let mut ids = BTreeSet::new();
    for key in blobs.list(&prefix).await? {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        if let Some(id) = rest.strip_suffix("/manifest.json")
            && rs3::is_item_id(id)
        {
            ids.insert(id.to_string());
        }
    }
    Ok(ids.into_iter().collect())
}

/// The stored files of one item, or why they cannot be read.
async fn stored_files<B: Blobs>(
    blobs: &B,
    work: &Path,
    run_id: &str,
    id: &str,
) -> Result<Vec<StoredFile>, String> {
    let cannot = |why: &str| {
        format!(
            "the stored research item {id} could not be checked ({why}), so the copy rule could not be applied"
        )
    };
    let manifest_file = work.join(format!("{id}.manifest.json"));
    match blobs
        .get_to_file(&rs3::manifest_key(run_id, id), &manifest_file)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => return Err(cannot("its manifest is gone")),
        Err(_) => return Err(cannot("its manifest could not be read")),
    }
    let bytes =
        std::fs::read(&manifest_file).map_err(|_| cannot("its manifest could not be read"))?;
    let m: ItemManifest =
        serde_json::from_slice(&bytes).map_err(|_| cannot("its manifest is not a manifest"))?;
    if m.validate().is_err() || m.item_id != id || m.run != run_id {
        return Err(cannot("its manifest breaks the contract"));
    }
    let body_file = work.join(format!("{id}.body"));
    match blobs.get_to_file(&m.body.key, &body_file).await {
        Ok(Some(_)) => {}
        Ok(None) => return Err(cannot("its body is gone")),
        Err(_) => return Err(cannot("its body could not be read")),
    }
    let (sha, size) = sha256_file(&body_file).map_err(|_| cannot("its body could not be read"))?;
    if sha != m.body.sha256 || size != m.body.bytes {
        return Err(cannot("its body is not what its manifest says"));
    }
    let licence = m.licence.spdx.clone();
    match m.kind {
        ItemKind::Page => {
            let body =
                std::fs::read(&body_file).map_err(|_| cannot("its body could not be read"))?;
            Ok(vec![StoredFile::from_bytes(
                id,
                &m.origin,
                &licence,
                &format!("pages/{id}.txt"),
                &body,
            )])
        }
        ItemKind::Repo => repo_files(&body_file, &m, id, &licence).map_err(cannot),
        ItemKind::Attachment => Ok(Vec::new()),
    }
}

/// The files of a stored repository archive that the rule indexes (those of [`COPY_FLOOR_BYTES`] or more whose names are not exempt),
/// after the archive has been shown to be what its manifest says: every listed file once, with the listed length and SHA-256, and nothing
/// else (a file the manifest does not list, a link or another kind of member, a repeated name, or a listed file that is missing all
/// fail). The exempt and the small files are checked too, because a archive that is wrong in one place is not trusted in another.
fn repo_files(
    body_file: &Path,
    m: &ItemManifest,
    id: &str,
    licence: &str,
) -> Result<Vec<StoredFile>, &'static str> {
    let unreadable = "its archive cannot be read";
    let file = std::fs::File::open(body_file).map_err(|_| "its body could not be read")?;
    let listed: BTreeMap<&str, (&str, u64)> = m
        .files
        .iter()
        .map(|f| (f.path.as_str(), (f.sha256.as_str(), f.bytes)))
        .collect();
    if listed.len() != m.files.len() {
        return Err("its manifest lists a path twice");
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut archive = tar::Archive::new(GzDecoder::new(file).take(MAX_DECODED_BYTES));
    let entries = archive.entries().map_err(|_| unreadable)?;
    for entry in entries {
        let mut entry = entry.map_err(|_| unreadable)?;
        let kind = entry.header().entry_type();
        if kind.is_dir()
            || kind.is_pax_global_extensions()
            || kind.is_pax_local_extensions()
            || kind.is_gnu_longname()
            || kind.is_gnu_longlink()
        {
            continue;
        }
        if !kind.is_file() {
            return Err("its archive holds a member that is neither a file nor a directory");
        }
        let path = entry
            .path()
            .map_err(|_| unreadable)?
            .to_string_lossy()
            .into_owned();
        let Some(&(want_sha, want_bytes)) = listed.get(path.as_str()) else {
            return Err("its archive holds a file its manifest does not list");
        };
        if !seen.insert(path.clone()) {
            return Err("its archive holds a file twice");
        }
        let size = entry.header().size().map_err(|_| unreadable)?;
        if size != want_bytes || size > MAX_STORED_FILE {
            return Err("a file of its archive is not what its manifest says");
        }
        let mut body = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        entry
            .by_ref()
            .take(size.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|_| unreadable)?;
        if body.len() as u64 != size || sha256_hex(&body) != want_sha {
            return Err("a file of its archive is not what its manifest says");
        }
        if size >= COPY_FLOOR_BYTES && !is_exempt_path(&path) {
            out.push(StoredFile::from_bytes(id, &m.origin, licence, &path, &body));
        }
    }
    if seen.len() != listed.len() {
        return Err("its archive lacks a file its manifest lists");
    }
    Ok(out)
}

/// The violations of the rule for the range `base..head` of the bare repository `repo`. `tree` is the head's tree (path to entry), for
/// telling a file that is delivered from one that is in the history only.
#[allow(clippy::too_many_arguments)]
pub async fn check<B: Blobs>(
    blobs: &B,
    git: &Git,
    repo: &Path,
    work: &Path,
    run_id: &str,
    base: Option<&str>,
    head: &str,
    tree: &BTreeMap<String, TreeEntry>,
    max_file_bytes: u64,
) -> Result<Vec<Violation>, DeliveryError> {
    let ids = item_ids(blobs, run_id).await?;
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    // A run that added no file is not asked anything; a stored item that cannot be checked blocks a run that added any file at all.
    let all_added = git.added_blobs(repo, base, head).await?;
    if !all_added.iter().any(|b| b.size > 0) {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stored = Vec::new();
    for id in &ids {
        match stored_files(blobs, work, run_id, id).await {
            Ok(files) => stored.extend(files),
            Err(why) => out.push(problem(why)),
        }
    }
    let index = CopyIndex::new(stored);
    if index.is_empty() {
        return Ok(out);
    }
    // Only a blob the normalised floor lets through can equal a stored file, and a blob over the largest stored file cannot.
    let added: Vec<AddedBlob> = all_added
        .into_iter()
        .filter(|b| {
            b.size >= COPY_NORMALISED_FLOOR_BYTES && b.size <= max_file_bytes.min(MAX_STORED_FILE)
        })
        .collect();
    let mut by_sha: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in tree.values() {
        by_sha
            .entry(e.sha.as_str())
            .or_default()
            .push(e.path.as_str());
    }
    let mut seen = BTreeSet::new();
    let mut found = 0usize;
    let mut at = 0;
    while at < added.len() {
        let mut end = at;
        let mut bytes = 0u64;
        while end < added.len() && (end == at || bytes + added[end].size <= BATCH_BYTES) {
            bytes += added[end].size;
            end += 1;
        }
        let batch = &added[at..end];
        let shas: Vec<&str> = batch.iter().map(|b| b.sha.as_str()).collect();
        let contents = git.blobs(repo, &shas).await?;
        for (blob, content) in batch.iter().zip(contents) {
            // A blob is listed once, at the first path it was seen at, which may be an exempt name (a licence text beside a real copy of
            // the same bytes): the other paths it has in the final tree are tried too.
            let paths = std::iter::once(blob.path.as_str())
                .chain(by_sha.get(blob.sha.as_str()).into_iter().flatten().copied());
            let Some((path, m)) = paths
                .filter_map(|p| index.find(p, &content).map(|m| (p, m)))
                .next()
            else {
                continue;
            };
            if !seen.insert((path.to_string(), blob.sha.clone())) {
                continue;
            }
            found += 1;
            if found > MAX_REPORTED {
                continue;
            }
            let how = match m.kind {
                MatchKind::Exact => "the bytes are the same",
                MatchKind::Normalised => {
                    "the same after line endings and trailing blanks are made alike"
                }
            };
            let delivered = tree.get(path).is_some_and(|e| e.sha == blob.sha);
            let place = if delivered {
                ""
            } else {
                " It is in the history of the delivered bundle, not in the final tree."
            };
            out.push(Violation {
                rule: Rule::ThirdPartyCopy,
                path: Some(path.to_string()),
                detail: format!(
                    "{path} equals {} of {} (item {}, licence {}): {how}.{place} A delivered copy of third-party text is not allowed.",
                    m.stored.path, m.stored.origin, m.stored.item_id, m.stored.licence
                ),
            });
        }
        at = end;
    }
    if found > MAX_REPORTED {
        out.push(problem(format!(
            "{} more file(s) equal stored third-party files and are not listed",
            found - MAX_REPORTED
        )));
    }
    Ok(out)
}
