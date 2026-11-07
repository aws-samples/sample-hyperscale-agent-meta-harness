//! The 400 `bad-request` of a request body that does not parse.
//!
//! serde's error text quotes what the caller sent: a wrong-typed value, an unknown member name, an unknown enum word, and the words of a
//! custom `Deserialize` (a repository target). The five routes below used to copy that text into the response. The answer now holds the
//! request type, one fixed phrase for the kind of problem and the place in the body (line and column), and nothing the caller wrote.
//!
//! Every bad body below carries a sentinel. Each case first proves its own trigger: the same body, parsed by the real type, makes serde's
//! text hold the sentinel, so a case that stops triggering fails instead of passing for the wrong reason.
#![allow(clippy::unwrap_used)]

mod chat_support;

use chat_support::{ME, SONNET, World, deps, request, ulid};
use ham_core::runtag::wire::TagEditRequest;
use harness_protocol::api::v1::{InputAnswer, RepoAdminRequest, StartDirectRunRequest};
use harness_protocol::messages::SendMessageRequest;
use serde_json::{Value, json};

/// Text a caller must never see again. It is a value in some cases and a member name in others.
const SECRET: &str = "SENTINEL-value-4c2f";
const NAME: &str = "SENTINEL-name-4c2f";

#[derive(Clone, Copy, Debug)]
enum Site {
    Start,
    Message,
    Repo,
    Input,
    Tags,
}

impl Site {
    /// The type name the 400 starts with.
    fn what(self) -> &'static str {
        match self {
            Site::Start => "StartDirectRunRequest",
            Site::Message => "SendMessageRequest",
            Site::Repo => "RepoAdminRequest",
            Site::Input => "InputAnswer",
            Site::Tags => "TagEditRequest",
        }
    }

    fn path(self) -> String {
        match self {
            Site::Start => "/v1/direct/runs".into(),
            Site::Message => format!("/v1/runs/{}/messages", ulid(1)),
            Site::Repo => "/v1/admin/repos".into(),
            Site::Input => format!("/v1/runs/{}/input", ulid(1)),
            Site::Tags => "/v1/tags/edit".into(),
        }
    }

    /// serde's own error for `body` as this site's type, or `None` when the body parses.
    fn serde_error(self, body: &str) -> Option<serde_json::Error> {
        match self {
            Site::Start => serde_json::from_str::<StartDirectRunRequest>(body).err(),
            Site::Message => serde_json::from_str::<SendMessageRequest>(body).err(),
            Site::Repo => serde_json::from_str::<RepoAdminRequest>(body).err(),
            Site::Input => serde_json::from_str::<InputAnswer>(body).err(),
            Site::Tags => serde_json::from_str::<TagEditRequest>(body).err(),
        }
    }
}

struct Answer {
    status: u16,
    body: Value,
    text: String,
}

impl Answer {
    fn message(&self) -> &str {
        self.body["message"].as_str().unwrap_or_default()
    }
}

async fn send(w: &World, site: Site, body: &str) -> Answer {
    let mut r = request(ME, "POST", &site.path(), &[], body);
    if matches!(site, Site::Start) {
        r.headers.insert("idempotency-key".into(), "k-1".into());
    }
    let mut d = deps(w);
    d.operators = vec![ME.to_string()];
    let out = control_api::handler::handle(r, &d).await;
    Answer {
        status: out.status,
        body: serde_json::from_str(&out.body).unwrap_or(Value::Null),
        text: out.body,
    }
}

/// The 400 for `body`: the trigger is present, the answer is a `bad-request` that holds none of the sentinel, and nothing was touched.
async fn refused(site: Site, what: &str, body: &str) -> Answer {
    let serde = site
        .serde_error(body)
        .expect("the body must not parse")
        .to_string();
    assert!(
        serde.contains("SENTINEL"),
        "{what}: TRIGGER ABSENT, serde's own text holds no sentinel, so this case proves nothing: {serde}"
    );
    let w = World::new();
    let a = send(&w, site, body).await;
    assert_eq!(a.status, 400, "{what}: {}", a.text);
    assert_eq!(a.body["code"], "bad-request", "{what}: {}", a.text);
    assert!(!a.text.contains("SENTINEL"), "{what}: {}", a.text);
    assert!(!a.text.contains("4c2f"), "{what}: {}", a.text);
    assert!(
        a.message().starts_with(&format!("{}: ", site.what())),
        "{what}: {}",
        a.text
    );
    assert!(
        w.calls.lock().unwrap().is_empty(),
        "{what}: a refused body touches no store, blob or execution: {:?}",
        w.calls.lock().unwrap()
    );
    assert!(w.runs.lock().unwrap().is_empty(), "{what}: no run");
    assert!(w.idem.lock().unwrap().is_empty(), "{what}: no claim");
    a
}

