//! The container signals: the list that decides whether a repository keeps its session on the microVM.
//!
//! The Instances machine has no Linux capabilities, so the container helper of the worker (`newuidmap`) fails there and a repository whose
//! checks start containers (Testcontainers, docker compose) cannot run them. The microVM can. The machine of a run is chosen once, when its
//! session starts, so the repository decides it from what the repo-gateway finds in the staged files:
//!
//! - `auto` keeps a repository with a signal on the microVM, whatever the stage's own policy says;
//! - an explicit `instances` request for such a repository ends the run before any machine starts, with [`instances_refusal`];
//! - a repository with no signal keeps the choice it had before.
//!
//! A signal is one line of text, `<files>` or `<files> contains <text>`. `<files>` is a comma-separated list of patterns. A pattern with no
//! slash names a file at any depth (`pom.xml` is `pom.xml` and `services/api/pom.xml`); a pattern with a slash, or one that starts with `./`,
//! is read from the root of the repository (`.ham/test-command`, `./pom.xml`, `deploy/**/*.yml`). `*` stands for characters inside one folder
//! name, `?` for one such character, `**/` for any number of folders. `contains <text>` asks for the text in the file, with no regard to the
//! case of ASCII letters; a signal with no `contains` is a file name alone.
//!
//! The list is the user's setting `run.container_signals`, which rides in the request as `compute.containerSignals`. Absent means
//! [`DEFAULT_SIGNALS`]. The entry `default` stands for the default list inside a longer one, so a list that adds one line keeps the later
//! improvements of the defaults; an empty list turns every signal off. The setting is one line of entries separated by `;` ([`parse_setting`]).
//!
//! The grammar, the defaults and the matcher live here and not in the wire crate: the wire carries only the list of lines. Nothing here reads
//! a file or the network; [`find_in`] takes the paths of the repository and a function that reads the files it asks for, and the pieces it is made of
//! ([`Signal::candidates`], [`Signal::text`], [`holds`]) are public for a caller that reads asynchronously.

use serde::{Deserialize, Serialize};

/// The signals in force when the user's list is absent. A signal is a line of the grammar above, in its normal form.
///
/// The first six lines are the dependency of a Testcontainers library in the build files of Java and Kotlin (Maven, Gradle, version
/// catalogs, sbt), Go, Python, Node, .NET and Rust. The seventh adds Ruby, PHP and Elixir. The eighth is a compose file by name, and the last is
/// the project's own test command when it mentions docker. Lock files are not signals: their entries are often transitive.
pub const DEFAULT_SIGNALS: [&str; 9] = [
    "pom.xml, *.gradle, *.gradle.kts, *.versions.toml, build.sbt contains testcontainers",
    "go.mod contains testcontainers",
    "requirements*.txt, requirements*.in, constraints*.txt, pyproject.toml, setup.py, setup.cfg, Pipfile, environment.yml contains testcontainers",
    "package.json contains testcontainers",
    "*.csproj, *.fsproj, *.vbproj, Directory.Packages.props, Directory.Build.props, packages.config, paket.dependencies contains testcontainers",
    "Cargo.toml contains testcontainers",
    "Gemfile, composer.json, mix.exs contains testcontainers",
    "docker-compose.yml, docker-compose.yaml, docker-compose.*.yml, docker-compose.*.yaml, compose.yml, compose.yaml, compose.*.yml, compose.*.yaml",
    ".ham/test-command contains docker",
];

/// The word that stands for [`DEFAULT_SIGNALS`] inside a list.
pub const DEFAULT_WORD: &str = "default";

/// The word of the setting that means an empty list.
pub const NONE_WORD: &str = "none";

/// How many files whose content is asked for are read in one call of the `read` function of [`find_in`]. This bounds the memory of one read, not the
/// decision: every candidate file is read.
pub const READ_BATCH: usize = 32;

const CONTAINS: &str = " contains ";

/// One signal: the file patterns and, when set, the text the file must hold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signal {
    patterns: Vec<Pattern>,
    contains: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Pattern {
    /// The pattern as written, in its normal form (a leading `./` is kept).
    text: String,
    /// True for a pattern read from the root of the repository.
    rooted: bool,
    /// The glob; for a rooted pattern with a leading `./`, the part after it.
    glob: String,
}

impl Signal {
    /// The signal in its normal form: the patterns joined by `, `, then ` contains <text>` when there is a text.
    pub fn line(&self) -> String {
        let files: Vec<&str> = self.patterns.iter().map(|p| p.text.as_str()).collect();
        match &self.contains {
            Some(text) => format!("{} contains {text}", files.join(", ")),
            None => files.join(", "),
        }
    }

    /// The text a file must hold for this signal; `None` for a signal that matches by file name alone.
    pub fn text(&self) -> Option<&str> {
        self.contains.as_deref()
    }

    /// What this signal found at `path`.
    pub fn found(&self, path: &str) -> Found {
        Found {
            signal: self.line(),
            path: path.to_string(),
        }
    }

