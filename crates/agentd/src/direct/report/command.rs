//! How the clean-checkout test report reads a command.
//!
//! The commands come from files the model wrote: `.ham/test-command`, the command column of the lifecycle log, the numbered checks of the goal
//! card. They are free text. Handed to `sh -c` as written, a line such as `python3 -m unittest pkg.test_mod (3 tests)` ends in
//! `syntax error near unexpected token '('` and exit 2, and the card said that a passing suite exited 2. So a command is read before it runs.
//! The reading has three results:
//!
//! - [`Parsed::Plain`]: words, quotes (`'...'`, `"..."`, a backslash), `NAME=value` prefixes and a comment at the end. It runs as an argument
//!   vector with no shell: what reaches the program is exactly the words. A `NAME=value` prefix is the program's environment, and the
//!   comment is left out.
//! - [`Parsed::Shell`]: what only a shell does the way the author meant. Unquoted shell syntax (`&&`, `||`, `;`, `|`, `&`, a redirect, a
//!   subshell, a line break), a word the shell expands (`$HOME`, a backquote, `*`, `?`, `[`, `{`, a `~` at the start of a word or after `=` or
//!   `:`), an assignment of `PATH`, a first word the shell owns (a builtin such as `echo` or `exit`, a keyword such as `if` or `time`: a file of
//!   that name on the search path is not what `sh -c` runs), and a line with no program. The caller runs it with `sh -c` after the shell
//!   itself has read it to the end (`sh -n`), so a line that does not parse is not run.
//! - [`Unreadable`]: nothing to run (empty, only a comment), a quote that is never closed, or a control character anywhere (a carriage return
//!   included: the shell would take it for a character of the word). The step is marked not run with the reason [`UNREADABLE`] and is no
//!   result of the suite.
//!
//! A `Plain` command also needs a program that starts without a shell: [`find_program_as`] finds an executable file the test user may run, and
//! [`runs_directly`] says whether it is a binary or a script with an interpreter line. A script with neither is started by the shell, which
//! runs it as a shell script, as it always did.
//!
//! [`strip_notes`] is for the lifecycle log only. Its command column has the exit code and the test counts in columns of their own, so text
//! after the command in that column is a note, and the planner cuts it before it records the command. The rules are on the function.
//!
//! Nothing here starts a process. [`find_program_as`] and [`runs_directly`] look at a file's mode and its first bytes.

use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The reason a command that could not be read is marked not run with (the step's output starts `NOT RUN: could not read the command`).
pub const UNREADABLE: &str = "could not read the command";

/// The longest log column [`strip_notes`] reads. A longer one is left as it is (the planner refuses a command of over 400 characters).
const MAX_COLUMN_CHARS: usize = 2000;

/// A command read as words or as a shell line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Words, quotes and `NAME=value` prefixes only: run as an argument vector, with no shell. `env` holds the prefixes in order.
    Plain {
        env: Vec<(String, String)>,
        argv: Vec<String>,
    },
    /// Something only the shell does as intended: needs the shell, and the shell must read it to the end first.
    Shell,
}

/// A command that cannot be read at all; the text says why in a few plain words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unreadable(pub String);

fn unreadable<T>(why: &str) -> Result<T, Unreadable> {
    Err(Unreadable(why.to_string()))
}

/// One word of the command.
#[derive(Default)]
struct Word {
    /// The word with its quotes taken off.
    text: String,
    /// A quote or a backslash was in the word: it cannot be an assignment name.
    quoted: bool,
    /// The shell would expand something here (`$`, a backquote, a glob character, a brace, a `~` at the start or after `=` or `:`).
    expands: bool,
    /// Where the `=` of a `NAME=value` prefix is in `text`.
    eq: Option<usize>,
}

fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The words a POSIX shell takes as its own when they start a command: the special and regular builtins and the reserved words (and `time`,
/// which bash reads as a keyword). A file of the same name on the search path (`/usr/bin/echo`, `/usr/bin/time`) is another program.
const SHELL_OWNED: &[&str] = &[
    "!", "{", "}", "[[", ".", ":", "alias", "bg", "break", "case", "cd", "command", "continue",
    "do", "done", "echo", "elif", "else", "esac", "eval", "exec", "exit", "export", "false", "fc",
    "fg", "fi", "for", "function", "getopts", "hash", "if", "in", "jobs", "kill", "local",
    "newgrp", "printf", "pwd", "read", "readonly", "return", "select", "set", "shift", "test",
    "then", "time", "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias",
    "unset", "until", "wait", "while", "source", "declare",
];

/// Reads `command`: see the module text.
pub fn classify(command: &str) -> Result<Parsed, Unreadable> {
    if command
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n'))
    {
        return unreadable("the command holds a control character");
    }
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Option<Word> = None;
    let mut shell = false;
    let mut it = command.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' => words.extend(cur.take()),
            '\n' => {
                words.extend(cur.take());
                shell = true;
            }
            '\'' => {
                let w = cur.get_or_insert_with(Word::default);
                w.quoted = true;
                loop {
                    match it.next() {
                        Some('\'') => break,
                        Some(ch) => w.text.push(ch),
                        None => return unreadable("a single quote is never closed"),
                    }
                }
            }
            '"' => {
                let w = cur.get_or_insert_with(Word::default);
                w.quoted = true;
                loop {
                    match it.next() {
                        Some('"') => break,
                        Some('\\') => match it.next() {
                            Some(n @ ('$' | '`' | '"' | '\\')) => w.text.push(n),
                            Some('\n') => {}
                            Some(n) => {
                                w.text.push('\\');
                                w.text.push(n);
                            }
                            None => return unreadable("a double quote is never closed"),
                        },
                        Some(ch) => {
                            if matches!(ch, '$' | '`') {
                                w.expands = true;
                            }
                            w.text.push(ch);
                        }
                        None => return unreadable("a double quote is never closed"),
                    }
                }
            }
            // A backslash escapes the next character. At the very end the shell keeps the backslash as a character of the word.
            '\\' => match it.next() {
                Some('\n') => {}
                Some(n) => {
                    let w = cur.get_or_insert_with(Word::default);
                    w.quoted = true;
                    w.text.push(n);
                }
                None => {
                    let w = cur.get_or_insert_with(Word::default);
                    w.quoted = true;
                    w.text.push('\\');
                }
            },
            // A comment runs to the end of the line, and only a `#` that starts a word begins one.
            '#' if cur.is_none() => while it.next_if(|n| *n != '\n').is_some() {},
            '&' | '|' | ';' | '<' | '>' | '(' | ')' => {
                words.extend(cur.take());
                shell = true;
            }
            '$' | '`' | '*' | '?' | '[' | '{' => {
                let w = cur.get_or_insert_with(Word::default);
                w.expands = true;
                w.text.push(c);
            }
            // The shell expands a tilde at the start of a word, and after `=` or `:` in an assignment value.
            '~' => {
                let w = cur.get_or_insert_with(Word::default);
                if w.text.is_empty() || w.text.ends_with(['=', ':']) {
                    w.expands = true;
                }
                w.text.push(c);
            }
            _ => {
                let w = cur.get_or_insert_with(Word::default);
                if c == '=' && w.eq.is_none() && !w.quoted && is_name(&w.text) {
                    w.eq = Some(w.text.len());
                }
                w.text.push(c);
            }
        }
    }
    words.extend(cur.take());
    if words.is_empty() && !shell {
        return unreadable("there is no command, only blanks or a comment");
    }
    // `NAME=value` words in front are the environment of the program; a value the shell would expand makes the line a shell line.
    let mut env: Vec<(String, String)> = Vec::new();
    let mut first_arg = 0;
    while let Some(eq) = words.get(first_arg).and_then(|w| w.eq) {
        let w = &words[first_arg];
        if w.expands {
            shell = true;
        }
        env.push((w.text[..eq].to_string(), w.text[eq + 1..].to_string()));
        first_arg += 1;
    }
    let argv_words = &words[first_arg..];
    // A change of `PATH` in front changes which program runs, and only the shell resolves that the way the author meant.
    let sets_path = env.iter().any(|(k, _)| k == "PATH");
    let owned = argv_words
        .first()
        .is_some_and(|w| SHELL_OWNED.contains(&w.text.as_str()));
    if shell || sets_path || owned || argv_words.is_empty() || words.iter().any(|w| w.expands) {
        return Ok(Parsed::Shell);
    }
    Ok(Parsed::Plain {
        env,
        argv: argv_words.iter().map(|w| w.text.clone()).collect(),
    })
}