fn start_body() -> Value {
    json!({
        "prompt": "Add percent_change(a, b) with tests",
        "target": {"mode": "s3-only"},
        "model": SONNET,
        "rules": []
    })
}

fn start_with(member: &str, value: Value) -> String {
    let mut b = start_body();
    b[member] = value;
    b.to_string()
}

// ---- the five sites ---------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn start_direct_run_request_never_echoes_a_value_or_a_member_name() {
    let cases: Vec<(&str, String)> = vec![
        (
            "a wrong-typed schema in engineConfig",
            start_with(
                "engineConfig",
                json!({"engine": "cline", "schema": SECRET, "settings": {}}),
            ),
        ),
        (
            "an unknown member of engineConfig",
            start_with(
                "engineConfig",
                json!({"engine": "cline", "schema": 1, "settings": {}, NAME: true}),
            ),
        ),
        ("an unknown top-level member", start_with(NAME, json!(1))),
        (
            "an unknown target mode",
            start_with("target", json!({"mode": SECRET})),
        ),
        (
            "a repository target that is not one (a custom Deserialize)",
            start_with("target", json!({"mode": "repo", "repo": SECRET})),
        ),
        (
            "an unknown thirdParty word",
            start_with("thirdParty", json!(SECRET)),
        ),
        (
            "a wrong-typed goal member",
            start_with("goal", json!({"enabled": SECRET})),
        ),
    ];
    for (what, body) in cases {
        refused(Site::Start, what, &body).await;
    }
}

#[tokio::test]
async fn send_message_request_never_echoes_a_value_or_a_member_name() {
    let id = ulid(7);
    let cases: Vec<(&str, String)> = vec![
        (
            "an unknown mode word",
            json!({"messageId": id, "text": "hi", "mode": SECRET}).to_string(),
        ),
        (
            "an unknown member",
            json!({"messageId": id, "text": "hi", NAME: 1}).to_string(),
        ),
        (
            "a wrong-typed confirms",
            json!({"messageId": id, "text": "hi", "confirms": SECRET}).to_string(),
        ),
    ];
    for (what, body) in cases {
        refused(Site::Message, what, &body).await;
    }
}

#[tokio::test]
async fn repo_admin_request_never_echoes_a_value_or_a_member_name() {
    let cases: Vec<(&str, String)> = vec![
        (
            "an unknown action word",
            json!({"action": SECRET, "target": "cc:demo"}).to_string(),
        ),
        (
            "an unknown member",
            json!({"action": "allow", "target": "cc:demo", NAME: 1}).to_string(),
        ),
        (
            "a target that is not one (a custom Deserialize)",
            json!({"action": "allow", "target": SECRET}).to_string(),
        ),
        (
            "a gh target without a slash (a custom Deserialize)",
            json!({"action": "allow", "target": format!("gh:{SECRET}")}).to_string(),
        ),
    ];
    for (what, body) in cases {
        refused(Site::Repo, what, &body).await;
    }
}

#[tokio::test]
async fn input_answer_never_echoes_a_value_or_a_member_name() {
    let id = ulid(9);
    let cases: Vec<(&str, String)> = vec![
        (
            "an unknown action word",
            json!({"requestId": id, "action": SECRET}).to_string(),
        ),
        (
            "an unknown member",
            json!({"requestId": id, "action": "finish", NAME: 1}).to_string(),
        ),
    ];
    for (what, body) in cases {
        refused(Site::Input, what, &body).await;
    }
}