    /// The files of `sorted` (see [`sorted_paths`]) that this signal names, in the same order.
    pub fn candidates<'a>(&self, sorted: &[&'a str]) -> Vec<&'a str> {
        sorted
            .iter()
            .copied()
            .filter(|p| self.matches_path(p))
            .collect()
    }

    /// True when `path` (relative to the root of the repository, `/` between folders) is a file this signal names.
    pub fn matches_path(&self, path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        self.patterns.iter().any(|p| {
            if p.rooted {
                glob_match(&p.glob, path)
            } else {
                glob_match(&p.glob, name)
            }
        })
    }
}

/// Reads one signal.
pub fn parse_signal(line: &str) -> Result<Signal, String> {
    let bad = |why: &str| format!("signal {line:?}: {why}");
    let text = line.trim();
    if text.is_empty() {
        return Err(bad("is empty"));
    }
    if text.chars().any(char::is_control) {
        return Err(bad("has a control character"));
    }
    if text == DEFAULT_WORD || text.eq_ignore_ascii_case(NONE_WORD) {
        return Err(bad(&format!(
            "{text} is a word of the setting, not a signal; write file names"
        )));
    }
    let (files, contains) = match text.find(CONTAINS) {
        Some(at) => {
            let found = text[at + CONTAINS.len()..].trim();
            if found.is_empty() {
                return Err(bad("says contains but gives no text"));
            }
            (text[..at].trim(), Some(found.to_string()))
        }
        None => (text, None),
    };
    let mut patterns = Vec::new();
    for raw in files.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(bad("has an empty file name between commas"));
        }
        if raw.chars().any(char::is_whitespace) {
            return Err(bad(&format!(
                "the file name {raw:?} has a space; write `<files> contains <text>`, with the files separated by commas"
            )));
        }
        let (rooted, glob) = match raw.strip_prefix("./") {
            Some(rest) if !rest.is_empty() => (true, rest),
            Some(_) => return Err(bad("has `./` with no file name")),
            None => (raw.contains('/'), raw),
        };
        if glob.starts_with('/') {
            return Err(bad(
                "a file name does not start with /; start from the root with a folder name or `./`",
            ));
        }
        patterns.push(Pattern {
            text: raw.to_string(),
            rooted,
            glob: glob.to_string(),
        });
    }
    Ok(Signal { patterns, contains })
}

/// Checks the entries of a list as they ride in the request: each is the word `default` or a signal. The control-api calls this at the start
/// of a run; the `Err` text names the entry.
pub fn check_list(entries: &[String]) -> Result<(), String> {
    for entry in entries {
        if entry == DEFAULT_WORD {
            continue;
        }
        parse_signal(entry)?;
    }
    Ok(())
}

/// The signals a list stands for: `None` is [`DEFAULT_SIGNALS`], `default` inside a list is the defaults, any other entry is a signal.
pub fn resolve(list: Option<&[String]>) -> Result<Vec<Signal>, String> {
    let defaults = || DEFAULT_SIGNALS.iter().map(|l| parse_signal(l));
    let Some(list) = list else {
        return defaults().collect();
    };
    let mut out = Vec::new();
    for entry in list {
        if entry == DEFAULT_WORD {
            for s in defaults() {
                out.push(s?);
            }
        } else {
            out.push(parse_signal(entry)?);
        }
    }
    Ok(out)
}

/// Reads the text of the setting `run.container_signals`: entries separated by `;`, each a signal or the word `default`, or the one word `none`
/// for an empty list. The entries come back in their normal form, ready for the request.
pub fn parse_setting(text: &str) -> Result<Vec<String>, String> {
    let text = text.trim();
    if text.eq_ignore_ascii_case(NONE_WORD) {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for raw in text.split(';') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if raw.eq_ignore_ascii_case(DEFAULT_WORD) {
            entries.push(DEFAULT_WORD.to_string());
        } else {
            entries.push(parse_signal(raw)?.line());
        }
    }
    if entries.is_empty() {
        return Err(format!(
            "takes signals separated by semicolons, the word {DEFAULT_WORD} for the default list, or the word {NONE_WORD} for no signals"
        ));
    }
    Ok(entries)
}

/// The text of the setting for a list: the entries joined by `; `, and `none` for an empty list.
pub fn setting_text(list: &[String]) -> String {
    if list.is_empty() {
        NONE_WORD.to_string()
    } else {
        list.join("; ")
    }
}

/// What a signal found: the signal in its normal form and the first path it matched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub signal: String,
    pub path: String,
}

/// The answer of the repo-gateway about one staged repository, as `containerCheck` in its output. Its presence says the check ran; `found` is the
/// first signal that matched, or absent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found: Option<Found>,
}

/// The paths of a repository in the order the signals look at them: sorted, each once.
pub fn sorted_paths(paths: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = paths.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sorted
}

/// The function that reads files for [`find_in`]: given paths, it returns one body for each, in the same order.
pub type ReadFiles<'a, E> = dyn FnMut(&[&str]) -> Result<Vec<Vec<u8>>, E> + 'a;

