//! Counts from a runner's own output.
//!
//! A report keeps the end of a command's output and the numbers of the runner's summary lines; it never keeps the rest. [`OutputScan`]
//! reads the stream as it arrives: lines of at most [`MAX_LINE_BYTES`], one small state per known runner, and a ring of the last bytes.
//! Memory and work stay in proportion to the stream, whatever the command prints.
//!
//! The counts are what the runner printed about itself. They are an observation about the repository's own tests and say nothing about
//! whether the tests are meaningful. A summary the parsers do not recognise gives no counts, and no count is ever invented: when the
//! command names a runner, only that runner's summary is read; when it names none (a wrapper script), a count is taken only if exactly
//! one runner printed one.
//!
//! Go tests are read in two ways. `go test -v` prints one `--- PASS:` line for each top-level test, which is read when the command asks for verbose output.
//! gotestsum prints a `DONE 80 tests, 2 skipped, 1 failure in 1.065s` line at the end of a run and no such line in its default format, which is read
//! by [`gotestsum_line`]. When both are in one stream the gotestsum line wins, because it counts subtests and the other lines do not.

use harness_protocol::direct::TestCounts;

/// The longest line the scanner reads; the rest of a longer line is dropped.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// What a scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scanned {
    /// The end of the output, at most the cap given to [`OutputScan::new`] bytes, without control characters other than newline and tab.
    pub tail: String,
    /// More output came than the tail holds.
    pub truncated: bool,
    /// The runner whose summary was found and what it counted; `None` when no parser recognised the output or two did and the command
    /// names none.
    pub counts: Option<(&'static str, TestCounts)>,
    /// A line of the output says the command found no container runtime to talk to (Testcontainers, the Docker client, Compose and the
    /// Python Docker library each have their own words, [`says_no_container_runtime`]). Read from the whole stream, not only the tail.
    pub container_runtime_missing: bool,
    /// The first program a line of the output says the shell could not find ([`missing_program_in`]): `cargo` for `sh: 1: cargo: not found`.
    /// Read from the whole stream. It says what the output claims; the caller decides whether it explains the exit code.
    pub missing_program: Option<String>,
    /// The first line of the output that says the disk is full ([`out_of_disk_line`]). Read from the whole stream, not only the tail.
    pub out_of_disk: Option<String>,
    /// The word of the first line in which pytest says it found no file or directory. Read from the whole stream.
    /// It says what the output claims; the caller checks the command and the checkout.
    pub pytest_not_found: Option<String>,
    /// The ports of this machine that a line of the output says a connection to was refused ([`refused_port`]), each once. Read from the whole
    /// stream, not only the tail, because the first error is often cut off from it.
    pub refused_ports: Vec<u16>,
    /// The lines of the output that concern the container runtime, from the whole stream, in the order they came, each
    /// cut to [`RUNTIME_LINE_BYTES`] and at most [`RUNTIME_LOG_BYTES`] in all. A step marked not run for want of a runtime shows them, because
    /// the first of them is often cut off from the tail.
    pub runtime_lines: Vec<String>,
}

/// The longest runtime line kept, in bytes.
pub const RUNTIME_LINE_BYTES: usize = 300;
/// The most bytes of runtime lines a scan keeps.
pub const RUNTIME_LOG_BYTES: usize = 2048;

/// The ports the report knows a local server for: PostgreSQL and RabbitMQ.
const SERVICE_PORTS: [u16; 2] = [5432, 5672];

/// The port of this machine that `line` says a connection could not be made to: a line that names a local address (`127.0.0.1`, `localhost`,
/// `::1`), one of [`SERVICE_PORTS`] and a refusal (`connection refused`, `ECONNREFUSED`, `could not connect`, `connection failed`). `None` for any other
/// line, so a test that only mentions a port, or an error about another host, adds nothing.
pub fn refused_port(line: &str) -> Option<u16> {
    let lower = line.to_ascii_lowercase();
    let local = lower.contains("127.0.0.1") || lower.contains("localhost") || lower.contains("::1");
    let refused = [
        "refused",
        "econnrefused",
        "could not connect",
        "connection failed",
    ]
    .iter()
    .any(|w| lower.contains(w));
    if !local || !refused {
        return None;
    }
    SERVICE_PORTS.into_iter().find(|p| {
        let n = p.to_string();
        lower.contains(&format!(":{n}"))
            || lower.contains(&format!("port {n}"))
            || lower.contains(&format!(" {n} "))
    })
}