/// Whether the user the step runs as may start the file: the harness's own user when `who` is `None`, else `(uid, gid)` with no other groups
/// (the launcher clears them), judged from the mode bits. A file the harness could start but the test user cannot is not found, so the shell
/// says "Permission denied" as it always did.
fn may_execute(meta: &std::fs::Metadata, who: Option<(u32, u32)>) -> bool {
    if !meta.is_file() {
        return false;
    }
    let mode = meta.permissions().mode();
    match who {
        None => mode & 0o111 != 0,
        // As the kernel decides: the owner's bits for the owner, else the group's for the group, else the others'.
        Some((uid, gid)) => {
            if meta.uid() == uid {
                mode & 0o100 != 0
            } else if meta.gid() == gid {
                mode & 0o010 != 0
            } else {
                mode & 0o001 != 0
            }
        }
    }
}

/// The executable file `program` names for the user `who`: a name with no slash is looked for in the directories of `path`, a name with a slash is
/// a path (relative to `cwd` when it is relative). The shell's own words (`exit`, `cd`, `if`) and programs the session lacks are not found.
pub fn find_program_as(
    program: &str,
    path: &[PathBuf],
    cwd: &Path,
    who: Option<(u32, u32)>,
) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let ok = |p: PathBuf| {
        std::fs::metadata(&p)
            .ok()
            .filter(|m| may_execute(m, who))
            .map(|_| p)
    };
    if program.contains('/') {
        let p = Path::new(program);
        return ok(if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        });
    }
    path.iter().find_map(|dir| ok(dir.join(program)))
}

/// [`find_program_as`] for the harness's own user.
pub fn program_found(program: &str, path: &[PathBuf], cwd: &Path) -> bool {
    find_program_as(program, path, cwd, None).is_some()
}

/// [`find_program_as`] as a yes or no.
pub fn program_found_as(
    program: &str,
    path: &[PathBuf],
    cwd: &Path,
    who: Option<(u32, u32)>,
) -> bool {
    find_program_as(program, path, cwd, who).is_some()
}

/// Whether the kernel can start `file` itself: it begins with `#!` (an interpreter line) or with the magic of a binary (`\x7fELF`). An
/// executable text file with neither is run by the shell as a shell script, so it goes to the shell.
pub fn runs_directly(file: &Path) -> bool {
    let mut head = [0u8; 4];
    let Ok(mut f) = std::fs::File::open(file) else {
        return false;
    };
    let Ok(n) = f.read(&mut head) else {
        return false;
    };
    head[..n].starts_with(b"#!") || head[..n].starts_with(b"\x7fELF")
}

// ---- notes in the command column of the lifecycle log ---------------------------------------------------------------------------

/// The characters of `s` that are outside quotes and not escaped, with their byte positions; `None` when a quote is never closed.
fn unquoted(s: &str) -> Option<Vec<(usize, char)>> {
    let mut out = Vec::new();
    let mut it = s.char_indices();
    while let Some((i, c)) = it.next() {
        match c {
            '\'' => loop {
                match it.next() {
                    Some((_, '\'')) => break,
                    Some(_) => {}
                    None => return None,
                }
            },
            '"' => loop {
                match it.next() {
                    Some((_, '"')) => break,
                    Some((_, '\\')) => {
                        it.next();
                    }
                    Some(_) => {}
                    None => return None,
                }
            },
            '\\' => {
                it.next();
            }
            _ => out.push((i, c)),
        }
    }
    Some(out)
}

fn starts_word(s: &str, i: usize) -> bool {
    s[..i].chars().next_back().is_none_or(char::is_whitespace)
}

