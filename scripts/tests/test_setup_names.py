"""Tests for scripts/setup/names.sh, the one definition of the client's names.

`names.sh` is sourced by `setup.sh` and by the scripts of `scripts/setup/`. It defines the client binary name (`HAMH_CLIENT_BIN`, default `ham`),
the product name (`HAMH_PRODUCT_NAME`), the config directory name (`HAMH_CONFIG_DIR_NAME`, default `ham`) and the table of client platforms
(`darwin-arm64`, `linux-amd64`, `linux-arm64`, each with its Rust target and the kind of binary it must be). The environment overrides every
value, which is how the tests change it: an override test sets a probe value (`widget`) that is none of the defaults, so that it fails when the override is ignored.

Cases:

  defaults    the three names and the three platform rows; environment overrides; sourcing twice; sourcing under `set -u`.
  table       `ham_client_platforms`, `ham_client_target`, `ham_client_kind`, an unknown platform, a replaced table, a malformed row.
  host        `ham_host_platform <uname -s> <uname -m>` for the machines the table serves, and the refusal text for those it does not.
  bash32      the file uses nothing newer than bash 3.2 (the bash that ships with macOS).
  literal     the detector that finds a script invoking the client by the literal word instead of "$HAMH_CLIENT_BIN": positives, the allowed
              forms (comments, text that is printed, heredoc bodies, other names that merely contain the letters), and the repository scan
              of `scripts/setup/*.sh` (strict) and `setup.sh` (strict, except the three default lines of `SETUP_SH_FALLBACK`, which must equal the defaults of `names.sh`).
  prose       the second scan, which finds the client name as a word in any non-comment word: printed text, prompts, the arguments of `fail`, `warn` and `die`, and the
              bodies of here-documents (the help writes `@CLIENT@`). Resource names (`hamh-direct-control`, `cc:hamh-e2e-`) and the namespace `/hamh/<stage>/` are other names.
  one         `names.sh` is the only place that assigns the names; a script that uses `HAMH_CLIENT_BIN` sources `names.sh`.

Run on a Linux machine with the tools pinned in `tools.lock`:
  $HAMH_PYTHON -m unittest discover -s scripts/tests -p test_setup_names.py
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess  # nosec B404: tests start bash with argument lists, never through a shell string built from data
import tempfile
import unittest
from pathlib import Path
from typing import Iterator, List, NamedTuple, Optional, Sequence, Tuple

from setup_waits import limit

REPO = Path(__file__).resolve().parents[2]
PROBE = "widget"  # a name that is none of the defaults: an override test with a value equal to the default could not fail
PROBE_FOLDER = "gadget"  # a second probe for the settings folder, so that a swap of the two names fails too
NAMES = REPO / "scripts" / "setup" / "names.sh"
SETUP_DIR = REPO / "scripts" / "setup"
SETUP_SH = REPO / "setup.sh"

PLATFORMS = [
    ("darwin-arm64", "aarch64-apple-darwin", "macho-arm64"),
    ("linux-amd64", "x86_64-unknown-linux-musl", "elf-x86_64"),
    ("linux-arm64", "aarch64-unknown-linux-musl", "elf-aarch64"),
]


def bash(script: str, env: Optional[dict] = None, source: bool = True) -> subprocess.CompletedProcess:
    """Run `script` in a fresh bash that has sourced names.sh (unless source is False), under `set -u`."""
    e = {"PATH": os.environ["PATH"], "HOME": os.environ.get("TMPDIR") or tempfile.gettempdir()}
    e.update(env or {})
    prefix = f'set -u; . "{NAMES}"; ' if source else "set -u; "
    return subprocess.run(["bash", "-c", prefix + script], capture_output=True, text=True, env=e, timeout=limit(60))  # nosec B603: argument list, no shell; B607: bash looked up on PATH as the other script tests do


class DefaultsCase(unittest.TestCase):
    def test_the_three_names_have_their_defaults(self) -> None:
        p = bash('printf "%s|%s|%s" "$HAMH_CLIENT_BIN" "$HAMH_PRODUCT_NAME" "$HAMH_CONFIG_DIR_NAME"')
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout, "ham|Hyperscale Agent Meta Harness|ham")

    def test_the_operator_tool_is_named_after_the_client_unless_it_is_set(self) -> None:
        p = bash('printf "%s" "$HAMH_ADMIN_BIN"')
        self.assertEqual((p.returncode, p.stdout), (0, "ham-admin"), p.stderr)
        p = bash('printf "%s" "$HAMH_ADMIN_BIN"', env={"HAMH_CLIENT_BIN": PROBE})
        self.assertEqual((p.returncode, p.stdout), (0, PROBE + "-admin"), p.stderr)
        p = bash('printf "%s" "$HAMH_ADMIN_BIN"', env={"HAMH_CLIENT_BIN": PROBE, "HAMH_ADMIN_BIN": "ops"})
        self.assertEqual((p.returncode, p.stdout), (0, "ops"), p.stderr)

    def test_a_name_of_the_operator_tool_the_scripts_could_not_use_is_refused(self) -> None:
        for bad in ("Ops", "1ops", "o ps", "ops;x", "-ops"):
            with self.subTest(bad=bad):
                p = bash(f'. "{NAMES}"; echo "rc=$?"', env={"HAMH_ADMIN_BIN": bad}, source=False)
                self.assertEqual(p.stdout.strip(), "rc=1", "HAMH_ADMIN_BIN=%r was accepted" % bad)
                self.assertIn("HAMH_ADMIN_BIN", p.stderr)

    def test_the_operator_tool_cannot_have_the_name_of_the_client(self) -> None:
        p = bash(f'. "{NAMES}"; echo "rc=$?"', env={"HAMH_CLIENT_BIN": "ops", "HAMH_ADMIN_BIN": "ops"}, source=False)
        self.assertEqual(p.stdout.strip(), "rc=1")
        self.assertIn("must differ", p.stderr)

    def test_run_directly_it_lists_the_operator_tool_too(self) -> None:
        p = subprocess.run(["bash", str(NAMES)], capture_output=True, text=True, env={"PATH": os.environ["PATH"]}, timeout=limit(60))  # nosec B603: argument list, no shell; B607: bash looked up on PATH as the other script tests do
        self.assertIn("HAMH_ADMIN_BIN=ham-admin", p.stdout.splitlines())

    def test_the_environment_overrides_every_name(self) -> None:
        p = bash(
            'printf "%s|%s|%s" "$HAMH_CLIENT_BIN" "$HAMH_PRODUCT_NAME" "$HAMH_CONFIG_DIR_NAME"',
            env={"HAMH_CLIENT_BIN": PROBE, "HAMH_PRODUCT_NAME": "Widget Harness", "HAMH_CONFIG_DIR_NAME": PROBE_FOLDER},
        )
        self.assertEqual((p.returncode, p.stdout), (0, PROBE + "|Widget Harness|" + PROBE_FOLDER), p.stderr)

    def test_overriding_the_client_name_leaves_the_other_names_at_their_defaults(self) -> None:
        p = bash('printf "%s|%s|%s" "$HAMH_CLIENT_BIN" "$HAMH_PRODUCT_NAME" "$HAMH_CONFIG_DIR_NAME"', env={"HAMH_CLIENT_BIN": PROBE})
        self.assertEqual((p.returncode, p.stdout), (0, PROBE + "|Hyperscale Agent Meta Harness|ham"), p.stderr)

    def test_sourcing_twice_changes_nothing_and_keeps_a_value_set_before(self) -> None:
        p = bash('HAMH_CLIENT_BIN=' + PROBE + '; . "' + str(NAMES) + '"; printf "%s" "$HAMH_CLIENT_BIN"', source=False)
        self.assertEqual((p.returncode, p.stdout), (0, PROBE), p.stderr)
        p = bash('. "' + str(NAMES) + '"; printf "%s" "$HAMH_CLIENT_BIN"')
        self.assertEqual((p.returncode, p.stdout), (0, "ham"), p.stderr)

    def test_sourcing_prints_nothing(self) -> None:
        p = bash("true")
        self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "", ""))

    def test_run_directly_it_lists_the_names_and_the_table(self) -> None:
        p = subprocess.run(["bash", str(NAMES)], capture_output=True, text=True, timeout=limit(60), env={"PATH": os.environ["PATH"]})  # nosec B603: argument list, no shell; B607: bash looked up on PATH as the other script tests do
        self.assertEqual(p.returncode, 0, p.stderr)
        lines = p.stdout.splitlines()
        self.assertIn("HAMH_CLIENT_BIN=ham", lines)
        self.assertIn("HAMH_CONFIG_DIR_NAME=ham", lines)
        self.assertIn("PLATFORM darwin-arm64 aarch64-apple-darwin macho-arm64", lines)


class TableCase(unittest.TestCase):
    def test_the_platforms_are_listed_in_table_order(self) -> None:
        p = bash("ham_client_platforms")
        self.assertEqual((p.returncode, p.stdout.split()), (0, [x[0] for x in PLATFORMS]), p.stderr)

    def test_each_platform_names_its_rust_target_and_binary_kind(self) -> None:
        for platform, target, kind in PLATFORMS:
            with self.subTest(platform=platform):
                p = bash(f'printf "%s %s" "$(ham_client_target {platform})" "$(ham_client_kind {platform})"')
                self.assertEqual((p.returncode, p.stdout), (0, f"{target} {kind}"), p.stderr)

    def test_an_unknown_platform_is_refused_with_the_known_ones_named(self) -> None:
        for fn in ("ham_client_target", "ham_client_kind"):
            with self.subTest(fn=fn):
                p = bash(f"{fn} darwin-amd64")
                self.assertEqual(p.returncode, 1)
                self.assertEqual(p.stdout, "")
                self.assertIn("darwin-amd64", p.stderr)
                self.assertIn("darwin-arm64 linux-amd64 linux-arm64", p.stderr)

    def test_a_replaced_table_is_what_the_functions_read(self) -> None:
        table = "# one row\n\nlinux-amd64 x86_64-unknown-linux-musl elf-x86_64\n"
        p = bash('ham_client_platforms; ham_client_target linux-amd64', env={"HAMH_CLIENT_PLATFORM_TABLE": table})
        self.assertEqual((p.returncode, p.stdout.split()), (0, ["linux-amd64", "x86_64-unknown-linux-musl"]), p.stderr)
        p = bash("ham_client_target darwin-arm64", env={"HAMH_CLIENT_PLATFORM_TABLE": table})
        self.assertEqual(p.returncode, 1)

    def test_a_malformed_row_fails_the_listing_and_names_the_row(self) -> None:
        table = "linux-amd64 x86_64-unknown-linux-musl\n"
        p = bash("ham_client_platforms", env={"HAMH_CLIENT_PLATFORM_TABLE": table})
        self.assertEqual(p.returncode, 1)
        self.assertEqual(p.stdout, "")
        self.assertIn("linux-amd64 x86_64-unknown-linux-musl", p.stderr)

    def test_an_unknown_binary_kind_is_a_malformed_row(self) -> None:
        table = "linux-amd64 x86_64-unknown-linux-musl elf-mips\n"
        p = bash("ham_client_platforms", env={"HAMH_CLIENT_PLATFORM_TABLE": table})
        self.assertEqual(p.returncode, 1)
        self.assertIn("elf-mips", p.stderr)

    def test_a_platform_name_with_shell_metacharacters_is_refused_not_interpreted(self) -> None:
        with tempfile.TemporaryDirectory(prefix="names-meta-", dir=os.environ.get("TMPDIR")) as td:
            marker = Path(td) / "touched"
            p = bash(f'ham_client_target "linux&amd64 ; touch {marker}"')
            self.assertEqual(p.returncode, 1)
            self.assertFalse(marker.exists())


class HostCase(unittest.TestCase):
    def host(self, os_name: str, arch: str) -> subprocess.CompletedProcess:
        return bash(f'ham_host_platform "{os_name}" "{arch}"')

    def test_the_machines_the_table_serves(self) -> None:
        for os_name, arch, want in [
            ("Darwin", "arm64", "darwin-arm64"),
            ("Linux", "x86_64", "linux-amd64"),
            ("Linux", "amd64", "linux-amd64"),
            ("Linux", "aarch64", "linux-arm64"),
            ("Linux", "arm64", "linux-arm64"),
        ]:
            with self.subTest(os=os_name, arch=arch):
                p = self.host(os_name, arch)
                self.assertEqual((p.returncode, p.stdout.strip()), (0, want), p.stderr)

    def test_an_intel_mac_is_refused_with_the_reason_and_the_way_out(self) -> None:
        p = self.host("Darwin", "x86_64")
        self.assertEqual(p.returncode, 1)
        self.assertEqual(p.stdout, "")
        self.assertIn("Intel", p.stderr)
        self.assertIn("darwin-arm64", p.stderr)

    def test_other_systems_are_refused(self) -> None:
        for os_name, arch in [("FreeBSD", "amd64"), ("Linux", "riscv64"), ("Darwin", "ppc")]:
            with self.subTest(os=os_name, arch=arch):
                p = self.host(os_name, arch)
                self.assertEqual(p.returncode, 1)
                self.assertEqual(p.stdout, "")
                self.assertIn(os_name, p.stderr)

    def test_with_no_arguments_the_function_asks_uname(self) -> None:
        p = bash("ham_host_platform")
        want = {("Darwin", "arm64"): "darwin-arm64", ("Linux", "x86_64"): "linux-amd64", ("Linux", "aarch64"): "linux-arm64"}.get(
            (os.uname().sysname, os.uname().machine)
        )
        if want is None:
            self.assertEqual(p.returncode, 1)
        else:
            self.assertEqual((p.returncode, p.stdout.strip()), (0, want), p.stderr)


BASH4_ONLY = [
    (r"declare\s+-[a-zA-Z]*A", "associative array"),
    (r"\bmapfile\b|\breadarray\b", "mapfile/readarray"),
    (r"\$\{[A-Za-z_][A-Za-z0-9_]*(,,|\^\^|,|\^)\}", "case-changing expansion"),
    (r"\blocal\s+-n\b|\bdeclare\s+-n\b", "nameref"),
    (r"&>>|\|&", "bash 4 redirection"),
    (r"\bcoproc\b", "coproc"),
    (r"\$\{[A-Za-z_][A-Za-z0-9_]*@[QEPAa]\}", "parameter transformation"),
]


class Bash32Case(unittest.TestCase):
    def test_names_sh_uses_nothing_newer_than_bash_3_2(self) -> None:
        text = NAMES.read_text(encoding="utf-8")
        for pattern, what in BASH4_ONLY:
            self.assertIsNone(re.search(pattern, text), f"names.sh uses {what}, which macOS bash 3.2 lacks")

    def test_sourcing_needs_no_program_at_all(self) -> None:
        """A library of names starts nothing: with an empty PATH the source still succeeds and prints nothing (only the functions use uname)."""
        with tempfile.TemporaryDirectory(prefix="names-nopath-", dir=os.environ.get("TMPDIR")) as td:
            bash_path = shutil.which("bash")
            p = subprocess.run([bash_path, "-c", f'set -u; . "{NAMES}"; printf "%s" "$HAMH_CLIENT_BIN"'], capture_output=True, text=True, env={"PATH": td}, timeout=limit(60))  # nosec B603: argument list, no shell
            self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "ham", ""))


# ----------------------------------------------------------------------------------------------------------------------------
# The detector: a script of the feature that runs the client by the literal word.


class Use(NamedTuple):
    line: int
    word: str
    text: str


# Commands whose arguments are text for a person, not a command to run (a one-word message such as `say ham` is prose).
PROSE_COMMANDS = {
    "echo", "printf", "say", "ok", "warn", "would", "step", "err", "die", "usage_die", "fail", "detail", "note", "info", "log", "phase",
}
WRAPPERS = {"command", "exec", "sudo", "nohup", "env", "time", "timeout", "xargs", "nice", "type", "which", "hash", "builtin", "stdbuf"}
KEYWORDS = {"if", "then", "else", "elif", "fi", "while", "until", "do", "done", "!", "{", "}", "function", "for", "in", "case", "esac", "select"}
SHELLS = {"bash", "sh", "zsh", "dash"}
# The letters of the client name as a word: not part of a longer name (`hamh-e2e-`, `hamh-direct-control`, `graham`, `HAMH_`), but a path component
# (`$BINDIR/ham`, `~/.config/ham/`), an assignment (`NAME=ham`), a default (`${X:-ham}`), an option value (`--bin ham`) or the bare word.
LITERAL = re.compile(r"(?<![A-Za-z0-9_])(?<![A-Za-z0-9_]-)ham(?![A-Za-z0-9_-])")


class Word(NamedTuple):
    text: str
    line: int


def names_the_client(word: str) -> bool:
    """True when the word holds the client name as a word. A word that begins `/hamh/` is the stage's parameter namespace in the Systems Manager
    parameter store (`/hamh/direct/deployment-digest`, written by scripts/lib/deploy.py), not a file system path of the client; only the rest of
    such a word is read."""
    if re.search(r"\s", word):
        return False
    return bool(LITERAL.search(word[len("/ham") :] if word.startswith("/hamh/") else word))


def _matching(text: str, i: int, open_c: str, close_c: str) -> int:
    """Index of the character that closes the group opened at text[i] (quotes inside the group are skipped)."""
    depth, j, n = 0, i, len(text)
    while j < n:
        c = text[j]
        if c == "\\":
            j += 2
            continue
        if c == "'" and open_c != "`":
            j = text.find("'", j + 1)
            if j < 0:
                return n
        elif c == '"':
            j += 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
        elif c == open_c and open_c != close_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return j
        j += 1
    return n


def lex(text: str, first_line: int = 1, bodies: bool = False) -> Iterator[List[Word]]:
    """Yield the simple commands of a shell text as lists of words (quotes removed, comments dropped, heredoc bodies dropped unless `bodies`).

    With `bodies` a here-document is yielded as a command of two words: `<<heredoc` and the text of its body (the text a person reads when it holds help or a message).

    A command substitution is lexed as commands of its own and leaves a placeholder in its word; the parameter expansion `${...}` stays in
    the word as written, so a default such as `${X:-ham}` is visible. This is a reader for a test, not a shell: it knows the forms the scripts of
    this repository use.
    """
    i, n, line = 0, len(text), first_line
    words: List[Word] = []
    cur: List[str] = []
    in_word, word_line = False, line
    heredocs: List[Tuple[str, bool]] = []
    pending: List[List[Word]] = []

    def end_word() -> None:
        nonlocal in_word, cur
        if in_word:
            words.append(Word("".join(cur), word_line))
        cur, in_word = [], False

    def start() -> None:
        nonlocal in_word, word_line
        if not in_word:
            in_word, word_line = True, line

    while i < n:
        c = text[i]
        if c == "\n":
            end_word()
            if words:
                pending.append(words)
                words = []
            line += 1
            i += 1
            while heredocs:
                delim, strip = heredocs.pop(0)
                body_line, body_lines = line, []
                while i < n:
                    j = text.find("\n", i)
                    j = n if j < 0 else j
                    body = text[i:j]
                    i = min(j + 1, n)
                    line += 1
                    if (body.lstrip("\t") if strip else body) == delim:
                        break
                    body_lines.append(body)
                if bodies:
                    pending.append([Word("<<heredoc", body_line), Word("\n".join(body_lines), body_line)])
            yield from pending
            pending = []
        elif c in " \t":
            end_word()
            i += 1
        elif c == "\\" and text[i + 1 : i + 2] == "\n":
            i += 2
            line += 1
        elif c == "\\":
            start()
            cur.append(text[i + 1 : i + 2])
            i += 2
        elif c == "#" and not in_word:
            j = text.find("\n", i)
            i = n if j < 0 else j
        elif c == "'":
            start()
            j = text.find("'", i + 1)
            j = n if j < 0 else j
            cur.append(text[i + 1 : j])
            line += text.count("\n", i, j)
            i = j + 1
        elif c == '"':
            start()
            i += 1
            while i < n and text[i] != '"':
                if text[i] == "\\" and i + 1 < n:
                    cur.append(text[i + 1])
                    i += 2
                elif text.startswith("$(", i):
                    j = _matching(text, i + 1, "(", ")")
                    for cmd in lex(text[i + 2 : j], line, bodies):
                        pending.append(cmd)
                    cur.append("$()")
                    line += text.count("\n", i, j)
                    i = j + 1
                elif text[i] == "`":
                    j = text.find("`", i + 1)
                    j = n if j < 0 else j
                    for cmd in lex(text[i + 1 : j], line, bodies):
                        pending.append(cmd)
                    cur.append("$()")
                    i = j + 1
                elif text.startswith("${", i):
                    j = _matching(text, i + 1, "{", "}")
                    cur.append(text[i : j + 1])
                    i = j + 1
                else:
                    if text[i] == "\n":
                        line += 1
                    cur.append(text[i])
                    i += 1
            i += 1
        elif text.startswith("$(", i) and not text.startswith("$((", i):
            start()
            j = _matching(text, i + 1, "(", ")")
            for cmd in lex(text[i + 2 : j], line, bodies):
                pending.append(cmd)
            cur.append("$()")
            line += text.count("\n", i, j)
            i = j + 1
        elif c == "`":
            start()
            j = text.find("`", i + 1)
            j = n if j < 0 else j
            for cmd in lex(text[i + 1 : j], line, bodies):
                pending.append(cmd)
            cur.append("$()")
            i = j + 1
        elif text.startswith("${", i):
            start()
            j = _matching(text, i + 1, "{", "}")
            cur.append(text[i : j + 1])
            i = j + 1
        elif text.startswith("<<<", i):
            end_word()
            i += 3
        elif text.startswith("<<", i) and not text.startswith("<<(", i):
            end_word()
            j = i + 2
            strip = text[j : j + 1] == "-"
            j += 1 if strip else 0
            while text[j : j + 1] in (" ", "\t"):
                j += 1
            m = re.match(r"""(?:'([^'\n]*)'|"([^"\n]*)"|([A-Za-z0-9_.\-]+))""", text[j:])
            if m:
                heredocs.append((m.group(1) or m.group(2) or m.group(3) or "", strip))
                j += m.end()
            i = j
        elif c in ";&|()":
            end_word()
            if words:
                pending.append(words)
                words = []
            i += 1
        else:
            start()
            cur.append(c)
            i += 1
    end_word()
    if words:
        pending.append(words)
    yield from pending