/// The system's words for a full disk (`strerror(ENOSPC)`), looked for without regard to letter case. Node's `ENOSPC: System limit for number of file
/// watchers reached` is a limit of the kernel and no full disk, so the code name alone does not count.
const DISK_FULL_PHRASE: &str = "no space left on device";
/// Rust's `io::Error` adds the errno to the phrase; it belongs to it.
const DISK_FULL_ERRNO: &str = " (os error 28)";
/// The most characters of the line a report keeps.
pub const MAX_DISK_LINE_CHARS: usize = 100;

/// The line of the output that says the disk is full, cut to what names the cause: the text up to the end of the system's phrase (and the errno that
/// follows it), without a leading `error: `, and at most [`MAX_DISK_LINE_CHARS`] characters of its end. `None` for any other line. A path or a
/// message after the phrase is dropped; the line is plain text, without escape codes and control characters.
pub fn out_of_disk_line(line: &str) -> Option<String> {
    let clean: String = strip_ansi(line)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let text = clean.trim();
    // ASCII lower-casing keeps every byte offset, so the position found in the folded copy is the position in `text`.
    let lower = text.to_ascii_lowercase();
    let at = lower.find(DISK_FULL_PHRASE)?;
    let mut end = at + DISK_FULL_PHRASE.len();
    if lower[end..].starts_with(DISK_FULL_ERRNO) {
        end += DISK_FULL_ERRNO.len();
    }
    let mut kept = &text[..end];
    if kept
        .get(..7)
        .is_some_and(|p| p.eq_ignore_ascii_case("error: "))
    {
        kept = &kept[7..];
    }
    let skip = kept.chars().count().saturating_sub(MAX_DISK_LINE_CHARS);
    let kept: String = kept.chars().skip(skip).collect();
    Some(kept.trim().to_string())
}

/// The word pytest says it found no file or directory for, in its usage error: `at` for `ERROR: file or directory not found: at`
/// (pytest 7 and later) and for `ERROR: file not found: at` (earlier). `None` for any other line.
pub fn pytest_not_found(line: &str) -> Option<String> {
    let line = line.trim();
    let word = line
        .strip_prefix("ERROR: file or directory not found: ")
        .or_else(|| line.strip_prefix("ERROR: file not found: "))?
        .trim();
    (!word.is_empty()).then(|| word.to_string())
}

/// The bare program name a shell's "not found" line names: `cargo` for `sh: 1: cargo: not found` (dash) and for `bash: line 2: cargo: command
/// not found` (bash), `None` for any other line. A name with a slash is a file of the repository (`./gradlew`, `scripts/test.sh`) and no
/// program of the environment, and an option, a word group or an empty name is no program either.
pub fn missing_program_in(line: &str) -> Option<String> {
    let line = line.trim_end();
    let head = line
        .strip_suffix(": command not found")
        .or_else(|| line.strip_suffix(": not found"))?;
    let name = head.rsplit(": ").next().unwrap_or(head);
    let bare = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'));
    bare.then(|| name.to_string())
}

/// Whether `line` is a runner's or a client's way of saying that there is no container runtime to reach.
pub fn says_no_container_runtime(line: &str) -> bool {
    const SIGNS: [&str; 12] = [
        "could not find a valid docker environment",
        "cannot connect to the docker daemon",
        "is the docker daemon running",
        "docker daemon is not running",
        "docker: command not found",
        "docker: not found",
        "podman: command not found",
        "podman: not found",
        "docker.sock: connect",
        "error during connect",
        "error while fetching server api version",
        "is not a docker command",
    ];
    let lower = line.to_ascii_lowercase();
    SIGNS.iter().any(|s| lower.contains(s))
}

/// Whether `word` of a command line names gotestsum: its last path segment, before any `@version`, is `gotestsum` (`gotestsum`, `/go/bin/gotestsum`,
/// `gotest.tools/gotestsum@v1.13.0`).
pub fn is_gotestsum_word(word: &str) -> bool {
    word.split('@')
        .next()
        .and_then(|head| head.rsplit('/').next())
        == Some("gotestsum")
}

