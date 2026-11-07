//! Placing what the research route stored.
//!
//! The route wrote an item (a page, or a filtered repository archive) and its manifest where the worker cannot write. [`Placer::place`]
//! reads both with the session credentials and trusts neither until the hashes agree:
//!
//! - the manifest must be this run's, this agent's and this item's, of a schema this code reads, with a body key under the item;
//! - the size caps are applied to what the manifest says, before the body is read (a page, an archive, the file count, the unpacked
//!   total, one file), and the run's total is reserved before the read, so concurrent placements cannot pass it together;
//! - the body must have the manifest's length and SHA-256; a repository is unpacked by [`extract_targz`], which checks every file again;
//! - everything lands under `<agent data>/context/research/` ([`ResearchDir`]) read-only, with a copy of the manifest, and the index
//!   (`INDEX.json`, `INDEX.md`) is written again.
//!
//! A failed placement leaves no file, no index row and no counted byte. Placing an item twice reads nothing the second time, and two
//! placements of one item at once read its body once (a lock per item).
//!
//! A repository takes the first free name of `<owner>__<repo>__<commit>`, then that name with `-<item>`, then with `-<item>-<n>`; the
//! unpack is renamed into place without replacing anything ([`rename_new`]), so a directory that is already there is never touched,
//! and a failed placement removes only what it made. The index is written from one snapshot, one writer at a time. A placement stands
//! even when the index cannot be written: [`Placer::index_dirty`] says so, and the next call that reaches the placer writes it again.

use crate::direct::context::ContextDir;
use crate::direct::research::context::{
    INDEX_JSON, INDEX_MD, ResearchDir, repo_dir_name, valid_item_id,
};
use crate::direct::research::extract::{
    ExtractCaps, ExtractError, WantedFile, extract_targz, remove_tree,
};
use crate::direct::research::index::{Item, render_json, render_md};
use crate::direct::research::settings::Settings;
use crate::direct::research::store::{ObjectStore, StoreError};
use harness_protocol::research::{ItemKind, ItemManifest, ItemRef, s3 as rs3};
use harness_protocol::research_copy::{CopyIndex, MatchKind, StoredFile, sha256_hex};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// A manifest larger than this is not read (a repository of 20,000 files lists about 3 MiB).
const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
/// `check_tree` skips a file larger than this, stops after this many files or after this many bytes.
const CHECK_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const CHECK_MAX_FILES: u64 = 50_000;
const CHECK_MAX_BYTES: u64 = 512 * 1024 * 1024;
/// `schema` of the placement listing ([`Placer::listing`]).
pub const LISTING_SCHEMA: &str = "hamh-research-placed/1";

fn guard<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---- errors and results -------------------------------------------------------------------------------------------------------

/// Why an item was not placed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum PlaceError {
    /// The manifest is not one this run can accept.
    #[error("the manifest is not acceptable: {0}")]
    BadManifest(String),
    /// A hash or a length differs from the manifest, or the archive is not what it lists.
    #[error("integrity: {0}")]
    Integrity(String),
    /// A size cap of one item.
    #[error("over a size cap: {0}")]
    TooLarge(String),
    /// The run's total for research material.
    #[error("the run's context storage is full: {0}")]
    LimitContext(String),
    /// Not enough free space on the disk.
    #[error("not enough free space for the unpack")]
    NoSpace,
    /// The bucket or the disk could not be used.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl PlaceError {
    /// The code the API answers with.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadManifest(_) | Self::Integrity(_) => "integrity",
            Self::TooLarge(_) => "too_large",
            Self::LimitContext(_) => "limit_context",
            Self::NoSpace => "no_space",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

fn from_extract(e: ExtractError) -> PlaceError {
    match e {
        ExtractError::NoSpace => PlaceError::NoSpace,
        ExtractError::TooLarge(_) | ExtractError::TooMany => PlaceError::TooLarge(e.to_string()),
        ExtractError::Exists | ExtractError::Io(_) => PlaceError::Unavailable(e.to_string()),
        other => PlaceError::Integrity(other.to_string()),
    }
}

fn from_store(e: StoreError, what: &str) -> PlaceError {
    match e {
        StoreError::Missing => PlaceError::Unavailable(format!("{what} is not in the bucket")),
        StoreError::Unavailable(why) => PlaceError::Unavailable(format!("{what}: {why}")),
    }
}