def command_word(words: Sequence[Word]) -> Tuple[Optional[str], int]:
    """The command a simple command runs (skipping assignments, keywords and wrappers such as `command -v`) and its index."""
    k = 0
    while k < len(words):
        w = words[k].text
        if w in KEYWORDS or re.match(r"^[A-Za-z_][A-Za-z0-9_]*(\[[^\]]*\])?\+?=", w):
            k += 1
        elif w in WRAPPERS:
            k += 1
            while k < len(words) and (words[k].text.startswith("-") or re.match(r"^[0-9]+[smhd]?$", words[k].text) or "=" in words[k].text):
                k += 1
        else:
            break
    return (words[k].text, k) if k < len(words) else (None, k)


def literal_uses(text: str) -> List[Use]:
    """Every place a script text runs, names or builds a path to the client by the literal word."""
    found: List[Use] = []
    for words in lex(text):
        cmd, k = command_word(words)
        if cmd in PROSE_COMMANDS:
            continue
        if cmd in SHELLS or cmd == "eval":
            rest = [w.text for w in words[k + 1 :]]
            inner = rest[rest.index("-c") + 1] if "-c" in rest and rest.index("-c") + 1 < len(rest) else (" ".join(rest) if cmd == "eval" else "")
            if inner:
                for sub in lex(inner, words[k].line):
                    for w in sub:
                        if names_the_client(w.text):
                            found.append(Use(w.line, w.text, inner))
        for w in words:
            if names_the_client(w.text):
                found.append(Use(w.line, w.text, " ".join(x.text for x in words)))
    return found