/// Whether the arguments after gotestsum (or after the module of `go run`) start a test run: not `--version`, `--help`, `-h`, `help` or the `tool`
/// subcommands (`tool slowest`, `tool ci-matrix`), which print and run no test. Only the words before `--` are gotestsum's own.
pub fn gotestsum_runs_tests(args: &[&str]) -> bool {
    let own: Vec<&str> = args.iter().copied().take_while(|a| *a != "--").collect();
    if own
        .iter()
        .any(|a| matches!(*a, "--version" | "--help" | "-h"))
    {
        return false;
    }
    !own.iter()
        .find(|a| !a.starts_with('-'))
        .is_some_and(|a| matches!(*a, "tool" | "help"))
}

/// What one gotestsum summary line says. gotestsum v1.13.0 prints it with `"\n%s %d tests%s%s%s in %s\n"` (`testjson.PrintSummary`): `DONE`, and ` N runs,`
/// after `--rerun-fails`, the tests that ran, `, N skipped`, `, N failure(s)`, `, N error(s)` and the time. A count of 0 is left out, 1 is singular.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GotestsumDone {
    /// The line starts with `DONE`. gotestsum leaves the word out (and the line starts with one space) when the run did not finish.
    finished: bool,
    /// The line has ` N runs,`: the tests that were run again are counted again, so the numbers are attempts and not tests.
    rerun: bool,
    /// Every test case that ran, subtests included; the failed and the skipped ones are in it.
    tests: u64,
    skipped: u64,
    /// Failed test cases and the packages that failed without a failed test (a build or `TestMain` failure, a timeout), which have no test in `tests`.
    failures: u64,
    /// Lines on standard error that are not module chatter: compiler output, a panic. They are no tests.
    errors: u64,
}

/// The digits of `s` as a number; `None` for anything else (a sign, a space, an empty text, more than 64 bits).
fn uint(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A time as gotestsum writes it (`%.3fs`): seconds with a fraction and an `s`, or `unknown` for a test that never finished.
fn gotestsum_time(s: &str) -> bool {
    if s == "unknown" {
        return true;
    }
    s.strip_suffix('s')
        .and_then(|n| n.split_once('.'))
        .is_some_and(|(whole, frac)| uint(whole).is_some() && uint(frac).is_some())
}

/// `, N <word>` of a summary line: the number of a part that is present (so 1 or more) and the word spelled for it.
fn count_part(part: &str, singular: &str, plural: &str) -> Option<u64> {
    let (n, word) = part.split_once(' ')?;
    let n = uint(n)?;
    let want = match n {
        0 => return None,
        1 => singular,
        _ => plural,
    };
    (word == want).then_some(n)
}

/// The summary `raw` (a line of the output without colour codes and not trimmed: gotestsum writes it at the start of a line) says, when it has exactly
/// gotestsum's form. Any other line gives `None`, also a line that begins with `DONE` and says something else.
fn gotestsum_line(raw: &str) -> Option<GotestsumDone> {
    let line = raw.trim_end();
    let (finished, rest) = match line.strip_prefix("DONE ") {
        Some(rest) => (true, rest),
        // One space and a number: the form of a run that did not finish.
        None => (false, line.strip_prefix(' ')?),
    };
    let (rerun, rest) = match rest.split_once(" runs, ") {
        Some((n, tail)) if uint(n).is_some() => (true, tail),
        _ => (false, rest),
    };
    let (head, time) = rest.rsplit_once(" in ")?;
    if !gotestsum_time(time) {
        return None;
    }
    let mut parts = head.split(", ");
    let tests = uint(parts.next()?.strip_suffix(" tests")?)?;
    let mut done = GotestsumDone {
        finished,
        rerun,
        tests,
        skipped: 0,
        failures: 0,
        errors: 0,
    };
    // The parts come in this order and each at most once.
    let mut stage = 0;
    for part in parts {
        if stage < 1
            && let Some(n) = count_part(part, "skipped", "skipped")
        {
            done.skipped = n;
            stage = 1;
        } else if stage < 2
            && let Some(n) = count_part(part, "failure", "failures")
        {
            done.failures = n;
            stage = 2;
        } else if stage < 3
            && let Some(n) = count_part(part, "error", "errors")
        {
            done.errors = n;
            stage = 3;
        } else {
            return None;
        }
    }
    Some(done)
}

/// Whether `line` is a stack frame of a Java exception (`at java.base/...`): never kept as a runtime line, whatever it names.
fn is_stack_frame(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("at ") && (t.contains('(') || t.contains('/'))
}

/// Whether `line` concerns the container runtime: Testcontainers' words about the strategies it tried, the endpoint named by
/// `DOCKER_HOST`, the socket, the Docker client's and the front socket's own messages. Read from the whole stream by the scan, so what the
/// 4 KiB tail of a step cuts off is still there for a step that is marked not run for want of a runtime.
pub fn runtime_line(line: &str) -> bool {
    const SIGNS: [&str; 11] = [
        "docker_host",
        "docker.sock",
        "docker environment",
        "attempted configurations",
        "clientproviderstrategy",
        "docker daemon",
        "testcontainers version",
        "container runtime is not ready",
        "libpod",
        "podman",
        "valid configuration was found",
    ];
    let lower = line.to_ascii_lowercase();
    SIGNS.iter().any(|s| lower.contains(s))
}

/// Whether `line` begins an exception (`java.net.ConnectException: Connection refused`, `Caused by: ...`): the cause under a warning.
fn exception_head(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Caused by:")
        || ((t.starts_with("java.") || t.starts_with("javax.") || t.starts_with("org."))
            && (t.contains("Exception") || t.contains("Error")))
}

/// `line` without a leading time of day (`12:55:54.431 `), so the same warning at two times is one line.
fn without_time(line: &str) -> &str {
    let b = line.as_bytes();
    let timed = b.len() > 8
        && b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2] == b':'
        && b[5] == b':';
    if timed {
        line.split_once(' ').map_or(line, |(_, rest)| rest)
    } else {
        line
    }
}