/// An item that is placed, and whether this call placed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub item: Item,
    pub new: bool,
}

/// A difference [`Placer::verify`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub item_id: String,
    /// A file of the item (relative), `manifest.json`, or `body` for a page.
    pub path: String,
    pub what: String,
}

/// A file of the work tree that equals a stored third-party file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckMatch {
    /// Relative to the work tree.
    pub path: String,
    pub item_id: String,
    pub origin: String,
    pub licence: String,
    /// The stored file's path inside its item.
    pub stored_path: String,
    /// `exact` or `normalised`.
    pub kind: String,
}

/// What [`Placer::check_tree`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// In path order.
    pub matches: Vec<CheckMatch>,
    pub scanned_files: u64,
    /// The scan stopped at its file or byte limit.
    pub truncated: bool,
}

// ---- the placer ---------------------------------------------------------------------------------------------------------------

#[derive(Default)]
struct State {
    items: BTreeMap<String, Item>,
    files: BTreeMap<String, Vec<StoredFile>>,
    used: u64,
    reserved: u64,
}

/// Places the items of one run.
pub struct Placer {
    run: String,
    settings: Settings,
    dir: ResearchDir,
    context_root: PathBuf,
    state: Mutex<State>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    stage_seq: AtomicU64,
    /// One index writer at a time; each writes from one snapshot.
    index_lock: Mutex<()>,
    /// An index write failed and no later one has succeeded.
    index_dirty: AtomicBool,
}

impl std::fmt::Debug for Placer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Placer")
            .field("run", &self.run)
            .finish_non_exhaustive()
    }
}

/// Bytes reserved against the run's total; released unless committed.
struct Reservation<'a> {
    placer: &'a Placer,
    need: u64,
    committed: bool,
}

impl Reservation<'_> {
    fn commit(mut self, actual: u64) {
        let mut st = guard(&self.placer.state);
        st.reserved = st.reserved.saturating_sub(self.need);
        st.used += actual;
        self.committed = true;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let mut st = guard(&self.placer.state);
            st.reserved = st.reserved.saturating_sub(self.need);
        }
    }
}

impl Placer {
    /// Makes `<context>/research/` and returns a placer for `run`.
    pub fn open(context: &ContextDir, run: &str, settings: Settings) -> Result<Self, String> {
        Ok(Self {
            run: run.to_string(),
            settings,
            dir: ResearchDir::create(context)?,
            context_root: context.path().to_path_buf(),
            state: Mutex::new(State::default()),
            locks: Mutex::new(HashMap::new()),
            stage_seq: AtomicU64::new(0),
            index_lock: Mutex::new(()),
            index_dirty: AtomicBool::new(false),
        })
    }

    pub fn research_dir(&self) -> &ResearchDir {
        &self.dir
    }

    /// The placed items, in item order.
    pub fn items(&self) -> Vec<Item> {
        guard(&self.state).items.values().cloned().collect()
    }

    /// The bytes placed so far (pages, and the unpacked files of repositories).
    pub fn used_bytes(&self) -> u64 {
        guard(&self.state).used
    }

    /// `INDEX.json`'s text.
    pub fn index_json(&self) -> String {
        render_json(&self.run, &guard(&self.state).items)
    }

    /// `INDEX.md`'s text: the table for `.ham/context/INDEX.md`.
    pub fn index_md(&self) -> String {
        render_md(&guard(&self.state).items)
    }