# The stage's namespace in the Systems Manager parameter store starts a word (` /hamh/direct/...`); the same letters after a path component (`.config/ham/`) are the client.
NAMESPACE = re.compile(r"(?<![^\s(\"'=:,])/hamh/")


def client_name_in(text: str) -> List["re.Match[str]"]:
    """Every place a text holds the client name as a word. The stage's parameter namespace `/hamh/<stage>/...` and names that merely begin with the letters
    (`hamh-direct-control`, `hamh-setup-<account>-<Region>`, `cc:hamh-e2e-<name>`) are other names and are not found."""
    return list(LITERAL.finditer(NAMESPACE.sub("/___/", text)))


def prose_uses(text: str) -> List[Use]:
    """Every place a script holds the client name as a word anywhere outside a comment: in a command, an assignment, a path, text that is printed, a prompt, an
    argument of `fail` or `die` and the body of a here-document (the help text and the next steps)."""
    found: List[Use] = []
    for words in lex(text, bodies=True):
        for w in words:
            for m in client_name_in(w.text):
                found.append(Use(w.line + w.text.count("\n", 0, m.start()), m.group(0), w.text))
    return found


class DetectorCase(unittest.TestCase):
    """The detector, on made-up scripts: it must flag every way of running or locating the client by name and nothing else."""

    def flagged(self, text: str) -> List[str]:
        return [u.word for u in literal_uses(text)]

    def test_a_command_run_by_the_literal_word_is_flagged(self) -> None:
        for text in [
            "ham models\n",
            "  ham doctor || true\n",
            'x=$(ham --version)\n',
            'if ham doctor; then echo yes; fi\n',
            "cd /tmp && ham rules init\n",
            "out=`ham models`\n",
            'say "version: $(ham --version)"\n',
            "command ham doctor\n",
            "exec ham\n",
            "env FOO=1 ham models\n",
            "timeout 30 ham doctor\n",
            "sudo ham doctor\n",
            'bash -c "ham doctor"\n',
            "eval ham doctor\n",
            "echo hi | ham config set a b\n",
        ]:
            with self.subTest(text=text):
                self.assertTrue(self.flagged(text), text)

    def test_a_probe_or_a_path_or_an_option_value_naming_the_client_is_flagged(self) -> None:
        for text in [
            "command -v ham\n",
            "type ham >/dev/null\n",
            "DEST=$BINDIR/ham\n",
            'DEST="$BINDIR/ham"\n',
            "SRC=$FROM/ham\n",
            "out=$target/release/ham\n",
            "cargo build --release --locked -p ham --bin ham\n",
            "cargo zigbuild --bin=ham\n",
            "CONF=${XDG_CONFIG_HOME:-$HOME/.config}/ham/config.toml\n",
            'mkdir -p "$HOME/.config/ham/setup"\n',
            'BIN=${HAMH_CLIENT_BIN:-ham}\n',
            "name=ham\n",
            "tmp=$BINDIR/.ham.setup-new\n",
            "[ \"$1\" = ham ]\n",
            "case $x in ham) ;; esac\n",
            "git archive --prefix=ham/ HEAD\n",
        ]:
            with self.subTest(text=text):
                self.assertTrue(self.flagged(text), text)

    def test_the_variable_forms_are_not_flagged(self) -> None:
        for text in [
            '"$HAMH_CLIENT_BIN" models\n',
            '"${HAMH_CLIENT_BIN}" doctor\n',
            "command -v \"$HAMH_CLIENT_BIN\"\n",
            'DEST=$BINDIR/$HAMH_CLIENT_BIN\n',
            'CONF=${XDG_CONFIG_HOME:-$HOME/.config}/$HAMH_CONFIG_DIR_NAME/config.toml\n',
            'cargo zigbuild -p "$HAMH_CLIENT_BIN" --bin "$HAMH_CLIENT_BIN"\n',
            '"$HAM" config set k v\n',
            "x=$(\"$HAMH_CLIENT_BIN\" --version)\n",
        ]:
            with self.subTest(text=text):
                self.assertEqual(self.flagged(text), [], text)

    def test_the_parameter_store_namespace_of_the_stage_is_not_the_client(self) -> None:
        for text in [
            'aws ssm get-parameter --name "/hamh/$STAGE/deployment-digest"\n',
            "current=$(aws ssm get-parameter --name /hamh/direct/image-digest --output json)\n",
        ]:
            with self.subTest(text=text):
                self.assertEqual(self.flagged(text), [], text)
        for text in ['x=/hamh/$STAGE/bin/ham\n', 'cp /tmp/x $HOME/.local/bin/ham\n', 'DEST=/usr/local/ham\n']:
            with self.subTest(text=text):
                self.assertTrue(self.flagged(text), text)

    def test_comments_text_for_a_person_and_heredoc_bodies_are_not_flagged(self) -> None:
        for text in [
            "# run ham models and then ham doctor\n",
            "x=1 # then ham doctor\n",
            'echo "run ham doctor"\n',
            "echo ham\n",
            'say "ham signs every call for one Region"\n',
            "printf '%s\\n' 'ham doctor'\n",
            'warn "ham --version did not print a version"\n',
            'die "could not install ham into $BINDIR"\n',
            "cat <<'EOF'\nusage: ham models\n  ham doctor\nEOF\n",
            "cat <<-EOF\n\tham models\n\tEOF\nrun_it\n",
            "cat <<EOF >out.txt\nham doctor\nEOF\n",
            "x=1\n# ham models\n",
        ]:
            with self.subTest(text=text):
                self.assertEqual(self.flagged(text), [], text)

    def test_other_names_that_merely_contain_the_letters_are_not_flagged(self) -> None:
        for text in [
            "aws cloudformation describe-stacks --stack-name hamh-direct-control\n",
            "aws s3 cp x s3://hamh-setup-123456789012-us-east-1/client/y\n",
            "aws cloudformation deploy --stack-name hamh-setup-runner --template-file t.yaml\n",
            "repo=cc:hamh-e2e-demo-20260101\n",
            "export HAMH_CLIENT_BIN=widget HAMH_CONFIG_DIR_NAME=widget\n",
            "echo \"$HAMH_PRODUCT_NAME\"\n",
            "shampoo --run\n",
            "project=hyperscale-agent-meta-harness\n",
            "tag=x-hamh-backend-version\n",
        ]:
            with self.subTest(text=text):
                self.assertEqual(self.flagged(text), [], text)

    def test_the_prose_scan_finds_the_name_in_every_text_a_person_reads(self) -> None:
        for text in [
            'say "run ham doctor"\n',
            'echo ham\n',
            "printf '%s\\n' 'ham doctor'\n",
            'warn "ham --version did not print a version"\n',
            'fail "x" "ham signs every call"\n',
            'die "could not install ham into $BINDIR"\n',
            'say "config file: $HOME/.config/ham/config.toml"\n',
            'ask "Choice for ham [3]: "\n',
            'PLAN="build ham here"\n',
            "cat <<'EOF'\nusage: ham models\nEOF\n",
            "cat <<EOF\nrun ham doctor\nEOF\n",
            "x=1 # fine\ncat <<-EOF\n\tham models\n\tEOF\n",
            'say "a\nb ham c"\n',
            'ham doctor\n',
        ]:
            with self.subTest(text=text):
                self.assertTrue(prose_uses(text), text)

    def test_the_prose_scan_leaves_the_variables_the_resource_names_and_comments(self) -> None:
        for text in [
            'say "run $HAMH_CLIENT_BIN doctor"\n',
            'say "${HAMH_CLIENT_BIN} signs every call"\n',
            'say "config file: $HOME/.config/$HAMH_CONFIG_DIR_NAME/config.toml"\n',
            'say "the stack hamh-direct-control and the bucket hamh-setup-123456789012-us-east-1"\n',
            'say "parameters under /hamh/direct/ and /hamh/$STAGE/deployment-digest"\n',
            'say "Write it as cc:hamh-e2e-<name>"\n',
            "# run ham models\nx=1\n",
            'x=1 # then ham doctor\n',
            'say "$HAMH_PRODUCT_NAME"\n',
            "sed -e \"s/@CLIENT@/$HAMH_CLIENT_BIN/g\" <<'EOF'\nrun @CLIENT@ models\nEOF\n",
            'say "SHAM and hamlet and graham"\n',
        ]:
            with self.subTest(text=text):
                self.assertEqual(prose_uses(text), [], text)

    def test_the_prose_scan_reports_the_line_the_name_is_written_on(self) -> None:
        text = 'x=1\nsay "first\nsecond ham third"\ncat <<EOF\nabove\nrun ham here\nEOF\ny=2\n'
        self.assertEqual([u.line for u in prose_uses(text)], [3, 6])

    def test_the_line_of_a_use_is_the_line_it_is_written_on(self) -> None:
        text = 'echo "a\nb"\n# note\nx=1\ncat <<EOF\nline\nEOF\nrun \\\n  ham models\n'
        self.assertEqual([u.line for u in literal_uses(text)], [9])

    def test_a_use_inside_a_quoted_multiline_string_is_still_text_for_a_person(self) -> None:
        self.assertEqual(self.flagged('say "line one\nrun ham models\nline three"\n'), [])

    def test_a_here_string_is_not_a_heredoc_and_the_rest_of_the_script_is_still_read(self) -> None:
        for text in [
            'while read -r l; do echo "$l"; done <<< "$list"\nham doctor\n',
            'read -r a b <<< "$line"\nham doctor\n',
            'grep -q x <<<"$y"\nham doctor\n',
            'cat <<EOF >out\nbody\nEOF\nwhile read -r l; do :; done <<< "$x"\nham doctor\n',
        ]:
            with self.subTest(text=text):
                uses = literal_uses(text)
                self.assertEqual([u.word for u in uses], ["ham"], text)

    def test_a_function_body_and_a_pipeline_are_read_command_by_command(self) -> None:
        text = 'f() {\n  local x=1\n  "$HAMH_CLIENT_BIN" models\n  ham doctor\n}\n'
        self.assertEqual([u.line for u in literal_uses(text)], [4])