/// The first `max` bytes of `s` at a character boundary, and whether anything was cut.
fn first_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

#[derive(Debug, Default)]
struct Sums {
    passed: u64,
    failed: u64,
    skipped: u64,
    seen: bool,
}

impl Sums {
    fn counts(&self) -> Option<TestCounts> {
        self.seen.then(|| TestCounts {
            passed: clamp(self.passed),
            failed: clamp(self.failed),
            skipped: clamp(self.skipped),
        })
    }
}

#[derive(Debug, Default)]
struct NodeState {
    tests: Option<u32>,
    pass: Option<u32>,
    fail: Option<u32>,
    cancelled: u32,
    skipped: u32,
    todo: u32,
}

/// A bounded reader of a command's output.
#[derive(Debug)]
pub struct OutputScan {
    cap: usize,
    tail: Vec<u8>,
    total: usize,
    line: Vec<u8>,
    line_cut: bool,
    pytest: Option<TestCounts>,
    unittest_ran: Option<u32>,
    unittest: Option<TestCounts>,
    jest: Option<TestCounts>,
    vitest: Option<TestCounts>,
    node: NodeState,
    go: Sums,
    /// The gotestsum summaries of the stream, added (a script may run it for several package lists, one `DONE` line each).
    gotestsum: Sums,
    /// A gotestsum summary was there and cannot be turned into counts (a rerun, errors with no failure): the whole gotestsum result is unreadable and
    /// the verbose Go lines must not stand in for it.
    gotestsum_unreadable: bool,
    cargo: Sums,
    nextest: Option<TestCounts>,
    maven: Sums,
    container_runtime_missing: bool,
    missing_program: Option<String>,
    out_of_disk: Option<String>,
    pytest_not_found: Option<String>,
    refused_ports: Vec<u16>,
    runtime_lines: Vec<String>,
    /// The bytes of `runtime_lines`, one more for each line's newline.
    runtime_bytes: usize,
    /// Lines after a runtime line that may still be its cause: an exception head. Counts down; a stack frame does not count.
    runtime_follow: u8,
}

impl OutputScan {
    /// A scan that keeps the last `tail_bytes` of the stream.
    pub fn new(tail_bytes: usize) -> Self {
        Self {
            cap: tail_bytes.max(1),
            tail: Vec::new(),
            total: 0,
            line: Vec::new(),
            line_cut: false,
            pytest: None,
            unittest_ran: None,
            unittest: None,
            jest: None,
            vitest: None,
            node: NodeState::default(),
            go: Sums::default(),
            gotestsum: Sums::default(),
            gotestsum_unreadable: false,
            cargo: Sums::default(),
            nextest: None,
            maven: Sums::default(),
            container_runtime_missing: false,
            missing_program: None,
            out_of_disk: None,
            pytest_not_found: None,
            refused_ports: Vec::new(),
            runtime_lines: Vec::new(),
            runtime_bytes: 0,
            runtime_follow: 0,
        }
    }

