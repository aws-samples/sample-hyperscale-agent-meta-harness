//! The size of a direct-run start request.
//!
//! A start request travels as one synchronous call to the control-api Lambda, and Lambda takes 6 MiB (6,291,456 bytes) per synchronous request
//! (the Lambda quotas page of the AWS documentation counts 1 MB as 1,024 KB). That is the one hard bound on the prompt, the rules, the skills, the
//! attachments and the two prompt files together; every smaller cap on a single member was an invention and is gone. This module holds the
//! bound once, so `ham` (before it sends), control-api (a stale or hand-made client) and the disk readers of `ham-core` name the same number.
//!
//! The Lambda counts the proxy event, in which the body is one JSON string: every quote and backslash costs two bytes, a control character two or
//! six, and the headers, the request context and the path come around it. [`event_body_bytes`] counts the first, and
//! [`REQUEST_ENVELOPE_MARGIN_BYTES`] reserves room for the second: two copies of at most 10,240 bytes of header values (`headers` and
//! `multiValueHeaders`), each doubled if every byte were escaped, plus a request context of a few KiB, which is about 50 KiB in the worst case,
//! so 64 KiB is enough.
//!
//! Assumption, pinned by the live test plan of the change that made it: API Gateway writes a character outside ASCII into the event as UTF-8,
//! not as a `\uXXXX` escape, and the REST API has no binary media type, so the body is not base64. If a live request shows otherwise,
//! [`event_body_bytes`] is the one place to change.
//!
//! Everything here is re-exported from [`crate::direct`].

use crate::api::v1::StartDirectRunRequest;
use std::fmt;

/// What the Lambda that serves the front door takes in one synchronous request, in bytes (6 MiB).
pub const PLATFORM_REQUEST_BYTES: usize = 6 * 1024 * 1024;
/// What is kept free of [`PLATFORM_REQUEST_BYTES`] for the proxy event around the body (headers, request context, path).
pub const REQUEST_ENVELOPE_MARGIN_BYTES: usize = 64 * 1024;
/// The most a start request may weigh, counted by [`event_body_bytes`] on its JSON body.
pub const MAX_REQUEST_BYTES: usize = PLATFORM_REQUEST_BYTES - REQUEST_ENVELOPE_MARGIN_BYTES;

/// The most rule files in one run. A count stays because `summary.json` lists every rule file (name, hash and size) and is held to 64 KiB: at
/// 100 files with the longest names the list takes about 36 KiB (`tests/b776_input_limits.rs` builds the worst case), and a count past that would
/// leave the end-of-run record unwritable. The size of a rule file has no bound of its own.
pub const MAX_RULE_FILES: usize = 100;
/// The longest rule file name, in bytes, `.md` included: the longest name a file system takes (255 bytes on ext4 and APFS).
pub const MAX_RULE_NAME_BYTES: usize = 255;

/// The size the JSON text `body` has inside the Lambda proxy event, where it is one JSON string: `"` and `\` cost two bytes, a control
/// character two (`\b`, `\f`, `\n`, `\r`, `\t`) or six (`\u00XX`), everything else its own length (a character outside ASCII counts as its
/// UTF-8 bytes). Three characters that some JSON writers escape although JSON does not require it are counted as escaped, because they almost never
/// occur in a prompt and the count costs nothing: DEL (`\u007f`, six bytes) and the line and paragraph separators U+2028 and U+2029 (`\u2028`, six
/// bytes instead of three). Whether `/` and the characters `< > & '` are escaped too is for the live test to show; they are common in code, so they
/// are counted as they are until it does.
pub fn event_body_bytes(body: &[u8]) -> usize {
    let mut total = 0;
    let mut i = 0;
    while i < body.len() {
        let b = body[i];
        total += match b {
            b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => 2,
            0x00..=0x1f | 0x7f => 6,
            0xe2 if matches!(body.get(i + 1..i + 3), Some([0x80, 0xa8 | 0xa9])) => {
                // U+2028 and U+2029 are `e2 80 a8` and `e2 80 a9`: one count for the three bytes.
                i += 2;
                6
            }
            _ => 1,
        };
        i += 1;
    }
    total
}

/// `n` with a comma between thousands: `6,225,920`.
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The limit in words, for a refusal and for a file that could never be sent: the number and where it comes from.
pub fn bound_words() -> String {
    format!(
        "{} bytes (the AWS Lambda limit of {} MiB per call, less {} KiB for headers)",
        group(MAX_REQUEST_BYTES),
        PLATFORM_REQUEST_BYTES / (1024 * 1024),
        REQUEST_ENVELOPE_MARGIN_BYTES / 1024
    )
}