    /// What agentd placed, with the SHA-256 of the bytes it finds on the disk now: `{schema "hamh-research-placed/1", run, items
    /// {<id>: {files {<path>: <sha256>}}}}`, in item and path order, with a closing newline. A repository lists every regular file under its
    /// directory by its path relative to it; a page lists its one file under the name of the manifest's body key (`body.txt`), which is the
    /// key the judge reads the route's manifest with. Nothing is taken from the route or from a manifest but that name: only agentd can hash
    /// what it placed on the VM, and a listing made from the manifests would agree with them by construction and prove nothing. A file that
    /// cannot be read, a link and anything that is not a regular file are left out, so a comparison with the manifests fails for them.
    pub fn listing(&self) -> String {
        let mut items = BTreeMap::new();
        for item in self.items() {
            let mut files: BTreeMap<String, String> = BTreeMap::new();
            if item.kind == "page" {
                let name = self.page_key_name(&item);
                if let Some(hash) = hash_regular_file(Path::new(&item.path)) {
                    files.insert(name, hash);
                }
            } else {
                let root = PathBuf::from(&item.path);
                let mut paths = Vec::new();
                list_files(&root, &root, &mut paths);
                for rel in paths {
                    if let Some(hash) = hash_regular_file(&root.join(&rel)) {
                        files.insert(rel, hash);
                    }
                }
            }
            items.insert(item.item_id.clone(), serde_json::json!({ "files": files }));
        }
        let mut text = serde_json::to_string_pretty(&serde_json::json!({
            "schema": LISTING_SCHEMA,
            "run": self.run,
            "items": items,
        }))
        .unwrap_or_default();
        text.push('\n');
        text
    }

    /// The name a page is listed under: the last part of the body key of its manifest (the local copy of it). A copy that is gone or does not
    /// parse leaves the name of the file on the disk, which no manifest has, so the comparison fails.
    fn page_key_name(&self, item: &Item) -> String {
        std::fs::read(self.dir.manifest_path(&item.item_id))
            .ok()
            .and_then(|b| serde_json::from_slice::<ItemManifest>(&b).ok())
            .and_then(|m| m.body.key.rsplit('/').next().map(str::to_string))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| {
                Path::new(&item.path)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
    }

    /// Writes `INDEX.json` and `INDEX.md` from one snapshot of the items, one writer at a time. A failure leaves the flag
    /// [`Self::index_dirty`] set until a write succeeds.
    fn write_index(&self) -> Result<(), String> {
        let _writer = guard(&self.index_lock);
        let (json, md) = {
            let st = guard(&self.state);
            (render_json(&self.run, &st.items), render_md(&st.items))
        };
        let written = self
            .dir
            .write_readonly(&self.dir.root().join(INDEX_JSON), json.as_bytes())
            .and_then(|()| {
                self.dir
                    .write_readonly(&self.dir.root().join(INDEX_MD), md.as_bytes())
            });
        self.index_dirty.store(written.is_err(), Ordering::SeqCst);
        written
    }

    /// Whether the last write of the index failed: the items are placed, the files `INDEX.json` and `INDEX.md` are older than they.
    pub fn index_dirty(&self) -> bool {
        self.index_dirty.load(Ordering::SeqCst)
    }

    fn item_lock(&self, item_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        guard(&self.locks)
            .entry(item_id.to_string())
            .or_default()
            .clone()
    }

    fn reserve(&self, need: u64) -> Result<Reservation<'_>, PlaceError> {
        let mut st = guard(&self.state);
        let after = st.used.saturating_add(st.reserved).saturating_add(need);
        if after > self.settings.max_run_bytes {
            return Err(PlaceError::LimitContext(format!(
                "{need} more bytes would pass the run's cap of {} (used {})",
                self.settings.max_run_bytes, st.used
            )));
        }
        st.reserved += need;
        Ok(Reservation {
            placer: self,
            need,
            committed: false,
        })
    }

    /// The manifest checks that need no other object: the contract's own rules (`ItemManifest::validate`), then this run, this item
    /// and a body key that is a plain name directly under the item. Returns the kind of the item.
    fn check_manifest(&self, m: &ItemManifest, item_id: &str) -> Result<ItemKind, PlaceError> {
        let bad = |why: &str| Err(PlaceError::BadManifest(why.to_string()));
        m.validate()
            .map_err(|e| PlaceError::BadManifest(e.message))?;
        if m.item_id != item_id {
            return bad("the manifest is for another item");
        }
        if m.run != self.run {
            return bad("the manifest is for another run");
        }
        if m.kind == ItemKind::Attachment {
            return bad("an attachment is placed by agentd itself, not from the route's store");
        }
        let item_prefix = format!("{}items/{item_id}/", rs3::prefix(&self.run));
        let name_ok = m.body.key.strip_prefix(&item_prefix).is_some_and(|n| {
            (1..=64).contains(&n.len())
                && n != "."
                && n != ".."
                && n.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        });
        if !name_ok {
            return bad("the body key is not a plain name directly under the item");
        }
        let mut seen = BTreeSet::new();
        if !m.files.iter().all(|f| seen.insert(f.path.as_str())) {
            return bad("the file list repeats a path");
        }
        Ok(m.kind)
    }

