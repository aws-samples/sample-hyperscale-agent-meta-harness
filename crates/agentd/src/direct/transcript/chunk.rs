//! The chunk object of the stored conversation: a gzip stream whose first line is a header and whose other lines
//! are record lines, one JSON object each. The chunks sit in a prefix the engine can write, so the read route trusts nothing in them;
//! this module makes exactly the objects the reader accepts (`ham_core::transcript::decode_chunk`: at most 1 MiB compressed and 4 MiB
//! inflated, version 1, a header seq equal to the key's, a ULID incarnation, a UTC time, short labels, record numbers strictly
//! increasing inside the header's range) and reads a header back from the start of an object for the writer's resume.

use crate::direct::transcript::pipeline::Line;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use harness_protocol::common::{is_ulid, is_utc_timestamp};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// The largest object the reader fetches.
pub const MAX_OBJECT_BYTES: usize = 1 << 20;
/// The largest body (header and records, before compression) a chunk may hold: the reader inflates at most four times this.
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// The format version of the header.
pub const CHUNK_VERSION: u32 = 1;
/// The most records one chunk holds.
pub const MAX_RECORDS: usize = 10_000;
/// The longest label (engine, protocol) a header carries.
pub const MAX_LABEL_CHARS: usize = 32;
/// A header is read from at most this many inflated bytes.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// The first line of a chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub seq: u64,
    /// The writer's process identifier (a ULID).
    pub incarnation: String,
    pub first_rec: u64,
    pub last_rec: u64,
    /// RFC 3339 UTC.
    pub created_at: String,
    /// The engine's display label and the protocol version of the writer.
    pub engine: String,
    pub protocol: String,
    /// The writer ended cleanly with this object: nothing follows from this incarnation.
    pub closed: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeaderWire {
    v: u32,
    seq: u64,
    incarnation: String,
    first_rec: u64,
    last_rec: u64,
    created_at: String,
    engine: String,
    protocol: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    closed: bool,
}

/// Why a chunk was not made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("the header is invalid")]
    BadHeader,
    #[error("record {n} is outside the header's range")]
    RecordOutsideRange { n: u64 },
    #[error("record {n} is out of order")]
    RecordOrder { n: u64 },
    #[error("the chunk would hold {bytes} bytes")]
    TooLarge { bytes: usize },
    #[error("compression failed")]
    Gzip,
}

fn label(s: &str, fallback: &str) -> String {
    let cut: String = s.chars().take(MAX_LABEL_CHARS).collect();
    if cut.is_empty() {
        fallback.to_string()
    } else {
        cut
    }
}

/// The gzip object for a header and its records. A label is cut or replaced to fit; a header that disagrees with its records, a body
/// over [`MAX_BODY_BYTES`] and an object over [`MAX_OBJECT_BYTES`] are refused, so the writer never makes what the reader would refuse.
pub fn encode(header: &Header, lines: &[Line]) -> Result<Vec<u8>, ChunkError> {
    if header.first_rec > header.last_rec
        || !is_ulid(&header.incarnation)
        || !is_utc_timestamp(&header.created_at)
        || lines.len() > MAX_RECORDS
    {
        return Err(ChunkError::BadHeader);
    }
    let mut prev: Option<u64> = None;
    for l in lines {
        if l.n < header.first_rec || l.n > header.last_rec {
            return Err(ChunkError::RecordOutsideRange { n: l.n });
        }
        if prev.is_some_and(|p| l.n <= p) {
            return Err(ChunkError::RecordOrder { n: l.n });
        }
        prev = Some(l.n);
    }
    let wire = HeaderWire {
        v: CHUNK_VERSION,
        seq: header.seq,
        incarnation: header.incarnation.clone(),
        first_rec: header.first_rec,
        last_rec: header.last_rec,
        created_at: header.created_at.clone(),
        engine: label(&header.engine, "engine"),
        protocol: label(&header.protocol, "unknown"),
        closed: header.closed,
    };
    let head = serde_json::to_vec(&wire).map_err(|_| ChunkError::BadHeader)?;
    let body: usize = head.len() + 1 + lines.iter().map(|l| l.bytes.len() + 1).sum::<usize>();
    if body > MAX_BODY_BYTES {
        return Err(ChunkError::TooLarge { bytes: body });
    }
    let mut enc = GzEncoder::new(Vec::with_capacity(body / 4 + 64), Compression::default());
    let mut write = |bytes: &[u8]| -> Result<(), ChunkError> {
        enc.write_all(bytes)
            .and_then(|()| enc.write_all(b"\n"))
            .map_err(|_| ChunkError::Gzip)
    };
    write(&head)?;
    for l in lines {
        write(&l.bytes)?;
    }
    let out = enc.finish().map_err(|_| ChunkError::Gzip)?;
    if out.len() > MAX_OBJECT_BYTES {
        return Err(ChunkError::TooLarge { bytes: out.len() });
    }
    Ok(out)
}

/// The header read from the start of a gzip object (a prefix is enough: the header is the first line), or `None` when the bytes are not
/// gzip, the first line is not a header of this format, or no line end comes within the first 8 KiB inflated.
pub fn decode_header(gz_prefix: &[u8]) -> Option<Header> {
    let mut dec = GzDecoder::new(gz_prefix);
    let mut inflated: Vec<u8> = Vec::new();
    let mut buf = [0u8; 1024];
    // A prefix ends inside the stream: what was inflated before the end is kept.
    while inflated.len() < MAX_HEADER_BYTES && !inflated.contains(&b'\n') {
        match dec.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => inflated.extend_from_slice(&buf[..n]),
        }
    }
    let end = inflated.iter().position(|b| *b == b'\n')?;
    let wire: HeaderWire = serde_json::from_slice(&inflated[..end]).ok()?;
    if wire.v != CHUNK_VERSION
        || wire.first_rec > wire.last_rec
        || !is_ulid(&wire.incarnation)
        || !is_utc_timestamp(&wire.created_at)
    {
        return None;
    }
    Some(Header {
        seq: wire.seq,
        incarnation: wire.incarnation,
        first_rec: wire.first_rec,
        last_rec: wire.last_rec,
        created_at: wire.created_at,
        engine: wire.engine,
        protocol: wire.protocol,
        closed: wire.closed,
    })
}