    /// Reads the next bytes of the stream.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        self.tail.extend_from_slice(bytes);
        if self.tail.len() > self.cap * 2 {
            let excess = self.tail.len() - self.cap;
            self.tail.drain(..excess);
        }
        for &b in bytes {
            if b == b'\n' || b == b'\r' {
                self.end_line();
            } else if self.line.len() < MAX_LINE_BYTES {
                self.line.push(b);
            } else {
                self.line_cut = true;
            }
        }
    }

    fn end_line(&mut self) {
        let cut = std::mem::take(&mut self.line_cut);
        let bytes = std::mem::take(&mut self.line);
        if cut || bytes.is_empty() {
            return;
        }
        let text = strip_ansi(&String::from_utf8_lossy(&bytes));
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.on_line(&text, trimmed);
    }

    /// Keeps the line when it concerns the container runtime ([`runtime_line`]) or is the exception head right under one, cut to
    /// [`RUNTIME_LINE_BYTES`], once, while the lines fit in [`RUNTIME_LOG_BYTES`]. The earliest lines are kept: they are nearest the cause.
    fn runtime_scan(&mut self, line: &str) {
        if is_stack_frame(line) {
            return;
        }
        let keep = if runtime_line(line) {
            self.runtime_follow = 2;
            true
        } else if self.runtime_follow > 0 && exception_head(line) {
            self.runtime_follow -= 1;
            true
        } else {
            self.runtime_follow = 0;
            false
        };
        if !keep {
            return;
        }
        let (cut, _) = first_bytes(line, RUNTIME_LINE_BYTES);
        let key = without_time(&cut);
        if self
            .runtime_lines
            .iter()
            .any(|kept| without_time(kept) == key)
        {
            return;
        }
        if self.runtime_bytes + cut.len() + 1 > RUNTIME_LOG_BYTES {
            return;
        }
        self.runtime_bytes += cut.len() + 1;
        self.runtime_lines.push(cut);
    }

    fn on_line(&mut self, raw: &str, line: &str) {
        if !self.container_runtime_missing && says_no_container_runtime(line) {
            self.container_runtime_missing = true;
        }
        self.runtime_scan(line);
        if self.missing_program.is_none() {
            self.missing_program = missing_program_in(line);
        }
        if self.out_of_disk.is_none() {
            self.out_of_disk = out_of_disk_line(line);
        }
        if self.pytest_not_found.is_none() {
            self.pytest_not_found = pytest_not_found(line);
        }
        if let Some(port) = refused_port(line)
            && !self.refused_ports.contains(&port)
        {
            self.refused_ports.push(port);
        }
        if let Some(c) = pytest_line(line) {
            self.pytest = Some(c);
        }
        self.unittest_line(line);
        if let Some(c) = jest_line(line) {
            self.jest = Some(c);
        }
        if let Some(c) = vitest_line(line) {
            self.vitest = Some(c);
        }
        self.node_line(line);
        // A top-level test function only: a subtest line is indented.
        for (prefix, which) in [("--- PASS: ", 0), ("--- FAIL: ", 1), ("--- SKIP: ", 2)] {
            if raw.starts_with(prefix) {
                self.go.seen = true;
                match which {
                    0 => self.go.passed += 1,
                    1 => self.go.failed += 1,
                    _ => self.go.skipped += 1,
                }
            }
        }
        if let Some(done) = gotestsum_line(raw) {
            self.add_gotestsum(done);
        }
        if let Some(c) = cargo_line(line) {
            self.cargo.seen = true;
            self.cargo.passed += u64::from(c.passed);
            self.cargo.failed += u64::from(c.failed);
            self.cargo.skipped += u64::from(c.skipped);
        }
        if let Some(c) = nextest_line(line) {
            self.nextest = Some(c);
        }
        if let Some(c) = maven_line(line) {
            self.maven.seen = true;
            self.maven.passed += u64::from(c.passed);
            self.maven.failed += u64::from(c.failed);
            self.maven.skipped += u64::from(c.skipped);
        }
    }

    /// Adds one gotestsum summary. The tests that ran contain the skipped and the failed ones, so the passed are what is left. A failure that is a
    /// package (a build failure) is no test of the total, so the passed can be one too few; the failure is never lost. Errors are lines of standard
    /// error and no tests: they are not added to the failures, and a summary with errors and no failure, which a clean run does not print, a
    /// summary of reruns, which counts the tests that were run again twice, and a summary of a run that did not finish make the whole result
    /// unreadable (no count is invented).
    fn add_gotestsum(&mut self, done: GotestsumDone) {
        if !done.finished || done.rerun || (done.errors > 0 && done.failures == 0) {
            self.gotestsum_unreadable = true;
            return;
        }
        self.gotestsum.seen = true;
        self.gotestsum.passed = self.gotestsum.passed.saturating_add(
            done.tests
                .saturating_sub(done.skipped)
                .saturating_sub(done.failures),
        );
        self.gotestsum.failed = self.gotestsum.failed.saturating_add(done.failures);
        self.gotestsum.skipped = self.gotestsum.skipped.saturating_add(done.skipped);
    }

    fn unittest_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("Ran ") {
            let mut words = rest.split_whitespace();
            if let (Some(n), Some(w), Some("in")) = (words.next(), words.next(), words.next())
                && (w == "test" || w == "tests")
                && let Ok(n) = n.parse::<u32>()
            {
                self.unittest_ran = Some(n);
            }
            return;
        }
        let Some(ran) = self.unittest_ran else {
            return;
        };
        let result = line == "OK" || line.starts_with("OK (") || line.starts_with("FAILED (");
        if !result {
            return;
        }
        let key = |name: &str| -> u32 {
            line.find(&format!("{name}="))
                .and_then(|i| {
                    let digits: String = line[i + name.len() + 1..]
                        .chars()
                        .take_while(char::is_ascii_digit)
                        .collect();
                    digits.parse().ok()
                })
                .unwrap_or(0)
        };
        let failed = key("failures").saturating_add(key("errors"));
        let skipped = key("skipped");
        self.unittest = Some(TestCounts {
            passed: ran.saturating_sub(failed).saturating_sub(skipped),
            failed,
            skipped,
        });
        self.unittest_ran = None;
    }

    fn node_line(&mut self, line: &str) {
        let Some(rest) = line.strip_prefix("ℹ ").or_else(|| line.strip_prefix("# ")) else {
            return;
        };
        let mut words = rest.split_whitespace();
        let (Some(key), Some(n), None) = (words.next(), words.next(), words.next()) else {
            return;
        };
        let Ok(n) = n.parse::<u32>() else {
            return;
        };
        match key {
            "tests" => self.node.tests = Some(n),
            "pass" => self.node.pass = Some(n),
            "fail" => self.node.fail = Some(n),
            "cancelled" => self.node.cancelled = n,
            "skipped" => self.node.skipped = n,
            "todo" => self.node.todo = n,
            _ => {}
        }
    }

    /// Every runner that printed a summary, with its counts.
    fn found(&self, verbose_go: bool) -> Vec<(&'static str, TestCounts)> {
        let mut out = Vec::new();
        let mut add = |name: &'static str, c: Option<TestCounts>| {
            if let Some(c) = c {
                out.push((name, c));
            }
        };
        add("pytest", self.pytest);
        add("unittest", self.unittest);
        add("jest", self.jest);
        add("vitest", self.vitest);
        if let (Some(_), Some(pass), Some(fail)) = (self.node.tests, self.node.pass, self.node.fail)
        {
            add(
                "node",
                Some(TestCounts {
                    passed: pass,
                    failed: fail.saturating_add(self.node.cancelled),
                    skipped: self.node.skipped.saturating_add(self.node.todo),
                }),
            );
        }
        // gotestsum's summary counts the subtests the `--- PASS:` lines leave out, so it stands for the stream when it is there; when it is there and
        // unreadable, the lines of the same tests do not bring a count back.
        if !self.gotestsum_unreadable {
            if self.gotestsum.seen {
                // A top-level `--- FAIL:` line that the summaries do not account for is a test of another run in the same stream (a script that runs
                // `go test -v` for one list of packages and gotestsum for another): the summary stands for part of the output, so nothing is read.
                if self.go.failed <= self.gotestsum.failed {
                    add("gotestsum", self.gotestsum.counts());
                }
            } else if verbose_go {
                add("go", self.go.counts());
            }
        }
        add("cargo", self.cargo.counts());
        add("nextest", self.nextest);
        add("maven", self.maven.counts());
        out
    }

    /// The tail and the counts, for the command that produced the stream.
    pub fn finish(self, command: &str) -> Scanned {
        let mut scan = self;
        // A last line with no newline after it.
        scan.end_line();
        let verbose_go = command
            .split_whitespace()
            .any(|w| w == "-v" || w == "-test.v");
        let found = scan.found(verbose_go);
        let named = named_runners(command);
        let counts = if named.is_empty() {
            match found.as_slice() {
                [one] => Some(*one),
                _ => None,
            }
        } else {
            named
                .iter()
                .find_map(|n| found.iter().find(|(f, _)| f == n).copied())
        };
        let start = scan.tail.len().saturating_sub(scan.cap);
        let mut bytes = &scan.tail[start..];
        // Do not start in the middle of a character.
        while let Some((&b, rest)) = bytes.split_first() {
            if b & 0b1100_0000 == 0b1000_0000 {
                bytes = rest;
            } else {
                break;
            }
        }
        let text = strip_ansi(&String::from_utf8_lossy(bytes));
        let clean: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
            .collect();
        let (tail, cut) = last_bytes(&clean, scan.cap);
        Scanned {
            tail,
            truncated: scan.total > scan.cap || cut,
            counts,
            container_runtime_missing: scan.container_runtime_missing,
            missing_program: scan.missing_program,
            out_of_disk: scan.out_of_disk,
            pytest_not_found: scan.pytest_not_found,
            refused_ports: scan.refused_ports,
            runtime_lines: scan.runtime_lines,
        }
    }
}