    /// Places item `item_id` from the route's store, or returns it if it is placed. The manifest is the only authority about it.
    pub async fn place(
        &self,
        store: &dyn ObjectStore,
        item_id: &str,
    ) -> Result<Placed, PlaceError> {
        self.place_with(store, item_id, None).await
    }

    /// As [`Self::place`], for an item the route's answer named: the answer's keys, hashes and kind must agree with the manifest
    /// before the body is read. The route and the bucket then vouch for each other.
    pub async fn place_ref(
        &self,
        store: &dyn ObjectStore,
        answer: &ItemRef,
    ) -> Result<Placed, PlaceError> {
        self.place_with(store, &answer.id, Some(answer)).await
    }

    async fn place_with(
        &self,
        store: &dyn ObjectStore,
        item_id: &str,
        answer: Option<&ItemRef>,
    ) -> Result<Placed, PlaceError> {
        if !valid_item_id(item_id) {
            return Err(PlaceError::BadManifest(format!(
                "{item_id:?} is not a plain item id"
            )));
        }
        let manifest_key = rs3::manifest_key(&self.run, item_id);
        if let Some(a) = answer
            && a.manifest_key != manifest_key
        {
            return Err(PlaceError::BadManifest(
                "the answer names a manifest key that is not this item's".into(),
            ));
        }
        let lock = self.item_lock(item_id);
        let _held = lock.lock().await;
        let known = guard(&self.state).items.get(item_id).cloned();
        if let Some(item) = known {
            if self.index_dirty() {
                // The item is placed; the files that list it are what is stale. A second failure leaves the flag set.
                let _ = self.write_index();
            }
            return Ok(Placed { item, new: false });
        }
        let manifest_bytes = store
            .get(&manifest_key)
            .await
            .map_err(|e| from_store(e, "the manifest"))?;
        if manifest_bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PlaceError::TooLarge("the manifest is over 8 MiB".into()));
        }
        let m: ItemManifest = serde_json::from_slice(&manifest_bytes).map_err(|_| {
            PlaceError::BadManifest("the manifest is not JSON of the expected shape".into())
        })?;
        let kind = self.check_manifest(&m, item_id)?;
        if let Some(a) = answer {
            let agree = a.kind == m.kind
                && a.manifest_sha256 == m.sha256()
                && a.body_key == m.body.key
                && a.body_sha256 == m.body.sha256;
            if !agree {
                return Err(PlaceError::Integrity(
                    "the route's answer and the manifest in the bucket disagree".into(),
                ));
            }
        }
        let item = match kind {
            ItemKind::Page => self.place_page(store, &m, &manifest_bytes).await?,
            ItemKind::Repo => self.place_repo(store, &m, &manifest_bytes).await?,
            ItemKind::Attachment => {
                return Err(PlaceError::BadManifest(
                    "attachments are placed from the start request by agentd, not from the route"
                        .into(),
                ));
            }
        };
        // The item stands whether or not the index files can be written; `index_dirty` records a failure and the next call repairs it.
        let _ = self.write_index();
        Ok(Placed { item, new: true })
    }

    async fn read_body(
        &self,
        store: &dyn ObjectStore,
        m: &ItemManifest,
    ) -> Result<Vec<u8>, PlaceError> {
        let body = store
            .get(&m.body.key)
            .await
            .map_err(|e| from_store(e, "the body"))?;
        if body.len() as u64 != m.body.bytes || sha256_hex(&body) != m.body.sha256 {
            return Err(PlaceError::Integrity(
                "the body is not what the manifest says (length or SHA-256)".into(),
            ));
        }
        Ok(body)
    }

    fn item_of(&self, m: &ItemManifest, path: &Path, bytes: u64, files: u32) -> Item {
        let rel = path
            .strip_prefix(&self.context_root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        Item {
            item_id: m.item_id.clone(),
            kind: match m.kind {
                ItemKind::Repo => "repo",
                ItemKind::Attachment => "attachment",
                ItemKind::Page => "page",
            }
            .to_string(),
            origin: m.origin.clone(),
            commit: m.commit.clone(),
            retrieved_at: m.retrieved_at.clone(),
            licence: m.licence.spdx.clone(),
            licence_source: m.licence.source.clone(),
            provider: serde_json::to_value(m.provider)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            manifest_sha256: m.sha256(),
            body_sha256: m.body.sha256.clone(),
            bytes,
            files,
            path: path.display().to_string(),
            rel,
        }
    }

    fn commit_item(&self, item: Item, files: Vec<StoredFile>, res: Reservation<'_>, actual: u64) {
        let mut st = guard(&self.state);
        st.items.insert(item.item_id.clone(), item.clone());
        st.files.insert(item.item_id, files);
        drop(st);
        res.commit(actual);
    }

    async fn place_page(
        &self,
        store: &dyn ObjectStore,
        m: &ItemManifest,
        manifest_bytes: &[u8],
    ) -> Result<Item, PlaceError> {
        if m.body.bytes > self.settings.max_page_bytes {
            return Err(PlaceError::TooLarge(format!(
                "the page is {} bytes, over the cap of {}",
                m.body.bytes, self.settings.max_page_bytes
            )));
        }
        let res = self.reserve(m.body.bytes)?;
        let body = self.read_body(store, m).await?;
        let page = self.dir.page_path(&m.item_id);
        let copy = self.dir.manifest_path(&m.item_id);
        let written = self
            .dir
            .write_readonly(&page, &body)
            .and_then(|()| self.dir.write_readonly(&copy, manifest_bytes));
        if let Err(why) = written {
            let _ = std::fs::remove_file(&page);
            let _ = std::fs::remove_file(&copy);
            return Err(PlaceError::Unavailable(why));
        }
        let item = self.item_of(m, &page, body.len() as u64, 1);
        let stored = vec![StoredFile::from_bytes(
            &m.item_id,
            &m.origin,
            &item.licence,
            &format!("pages/{}.txt", m.item_id),
            &body,
        )];
        self.commit_item(item.clone(), stored, res, body.len() as u64);
        Ok(item)
    }

    async fn place_repo(
        &self,
        store: &dyn ObjectStore,
        m: &ItemManifest,
        manifest_bytes: &[u8],
    ) -> Result<Item, PlaceError> {
        let s = &self.settings;
        let too_large = |what: String| Err(PlaceError::TooLarge(what));
        if m.body.bytes > s.max_archive_bytes {
            return too_large(format!(
                "the archive is {} bytes, over the cap of {}",
                m.body.bytes, s.max_archive_bytes
            ));
        }
        if m.files.len() as u64 > u64::from(s.max_files) {
            return too_large(format!(
                "{} files, over the cap of {}",
                m.files.len(),
                s.max_files
            ));
        }
        let total: u64 = m.files.iter().map(|f| f.bytes).sum();
        if total > s.max_unpacked_bytes {
            return too_large(format!(
                "{total} unpacked bytes, over the cap of {}",
                s.max_unpacked_bytes
            ));
        }
        if let Some(f) = m.files.iter().find(|f| f.bytes > s.max_file_bytes) {
            return too_large(format!(
                "{:?} is {} bytes, over the file cap of {}",
                f.path, f.bytes, s.max_file_bytes
            ));
        }
        let res = self.reserve(total)?;
        let archive = self.read_body(store, m).await?;
        let stage = self
            .dir
            .stage_path(&m.item_id, self.stage_seq.fetch_add(1, Ordering::SeqCst));
        let wanted: BTreeMap<String, WantedFile> = m
            .files
            .iter()
            .map(|f| {
                (
                    f.path.clone(),
                    WantedFile {
                        sha256: f.sha256.clone(),
                        bytes: f.bytes,
                    },
                )
            })
            .collect();
        let caps = ExtractCaps::from_settings(s);
        let unpack_into = stage.clone();
        let extracted = tokio::task::spawn_blocking(move || {
            extract_targz(&archive, &unpack_into, &wanted, &caps)
        })
        .await
        .map_err(|_| PlaceError::Unavailable("the unpack task failed".into()))?
        .map_err(from_extract)?;
        let name = repo_dir_name(&m.origin, m.commit.as_deref())
            .unwrap_or_else(|| format!("item-{}", m.item_id));
        let repos = self.dir.repos_dir();
        let copy = self.dir.manifest_path(&m.item_id);
        let target = match publish_dir(&stage, &repos, &name, &m.item_id) {
            Ok(t) => t,
            Err(why) => {
                // Only the unpack is this call's; whatever holds a name in `repos/` is another item's.
                remove_tree(&stage);
                return Err(PlaceError::Unavailable(why));
            }
        };
        if let Err(why) = self.dir.write_readonly(&copy, manifest_bytes) {
            // The rename above made `target` out of this call's own unpack, so it is this call's to remove.
            remove_tree(&target);
            let _ = std::fs::remove_file(&copy);
            return Err(PlaceError::Unavailable(why));
        }
        let item = self.item_of(
            m,
            &target,
            extracted.unpacked_bytes,
            u32::try_from(extracted.files.len()).unwrap_or(u32::MAX),
        );
        let stored = extracted
            .files
            .iter()
            .map(|f| StoredFile {
                item_id: m.item_id.clone(),
                origin: m.origin.clone(),
                licence: item.licence.clone(),
                path: f.path.clone(),
                bytes: f.bytes,
                sha256: f.sha256.clone(),
                norm_sha256: f.norm_sha256.clone(),
            })
            .collect();
        self.commit_item(item.clone(), stored, res, extracted.unpacked_bytes);
        Ok(item)
    }

    // ---- verify -------------------------------------------------------------------------------------------------------------

    /// Compares what is on disk with what the route says: for each item the route's manifest (read again), the local copy of it, the
    /// page or every file of the repository, and files the manifest does not list. An empty answer means every item is as placed.
    pub async fn verify(&self, store: &dyn ObjectStore) -> Vec<Problem> {
        let items = self.items();
        let mut problems = Vec::new();
        for item in items {
            let id = item.item_id.clone();
            let problem = |path: &str, what: &str| Problem {
                item_id: id.clone(),
                path: path.to_string(),
                what: what.to_string(),
            };
            match store.get(&rs3::manifest_key(&self.run, &id)).await {
                Ok(fresh)
                    if serde_json::from_slice::<ItemManifest>(&fresh)
                        .map(|m| m.sha256())
                        .ok()
                        .as_deref()
                        != Some(item.manifest_sha256.as_str()) =>
                {
                    problems.push(problem(
                        "manifest.json",
                        "the route's manifest is not the one that was placed",
                    ));
                }
                Ok(_) => {}
                Err(StoreError::Missing) => {
                    problems.push(problem(
                        "manifest.json",
                        "the route no longer has the manifest",
                    ));
                }
                Err(StoreError::Unavailable(_)) => {
                    problems.push(problem(
                        "manifest.json",
                        "the route's manifest could not be read",
                    ));
                }
            }
            let local = std::fs::read(self.dir.manifest_path(&id));
            let Ok(local) = local else {
                problems.push(problem(
                    "manifest.json",
                    "the local copy of the manifest is gone",
                ));
                continue;
            };
            let Ok(m) = serde_json::from_slice::<ItemManifest>(&local) else {
                problems.push(problem(
                    "manifest.json",
                    "the local copy of the manifest does not parse",
                ));
                continue;
            };
            if m.sha256() != item.manifest_sha256 {
                problems.push(problem(
                    "manifest.json",
                    "the local copy of the manifest changed",
                ));
                continue;
            }
            if item.kind == "page" {
                match std::fs::read(&item.path) {
                    Ok(b) if sha256_hex(&b) == item.body_sha256 => {}
                    Ok(_) => problems.push(problem("body", "the page changed")),
                    Err(_) => problems.push(problem("body", "the page is gone")),
                }
                continue;
            }
            let root = PathBuf::from(&item.path);
            let listed: BTreeSet<&str> = m.files.iter().map(|f| f.path.as_str()).collect();
            for f in &m.files {
                match std::fs::read(root.join(&f.path)) {
                    Ok(b) if sha256_hex(&b) == f.sha256 && b.len() as u64 == f.bytes => {}
                    Ok(_) => problems.push(problem(&f.path, "the file changed")),
                    Err(_) => problems.push(problem(&f.path, "the file is gone")),
                }
            }
            let mut extra = Vec::new();
            list_files(&root, &root, &mut extra);
            for path in extra {
                if !listed.contains(path.as_str()) {
                    problems.push(problem(&path, "the file is not in the manifest"));
                }
            }
        }
        problems
    }

    // ---- the copy check -----------------------------------------------------------------------------------------------------

    /// The files under `dir` that equal a stored third-party file of this run (`harness_protocol::research_copy`): the same code the
    /// bundle validator runs. `root` is the work tree; `dir` must be inside it. Links are not followed, `.git` is skipped, a file over
    /// 4 MiB is not read, and the scan stops after 50,000 files or 512 MiB.
    pub fn check_tree(&self, root: &Path, dir: &Path) -> Result<CheckReport, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("the work tree cannot be read: {}", e.kind()))?;
        let start = dir
            .canonicalize()
            .map_err(|e| format!("the directory cannot be read: {}", e.kind()))?;
        if !start.starts_with(&root) {
            return Err("the directory is outside the work tree".to_string());
        }
        let stored: Vec<StoredFile> = guard(&self.state)
            .files
            .values()
            .flat_map(|v| v.iter().cloned())
            .collect();
        let index = CopyIndex::new(stored);
        let mut report = CheckReport::default();
        if index.is_empty() {
            return Ok(report);
        }
        let mut bytes_read: u64 = 0;
        let mut stack = vec![start];
        'walk: while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in rd.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let path = entry.path();
                if ft.is_dir() {
                    if entry.file_name() != ".git" {
                        stack.push(path);
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if meta.len() == 0 || meta.len() > CHECK_MAX_FILE_BYTES {
                    continue;
                }
                if report.scanned_files >= CHECK_MAX_FILES
                    || bytes_read.saturating_add(meta.len()) > CHECK_MAX_BYTES
                {
                    report.truncated = true;
                    break 'walk;
                }
                let Ok(mut file) = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                    .open(&path)
                else {
                    continue;
                };
                let mut content = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
                if file.read_to_end(&mut content).is_err() {
                    continue;
                }
                report.scanned_files += 1;
                bytes_read += content.len() as u64;
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                if let Some(found) = index.find(&rel, &content) {
                    report.matches.push(CheckMatch {
                        path: rel,
                        item_id: found.stored.item_id.clone(),
                        origin: found.stored.origin.clone(),
                        licence: found.stored.licence.clone(),
                        stored_path: found.stored.path.clone(),
                        kind: match found.kind {
                            MatchKind::Exact => "exact",
                            MatchKind::Normalised => "normalised",
                        }
                        .to_string(),
                    });
                }
            }
        }
        report.matches.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(report)
    }
}