# ----------------------------------------------------------------------------------------------------------------------------
# The repository scan.

# The lines of setup.sh (stripped) that name the client by the literal word on purpose: the defaults it falls back to when scripts/setup/names.sh is
# missing next to it, which is how a client-only setup works from setup.sh alone. They must equal the defaults of names.sh
# (`test_the_fallback_equals_the_defaults_of_names_sh`), every other use in setup.sh goes through the variables, and a line that is gone makes
# `test_every_fallback_line_of_setup_sh_still_exists` fail so the list cannot go stale.
SETUP_SH_FALLBACK = [
    ': "${HAMH_CLIENT_BIN:=ham}"',
    ': "${HAMH_ADMIN_BIN:=${HAMH_CLIENT_BIN}-admin}"',
    ': "${HAMH_PRODUCT_NAME:=Hyperscale Agent Meta Harness}"',
    ': "${HAMH_CONFIG_DIR_NAME:=ham}"',
]


def feature_scripts() -> List[Path]:
    """The scripts of this feature except names.sh: every shell script of scripts/setup/ (the strict set)."""
    return sorted(p for p in SETUP_DIR.glob("*.sh") if p.name != "names.sh")


def lines_of(path: Path, uses: Sequence[Use]) -> List[str]:
    src = path.read_text(encoding="utf-8").splitlines()
    return [src[u.line - 1].strip() if 0 < u.line <= len(src) else u.text for u in uses]