/// The counts in a finished output, for the command that produced it.
pub fn count_in(command: &str, output: &str) -> Option<(&'static str, TestCounts)> {
    let mut scan = OutputScan::new(output.len().max(1));
    scan.feed(output.as_bytes());
    scan.finish(command).counts
}

fn clamp(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The last `max` bytes of `s` at a character boundary, and whether anything was cut.
pub(crate) fn last_bytes(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    (s[start..].to_string(), true)
}

/// `s` without ANSI escape sequences (colours, cursor moves).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for n in chars.by_ref() {
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        }
    }
    out
}

/// The runners the command line names, in the order their summaries are looked for.
fn named_runners(command: &str) -> Vec<&'static str> {
    let words: Vec<&str> = command
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '&' | '|'))
        .filter(|w| !w.is_empty())
        .collect();
    let mut out: Vec<&'static str> = Vec::new();
    let mut push = |n: &'static str| {
        if !out.contains(&n) {
            out.push(n);
        }
    };
    for (i, w) in words.iter().enumerate() {
        let next = words.get(i + 1).copied().unwrap_or("");
        match *w {
            "pytest" | "py.test" => push("pytest"),
            "unittest" => push("unittest"),
            "jest" => push("jest"),
            "vitest" => push("vitest"),
            "go" if next == "test" => push("go"),
            w if is_gotestsum_word(w) => push("gotestsum"),
            "cargo" if next == "nextest" => push("nextest"),
            "cargo" if next == "test" => push("cargo"),
            "node" if next == "--test" => push("node"),
            "npm" | "pnpm" | "yarn" if next == "test" || next == "t" => {
                push("jest");
                push("vitest");
                push("node");
            }
            "npm" | "pnpm" | "yarn"
                if next == "run" && words.get(i + 2).is_some_and(|t| t.starts_with("test")) =>
            {
                push("jest");
                push("vitest");
                push("node");
            }
            w if w == "mvn" || w.ends_with("/mvn") || w == "mvnw" || w.ends_with("/mvnw") => {
                push("maven")
            }
            _ => {}
        }
    }
    out
}