#[tokio::test]
async fn tag_edit_request_never_echoes_a_value() {
    let cases: Vec<(&str, String)> = vec![
        ("edits is a string", json!({"edits": SECRET}).to_string()),
        (
            "add is a string",
            json!({"edits": [{"runId": "01ABC", "add": SECRET}]}).to_string(),
        ),
        (
            "remove is a string",
            json!({"edits": [{"runId": "01ABC", "remove": SECRET}]}).to_string(),
        ),
        (
            "an edit that is a string",
            json!({"edits": [SECRET]}).to_string(),
        ),
    ];
    for (what, body) in cases {
        refused(Site::Tags, what, &body).await;
    }
}

/// The run id of a tag edit is checked after the body parses; its refusal used to print the id it refused (`got "..."`). The id is caller
/// text and a plain-looking id is not a promise, so the 400 names the rule and not the id.
#[tokio::test]
async fn a_refused_run_id_in_a_tag_edit_is_not_repeated() {
    let body = json!({"edits": [{"runId": format!("{SECRET} not/plain"), "add": ["topic:x"]}]})
        .to_string();
    let wire = serde_json::from_str::<TagEditRequest>(&body)
        .unwrap()
        .validate()
        .unwrap_err()
        .to_string();
    assert!(
        wire.contains("SENTINEL"),
        "TRIGGER ABSENT, the validator's own text holds no sentinel: {wire}"
    );
    let w = World::new();
    let a = send(&w, Site::Tags, &body).await;
    assert_eq!(a.status, 400, "{}", a.text);
    assert_eq!(a.body["code"], "bad-request", "{}", a.text);
    assert!(!a.text.contains("SENTINEL"), "{}", a.text);
    assert!(a.message().contains("run id"), "{}", a.text);
    assert!(w.calls.lock().unwrap().is_empty());
}

// ---- the words ---------------------------------------------------------------------------------------------------------------

const SYNTAX: &str = "the body is not valid JSON";
const EOF: &str = "the body ends before the JSON is complete";
const RANGE: &str = "the body holds a number that is out of range";
const MISSING: &str = "a required member is missing";
const UNKNOWN: &str = "the body has a member this API does not know";
const TWICE: &str = "a member appears twice";
const WRONG: &str = "a member has the wrong type or a value this API does not accept";
const OTHER: &str = "the body does not match the request format";