class RepositoryScanCase(unittest.TestCase):
    def test_names_sh_exists_and_holds_the_only_literal_default(self) -> None:
        self.assertTrue(NAMES.is_file(), "scripts/setup/names.sh is missing")
        text = NAMES.read_text(encoding="utf-8")
        self.assertIn("HAMH_CLIENT_BIN", text)
        self.assertTrue(literal_uses(text), "names.sh holds the literal default of the client name (the detector must see it)")

    def test_no_script_of_scripts_setup_runs_the_client_by_the_literal_word(self) -> None:
        bad = []
        for path in feature_scripts():
            uses = literal_uses(path.read_text(encoding="utf-8"))
            bad += [f"{path.relative_to(REPO)}:{u.line}: {t}" for u, t in zip(uses, lines_of(path, uses))]
        self.assertEqual(bad, [], 'use "$HAMH_CLIENT_BIN" (and "$HAMH_CONFIG_DIR_NAME") from scripts/setup/names.sh instead of the literal word')

    def test_setup_sh_has_no_literal_use_beyond_the_fallback(self) -> None:
        uses = literal_uses(SETUP_SH.read_text(encoding="utf-8"))
        extra = [f"setup.sh:{u.line}: {t}" for u, t in zip(uses, lines_of(SETUP_SH, uses)) if t not in SETUP_SH_FALLBACK]
        self.assertEqual(extra, [], 'use "$HAMH_CLIENT_BIN" from scripts/setup/names.sh; SETUP_SH_FALLBACK holds only the defaults for a setup.sh without names.sh')

    def test_no_message_help_text_or_prompt_names_the_client_by_the_literal_word(self) -> None:
        """The words a person reads come from the same definition as the commands: `$HAMH_CLIENT_BIN` for the client and `$HAMH_CONFIG_DIR_NAME` for its settings folder.

        What this scan holds to the variables is every non-comment word of `setup.sh` and of the scripts of `scripts/setup/` (except `names.sh`): messages, prompts,
        the arguments of `fail`, `warn` and `die`, and the bodies of here-documents (the help text writes `@CLIENT@`, which `show_help` replaces). The only literal
        uses left are the three default lines of `SETUP_SH_FALLBACK`. What stays literal on purpose and is not found here: the names of resources
        (`hamh-direct-control`, `hamh-setup-<account>-<Region>`, the repository prefix `cc:hamh-e2e-`) and the parameter namespace `/hamh/<stage>/`, which the infrastructure
        code names and the rename does not touch."""
        bad = []
        for path in feature_scripts() + [SETUP_SH]:
            uses = prose_uses(path.read_text(encoding="utf-8"))
            for u, t in zip(uses, lines_of(path, uses)):
                if path == SETUP_SH and t in SETUP_SH_FALLBACK:
                    continue
                bad.append(f"{path.relative_to(REPO)}:{u.line}: {t}")
        self.assertEqual(bad, [], 'say "$HAMH_CLIENT_BIN" (or "$HAMH_CONFIG_DIR_NAME" for the folder) in messages; the help text writes @CLIENT@')

    def test_the_prose_scan_reaches_the_end_of_every_script_of_the_feature(self) -> None:
        """Mutation proof on the real files: a message that names the client, appended to a script, is found on its new last line."""
        for path in feature_scripts() + [SETUP_SH]:
            with self.subTest(path=path.name):
                text = path.read_text(encoding="utf-8")
                n = len(text.splitlines())
                found = [u for u in prose_uses(text + '\nsay "run ham doctor"\n') if u.word == "ham" and u.text == "run ham doctor"]
                self.assertEqual([u.line for u in found], [n + 2])

    def test_every_fallback_line_of_setup_sh_still_exists(self) -> None:
        present = {line.strip() for line in SETUP_SH.read_text(encoding="utf-8").splitlines()}
        stale = [t for t in SETUP_SH_FALLBACK if t not in present]
        self.assertEqual(stale, [], "these lines are gone: remove them from SETUP_SH_FALLBACK")

    def test_the_fallback_equals_the_defaults_of_names_sh(self) -> None:
        p = bash('printf "%s|%s|%s|%s" "$HAMH_CLIENT_BIN" "$HAMH_PRODUCT_NAME" "$HAMH_CONFIG_DIR_NAME" "$HAMH_ADMIN_BIN"', env={})
        self.assertEqual(p.returncode, 0, p.stderr)
        defaults = dict(zip(("HAMH_CLIENT_BIN", "HAMH_PRODUCT_NAME", "HAMH_CONFIG_DIR_NAME", "HAMH_ADMIN_BIN"), p.stdout.split("|")))
        fallback = {}
        for line in SETUP_SH_FALLBACK:
            m = re.fullmatch(r': "\$\{(HAMH_[A-Z_]+):=(.*)\}"', line)
            self.assertIsNotNone(m, line)
            fallback[m.group(1)] = m.group(2).replace("${HAMH_CLIENT_BIN}", fallback.get("HAMH_CLIENT_BIN", ""))
        self.assertEqual(fallback, defaults, "setup.sh without names.sh must use the defaults names.sh has")

    def test_the_reader_reaches_the_end_of_every_script_of_the_feature(self) -> None:
        """Mutation proof on the real files: a literal command appended to a script is found on its new last line, so no construct in the
        script made the reader swallow the rest of it (a here-string once did)."""
        for path in feature_scripts() + [SETUP_SH, NAMES]:
            with self.subTest(path=path.name):
                text = path.read_text(encoding="utf-8")
                n = len(text.splitlines())
                uses = [u for u in literal_uses(text + "\nham doctor\n") if u.text.strip() == "ham doctor"]
                self.assertEqual([u.line for u in uses], [n + 2])

    def test_the_scan_of_a_script_with_a_literal_use_fails(self) -> None:
        """Mutation proof: the same scan, run on a script that runs the client by name, finds it."""
        with tempfile.TemporaryDirectory(prefix="names-mut-", dir=os.environ.get("TMPDIR")) as td:
            f = Path(td) / "mutant.sh"
            f.write_text('#!/usr/bin/env bash\n. "$(dirname "$0")/names.sh"\n"$HAMH_CLIENT_BIN" models\nham doctor\n', encoding="utf-8")
            uses = literal_uses(f.read_text(encoding="utf-8"))
            self.assertEqual([u.line for u in uses], [4])