/// `"<n> <word>"` pairs of a summary line, split by `sep`: the number and the first word after it.
fn pairs<'a>(text: &'a str, sep: &str) -> Vec<(u32, &'a str)> {
    text.split(sep)
        .filter_map(|t| {
            let mut it = t.split_whitespace();
            let n = it.next()?.parse::<u32>().ok()?;
            Some((n, it.next()?))
        })
        .collect()
}

fn pytest_line(line: &str) -> Option<TestCounts> {
    let s = line.trim_matches(|c| c == '=' || c == '-' || c == ' ');
    let idx = s.rfind(" in ")?;
    let (head, rest) = (&s[..idx], &s[idx + 4..]);
    // "0.31s" or "0.31s (0:00:00)".
    let time = rest.split_whitespace().next()?;
    let secs = time.strip_suffix('s')?;
    if secs.is_empty() || !secs.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    if head == "no tests ran" {
        return Some(TestCounts::default());
    }
    let mut c = TestCounts::default();
    let mut any = false;
    for token in head.split(", ") {
        let mut it = token.split_whitespace();
        let n: u32 = it.next()?.parse().ok()?;
        let word = it.next()?;
        match word {
            "passed" | "xpassed" => {
                c.passed = c.passed.saturating_add(n);
                any = true;
            }
            "failed" | "error" | "errors" => {
                c.failed = c.failed.saturating_add(n);
                any = true;
            }
            "skipped" | "xfailed" => {
                c.skipped = c.skipped.saturating_add(n);
                any = true;
            }
            "warning" | "warnings" | "deselected" | "rerun" | "reruns" => {}
            _ => return None,
        }
    }
    any.then_some(c)
}