/// What a case expects: a whole message, or a fixed phrase followed by the place serde_json itself reports for that body (1-based line, 1-based
/// column in bytes), read from the real type so the numbers are the parser's and not a guess.
enum Want {
    Whole(&'static str),
    Phrase(&'static str),
}

/// One fixed phrase for each kind of problem, with the place.
#[tokio::test]
async fn each_kind_of_problem_has_one_fixed_phrase_and_a_place() {
    let id = ulid(7);
    // A literal serde_json's float parser overflows on; `json!` cannot build it, so the member is appended to the text.
    let past_range = {
        let mut b = start_body().to_string();
        b.pop();
        format!("{b},\"inputWaitS\":1e999}}")
    };
    let cases: Vec<(Site, &str, String, Want)> = vec![
        (
            Site::Tags,
            "an empty body",
            String::new(),
            Want::Whole("TagEditRequest: the body is empty"),
        ),
        (
            Site::Tags,
            "a blank body",
            "  \n ".into(),
            Want::Whole("TagEditRequest: the body is empty"),
        ),
        (Site::Tags, "not JSON", "nope".into(), Want::Phrase(SYNTAX)),
        (
            Site::Tags,
            "text after the JSON",
            r#"{"edits":[]} x"#.into(),
            Want::Phrase(SYNTAX),
        ),
        (
            Site::Tags,
            "JSON that ends early",
            r#"{"edits":["#.into(),
            Want::Phrase(EOF),
        ),
        (
            Site::Tags,
            "a missing member",
            "{}".into(),
            Want::Phrase(MISSING),
        ),
        (
            Site::Tags,
            "a member twice",
            r#"{"edits":[],"edits":[]}"#.into(),
            Want::Phrase(TWICE),
        ),
        (
            Site::Tags,
            "a wrong type",
            r#"{"edits":"x"}"#.into(),
            Want::Phrase(WRONG),
        ),
        (
            Site::Message,
            "an unknown member",
            json!({"messageId": id, "text": "hi", "wat": 1}).to_string(),
            Want::Phrase(UNKNOWN),
        ),
        (
            Site::Message,
            "an unknown enum word",
            json!({"messageId": id, "text": "hi", "mode": "later"}).to_string(),
            Want::Phrase(WRONG),
        ),
        (
            Site::Repo,
            "a target of a custom Deserialize",
            json!({"action": "allow", "target": "nope"}).to_string(),
            Want::Phrase(OTHER),
        ),
        (
            Site::Start,
            "a number past the range of a float",
            past_range,
            Want::Phrase(RANGE),
        ),
    ];
    for (site, what, body, want) in cases {
        let expected = match want {
            Want::Whole(m) => m.to_string(),
            Want::Phrase(p) => {
                let e = site.serde_error(&body).expect("the body must not parse");
                format!(
                    "{}: {p} (line {}, column {})",
                    site.what(),
                    e.line(),
                    e.column()
                )
            }
        };
        let w = World::new();
        let a = send(&w, site, &body).await;
        assert_eq!(a.status, 400, "{what}: {}", a.text);
        assert_eq!(a.body["code"], "bad-request", "{what}: {}", a.text);
        assert_eq!(a.message(), expected, "{what}: {}", a.text);
    }
}

/// A caller controls the size of a body and the bytes in it. A name or a value of about a mebibyte, text of three-byte characters and a
/// nesting deeper than the parser reads are each answered with the same short, plain-ASCII text as a small body: the fixed phrase of the kind
/// and the place, and none of the caller's bytes. Where serde's own text holds the caller's text, the case first proves it (the trigger), so a
/// case that stops echoing in serde fails instead of passing for the wrong reason.
#[tokio::test]
async fn a_huge_or_multi_byte_body_gets_the_same_short_answer() {
    // 350,000 euro signs are 1,050,000 bytes: about a mebibyte of three-byte characters.
    let long = "\u{20ac}".repeat(350_000);
    // Characters of one, two, three and four bytes around the sentinel.
    let mixed = "a\u{e9}\u{20ac}\u{1d11e}-SENTINEL-name-4c2f";
    let id = ulid(7);
    // The settings of an engine are a free JSON value, so the parser's depth limit (128) is what stops this body; the body is never closed.
    let deep = {
        let mut b = start_body().to_string();
        b.pop();
        format!(
            "{b},\"engineConfig\":{{\"engine\":\"cline\",\"schema\":1,\"settings\":{{\"a\":{}",
            "[".repeat(100_000)
        )
    };
    // (site, case, body, phrase, a piece of the caller's text that serde's own error must hold for this case to prove anything)
    let cases: Vec<(Site, &str, String, &str, Option<&str>)> = vec![
        (
            Site::Start,
            "an unknown member whose name is about a mebibyte",
            start_with(&long, json!(1)),
            UNKNOWN,
            Some("\u{20ac}\u{20ac}\u{20ac}"),
        ),
        (
            Site::Start,
            "an unknown member whose name holds characters of every width",
            start_with(mixed, json!(1)),
            UNKNOWN,
            Some("\u{1d11e}-SENTINEL"),
        ),
        (
            Site::Message,
            "an enum word of characters of every width",
            json!({"messageId": id, "text": "hi", "mode": mixed}).to_string(),
            WRONG,
            Some("\u{1d11e}-SENTINEL"),
        ),
        (
            Site::Start,
            "a body that is one string of about a mebibyte",
            format!("\"{long}\""),
            WRONG,
            Some("\u{20ac}\u{20ac}\u{20ac}"),
        ),
        (
            Site::Tags,
            "a list that is a string of about a mebibyte",
            json!({"edits": long}).to_string(),
            WRONG,
            Some("\u{20ac}\u{20ac}\u{20ac}"),
        ),
        (
            Site::Repo,
            "a target of a custom Deserialize that is about a mebibyte",
            json!({"action": "allow", "target": long}).to_string(),
            OTHER,
            Some("\u{20ac}\u{20ac}\u{20ac}"),
        ),
        (
            Site::Tags,
            "a string that never ends",
            format!("{{\"edits\":\"{long}"),
            EOF,
            None,
        ),
        (
            Site::Start,
            "arrays nested 100,000 deep in an engine setting",
            deep,
            SYNTAX,
            None,
        ),
    ];
    for (site, what, body, phrase, marker) in cases {
        let serde = site.serde_error(&body).expect("the body must not parse");
        if let Some(piece) = marker {
            assert!(
                serde.to_string().contains(piece),
                "{what}: TRIGGER ABSENT, serde's own text holds none of the caller's text, so this case proves nothing"
            );
        }
        let expected = format!(
            "{}: {phrase} (line {}, column {})",
            site.what(),
            serde.line(),
            serde.column()
        );
        let w = World::new();
        let a = send(&w, site, &body).await;
        // A failure prints the start of the answer only: an answer that echoes the body is a mebibyte long.
        let shown: String = a.text.chars().take(300).collect();
        assert!(
            a.text.len() < 400,
            "{what}: the answer is {} bytes and starts: {shown}",
            a.text.len()
        );
        assert_eq!(a.status, 400, "{what}: {shown}");
        assert_eq!(a.body["code"], "bad-request", "{what}: {shown}");
        assert_eq!(a.message(), expected, "{what}");
        assert!(
            a.text.is_ascii(),
            "{what}: a byte of the caller's text reached the answer: {shown}"
        );
        assert!(!a.text.contains("SENTINEL"), "{what}: {shown}");
        assert!(
            w.calls.lock().unwrap().is_empty(),
            "{what}: a refused body touches no store, blob or execution"
        );
    }
}

// ---- the next copy -----------------------------------------------------------------------------------------------------------

/// The calls that read JSON from a string, a slice or a reader.
const JSON_READERS: [&str; 3] = ["from_str", "from_slice", "from_reader"];

/// The 1-based numbers of the lines of `text` that parse a request body by itself: a call of `from_str`, `from_slice` or `from_reader` whose
/// arguments mention `body` (`req.body`, `r.body`, `body`, `request_body`). A turbofish and a call wrapped over several lines count as well.
/// The name `body` is all the scan can read, so `serde_json::from_slice(&bytes)` of a cursor or a stored object is left alone, and so is a
/// function whose name only begins like a reader (`from_str_radix`). A body copied into a variable whose name lacks `body` before it is
/// parsed, a renamed import (`use serde_json::from_str as decode`) and a comment between the name and the parenthesis are not found.
fn parses_a_body_itself(text: &str) -> Vec<usize> {
    let bytes = text.as_bytes();
    let ident = |at: usize| {
        bytes
            .get(at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    };
    let after_space = |mut at: usize| {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        at
    };
    let mut found = Vec::new();
    for reader in JSON_READERS {
        for (start, _) in text.match_indices(reader) {
            let end = start + reader.len();
            if (start > 0 && ident(start - 1)) || ident(end) {
                continue;
            }
            // `::<T>` between the name and the arguments; the angle brackets nest (`Vec<Edit>`).
            let mut at = after_space(end);
            if text[at..].starts_with("::") {
                at = after_space(at + 2);
                if bytes.get(at) != Some(&b'<') {
                    continue;
                }
                let mut depth = 0_usize;
                while let Some(b) = bytes.get(at) {
                    // The arrow of a closure type (`Fn() -> T`) is not a closing bracket.
                    let arrow = at > 0 && bytes[at - 1] == b'-';
                    at += 1;
                    match b {
                        b'<' => depth += 1,
                        b'>' if !arrow => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                at = after_space(at);
            }
            if bytes.get(at) != Some(&b'(') {
                continue;
            }
            // The arguments run to the matching parenthesis, or to the end of the text when there is none.
            let open = at + 1;
            let mut depth = 1_usize;
            let mut close = text.len();
            for (i, b) in bytes.iter().enumerate().skip(open) {
                match b {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            close = i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if text[open..close].to_ascii_lowercase().contains("body") {
                found.push(1 + text[..start].matches('\n').count());
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Every `.rs` file under `dir`, the sub-directories included.
fn rust_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Every route that reads a request body goes through `body::parse_body`. A new handler that parses `req.body` itself and prints serde's
/// error would bring the echo back, so the source is searched for the idiom.
#[test]
fn no_handler_parses_the_request_body_by_itself() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    for path in rust_files(&dir) {
        if path.ends_with("body.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for n in parses_a_body_itself(&text) {
            offenders.push(format!("{}:{}: {}", path.display(), n, lines[n - 1].trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "parse a request body with body::parse_body, which never repeats the caller's text. The scan reads the name `body`: bytes that are \
         not a request body (an S3 object, say) are bound to a name without it before they are parsed:\n{}",
        offenders.join("\n")
    );
}

/// The scan of `code` written inside a function, so the first line of `code` is line 2.
fn scan_inside_a_function(code: &str) -> Vec<usize> {
    parses_a_body_itself(&format!("fn handle() {{\n{code}\n}}\n"))
}

/// The scan has to be able to fail. Each text below parses a request body by itself, in a way a handler could write, and the scan names the
/// line of the call in each. The wrapped forms are what `cargo fmt` makes of a long call.
#[test]
fn the_scan_flags_every_way_to_parse_a_body_by_itself() {
    let forms: [(&str, &str); 13] = [
        (
            "the plain form",
            "let r: T = serde_json::from_str(&req.body)?;",
        ),
        (
            "a slice of the body",
            "let r: T = serde_json::from_slice(req.body.as_bytes())?;",
        ),
        (
            "a turbofish",
            "let r = serde_json::from_str::<StartDirectRunRequest>(&req.body);",
        ),
        (
            "a turbofish with a generic type",
            "let r = serde_json::from_str::<Vec<Edit>>(&req.body);",
        ),
        (
            "a turbofish with the arrow of a closure type",
            "let r = serde_json::from_str::<Box<dyn Fn() -> T>>(&req.body);",
        ),
        (
            "another variable name",
            "let r: T = serde_json::from_str(&r.body)?;",
        ),
        ("a bare name", "let r: T = serde_json::from_str(body)?;"),
        (
            "a longer name",
            "let r: T = serde_json::from_slice(&request_body)?;",
        ),
        (
            "a reader",
            "let r: T = serde_json::from_reader(req.body.as_bytes())?;",
        ),
        (
            "a space before the parenthesis",
            "let r: T = serde_json::from_str (&req.body)?;",
        ),
        (
            "the argument on the next line",
            "let r: T = serde_json::from_str(\n    &req.body,\n)?;",
        ),
        (
            "the call split before the turbofish",
            "let r = serde_json::from_str\n    ::<T>(&req.body);",
        ),
        (
            "the turbofish wrapped",
            "let r = serde_json::from_str::<\n    T,\n>(&req.body);",
        ),
    ];
    let missed: Vec<&str> = forms
        .iter()
        .filter(|(_, code)| scan_inside_a_function(code) != [2])
        .map(|(what, _)| *what)
        .collect();
    assert!(missed.is_empty(), "the scan misses: {missed:?}");
}

/// The scan must not flag what is not a request body: the cursor, a stored object, a value, the parser of the body itself and a body that is
/// only measured.
#[test]
fn the_scan_leaves_alone_what_is_not_a_request_body() {
    let fine: [(&str, &str); 8] = [
        (
            "the parser of the body",
            "let r: T = match parse_body(\"T\", &req.body) { Ok(r) => r, Err(e) => return e };",
        ),
        (
            "a cursor",
            "let t: PageToken = serde_json::from_slice(&bytes)?;",
        ),
        (
            "a stored object",
            "let v: Value = serde_json::from_slice(&object.bytes)?;",
        ),
        (
            "a value",
            "let p: Payload = serde_json::from_value(value.clone())?;",
        ),
        (
            "a longer function name",
            "let n = u32::from_str_radix(body, 16)?;",
        ),
        ("a function of another name", "let n = my_from_str(body);"),
        ("a body that is measured", "let n = req.body.len();"),
        (
            "a parser far from the body",
            "let t: T = serde_json::from_slice(&bytes)?;\nlet n = req.body.len();",
        ),
    ];
    let flagged: Vec<&str> = fine
        .iter()
        .filter(|(_, code)| !scan_inside_a_function(code).is_empty())
        .map(|(what, _)| *what)
        .collect();
    assert!(flagged.is_empty(), "the scan flags: {flagged:?}");
}

/// The scan reads the whole source tree of the crate, the sub-directories too.
#[test]
fn the_scan_reads_the_subdirectories_of_src() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_files(&dir);
    assert!(
        files.iter().any(|p| p.ends_with("handler.rs")),
        "a file of src: {files:?}"
    );
    assert!(
        files.iter().any(|p| p.ends_with("cost/mod.rs")),
        "a file of a sub-directory of src: {files:?}"
    );
}