/// Every regular file under `dir`, as a path relative to `root`; a link or another kind of entry is listed too (as a path), so the
/// caller sees it as a file the manifest does not list.
/// The SHA-256 of a regular file; `None` for a link, a directory, a special file or a file that cannot be read.
fn hash_regular_file(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

fn list_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            list_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().into_owned());
        }
    }
}

/// Renames `from` (a directory of `repos`) to a name in `repos` that nothing holds: `name`, then `name-<item>`, then `name-<item>-<n>`
/// for `n` from 1 to 99. Nothing that holds a name is replaced or touched. Returns the path the directory now has.
fn publish_dir(from: &Path, repos: &Path, name: &str, item_id: &str) -> Result<PathBuf, String> {
    let mut candidates = vec![name.to_string(), format!("{name}-{item_id}")];
    candidates.extend((1..100).map(|n| format!("{name}-{item_id}-{n}")));
    for candidate in candidates {
        let target = repos.join(candidate);
        match rename_new(from, &target) {
            Ok(()) => return Ok(target),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::DirectoryNotEmpty
                ) => {}
            Err(e) => return Err(format!("research context: {}", e.kind())),
        }
    }
    Err("research context: no free name for the repository".to_string())
}

/// Renames `from` to `to` only if nothing is at `to` (`renameat2` with `RENAME_NOREPLACE` on Linux, which agentd runs on; a check
/// then a rename elsewhere, for tools that build on another system).
fn rename_new(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        rustix::fs::renameat_with(
            rustix::fs::CWD,
            from,
            rustix::fs::CWD,
            to,
            rustix::fs::RenameFlags::NOREPLACE,
        )
        .map_err(std::io::Error::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        if std::fs::symlink_metadata(to).is_ok() {
            return Err(std::io::ErrorKind::AlreadyExists.into());
        }
        std::fs::rename(from, to)
    }
}
