//! The flow a run declared, in a pulled zip.
//!
//! The harness records the flow (`quick`, `full` or `none`) in `summary.flow` of `evidence/direct.json`, and the packager puts that file in
//! the zip as `<top>/evidence/direct.json`. The client reads that one entry and shows one `Flow` row beside the label: `ham pull` and `ham watch`
//! print it, and the delivery card of the terminal app has it once the zip is downloaded ([`with_flow`]; the terminal app reads the zip once,
//! when the download ends, and keeps the row). The row never changes the label or the exit code. A zip without the entry, an entry that is too
//! large or not JSON, a record with no flow or with a word this build does not know, or a zip that cannot be read gives nothing.

use crate::pull::Pulled;
use ham_core::head::flow_in_evidence;
use ham_core::render::{FLOW_LABEL, FinalCard};
use harness_protocol::direct::DirectFlow;
use harness_protocol::store::ArtifactKind;
use std::io::Read;
use std::path::Path;

/// The most bytes of `evidence/direct.json` read. The file holds the envelope and agentd's summary, which is under 64 KiB.
pub const MAX_EVIDENCE_BYTES: u64 = 2 * 1024 * 1024;

/// The name of the record inside a delivered zip: one top directory, then `evidence/direct.json`.
fn is_evidence_entry(name: &str) -> bool {
    let parts: Vec<&str> = name.split('/').collect();
    parts.len() == 3 && !parts[0].is_empty() && parts[1] == "evidence" && parts[2] == "direct.json"
}

/// The flow `evidence/direct.json` of the delivered zip at `path` records; `None` when the zip has no such entry, it cannot be read, or it
/// records no flow.
pub fn flow_of_zip(path: &Path) -> Option<DirectFlow> {
    let file = std::fs::File::open(path).ok()?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).ok()?;
    let name = (0..zip.len()).find_map(|i| {
        let entry = zip.by_index_raw(i).ok()?;
        let name = entry.name().ok()?;
        is_evidence_entry(&name).then(|| name.into_owned())
    })?;
    let entry = zip.by_name(&name).ok()?;
    if entry.size() > MAX_EVIDENCE_BYTES {
        return None;
    }
    let mut body = Vec::new();
    entry
        .take(MAX_EVIDENCE_BYTES + 1)
        .read_to_end(&mut body)
        .ok()?;
    if u64::try_from(body.len()).ok()? > MAX_EVIDENCE_BYTES {
        return None;
    }
    let evidence: serde_json::Value = serde_json::from_slice(&body).ok()?;
    flow_in_evidence(&evidence)
}

/// The `Flow` row of the card: a fixed label and the word.
pub fn flow_row(flow: DirectFlow) -> String {
    format!("{FLOW_LABEL}{}", flow.word())
}

/// The flow of the zip `pulled` names, when it is the delivered zip and records one: the one read of the zip for a caller that needs the flow
/// itself (`ham pull` adds it to `--json`).
pub fn flow_of_pulled(pulled: &Pulled) -> Option<DirectFlow> {
    (pulled.artifact.kind == ArtifactKind::Zip)
        .then(|| flow_of_zip(&pulled.path))
        .flatten()
}

/// `card` with the flow of a zip that was already read; no flow, no row.
pub fn with_flow_of(card: FinalCard, flow: Option<DirectFlow>) -> FinalCard {
    FinalCard {
        flow: flow.map(|f| f.word().to_string()),
        ..card
    }
}

/// `card` with the `Flow` row of the zip `pulled` names, when it is the delivered zip and records one. Reads the zip once: a caller that
/// draws the card again and again keeps the row instead (the terminal app).
pub fn with_flow(card: FinalCard, pulled: Option<&Pulled>) -> FinalCard {
    with_flow_of(card, pulled.and_then(flow_of_pulled))
}

/// The word of the `Flow` row of the zip at `path`, for a caller that keeps it (the terminal app).
pub fn flow_word_of_zip(path: &Path) -> Option<String> {
    flow_of_zip(path).map(|f| f.word().to_string())
}