class OneDefinitionCase(unittest.TestCase):
    ASSIGN = re.compile(r"^\s*(?:export\s+|readonly\s+|declare\s+(?:-x\s+)?)?(HAMH_CLIENT_BIN|HAMH_ADMIN_BIN|HAMH_PRODUCT_NAME|HAMH_CONFIG_DIR_NAME)=", re.M)

    def test_only_names_sh_assigns_the_names(self) -> None:
        offenders = []
        for path in feature_scripts() + [SETUP_SH]:
            m = self.ASSIGN.search(path.read_text(encoding="utf-8"))
            if m:
                offenders.append(f"{path.relative_to(REPO)} assigns {m.group(1)}")
        self.assertEqual(offenders, [], "names.sh is the one definition; a script reads the variables")

    @staticmethod
    def sourced_by_setup_sh() -> List[str]:
        """The helper files setup.sh sources (the list of its last `for helper in ...; do` loop), as file names."""
        loops = re.findall(r"for helper in ([a-z ]+); do", SETUP_SH.read_text(encoding="utf-8"))
        return ["%s.sh" % h for h in loops[-1].split()] if loops else []

    def test_setup_sh_sources_names_sh_before_it_sources_the_helpers(self) -> None:
        text = SETUP_SH.read_text(encoding="utf-8")
        names_at = text.find('. "$SELF_DIR/scripts/setup/names.sh"')
        helpers_at = text.find('. "$SELF_DIR/scripts/setup/$helper.sh"')
        self.assertTrue(0 < names_at < helpers_at, "setup.sh must source names.sh before the helper files that read the names")
        helpers = self.sourced_by_setup_sh()
        self.assertTrue({"lib.sh", "client.sh", "runner.sh", "full.sh"} <= set(helpers), helpers)
        for name in helpers:
            self.assertTrue((SETUP_DIR / name).is_file(), name)

    def test_a_script_that_uses_the_client_name_variable_sources_names_sh(self) -> None:
        missing = []
        exempt = set(self.sourced_by_setup_sh())  # sourced by setup.sh, which has sourced names.sh already (the test above)
        for path in feature_scripts() + [SETUP_SH]:
            if path.name in exempt and path.parent == SETUP_DIR:
                continue
            text = path.read_text(encoding="utf-8")
            code = re.sub(r"(?m)^\s*#.*$", "", text)
            if re.search(r"HAMH_CLIENT_BIN|HAMH_ADMIN_BIN|HAMH_CONFIG_DIR_NAME|HAMH_PRODUCT_NAME|ham_client_|ham_host_platform", code):
                if not re.search(r"(?m)^\s*(?:\.|source)\s+\S*names\.sh", code):
                    missing.append(str(path.relative_to(REPO)))
        self.assertEqual(missing, [], "a script that reads the names must source scripts/setup/names.sh first")


if __name__ == "__main__":
    unittest.main()