fn jest_line(line: &str) -> Option<TestCounts> {
    let rest = line.strip_prefix("Tests:")?;
    let mut c = TestCounts::default();
    let mut any = false;
    for (n, w) in pairs(rest, ",") {
        match w {
            "passed" => c.passed = n,
            "failed" => c.failed = n,
            "skipped" | "todo" => c.skipped = c.skipped.saturating_add(n),
            "total" => {}
            _ => continue,
        }
        any = true;
    }
    any.then_some(c)
}

fn vitest_line(line: &str) -> Option<TestCounts> {
    let rest = line.strip_prefix("Tests ")?;
    let body = rest.rsplit_once(" (").map_or(rest, |(b, _)| b);
    let mut c = TestCounts::default();
    let mut any = false;
    for (n, w) in pairs(body, "|") {
        match w {
            "passed" => c.passed = n,
            "failed" => c.failed = n,
            "skipped" | "todo" => c.skipped = c.skipped.saturating_add(n),
            _ => continue,
        }
        any = true;
    }
    any.then_some(c)
}

fn cargo_line(line: &str) -> Option<TestCounts> {
    let rest = line.strip_prefix("test result:")?;
    let (_, counts) = rest.split_once(". ")?;
    let mut c = TestCounts::default();
    let mut any = false;
    for (n, w) in pairs(counts, ";") {
        match w {
            "passed" => c.passed = n,
            "failed" => c.failed = n,
            "ignored" => c.skipped = n,
            _ => continue,
        }
        any = true;
    }
    any.then_some(c)
}

fn nextest_line(line: &str) -> Option<TestCounts> {
    let rest = line.strip_prefix("Summary [")?;
    let (_, counts) = rest.split_once(" run: ")?;
    let mut c = TestCounts::default();
    let mut any = false;
    for (n, w) in pairs(counts, ",") {
        match w {
            "passed" => c.passed = n,
            "failed" | "timed" => c.failed = c.failed.saturating_add(n),
            "skipped" => c.skipped = n,
            _ => continue,
        }
        any = true;
    }
    any.then_some(c)
}

fn maven_line(line: &str) -> Option<TestCounts> {
    let mut s = line;
    for p in ["[INFO] ", "[ERROR] ", "[WARNING] "] {
        if let Some(r) = s.strip_prefix(p) {
            s = r;
            break;
        }
    }
    let rest = s.strip_prefix("Tests run:")?;
    if rest.contains("Time elapsed") {
        return None;
    }
    let num = |key: &str| -> Option<u32> {
        let i = rest.find(key)? + key.len();
        let digits: String = rest[i..]
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()
    };
    let run: u32 = rest
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    let failures = num("Failures:").unwrap_or(0);
    let errors = num("Errors:").unwrap_or(0);
    let skipped = num("Skipped:").unwrap_or(0);
    let failed = failures.saturating_add(errors);
    Some(TestCounts {
        passed: run.saturating_sub(failed).saturating_sub(skipped),
        failed,
        skipped,
    })
}