/// Whether `raw_bytes` of one file (or the sum of several) could never be sent: more than a whole request carries.
pub fn exceeds_request(raw_bytes: u64) -> bool {
    raw_bytes > MAX_REQUEST_BYTES as u64
}

/// A start request over [`MAX_REQUEST_BYTES`]: the size, the bound and the part that weighs most.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestTooLarge {
    /// The size of the request in the event, in bytes.
    pub bytes: usize,
    /// [`MAX_REQUEST_BYTES`].
    pub max: usize,
    /// The member that weighs most, in words a person knows (`the prompt`, `the attachments`, `the rules`).
    pub largest: String,
    /// The event bytes of that member.
    pub largest_bytes: usize,
    /// For a member that is a list of files: the name and the event bytes of the largest one.
    pub largest_item: Option<(String, usize)>,
}

impl fmt::Display for RequestTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = self
            .largest_item
            .as_ref()
            .map_or_else(String::new, |(name, bytes)| {
                format!(" (the largest file is {name}, {} bytes)", group(*bytes))
            });
        write!(
            f,
            "the request is {} bytes; the limit is {}, {} bytes over. Quotes, backslashes and line breaks count twice. The largest part is {}, {} bytes{file}. Shorten it or send less.",
            group(self.bytes),
            bound_words(),
            group(self.bytes.saturating_sub(self.max)),
            self.largest,
            group(self.largest_bytes)
        )
    }
}

impl std::error::Error for RequestTooLarge {}

/// The words a person knows for a member of the request.
fn member_words(key: &str) -> String {
    match key {
        "prompt" => "the prompt".to_string(),
        "rules" => "the rules".to_string(),
        "skills" => "the skills".to_string(),
        "attachments" => "the attachments".to_string(),
        "nudge" => "the continue file".to_string(),
        "review" => "the review file".to_string(),
        other => format!("the {other} setting"),
    }
}

fn json_event_bytes(v: &serde_json::Value) -> usize {
    event_body_bytes(&serde_json::to_vec(v).unwrap_or_default())
}

/// Where the weight is, for a request already known to be over: the member that weighs most and, for a list of files, the largest file.
fn largest_part(request: &StartDirectRunRequest) -> (String, usize, Option<(String, usize)>) {
    // A request always serializes to an object that holds a prompt; the seed names the prompt only for the object that is not there.
    let value = serde_json::to_value(request).unwrap_or_default();
    let nothing = serde_json::Value::Null;
    let mut best: (&str, &serde_json::Value, usize) = ("prompt", &nothing, 0);
    for (key, member) in value.as_object().into_iter().flatten() {
        let bytes = json_event_bytes(member);
        if bytes > best.2 {
            best = (key, member, bytes);
        }
    }
    let (key, value, bytes) = best;
    let item = value.as_array().and_then(|items| {
        items
            .iter()
            .map(|i| {
                let name = i
                    .get("name")
                    .or_else(|| i.get("path"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("?")
                    .chars()
                    .take(80)
                    .map(|c| if c.is_control() { '?' } else { c })
                    .collect::<String>();
                (name, json_event_bytes(i))
            })
            .max_by_key(|(_, b)| *b)
    });
    (member_words(key), bytes, item)
}

/// Checks the JSON `body` of `request` (what is sent) against [`MAX_REQUEST_BYTES`]. On a request over the bound the error names the part that
/// weighs most. Nothing else is judged here: the members have their own checks, and none of them has a size of its own.
pub fn check_request_body(
    body: &[u8],
    request: &StartDirectRunRequest,
) -> Result<(), RequestTooLarge> {
    let bytes = event_body_bytes(body);
    if bytes <= MAX_REQUEST_BYTES {
        return Ok(());
    }
    let (largest, largest_bytes, largest_item) = largest_part(request);
    Err(RequestTooLarge {
        bytes,
        max: MAX_REQUEST_BYTES,
        largest,
        largest_bytes,
        largest_item,
    })
}

/// [`check_request_body`] on the canonical JSON of `request` (what control-api stores as `request.json`).
pub fn check_request(request: &StartDirectRunRequest) -> Result<(), RequestTooLarge> {
    let body = serde_json::to_vec(request).unwrap_or_default();
    check_request_body(&body, request)
}