/// The start of a trailing group `open ... close` that is a note, found from the end. The text in front must close its own quotes (an apostrophe in
/// the note, as in `(didn't run it)`, is no reason to leave the note in), follow a word and a space, and not be an operator, a keyword that
/// takes a command next (`! (pytest)`, `if (pytest)`) or a function head (`f() (pytest)`): in all of those the group belongs to the command.
fn note_group(s: &str, open: char, close: char) -> Option<usize> {
    if !s.ends_with(close) {
        return None;
    }
    let mut depth = 0usize;
    let mut start = None;
    for (i, c) in s.char_indices().rev() {
        if c == close {
            depth += 1;
        } else if c == open {
            depth -= 1;
            if depth == 0 {
                start = Some(i);
                break;
            }
        }
    }
    let at = start?;
    let before = &s[..at];
    if !before.ends_with(char::is_whitespace) {
        return None;
    }
    unquoted(before)?;
    let head = before.trim_end();
    let last_word = head.rsplit(char::is_whitespace).next().unwrap_or("");
    let operator = head.ends_with(['&', '|', ';', '(', '<', '>']);
    let takes_a_command = matches!(
        last_word,
        "!" | "if" | "then" | "else" | "elif" | "do" | "while" | "until" | "time" | "{" | "}"
    );
    let function_head = head.ends_with("()");
    if head.is_empty() || operator || takes_a_command || function_head {
        return None;
    }
    // A pair of brackets is a note only with a space inside and none at the start: `[3 tests]`. One word, `[ab]`, is a glob, and `[ -d x ]` is a test.
    let inside = &s[at + 1..s.len() - 1];
    if open == '['
        && (inside.starts_with(char::is_whitespace) || !inside.contains(char::is_whitespace))
    {
        return None;
    }
    Some(at)
}