/// The first signal of `signals` that matches the repository whose files are `paths`, or `None`. The signals are taken in their order and, inside
/// one signal, the paths in sorted order, so the same repository and list always give the same answer. A signal with a `contains` text asks `read`
/// for the bytes of the candidate files, [`READ_BATCH`] at a time; a failure of `read` is the answer, never "nothing found". A caller that reads
/// asynchronously (the repo-gateway) runs the same loop with [`Signal::candidates`], [`Signal::text`] and [`holds`].
pub fn find_in<E>(
    signals: &[Signal],
    paths: &[String],
    read: &mut ReadFiles<'_, E>,
) -> Result<Option<Found>, E> {
    let sorted = sorted_paths(paths);
    for signal in signals {
        let candidates = signal.candidates(&sorted);
        let Some(needle) = signal.text() else {
            if let Some(first) = candidates.first() {
                return Ok(Some(signal.found(first)));
            }
            continue;
        };
        for batch in candidates.chunks(READ_BATCH) {
            let bodies = read(batch)?;
            for (path, body) in batch.iter().zip(&bodies) {
                if holds(body, needle) {
                    return Ok(Some(signal.found(path)));
                }
            }
        }
    }
    Ok(None)
}

/// True when `body` has `needle`, comparing ASCII letters without regard to case. An empty needle is never held.
pub fn holds(body: &[u8], needle: &str) -> bool {
    let needle = needle.as_bytes();
    !needle.is_empty()
        && body
            .windows(needle.len())
            .any(|w| w.eq_ignore_ascii_case(needle))
}

/// The one-line reason an explicit `instances` request is refused for a repository with a signal. It names the signal and the path and the two ways
/// to change the choice: another machine for this run, or a list without the signal.
pub fn instances_refusal(found: &Found) -> String {
    format!(
        "this run asked for the Instances machine, but the repository uses containers: {} matches the container signal \"{}\", and the Instances machine cannot run containers. Start the run with --compute microvm, or with --compute auto, which keeps this repository on the microVM. If the repository does not need containers for its checks, take the signal out of your list with ham config set run.container_signals <signals> (the word {DEFAULT_WORD} stands for the default list, and the word {NONE_WORD} turns every signal off)",
        found.path, found.signal
    )
}

/// The one-line reason an explicit `instances` request is refused when the stage did not report the check of the repository.
pub fn not_checked_refusal() -> String {
    "this run asked for the Instances machine, but the stage did not check the repository for container use, so the machine cannot be chosen. Start the run again; if this repeats, start it with --compute microvm".to_string()
}

/// The note for the log when `auto` keeps a repository on the microVM because of a signal.
pub fn auto_note(found: &Found) -> String {
    format!(
        "the session runs on the microVM: {} matches the container signal \"{}\"",
        found.path, found.signal
    )
}

// ---- file name patterns -------------------------------------------------------------------------------------------------------------------

enum Tok {
    Lit(char),
    One,
    Star,
    Deep,
    DeepSlash,
}

/// True when the glob matches the whole of `text`. `*` and `?` stay inside one folder name, `**` crosses folders, and `**/` is any number of folders
/// (none included).
fn glob_match(glob: &str, text: &str) -> bool {
    let g: Vec<char> = glob.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < g.len() {
        match g[i] {
            '*' if g.get(i + 1) == Some(&'*') => {
                while g.get(i) == Some(&'*') {
                    i += 1;
                }
                if g.get(i) == Some(&'/') {
                    i += 1;
                    toks.push(Tok::DeepSlash);
                } else {
                    toks.push(Tok::Deep);
                }
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '?' => {
                toks.push(Tok::One);
                i += 1;
            }
            c => {
                toks.push(Tok::Lit(c));
                i += 1;
            }
        }
    }
    let t: Vec<char> = text.chars().collect();
    let n = t.len();
    let mut cur = vec![false; n + 1];
    cur[0] = true;
    for tok in toks {
        let mut next = vec![false; n + 1];
        match tok {
            Tok::Lit(c) => {
                for j in 0..n {
                    if cur[j] && t[j] == c {
                        next[j + 1] = true;
                    }
                }
            }
            Tok::One => {
                for j in 0..n {
                    if cur[j] && t[j] != '/' {
                        next[j + 1] = true;
                    }
                }
            }
            Tok::Star => {
                for j in 0..=n {
                    next[j] = cur[j] || (j > 0 && next[j - 1] && t[j - 1] != '/');
                }
            }
            Tok::Deep => {
                for j in 0..=n {
                    next[j] = cur[j] || (j > 0 && next[j - 1]);
                }
            }
            Tok::DeepSlash => {
                let mut seen = false;
                for j in 0..=n {
                    if j > 0 && t[j - 1] == '/' && seen {
                        next[j] = true;
                    }
                    if cur[j] {
                        next[j] = true;
                        seen = true;
                    }
                }
            }
        }
        cur = next;
    }
    cur[n]
}
