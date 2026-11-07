//! The words of what `ham` prints: the real binary runs the commands a person reads the output of, against the local stand-in of
//! the `/v1` API, and no line of any stream carries a backlog id, a decision record, an attribution to the people who built the product or a
//! date of the project. The help texts of the command registry are read the same way, without a process.
//!
#![allow(clippy::unwrap_used)]

mod support;

use serde_json::Value;
use std::path::PathBuf;
use std::process::Output;
use support::{AKID, Fake, SECRET, TOKEN};

const RULE_IDS: [&str; 13] = [
    "backlog-id",
    "adr-id",
    "decision-id",
    "owner-id",
    "criteria-id",
    "evidence-id",
    "spec-id",
    "owner-attribution",
    "release-candidate",
    "project-date",
    "line-name",
    "person-name",
    "review-process",
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The byte offsets where `needle` starts at the start of a word.
fn word_starts<'a>(text: &'a str, needle: &'a str) -> impl Iterator<Item = usize> + 'a {
    text.match_indices(needle)
        .map(|(i, _)| i)
        .filter(|&i| text[..i].chars().next_back().is_none_or(|c| !is_word(c)))
}

/// The number of ASCII digits at the start of `s`.
fn digits(s: &str) -> usize {
    s.chars().take_while(char::is_ascii_digit).count()
}

/// True when `s` is empty or starts with a character that cannot continue a word.
fn word_ends(s: &str) -> bool {
    s.chars().next().is_none_or(|c| !is_word(c))
}

fn numbered(text: &str, prefix: &str, min: usize, max: usize) -> bool {
    word_starts(text, prefix).any(|i| {
        let Some(rest) = text[i + prefix.len()..].strip_prefix('-') else {
            return false;
        };
        let n = digits(rest);
        (min..=max).contains(&n) && word_ends(&rest[n..])
    })
}

fn adr(text: &str) -> bool {
    word_starts(text, "ADR").any(|i| {
        let rest = &text[i + 3..];
        let rest = rest.strip_prefix('s').unwrap_or(rest);
        let rest = rest.strip_prefix([' ', '-']).unwrap_or(rest);
        let n = digits(rest);
        (2..=4).contains(&n) && word_ends(&rest[n..])
    })
}

fn decision(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    word_starts(&low, "decision").any(|i| {
        let rest = &low[i + 8..];
        let rest = rest.strip_prefix('s').unwrap_or(rest);
        let Some(rest) = rest.strip_prefix(' ') else {
            return false;
        };
        digits(rest) == 4 && word_ends(&rest[4..])
    })
}

/// `EV-` and a letter or digit.
fn evidence(text: &str) -> bool {
    word_starts(text, "EV-").any(|i| {
        text[i + 3..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// A date `dddd-dd-dd` that does not continue a longer word or a path, with its year.
fn dates(text: &str) -> Vec<(usize, u32)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 10 <= b.len() {
        let w = &b[i..i + 10];
        let shaped = w.iter().enumerate().all(|(k, c)| match k {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        });
        let after_ok = b
            .get(i + 10)
            .is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'_');
        let before_ok = i == 0
            || !matches!(b[i - 1], b'/' | b'-' | b'_' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z');
        if shaped && after_ok && before_ok {
            let year = text[i..i + 4].parse::<u32>().unwrap_or(0);
            out.push((i, year));
            i += 10;
        } else {
            i += 1;
        }
    }
    out
}

/// A date in a year the project lived in: 2026 or later.
fn project_date(text: &str) -> bool {
    dates(text).iter().any(|(_, y)| (2026..2100).contains(y))
}

/// `maintainer order`, `maintainer decision` and the like, or the word owner within 60 bytes of a date.
fn owner_attribution(text: &str) -> bool {
    const KINDS: [&str; 6] = [
        "order",
        "decision",
        "feedback",
        "direction",
        "reminder",
        "clarification",
    ];
    let low = text.to_ascii_lowercase();
    let owners: Vec<usize> = word_starts(&low, "owner").collect();
    let kind = owners.iter().any(|&i| {
        let rest = &low[i + 5..];
        let rest = rest
            .strip_prefix("'s")
            .or_else(|| rest.strip_prefix("\u{2019}s"))
            .unwrap_or(rest);
        let trimmed = rest.trim_start();
        trimmed.len() < rest.len()
            && KINDS.iter().any(|k| {
                trimmed.strip_prefix(k).is_some_and(|after| {
                    let after = after.strip_prefix('s').unwrap_or(after);
                    word_ends(after)
                })
            })
    });
    let near = owners.iter().any(|&o| {
        dates(text)
            .iter()
            .any(|&(d, _)| o.abs_diff(d) <= 60 && !text[o.min(d)..o.max(d)].contains('\n'))
    });
    kind || near
}

fn release_candidate(text: &str) -> bool {
    let rc = word_starts(text, "rc").any(|i| {
        let rest = &text[i + 2..];
        let n = digits(rest);
        if n == 0 {
            return false;
        }
        let rest = &rest[n..];
        let rest = rest
            .strip_prefix(|c: char| c.is_ascii_lowercase())
            .unwrap_or(rest);
        word_ends(rest)
    });
    rc || text.to_ascii_lowercase().contains("release candidate")
}

// The words of the authors that the rules look for, spelled from pieces. The public export replaces these two items with neutral
// stand-ins, so keep them together and unchanged in form.
const LINE_NAME: &str = "example-line";
const PEOPLE: [&str; 3] = ["Alice", "Bob", "Carol"];

/// The name of the review process that the shipped words must not mention.
fn review_process(text: &str) -> bool {
    let low = text.to_ascii_lowercase().replace(' ', "");
    low.contains("audrec")
}

/// The name of the internal line of the product.
fn line_name(text: &str) -> bool {
    let low = text.to_ascii_lowercase();
    word_starts(&low, LINE_NAME).any(|i| word_ends(&low[i + LINE_NAME.len()..]))
}

/// The first and last names of the people who built the product.
fn person_name(text: &str) -> bool {
    PEOPLE
        .iter()
        .any(|n| word_starts(text, n).any(|i| word_ends(&text[i + n.len()..])))
}

/// The names of the rules that `text` breaks.
fn hits(text: &str) -> Vec<&'static str> {
    let checks: [(&str, bool); 13] = [
        ("backlog-id", numbered(text, "B", 1, 4)),
        ("adr-id", adr(text)),
        ("decision-id", decision(text)),
        ("owner-id", numbered(text, "O", 2, 4)),
        ("criteria-id", numbered(text, "C", 2, 4)),
        ("evidence-id", evidence(text)),
        (
            "spec-id",
            ["FR", "NFR", "AC", "SPEC"]
                .iter()
                .any(|p| numbered(text, p, 1, 9)),
        ),
        ("owner-attribution", owner_attribution(text)),
        ("release-candidate", release_candidate(text)),
        ("project-date", project_date(text)),
        ("line-name", line_name(text)),
        ("person-name", person_name(text)),
        ("review-process", review_process(text)),
    ];
    let mut out = Vec::new();
    for (name, broken) in checks {
        if broken {
            let id = RULE_IDS.iter().find(|r| **r == name).unwrap();
            out.push(*id);
        }
    }
    out
}

fn id(prefix: &str, n: u32) -> String {
    format!("{prefix}-{n}")
}

/// A date `yyyy-mm-dd`, built from its numbers for the same reason.
fn date(y: u32, m: u32, d: u32) -> String {
    format!("{y}-{m:02}-{d:02}")
}

#[test]
fn the_rules_find_their_words_and_leave_the_vocabulary_alone() {
    let adr = "ADR";
    let positives: Vec<(String, &str)> = vec![
        (
            format!("gh: out of scope for now ({})", id("B", 136)),
            "backlog-id",
        ),
        (
            format!("is delivered by GitLab CI ({adr} 0010 D4)"),
            "adr-id",
        ),
        (format!("see {adr}-0045 and {adr}s 0001"), "adr-id"),
        (
            format!("{} 0224 says", ["deci", "sion"].concat()),
            "decision-id",
        ),
        (format!("per {}", id("O", 481)), "owner-id"),
        (format!("criteria C-{:04}", 15), "criteria-id"),
        (format!("evidence {}-A7-12", "EV"), "evidence-id"),
        (
            format!(
                "{} and {} and {} and {}",
                id("FR", 36),
                id("AC", 20),
                id("NFR", 3),
                id("SPEC", 1)
            ),
            "spec-id",
        ),
        (
            format!("(owner {}, why)", date(2026, 10, 8)),
            "owner-attribution",
        ),
        (
            format!("built in {0}3 and {0}3b", "rc"),
            "release-candidate",
        ),
        (
            format!("the {} {}", "release", "candidate"),
            "release-candidate",
        ),
        (format!("since {}", date(2026, 10, 8)), "project-date"),
        (format!("the {LINE_NAME} stage"), "line-name"),
        (
            format!("asked {} and {}'s branch", PEOPLE[2], PEOPLE[1]),
            "person-name",
        ),
        (format!("{} wrote it", PEOPLE[0]), "person-name"),
        (
            "allow it with an audrec record ID".to_string(),
            "review-process",
        ),
    ];
    for (text, want) in &positives {
        assert!(hits(text).contains(want), "{text:?} should break {want}");
    }
    let negatives: Vec<String> = vec![
        "the repository owner and the run owner".into(),
        "owner-only reads of a run".into(),
        "a B-tree and an AB-1 board".into(),
        format!("{}burg and {}sdale", PEOPLE[1], PEOPLE[2]),
        "src, .bashrc, rc and the rc.d folder".into(),
        format!("{} 12 of 20 and {adr}", ["deci", "sion"].concat()),
        "a C-style cast, a C-3PO and C-1".into(),
        "SPEC-driven and FR-A".into(),
        "EV charging".into(),
        format!(
            "the MCP versions {} and {}",
            date(2025, 11, 25),
            date(2012, 10, 17)
        ),
        format!(
            "https://static.example.test/dist/{}/tool.tar.xz",
            date(2026, 9, 3)
        ),
        id("B", 12345),
    ];
    for text in &negatives {
        assert_eq!(hits(text), Vec::<&str>::new(), "{text:?}");
    }
}

#[test]
fn the_doctor_line_that_started_the_item_breaks_three_rules() {
    let line = format!(
        "cc: iam (repo-gateway role credentials; no git secret) | gh: out of scope for now (owner {}, {}) | gl: not served ({})",
        date(2026, 10, 8),
        id("B", 136),
        id("B", 137)
    );
    assert_eq!(
        hits(&line),
        vec!["backlog-id", "owner-attribution", "project-date"]
    );
}

#[test]
fn the_help_of_every_command_carries_none() {
    let mut found = Vec::new();
    let mut texts = vec![("/help".to_string(), ham_core::registry::help_text())];
    for name in ham_core::registry::subcommand_names() {
        if let Some(t) = ham_core::registry::command_help(name) {
            texts.push((format!("/help {name}"), t));
        }
        // The help of the command line (D6): `ham help` and `ham help <command>`.
        if let Some(t) = ham_core::help::command_text(name) {
            texts.push((format!("ham help {name}"), t));
        }
    }
    texts.push(("ham help".to_string(), ham_core::help::top_text()));
    texts.push((
        "ham help nosuchword".to_string(),
        ham_core::help::unknown_text("nosuchword"),
    ));
    for (case, t) in &texts {
        for (n, line) in t.lines().enumerate() {
            for rule in hits(line) {
                found.push(format!("{case}:{}: {rule}: {line}", n + 1));
            }
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn the_screens_in_the_golden_snapshots_carry_none() {
    // The snapshots of the terminal app and of the renderings are the screens as drawn. The header between the first two `---` lines names the
    // test; the body is the screen. A date is a fixture of the test there (the time of an export), so the rule on dates is not applied.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    let mut found = Vec::new();
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "snap") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let body = text.splitn(3, "---\n").nth(2).unwrap_or(&text);
        files += 1;
        for (n, line) in body.lines().enumerate() {
            for rule in hits(line) {
                if rule != "project-date" {
                    found.push(format!("{}:{}: {rule}: {line}", path.display(), n + 1));
                }
            }
        }
    }
    assert!(files > 50, "only {files} snapshots were read");
    assert!(found.is_empty(), "{}", found.join("\n"));
}

struct Home {
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    downloads: PathBuf,
}

/// Fresh XDG directories, a config naming the stand-in and a `credential_process` profile.
fn home(fake: &Fake, label: &str) -> Home {
    let root = support::scratch(label);
    let h = Home {
        config: root.join("config"),
        state: root.join("state"),
        cache: root.join("cache"),
        downloads: root.join("downloads"),
        root,
    };
    for d in [&h.config, &h.state, &h.cache, &h.downloads] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::create_dir_all(h.config.join("ham")).unwrap();
    std::fs::write(
        h.config.join("ham/config.toml"),
        format!(
            "apiEndpoint = \"{}\"\nregion = \"{}\"\nawsProfile = \"ham-test\"\n",
            fake.endpoint,
            support::REGION
        ),
    )
    .unwrap();
    let script = h.root.join("creds.sh");
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
        h.root.join("aws-config"),
        format!(
            "[profile ham-test]\ncredential_process = {}\nregion = {}\n",
            script.display(),
            support::REGION
        ),
    )
    .unwrap();
    std::fs::write(h.root.join("aws-credentials"), "").unwrap();
    h
}

/// A clean environment that keeps coverage instrumentation (`LLVM_PROFILE_FILE` and friends).
fn clean(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    cmd.env_clear();
    for (k, v) in std::env::vars() {
        if k.starts_with("LLVM_") || k.starts_with("CARGO_LLVM_COV") {
            cmd.env(k, v);
        }
    }
    cmd
}

async fn ham(h: &Home, args: &[&str]) -> Output {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_ham"));
    clean(&mut cmd)
        .args(args)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("TMPDIR", std::env::temp_dir())
        .env("HOME", &h.root)
        .env("XDG_CONFIG_HOME", &h.config)
        .env("XDG_STATE_HOME", &h.state)
        .env("XDG_CACHE_HOME", &h.cache)
        .env("HAMH_DOWNLOAD_DIR", &h.downloads)
        .env("AWS_CONFIG_FILE", h.root.join("aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            h.root.join("aws-credentials"),
        )
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap()
}

/// One command, the streams it wrote, and whether its output may hold a date: a clock reading (the expiry of a link, the age of a smoke record)
/// is data, so the rule on the dates of the project is not applied to the commands that print one.
struct Seen {
    case: String,
    out: String,
    dates: bool,
}

async fn run(h: &Home, args: &[&str], dates: bool) -> Seen {
    let o = ham(h, args).await;
    Seen {
        case: format!("ham {}", args.join(" ")),
        out: format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        dates,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_output_of_the_commands_a_person_reads_carries_none() {
    let fake = Fake::start_with(true).await;
    let h = home(&fake, "output-words");
    let mut seen = Vec::new();
    // The static texts: version, help, the settings, the model list and the refusals of a mistyped line.
    for args in [
        vec!["--version"],
        vec!["help"],
        vec!["--help"],
        vec!["-h"],
        vec!["runs", "--help"],
        vec!["help", "runs"],
        vec!["help", "nosuchword"],
        vec!["models"],
        vec!["--json", "models"],
        vec!["config"],
        vec!["config", "docs"],
        vec!["config", "check"],
        vec!["rules", "show"],
        vec!["doctr"],
        vec!["pull"],
        vec!["watch"],
        vec!["cancel"],
        vec!["push"],
        vec!["clone"],
        vec!["config", "set", "no_such_key", "x"],
        vec!["config", "set", "default_model", "no-such-model"],
        vec!["models", "--no-such-flag"],
        vec!["do something", "--model", "no-such-model", "--direct"],
    ] {
        seen.push(run(&h, &args, true).await);
    }
    // The commands that talk to the stand-in: the checks, the list of runs, and a run from its start to its card.
    seen.push(run(&h, &["doctor"], false).await);
    seen.push(run(&h, &["--json", "doctor"], false).await);
    let start = ham(
        &h,
        &[
            "Add a percent_change(a, b) function with tests",
            "--s3-only",
            "--direct",
            "--no-rules",
            "--json",
        ],
    )
    .await;
    let first = String::from_utf8_lossy(&start.stdout).into_owned();
    let id = serde_json::from_str::<Value>(first.lines().next().unwrap()).unwrap()["runId"]
        .as_str()
        .unwrap()
        .to_string();
    seen.push(Seen {
        case: "ham <prompt> --direct --json".into(),
        out: first,
        dates: false,
    });
    for args in [
        vec!["runs"],
        vec!["--json", "runs"],
        vec!["watch", id.as_str()],
        vec!["pull", id.as_str()],
        vec!["link", id.as_str()],
        vec!["cancel", id.as_str()],
    ] {
        seen.push(run(&h, &args, false).await);
    }
    let mut found = Vec::new();
    for s in &seen {
        assert!(
            !s.out.is_empty() || s.case.contains("cancel"),
            "{} printed nothing",
            s.case
        );
        for (n, line) in s.out.lines().enumerate() {
            for rule in hits(line) {
                if rule == "project-date" && !s.dates {
                    continue;
                }
                found.push(format!("{}:{}: {rule}: {line}", s.case, n + 1));
            }
        }
    }
    assert!(found.is_empty(), "{}", found.join("\n"));
}