/// The words of `s` with the byte position each starts at.
fn tokens(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(b)) => {
                out.push((b, &s[b..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(b) = start {
        out.push((b, &s[b..]));
    }
    out
}

/// A tail that says how the command ended: `exit 0`, `exit code 1`, `exit code: 1`, `exit=2`, after a space or after `->`, `=>`, `-`, `,` or
/// `:`. Not a note: a command that follows an operator (`cmd; exit 3`, `cmd && exit 1`), an `exit` that is the whole command, and a number
/// after a short option (`pytest -k exit 0`), which may be that option's value.
fn note_exit(s: &str) -> Option<usize> {
    let tokens = tokens(s);
    let n = tokens.len();
    let is_code = |t: &str| !t.is_empty() && t.len() <= 3 && t.bytes().all(|b| b.is_ascii_digit());
    let label = |t: &str| matches!(t.trim_end_matches([':', '=']), "code" | "status");
    let (mut k, bare) = match tokens.as_slice() {
        [.., (_, "exit"), (_, l), (_, code)] if label(l) && is_code(code) => (n - 3, false),
        [.., (_, "exit" | "exit:"), (_, code)] if is_code(code) => (n - 2, true),
        [.., (_, last)]
            if last
                .strip_prefix("exit=")
                .or_else(|| last.strip_prefix("exit:"))
                .is_some_and(is_code) =>
        {
            (n - 1, false)
        }
        _ => return None,
    };
    let mut separated = false;
    while k > 0
        && matches!(
            tokens[k - 1].1,
            "->" | "=>" | "-" | "\u{2192}" | "\u{2014}" | "\u{2013}" | "," | ":"
        )
    {
        k -= 1;
        separated = true;
    }
    if k == 0 {
        return None;
    }
    let prev = tokens[k - 1].1;
    if matches!(prev, "&&" | "||" | ";" | "|" | "&") || prev.ends_with(['&', '|', ';']) {
        return None;
    }
    let short_option =
        prev.len() == 2 && prev.starts_with('-') && prev.as_bytes()[1].is_ascii_alphabetic();
    if bare && !separated && short_option {
        return None;
    }
    // A comma or a colon glued to the word in front (`--workspace,`) belongs to the note.
    let mut end = tokens[k].0;
    let kept = s[..end].trim_end();
    if kept.ends_with([',', ':']) {
        end = kept.len() - 1;
    }
    Some(end)
}

/// A last pipe into a number, `| 12 tests`: no program is called `12`, so the text after the pipe is a note.
fn note_pipe(s: &str) -> Option<usize> {
    let chars = unquoted(s)?;
    let (p, _) = chars.iter().rev().find(|(_, c)| *c == '|').copied()?;
    let bytes = s.as_bytes();
    if p == 0 || bytes[p - 1] == b'|' || bytes.get(p + 1) == Some(&b'|') || bytes[p - 1] == b'>' {
        return None;
    }
    let first = s[p + 1..].split_whitespace().next()?;
    (first.bytes().all(|b| b.is_ascii_digit()) && !s[..p].trim().is_empty()).then_some(p)
}

/// A `#` comment that starts a word and runs to the end of the text. Read left to right with the quotes of the command; what follows the `#`
/// is not read, so an apostrophe in the comment is no quote. A comment followed by another line is not a note: the next line is a command.
fn note_comment(s: &str) -> Option<usize> {
    let mut it = s.char_indices();
    while let Some((i, c)) = it.next() {
        match c {
            '\'' => loop {
                match it.next() {
                    Some((_, '\'')) => break,
                    Some(_) => {}
                    None => return None,
                }
            },
            '"' => loop {
                match it.next() {
                    Some((_, '"')) => break,
                    Some((_, '\\')) => {
                        it.next();
                    }
                    Some(_) => {}
                    None => return None,
                }
            },
            '\\' => {
                it.next();
            }
            '#' if starts_word(s, i) => return (!s[i..].contains('\n')).then_some(i),
            _ => {}
        }
    }
    None
}

/// The command column of a lifecycle log line without the notes after the command. The log has the exit code and the counts in columns of their
/// own (`RUN | <command> | exit N | pass P fail F | <summary>`), so text after the command in this column is a note. The notes cut, from the end
/// and in a row, as long as one is there:
///
/// - a trailing group after a space: `(3 tests)`, `(final run after the last edit)`, `(didn't run the slow tests)`, `[3 tests]`;
/// - an exit tail: `exit 0`, `exit code 1`, `exit code: 1`, `exit=2`, also after `->`, `=>`, `-`, `,` or `:`;
/// - a last pipe into a number: `| 12 tests`;
/// - a comment that starts a word: `# all green`.
///
/// Everything a shell can read stays: `cd api && cmd`, `cmd | tail -5`, `(cd api && cmd)`, `make test && (cd x && ls)`, `! (cmd)`, `cmd; exit 3`,
/// `exit 3`, `pytest -k exit 0` (a short option may take that value), a `#` inside a word or inside quotes, one word in brackets (`[ab]`, a glob).
/// A column of over 2,000 characters is left as it is.
pub fn strip_notes(column: &str) -> String {
    let mut s = column.trim();
    if s.chars().count() > MAX_COLUMN_CHARS {
        return s.to_string();
    }
    loop {
        let cut = note_group(s, '(', ')')
            .or_else(|| note_group(s, '[', ']'))
            .or_else(|| note_exit(s))
            .or_else(|| note_pipe(s))
            .or_else(|| note_comment(s));
        match cut {
            Some(end) => s = s[..end].trim_end(),
            None => return s.to_string(),
        }
    }
}

/// The next word of `s` (up to a space or tab) and what follows it; `None` when `s` holds only white space.
fn next_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    let end = s.find(char::is_whitespace).unwrap_or(s.len());
    Some((&s[..end], &s[end..]))
}

/// A `NAME=value` word with no quote or backslash in it, so that the word is the whole assignment.
fn plain_assignment(word: &str) -> bool {
    let Some((name, value)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !value.contains(['\'', '"', '\\'])
}

/// The signals `timeout -s` takes by name (with or without `SIG`) or by number.
fn timeout_signal(word: &str) -> bool {
    const NAMES: [&str; 31] = [
        "HUP", "INT", "QUIT", "ILL", "TRAP", "ABRT", "BUS", "FPE", "KILL", "USR1", "SEGV", "USR2",
        "PIPE", "ALRM", "TERM", "STKFLT", "CHLD", "CONT", "STOP", "TSTP", "TTIN", "TTOU", "URG",
        "XCPU", "XFSZ", "VTALRM", "PROF", "WINCH", "IO", "PWR", "SYS",
    ];
    if word.bytes().all(|b| b.is_ascii_digit()) && !word.is_empty() {
        return word.parse::<u32>().is_ok_and(|n| n <= 64);
    }
    let name = word.strip_prefix("SIG").unwrap_or(word);
    NAMES.contains(&name)
}

/// A duration as `timeout` reads it: digits with an optional fraction and an optional `s`, `m`, `h` or `d`.
fn timeout_duration(word: &str) -> bool {
    let n = word.strip_suffix(['s', 'm', 'h', 'd']).unwrap_or(word);
    let (whole, frac) = n.split_once('.').unwrap_or((n, "0"));
    let digits = |t: &str| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit());
    digits(whole) && digits(frac)
}

/// `s` without one leading `timeout` wrapper, `None` when `s` does not start with one in a form this reads. The form is
/// `[NAME=value ...] timeout [-k DURATION | --kill-after=DURATION | -s SIGNAL | --signal=SIGNAL | --foreground | --preserve-status | -v | --verbose | --] DURATION command`;
/// the assignments stay in front of the command and the command is kept as typed, quotes and all.
fn without_one_timeout(s: &str) -> Option<String> {
    let mut env: Vec<&str> = Vec::new();
    let mut rest = s;
    let (word, mut tail) = loop {
        let (w, t) = next_word(rest)?;
        if plain_assignment(w) {
            env.push(w);
            rest = t;
        } else {
            break (w, t);
        }
    };
    if !matches!(
        word,
        "timeout" | "/usr/bin/timeout" | "/bin/timeout" | "/usr/local/bin/timeout"
    ) {
        return None;
    }
    // An option and its operand are cut together only when the operand is one `timeout` takes; else the shell would not have run the command.
    let operand = |kind: char, value: &str| match kind {
        'k' => timeout_duration(value),
        _ => timeout_signal(value),
    };
    loop {
        let (o, t) = next_word(tail)?;
        match o {
            "--" => {
                tail = t;
                break;
            }
            "--foreground" | "--preserve-status" | "-v" | "--verbose" => tail = t,
            "-k" | "--kill-after" | "-s" | "--signal" => {
                let (value, after) = next_word(t)?;
                if !operand(if o.contains('k') { 'k' } else { 's' }, value) {
                    return None;
                }
                tail = after;
            }
            _ if o.starts_with("--kill-after=") => {
                if !operand('k', &o["--kill-after=".len()..]) {
                    return None;
                }
                tail = t;
            }
            _ if o.starts_with("--signal=") => {
                if !operand('s', &o["--signal=".len()..]) {
                    return None;
                }
                tail = t;
            }
            _ if o.len() > 2
                && !o.starts_with("--")
                && (o.starts_with("-k") || o.starts_with("-s")) =>
            {
                if !operand(if o.starts_with("-k") { 'k' } else { 's' }, &o[2..]) {
                    return None;
                }
                tail = t;
            }
            _ => break,
        }
    }
    let (duration, command) = next_word(tail)?;
    let command = command.trim_start();
    // `timeout 5 FOO=1 cmd` runs a program named `FOO=1`; cut, it would read as `FOO=1 cmd`, which is another command.
    let first = next_word(command).map(|(w, _)| w);
    if !timeout_duration(duration) || first.is_none_or(plain_assignment) {
        return None;
    }
    Some(if env.is_empty() {
        command.to_string()
    } else {
        format!("{} {command}", env.join(" "))
    })
}

/// `command` without a leading `timeout` wrapper: the model's own limit on a run (`timeout 240 bash scripts/test-unit.sh`), which the report
/// replaces with its own per-step limit. It is cut from the command column of the lifecycle log only, before the log's commands are compared, so a run
/// typed with the wrapper and the same run typed without it are one command. A form this does not read exactly (an unknown option, a duration that is
/// no number, a quoted assignment in front, `sudo timeout ...`) is left as it is, and so is a command with nothing after the duration.
pub fn strip_timeout(command: &str) -> String {
    let mut s = command.trim().to_string();
    while let Some(inner) = without_one_timeout(&s) {
        s = inner;
    }
    s
}
