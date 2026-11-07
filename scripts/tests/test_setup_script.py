"""Tests for setup.sh, the one-step setup of the `ham` terminal app.

setup.sh checks the prerequisites, installs a `ham` binary the user already built (a directory from
scripts/build-macos-driver.sh with its SHA256SUMS, or one file with --sha256) or builds one with an explicit
--build, writes the config file only from flags the user gave, runs `ham rules init`, `ham models` and
`ham doctor`, and prints the next steps. No release artifact of `ham` is published, so the client-only path has no
download path (the full setup downloads the client from the user's own bucket; test_setup_full_*.py). Every case runs the real script in a fresh fake HOME with a PATH that holds only stub
commands (git, id, cargo, ham) and symlinks to the system tools the script needs; nothing reaches the
network, the real HOME or a real `ham`.
"""

from __future__ import annotations

import hashlib
import os
import re
import shutil
import stat
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import tempfile
import unittest
from pathlib import Path

from setup_waits import limit

REPO = Path(__file__).resolve().parents[2]
# The limit of one run of setup.sh against the stand-in AWS CLI, in seconds on a free host (HAMH_TEST_WAIT_SCALE stretches it). The slowest run, a teardown, takes about
# SLOWEST_RUN_CPU seconds of CPU there (measured over the 33 setup test files); the limit allows it four times that, so a busy host does not expire a run that is fine.
SLOWEST_RUN_CPU = 42.0
RUN_BASE = 180.0
SCRIPT = REPO / "setup.sh"
BASH = shutil.which("bash") or "/bin/bash"

def _bash3() -> str:
    """/bin/bash when it is a 3.x (macOS), else empty; the parse check against the oldest supported bash."""
    try:
        v = subprocess.run(["/bin/bash", "-c", "echo $BASH_VERSINFO"], capture_output=True, text=True, timeout=limit(60)).stdout.strip()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
    except OSError:
        return ""
    return "/bin/bash" if v == "3" else ""


BASH3 = _bash3()

# System tools setup.sh may call. Everything else is absent from the test PATH.
SYSTEM_TOOLS = [
    "awk", "basename", "cat", "chmod", "cmp", "cp", "cut", "dirname", "grep", "head", "ln", "ls",
    "mkdir", "mv", "printenv", "readlink", "sed", "sha256sum", "shasum", "sort", "tail", "touch", "tr", "uname", "wc",
]

STUB_GIT = "#!/bin/sh\necho 'git version 2.50.0'\n"
STUB_ID = "#!/bin/sh\necho 1000\n"

# A fake AWS CLI: answers `--version` (to stdout like AWS CLI 2, or to stderr like 1.x when FAKE_AWS_VERSION_ON_STDERR is set) and refuses
# anything else, because setup.sh may ask the AWS CLI for its version and for nothing more (it reads no credential).
STUB_AWS = r'''#!/bin/sh
printf '%s\n' "$*" >> "${STUB_LOG:?}/aws.log"
if [ "$1" = "--version" ]; then
  line="${FAKE_AWS_VERSION-aws-cli/2.37.2 Python/3.13.9 Linux/6.1.0 exe/x86_64}"
  if [ -n "${FAKE_AWS_VERSION_ON_STDERR:-}" ]; then echo "$line" >&2; else echo "$line"; fi
  exit 0
fi
echo "stub aws: setup.sh must not run: $*" >&2
exit 64
'''

# A fake `ham`: keeps config keys in the real file layout (camelCase TOML keys), creates the rules file only
# when absent, logs every call, and prints what FAKE_DOCTOR / FAKE_MODELS_RC tell it to.
STUB_HAM = r'''#!/bin/sh
# x-hamh-backend-version: the header the client reads the backend version from; setup.sh finds the client on the PATH by this name
printf '%s\n' "$*" >> "${STUB_LOG:?}/ham.log"
CFG="${XDG_CONFIG_HOME:-$HOME/.config}/ham/config.toml"
camel() { case $1 in api_endpoint) echo apiEndpoint;; aws_profile) echo awsProfile;; default_repo) echo defaultRepo;; *) echo "$1";; esac; }
# like the real ham: an unknown key in the file is an error that names the file and the line, for every command that reads the config
check_cfg() {
  [ -f "$CFG" ] || return 0
  bad=$(awk -F= '/^[ \t]*#/ { next } /^[ \t]*\[/ { exit } NF > 1 { k = $1; gsub(/[ \t]/, "", k); if (k !~ /^(apiEndpoint|region|awsProfile|defaultRepo|downloadDir|defaultModel|inputWaitMinutes|reportTests|mouse|configVersion)$/) { print NR ":" k; exit } }' "$CFG")
  if [ -n "$bad" ]; then echo "ham: $CFG: line ${bad%%:*}, column 1: unknown key '${bad#*:}'" >&2; exit 1; fi
}
case "$1 $2" in
  "config get")
    check_cfg
    k=$(camel "$3")
    v=$(awk -v k="$k" '/^[ \t]*\[/ { exit } { l = $0; sub(/^[ \t]+/, "", l); if (l ~ /^#/) next; i = index(l, "="); if (!i) next; n = substr(l, 1, i - 1); gsub(/[ \t]+$/, "", n); if (n != k) next; v = substr(l, i + 1); sub(/^[ \t]+/, "", v); q = substr(v, 1, 1); if (q == "\"" || q == "\047") { r = substr(v, 2); e = index(r, q); print substr(r, 1, e - 1); exit } }' "$CFG" 2>/dev/null)
    if [ -n "$v" ]; then echo "$v"; exit 0; fi
    echo "ham: $3 is not set" >&2; exit 1;;
  "config set")
    check_cfg
    k=$(camel "$3")
    mkdir -p "$(dirname "$CFG")"; touch "$CFG"
    grep -v "^$k = " "$CFG" > "$CFG.new"
    printf '%s = "%s"\n' "$k" "$4" >> "$CFG.new"
    mv "$CFG.new" "$CFG"; exit 0;;
  "rules init")
    d="$(dirname "$CFG")/rules"
    if [ -e "$d/00-my-rules.md" ]; then echo "kept $d/00-my-rules.md (already there; not changed)"; exit 0; fi
    mkdir -p "$d"; echo "default rules" > "$d/00-my-rules.md"
    echo "created $d/00-my-rules.md"; exit 0;;
  "--version "*|"-V "*)
    echo "ham v9.99"
    exit "${FAKE_VERSION_RC:-0}";;
  "models "*|"models ")
    check_cfg
    echo "sonnet  global.anthropic.claude-sonnet-5-5  Claude Sonnet 5.5  default"
    exit "${FAKE_MODELS_RC:-0}";;
  "doctor "*|"doctor ")
    if [ -n "${FAKE_DOCTOR:-}" ]; then printf '%s\n' "$FAKE_DOCTOR"; else
      for c in identity api catalog git-host allowlist; do echo "PASS $c ok"; done; fi
    if [ -n "${FAKE_DOCTOR_RC:-}" ]; then exit "$FAKE_DOCTOR_RC"; fi
    case "${FAKE_DOCTOR:-}" in *FAIL*) exit 1;; esac
    exit 0;;
esac
echo "stub ham: unexpected call: $*" >&2
exit 64
'''

STUB_CARGO = r'''#!/bin/sh
printf '%s|%s\n' "$PWD" "$*" >> "${STUB_LOG:?}/cargo.log"
mkdir -p "$PWD/target/release"
cp "${STUB_HAM_SOURCE:?}" "$PWD/target/release/ham"
chmod 755 "$PWD/target/release/ham"
# the operator tool is built in the same cargo run when the arguments name its package (the full setup on this machine)
case " $* " in
  *" -p ham-admin "*)
    printf '#!/bin/sh\necho "ham-admin v9.99 built here"\n' > "$PWD/target/release/ham-admin"
    chmod 755 "$PWD/target/release/ham-admin" ;;
esac
'''

AWS_CONFIG_CRED_PROCESS = ("[default]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n\n"
                           "[profile work]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n")
AWS_CONFIG_WORK_ONLY = "[profile work]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n"


def sha256_of(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree_state(root: Path) -> dict:
    """Every path under root with its size, mtime and content hash, to compare before and after a run."""
    state = {}
    if not root.exists():
        return state
    for p in sorted(root.rglob("*")):
        rel = str(p.relative_to(root))
        if p.is_symlink():
            state[rel] = ("link", os.readlink(p))
        elif p.is_dir():
            state[rel] = ("dir",)
        else:
            st = p.stat()
            state[rel] = ("file", st.st_size, st.st_mtime_ns, sha256_of(p))
    return state


class SetupBase(unittest.TestCase):
    def setUp(self) -> None:
        td = tempfile.TemporaryDirectory(prefix="setup-sh-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(td.cleanup)
        self.tmp = Path(td.name).resolve()
        self.home = self.tmp / "home"
        (self.home / ".aws").mkdir(parents=True)
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_CRED_PROCESS)
        self.logs = self.tmp / "logs"
        self.logs.mkdir()
        self.stub = self.tmp / "stubbin"
        self.stub.mkdir()
        self.sysbin = self.tmp / "sysbin"
        self.link_system_tools()
        self.write_stub("git", STUB_GIT)
        self.write_stub("id", STUB_ID)
        self.prefix = self.home / ".local"
        self.ham_source = self.tmp / "ham-source-stub"
        self.ham_source.write_text(STUB_HAM)
        self.ham_source.chmod(0o755)

    # ---- helpers -------------------------------------------------------------------------------
    def link_system_tools(self, without: tuple = ()) -> None:
        self.sysbin.mkdir(exist_ok=True)
        for name in SYSTEM_TOOLS:
            link = self.sysbin / name
            if name in without:
                if link.is_symlink():
                    link.unlink()
                continue
            real = shutil.which(name)
            if real and not link.exists():
                link.symlink_to(real)

    def write_stub(self, name: str, body: str, directory: Path | None = None) -> Path:
        p = (directory or self.stub) / name
        p.write_text(body)
        p.chmod(0o755)
        return p

    def ham_on_path(self) -> None:
        self.write_stub("ham", STUB_HAM)

    def aws_on_path(self) -> None:
        self.write_stub("aws", STUB_AWS)

    def make_artifact(self, name: str = "artifact", good: bool = True) -> Path:
        """A directory shaped like the output of scripts/build-macos-driver.sh: ham and SHA256SUMS."""
        d = self.tmp / name
        d.mkdir()
        (d / "ham").write_text(STUB_HAM)
        (d / "ham").chmod(0o755)
        digest = sha256_of(d / "ham") if good else "0" * 64
        (d / "SHA256SUMS").write_text(f"{digest}  ham\n{'1' * 64}  ham-e2e\n")
        return d

    def env(self, **extra: str) -> dict:
        e = {
            "HOME": str(self.home),
            "PATH": f"{self.stub}:{self.sysbin}",
            "TMPDIR": str(self.tmp),
            "STUB_LOG": str(self.logs),
            "LC_ALL": "C",
        }
        e.update(extra)
        return e

    def run_setup(self, *args: str, script: Path = SCRIPT, stdin: int = subprocess.DEVNULL, **env: str) -> subprocess.CompletedProcess:
        return subprocess.run([BASH, str(script), *args], capture_output=True, text=True, env=self.env(**env),  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
                              stdin=stdin, timeout=limit(RUN_BASE), cwd=str(self.tmp))

    def log_lines(self, name: str = "ham.log") -> list:
        p = self.logs / name
        return p.read_text().splitlines() if p.is_file() else []

    def out(self, p: subprocess.CompletedProcess) -> str:
        return p.stdout + p.stderr

    def config_file(self) -> Path:
        return self.home / ".config" / "ham" / "config.toml"


class UsageCase(SetupBase):
    def test_help_lists_every_flag_and_exits_zero(self) -> None:
        p = self.run_setup("--help")
        self.assertEqual(p.returncode, 0, self.out(p))
        for flag in ("--dry-run", "--prefix", "--endpoint", "--profile", "--yes", "--help", "--from", "--sha256", "--build"):
            self.assertIn(flag, p.stdout)
        self.assertNotIn("agents never run", p.stdout.lower())

    def test_help_works_when_nothing_else_does(self) -> None:
        # no git, no credentials, no ham: --help still answers
        (self.home / ".aws" / "config").write_text("")
        p = self.run_setup("--help")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_unknown_option_is_a_usage_error(self) -> None:
        p = self.run_setup("--frobnicate")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--frobnicate", p.stderr)

    def test_option_without_value_is_a_usage_error(self) -> None:
        p = self.run_setup("--prefix")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--prefix", p.stderr)

    def test_relative_prefix_is_refused(self) -> None:
        p = self.run_setup("--prefix", "relative/dir", "--yes")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("absolute", p.stderr)

    def test_endpoint_must_be_https_and_not_a_placeholder(self) -> None:
        for bad in ("http://example.com/direct", "ftp://x", "https://<api-id>.execute-api.us-east-1.amazonaws.com/direct", "https://a b", "not-a-url"):
            with self.subTest(bad=bad):
                p = self.run_setup("--endpoint", bad, "--yes")
                self.assertEqual(p.returncode, 2, self.out(p))
                self.assertIn("--endpoint", p.stderr)

    def test_from_and_build_together_are_refused(self) -> None:
        p = self.run_setup("--from", str(self.tmp), "--build", "--yes")
        self.assertEqual(p.returncode, 2, self.out(p))

    def test_sha256_without_from_is_refused(self) -> None:
        p = self.run_setup("--sha256", "a" * 64, "--yes")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--from", p.stderr)

    def test_malformed_sha256_is_refused(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art / "ham"), "--sha256", "xyz", "--yes")
        self.assertEqual(p.returncode, 2, self.out(p))

    def test_a_prompt_cannot_be_answered_without_a_terminal(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art))
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--yes", p.stderr)
        self.assertFalse((self.prefix / "bin" / "ham").exists())


class PrerequisiteCase(SetupBase):
    def test_refuses_to_run_as_root(self) -> None:
        self.write_stub("id", "#!/bin/sh\necho 0\n")
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("root", p.stderr)
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_missing_git_names_git_and_changes_nothing(self) -> None:
        (self.stub / "git").unlink()
        art = self.make_artifact()
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("git", p.stderr)
        self.assertEqual(before, tree_state(self.home))

    def test_git_that_does_not_run_is_a_failure(self) -> None:
        # on a Mac without the command line tools, /usr/bin/git is a stub that exits non-zero
        self.write_stub("git", "#!/bin/sh\necho 'xcode-select: note: no developer tools were found' >&2\nexit 1\n")
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("xcode-select --install", p.stderr)

    def test_no_aws_credentials_source_explains_the_aws_cli_ways_to_make_a_profile(self) -> None:
        (self.home / ".aws" / "config").unlink()
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        for needle in ("aws configure sso", "aws configure", "credential_process", "https://docs.aws.amazon.com/cli/"):
            self.assertIn(needle, p.stderr)
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_named_profile_that_does_not_exist_is_a_failure(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--profile", "nope", "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("nope", p.stderr)
        self.assertIn("not found", p.stderr)

    def test_sso_profile_is_accepted_when_the_aws_cli_is_recent_enough(self) -> None:
        (self.home / ".aws" / "config").write_text("[profile sso1]\nsso_session = s\nsso_account_id = 1\n")
        self.aws_on_path()
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--profile", "sso1", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("sso1", p.stdout)

    def test_assume_role_profile_is_accepted_when_the_aws_cli_is_recent_enough(self) -> None:
        (self.home / ".aws" / "config").write_text("[profile r1]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = work\n")
        self.aws_on_path()
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--profile", "r1", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_credential_process_profile_passes(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--profile", "work", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_static_key_profile_in_the_credentials_file_passes(self) -> None:
        (self.home / ".aws" / "config").unlink()
        (self.home / ".aws" / "credentials").write_text("[team]\naws_access_key_id = AKIAEXAMPLEEXAMPLE0\naws_secret_access_key = x\n")
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--profile", "team", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_environment_keys_count_as_a_credentials_source(self) -> None:
        (self.home / ".aws" / "config").unlink()
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes", AWS_ACCESS_KEY_ID="AKIAEXAMPLEEXAMPLE0", AWS_SECRET_ACCESS_KEY="s")  # nosec B106: keyword argument carries a made-up value into a test child environment, not a credential
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_default_profile_counts(self) -> None:
        (self.home / ".aws" / "config").write_text("[default]\ncredential_process = helper print\n")
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_all_missing_prerequisites_are_listed_together(self) -> None:
        (self.stub / "git").unlink()
        (self.home / ".aws" / "config").unlink()
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("git", p.stderr)
        self.assertIn("aws configure", p.stderr)
        self.assertIn("Nothing was changed", p.stderr)

    def test_no_checksum_tool_is_a_failure_when_a_binary_is_to_be_checked(self) -> None:
        self.link_system_tools(without=("sha256sum", "shasum"))
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("sha256sum", p.stderr)

    def test_build_without_cargo_fails_naming_cargo(self) -> None:
        p = self.run_setup("--build", "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("cargo", p.stderr)

    def test_unzip_is_optional(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")  # no unzip on the test PATH
        self.assertEqual(p.returncode, 0, self.out(p))


class SourceCase(SetupBase):
    def test_no_ham_anywhere_and_no_source_flag_lists_the_ways_to_get_one(self) -> None:
        p = self.run_setup("--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("--from", p.stderr)
        self.assertIn("--build", p.stderr)
        self.assertIn("build-macos-driver.sh", p.stderr)
        self.assertNotRegex(p.stderr, r"https?://")  # no download URL is invented

    def test_install_from_a_build_directory_verifies_and_copies(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        dest = self.prefix / "bin" / "ham"
        self.assertTrue(dest.is_file())
        self.assertTrue(os.access(dest, os.X_OK))
        self.assertEqual(sha256_of(dest), sha256_of(art / "ham"))
        self.assertEqual(stat.S_IMODE(dest.stat().st_mode), 0o755)
        # no temp file is left beside it
        self.assertEqual(sorted(x.name for x in (self.prefix / "bin").iterdir()), ["cah", "ham"])  # COMPAT-CAH: the client and the link with the earlier command name

    def test_checksum_mismatch_is_refused_and_nothing_is_installed(self) -> None:
        art = self.make_artifact(good=False)
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("checksum mismatch", p.stderr)
        self.assertEqual(before, tree_state(self.home))
        self.assertEqual(self.log_lines(), [])

    def test_checksum_mismatch_is_also_refused_in_a_dry_run(self) -> None:
        art = self.make_artifact(good=False)
        p = self.run_setup("--from", str(art), "--dry-run")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("checksum mismatch", p.stderr)

    def test_a_binary_with_no_checksum_to_compare_is_refused(self) -> None:
        art = self.make_artifact()
        (art / "SHA256SUMS").unlink()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("no checksum", p.stderr)
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_sums_file_without_a_ham_line_is_refused(self) -> None:
        art = self.make_artifact()
        (art / "SHA256SUMS").write_text(f"{'1' * 64}  ham-e2e\n")
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("no checksum", p.stderr)

    def test_single_file_with_the_right_sha256_installs(self) -> None:
        art = self.make_artifact()
        (art / "SHA256SUMS").unlink()
        p = self.run_setup("--from", str(art / "ham"), "--sha256", sha256_of(art / "ham").upper(), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((self.prefix / "bin" / "ham").is_file())

    def test_single_file_with_the_wrong_sha256_is_refused(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art / "ham"), "--sha256", "f" * 64, "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("checksum mismatch", p.stderr)
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_the_sha256_flag_wins_over_the_sums_file(self) -> None:
        art = self.make_artifact(good=False)  # SHA256SUMS holds zeros
        p = self.run_setup("--from", str(art / "ham"), "--sha256", sha256_of(art / "ham"), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_from_a_path_that_is_not_a_file_is_refused(self) -> None:
        p = self.run_setup("--from", str(self.tmp / "missing"), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("missing", p.stderr)

    def test_prefix_is_respected(self) -> None:
        art = self.make_artifact()
        pre = self.tmp / "custom-prefix"
        p = self.run_setup("--from", str(art), "--prefix", str(pre), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((pre / "bin" / "ham").is_file())
        self.assertFalse((self.home / ".local").exists())

    def test_the_bin_directory_outside_path_is_reported_without_editing_shell_files(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertIn("PATH", p.stdout)
        self.assertIn(str(self.prefix / "bin"), p.stdout)
        for rc in (".bashrc", ".zshrc", ".profile", ".bash_profile", ".zprofile"):
            self.assertFalse((self.home / rc).exists(), rc)

    def test_a_ham_already_on_path_is_used_and_nothing_is_installed(self) -> None:
        self.ham_on_path()
        p = self.run_setup("--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse((self.prefix / "bin").exists())
        self.assertIn("rules init", " ".join(self.log_lines()))

    def test_a_different_ham_from_flag_replaces_the_installed_one_atomically(self) -> None:
        art = self.make_artifact()
        self.assertEqual(self.run_setup("--from", str(art), "--yes").returncode, 0)
        art2 = self.tmp / "artifact2"
        art2.mkdir()
        (art2 / "ham").write_text(STUB_HAM + "\n# second build\n")
        (art2 / "ham").chmod(0o755)
        (art2 / "SHA256SUMS").write_text(f"{sha256_of(art2 / 'ham')}  ham\n")
        p = self.run_setup("--from", str(art2), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(sha256_of(self.prefix / "bin" / "ham"), sha256_of(art2 / "ham"))
        self.assertEqual(sorted(x.name for x in (self.prefix / "bin").iterdir()), ["cah", "ham"])  # COMPAT-CAH: the client and the link with the earlier command name


class BuildCase(SetupBase):
    def make_checkout(self) -> Path:
        repo = self.tmp / "checkout"
        (repo / "crates" / "ham").mkdir(parents=True)
        (repo / "Cargo.toml").write_text("[workspace]\nmembers = [\"crates/ham\"]\n")
        (repo / "crates" / "ham" / "Cargo.toml").write_text("[package]\nname = \"ham\"\n")
        shutil.copy(SCRIPT, repo / "setup.sh")
        return repo

    def test_build_runs_the_one_cargo_command_in_the_checkout_and_installs_the_result(self) -> None:
        repo = self.make_checkout()
        self.write_stub("cargo", STUB_CARGO)
        p = self.run_setup("--build", "--yes", script=repo / "setup.sh", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.log_lines("cargo.log"), [f"{repo}|build --release --locked -p ham --bin ham"])
        self.assertEqual(sha256_of(self.prefix / "bin" / "ham"), sha256_of(self.ham_source))
        self.assertNotIn("agents never run", p.stdout.lower())

    def test_build_outside_a_checkout_is_refused(self) -> None:
        self.write_stub("cargo", STUB_CARGO)
        lonely = self.tmp / "lonely"
        lonely.mkdir()
        shutil.copy(SCRIPT, lonely / "setup.sh")
        p = self.run_setup("--build", "--yes", script=lonely / "setup.sh", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("checkout", p.stderr)
        self.assertEqual(self.log_lines("cargo.log"), [])

    def test_a_failing_build_stops_before_anything_is_installed(self) -> None:
        repo = self.make_checkout()
        self.write_stub("cargo", "#!/bin/sh\necho 'error: could not compile' >&2\nexit 101\n")
        p = self.run_setup("--build", "--yes", script=repo / "setup.sh")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("build failed", p.stderr)
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_dry_run_with_build_does_not_run_cargo(self) -> None:
        repo = self.make_checkout()
        self.write_stub("cargo", STUB_CARGO)
        p = self.run_setup("--build", "--dry-run", script=repo / "setup.sh", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.log_lines("cargo.log"), [])
        self.assertIn("would", p.stdout)


class DryRunAndIdempotenceCase(SetupBase):
    def test_dry_run_writes_nothing_and_runs_no_ham(self) -> None:
        art = self.make_artifact()
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art), "--endpoint", "https://abc.execute-api.us-east-1.amazonaws.com/direct",
                           "--profile", "work", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(before, tree_state(self.home))
        self.assertFalse(self.prefix.exists())
        self.assertEqual(self.log_lines(), [])
        self.assertIn("would", p.stdout)
        self.assertIn("DRY RUN", p.stdout)

    def test_dry_run_needs_no_yes_and_no_terminal(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_second_run_changes_nothing_that_exists(self) -> None:
        art = self.make_artifact()
        args = ("--from", str(art), "--endpoint", "https://abc.execute-api.us-east-1.amazonaws.com/direct",
                "--profile", "work", "--yes")
        first = self.run_setup(*args)
        self.assertEqual(first.returncode, 0, self.out(first))
        after_first = tree_state(self.home)
        self.assertTrue(self.config_file().is_file())
        second = self.run_setup(*args)
        self.assertEqual(second.returncode, 0, self.out(second))
        self.assertEqual(after_first, tree_state(self.home))
        self.assertIn("already", second.stdout)

    def test_a_second_run_without_flags_keeps_the_binary_and_the_config(self) -> None:
        art = self.make_artifact()
        self.assertEqual(self.run_setup("--from", str(art), "--endpoint", "https://abc.execute-api.us-east-1.amazonaws.com/direct", "--profile", "work", "--yes").returncode, 0)
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)  # no default profile: awsProfile must carry it
        after_first = tree_state(self.home)
        p = self.run_setup("--yes", PATH=f"{self.stub}:{self.sysbin}:{self.prefix / 'bin'}")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(after_first, tree_state(self.home))


class ConfigCase(SetupBase):
    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"

    def test_no_endpoint_flag_means_no_config_file_and_no_config_call(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse(self.config_file().exists())
        self.assertFalse(any(ln.startswith("config set") for ln in self.log_lines()))

    def test_endpoint_flag_is_written_through_ham_config_set(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f'apiEndpoint = "{self.ENDPOINT}"', self.config_file().read_text())
        self.assertIn(f"config set api_endpoint {self.ENDPOINT}", self.log_lines())
        self.assertFalse(any("aws_profile" in ln for ln in self.log_lines()))

    def test_profile_flag_is_written_as_aws_profile(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--profile", "work", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn('awsProfile = "work"', self.config_file().read_text())

    def test_region_flag_is_written(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--region", "us-east-1", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn('region = "us-east-1"', self.config_file().read_text())

    def test_a_changed_endpoint_replaces_the_old_one_and_says_so(self) -> None:
        art = self.make_artifact()
        self.assertEqual(self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes").returncode, 0)
        other = "https://zzz999.execute-api.us-east-1.amazonaws.com/direct"
        p = self.run_setup("--from", str(art), "--endpoint", other, "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f'apiEndpoint = "{other}"', self.config_file().read_text())
        self.assertNotIn(self.ENDPOINT, self.config_file().read_text())
        self.assertIn("changed", p.stdout)

    def test_an_existing_config_is_left_alone_when_no_flag_names_a_key(self) -> None:
        self.config_file().parent.mkdir(parents=True)
        self.config_file().write_text('apiEndpoint = "https://old.example/direct"\ndownloadDir = "/x"\n')
        before = self.config_file().read_bytes()
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(before, self.config_file().read_bytes())

    def test_xdg_config_home_is_honoured(self) -> None:
        art = self.make_artifact()
        xdg = self.tmp / "xdg"
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes", XDG_CONFIG_HOME=str(xdg))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((xdg / "ham" / "config.toml").is_file())
        self.assertFalse(self.config_file().exists())

    def test_the_profile_in_the_ham_config_satisfies_the_credentials_check_on_a_rerun(self) -> None:
        art = self.make_artifact()
        self.assertEqual(self.run_setup("--from", str(art), "--profile", "work", "--yes").returncode, 0)
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)  # no default profile; the check follows awsProfile
        p = self.run_setup("--yes", PATH=f"{self.stub}:{self.sysbin}:{self.prefix / 'bin'}")
        self.assertEqual(p.returncode, 0, self.out(p))


class StepsCase(SetupBase):
    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"

    def test_rules_init_is_called_once_per_run_and_models_before_it(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        lines = self.log_lines()
        self.assertEqual(lines.count("rules init"), 1, lines)
        self.assertLess(lines.index("models"), lines.index("rules init"))
        self.assertTrue((self.home / ".config" / "ham" / "rules" / "00-my-rules.md").is_file())

    def test_a_binary_that_cannot_list_models_fails_the_setup(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes", FAKE_MODELS_RC="1")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("ham models", p.stderr)
        self.assertNotIn("rules init", self.log_lines())

    def test_doctor_is_skipped_with_an_explanation_when_no_endpoint_is_set(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("doctor", self.log_lines())
        self.assertIn("V1ApiUrl", p.stdout)
        self.assertIn("hamh-direct-control", p.stdout)

    def test_doctor_runs_once_when_the_endpoint_is_set(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.log_lines().count("doctor"), 1)
        self.assertIn("PASS identity", p.stdout)

    def test_a_catalog_warn_is_advice_and_not_a_setup_problem(self) -> None:
        # `ham doctor` prints WARN (and exits 0) for smoke records older than 24 h; the models are still selectable.
        art = self.make_artifact()
        doctor = ("PASS identity ok\nPASS api ok\n"
                  "WARN catalog 5 models; catalog smoke records are 35 hours old (refresh them)\nPASS git-host ok\nPASS allowlist ok")
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes", FAKE_DOCTOR=doctor)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("WARN catalog", p.stdout)  # doctor's own line is shown
        self.assertIn("advice", p.stdout)
        self.assertIn("not a setup problem", p.stdout)
        self.assertNotIn("check failed", self.out(p))

    def test_a_failing_catalog_check_is_a_setup_problem_and_says_why(self) -> None:
        # `ham doctor` fails the catalog check only when no model is selectable or no row has a smokedAt time (or the list cannot be read).
        art = self.make_artifact()
        doctor = "PASS identity ok\nPASS api ok\nFAIL catalog no selectable model\nPASS git-host ok\nPASS allowlist ok"
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes", FAKE_DOCTOR=doctor)
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertIn("selectable", p.stdout)
        self.assertIn("ham models --server", p.stdout)
        self.assertNotIn("older than 24 h", self.out(p))  # the old message blamed the age of the records
        self.assertNotIn("not a setup problem", self.out(p))
        self.assertTrue((self.prefix / "bin" / "ham").is_file())  # the install itself stays done

    def test_a_failing_identity_check_exits_3_and_says_to_sign_in_again(self) -> None:
        art = self.make_artifact()
        doctor = "FAIL identity the credentials were refused\nPASS api ok\nPASS catalog ok"
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes", FAKE_DOCTOR=doctor)
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertIn("sign in again", self.out(p))
        self.assertTrue((self.prefix / "bin" / "ham").is_file())  # the install itself stays done

    def test_a_failing_api_check_exits_3(self) -> None:
        art = self.make_artifact()
        doctor = "PASS identity ok\nFAIL api cannot reach the endpoint\nPASS catalog ok"
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--yes", FAKE_DOCTOR=doctor)
        self.assertEqual(p.returncode, 3, self.out(p))
        self.assertIn("apiEndpoint", self.out(p))

    def test_next_steps_use_real_commands_only(self) -> None:
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        out = p.stdout
        self.assertIn("Next steps", out)
        for cmd in ("ham rules edit", "ham models", "--direct", "ham watch", "ham pull", "ham clone"):
            self.assertIn(cmd, out)
        for nonexistent in ("ham stop", "ham login", "ham logs", "ham status", "ham run ", "ham start", "--discard"):
            self.assertNotIn(nonexistent, out)

    def test_no_secret_is_written_or_printed(self) -> None:
        art = self.make_artifact()
        secrets = {"AWS_SECRET_ACCESS_KEY": "SECRETKEYVALUE0123456789abcdef",
                   "AWS_SESSION_TOKEN": "SESSIONTOKENVALUE0123456789abcdef",
                   "AWS_ACCESS_KEY_ID": "AKIA" + "MARKERMARKER0123"}
        p = self.run_setup("--from", str(art), "--endpoint", self.ENDPOINT, "--profile", "work", "--yes", **secrets)
        self.assertEqual(p.returncode, 0, self.out(p))
        blob = self.out(p) + "".join(self.log_lines())
        for root in (self.home, self.logs):
            for f in root.rglob("*"):
                if f.is_file() and not f.is_symlink():
                    blob += f.read_text(errors="replace")
        for name, value in secrets.items():
            self.assertNotIn(value, blob, name)
        self.assertNotRegex(self.config_file().read_text(), r"(?i)secret|token|password|aws_access_key_id")


AWS_CONFIG_SEVERAL = (
    "[profile work]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n\n"
    "[profile my-sso]\nregion = eu-west-1\nsso_session = corp\nsso_account_id = 1\n\n"
    "[profile jump]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = work\n")


class ProfileFailureTextCase(SetupBase):
    """S-08: a failure about the AWS profile lists the profiles the script can see, each with its kind and the option to run."""

    def seed_several(self) -> None:
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_SEVERAL)
        (self.home / ".aws" / "credentials").write_text("[team]\naws_access_key_id = AKIAEXAMPLEEXAMPLE0\naws_secret_access_key = x\n")

    def row(self, text: str, name: str) -> str:
        """The one line of text that names the profile `name` as a listed profile (its kind and the option are on that line)."""
        lines = [ln for ln in text.splitlines() if re.search(rf"(^|\s){re.escape(name)}(\s|$)", ln) and "--profile" in ln]
        self.assertEqual(len(lines), 1, f"{name}: {lines!r} in\n{text}")
        return lines[0]

    def test_no_default_profile_failure_lists_the_profiles_found_with_their_kind(self) -> None:
        self.seed_several()
        art = self.make_artifact()
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("no AWS credentials source was found", p.stderr)
        work, sso, jump, team = (self.row(p.stderr, n) for n in ("work", "my-sso", "jump", "team"))
        self.assertIn("credential_process", work)
        self.assertIn("--profile work", work)
        self.assertIn("SSO", sso)
        self.assertIn("--profile my-sso", sso)
        self.assertIn("usable", sso)
        self.assertNotIn("bridge", sso)
        self.assertIn("assumes a role", jump)
        self.assertIn("usable", jump)
        self.assertIn("--profile jump", jump)
        self.assertIn("static keys", team)
        self.assertIn("--profile team", team)
        self.assertEqual(before, tree_state(self.home))
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_missing_named_profile_failure_lists_the_profiles_found(self) -> None:
        self.seed_several()
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--profile", "nope", "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("nope", p.stderr)
        self.assertIn("not found", p.stderr)
        self.assertIn("credential_process", self.row(p.stderr, "work"))
        self.assertIn("--profile team", self.row(p.stderr, "team"))

    def test_a_default_profile_without_credentials_still_lists_the_other_profiles(self) -> None:
        (self.home / ".aws" / "config").write_text("[default]\nregion = us-east-1\n\n[profile work]\ncredential_process = x\n")
        art = self.make_artifact()
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("has no credentials", p.stderr)  # the existing check_profile text for default stays
        self.assertIn("--profile work", p.stderr)

    def test_no_profile_files_failure_names_the_aws_cli_commands_and_runs_none_of_them(self) -> None:
        (self.home / ".aws" / "config").unlink()
        for name in ("aws",):
            self.write_stub(name, f'#!/bin/sh\nprintf \'%s %s\\n\' {name} "$*" >> "${{STUB_LOG:?}}/calls.log"\nexit 0\n')
        art = self.make_artifact()
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        for needle in ("aws configure sso", "aws configure", "credential_process", "AWS_*", "https://docs.aws.amazon.com/cli/"):
            self.assertIn(needle, p.stderr)
        self.assertFalse((self.logs / "calls.log").exists(), "setup.sh must run none of the commands it names")
        self.assertEqual(before, tree_state(self.home))


class SsoAndAssumeRoleCase(SetupBase):
    """SETUP-AWSCLI-PROFILES: a profile that signs in through IAM Identity Center (SSO) or assumes a role is accepted when the AWS CLI is
    there and is version 2.9.0 or later (the first version with `aws configure export-credentials`, which ham runs itself). setup.sh asks the
    AWS CLI for its version and for nothing else, prints what to run before a session, and prints no helper profile."""

    CONFIG = ("[profile my-sso]\nregion = eu-west-1\nsso_session = corp\nsso_account_id = 1\n\n"
              "[sso-session corp]\nsso_start_url = https://example.invalid/start\nsso_region = eu-west-1\n\n"
              "[profile jump]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = my-sso\n\n"
              "[profile on-host]\nrole_arn = arn:aws:iam::1:role/x\ncredential_source = Ec2InstanceMetadata\n\n"
              "[profile work]\nregion = us-east-1\ncredential_process = /usr/bin/env helper print\n")
    INSTALL_URL = "https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html"

    def setUp(self) -> None:
        super().setUp()
        (self.home / ".aws" / "config").write_text(self.CONFIG)

    def run_with(self, profile: str, *extra: str, **env: str) -> subprocess.CompletedProcess:
        self.runs = getattr(self, "runs", 0) + 1
        return self.run_setup("--from", str(self.make_artifact(f"art{self.runs}")), "--profile", profile, "--yes", *extra, **env)

    def aws_calls(self) -> list:
        return self.log_lines("aws.log")

    def test_an_sso_profile_is_accepted_and_the_sign_in_command_is_printed(self) -> None:
        self.aws_on_path()
        p = self.run_with("my-sso")
        self.assertEqual(p.returncode, 0, self.out(p))
        ok = [ln for ln in p.stdout.splitlines() if ln.startswith("ok") and "AWS profile my-sso" in ln]
        self.assertEqual(len(ok), 1, p.stdout)
        self.assertIn("SSO", ok[0])
        self.assertIn("aws sso login --profile my-sso", p.stdout)

    def test_an_assume_role_profile_is_accepted_without_a_sign_in_line_of_its_own(self) -> None:
        self.aws_on_path()
        for profile in ("jump", "on-host"):
            with self.subTest(profile=profile):
                p = self.run_with(profile)
                self.assertEqual(p.returncode, 0, self.out(p))
                ok = [ln for ln in p.stdout.splitlines() if ln.startswith("ok") and f"AWS profile {profile}" in ln]
                self.assertEqual(len(ok), 1, p.stdout)
                self.assertIn("role", ok[0])
                self.assertNotIn(f"aws sso login --profile {profile}", p.stdout)

    def test_the_profile_is_written_to_the_ham_config_like_any_other(self) -> None:
        self.aws_on_path()
        p = self.run_with("my-sso", "--endpoint", "https://abc123.execute-api.eu-west-1.amazonaws.com/direct")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn('awsProfile = "my-sso"', self.config_file().read_text())

    def test_setup_asks_the_aws_cli_for_its_version_and_nothing_else(self) -> None:
        self.aws_on_path()
        for profile in ("my-sso", "jump"):
            p = self.run_with(profile)
            self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.aws_calls()
        self.assertTrue(calls)
        self.assertEqual(set(calls), {"--version"}, calls)  # never export-credentials, never sso login: it reads no credential
        self.assertNotIn("for-ham", self.out(p))

    def test_a_dry_run_checks_the_same_way_and_writes_nothing(self) -> None:
        self.aws_on_path()
        before = tree_state(self.home)
        p = self.run_setup("--from", str(self.make_artifact()), "--profile", "my-sso", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("aws sso login --profile my-sso", p.stdout)
        self.assertEqual(set(self.aws_calls()), {"--version"})
        self.assertEqual(before, tree_state(self.home))

    def test_the_next_steps_remind_an_sso_user_to_sign_in_first(self) -> None:
        self.aws_on_path()
        p = self.run_with("my-sso")
        steps = p.stdout.split("== Next steps", 1)[1]
        self.assertIn("aws sso login --profile my-sso", steps)
        q = self.run_with("work")
        self.assertNotIn("aws sso login", q.stdout.split("== Next steps", 1)[1])

    def test_a_profile_chosen_by_aws_profile_or_the_ham_config_is_checked_the_same_way(self) -> None:
        self.aws_on_path()
        p = self.run_setup("--from", str(self.make_artifact()), "--yes", AWS_PROFILE="my-sso")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("aws sso login --profile my-sso", p.stdout)
        self.config_file().parent.mkdir(parents=True, exist_ok=True)
        self.config_file().write_text('awsProfile = "jump"\n')
        q = self.run_setup("--from", str(self.make_artifact("art-cfg")), "--yes")
        self.assertEqual(q.returncode, 0, self.out(q))

    # ---- the AWS CLI that is not there, or too old --------------------------------------------------
    def test_without_an_aws_cli_the_failure_says_how_to_install_one(self) -> None:
        before = tree_state(self.home)
        for profile in ("my-sso", "jump"):
            with self.subTest(profile=profile):
                p = self.run_with(profile)
                self.assertEqual(p.returncode, 1, self.out(p))
                for needle in ("AWS CLI", "2.9.0", self.INSTALL_URL, "HAMH_AWS_CLI", "Nothing was changed"):
                    self.assertIn(needle, p.stderr)
                self.assertEqual(before, tree_state(self.home))
        self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_an_old_aws_cli_is_named_with_its_version(self) -> None:
        self.aws_on_path()
        for version, on_stderr in (("aws-cli/1.18.147 Python/3.6.9 Linux/5.4.0 botocore/1.17.70", "1"),
                                   ("aws-cli/2.8.12 Python/3.11.6 Darwin/23.1.0 exe/x86_64 prompt/off", ""),
                                   ("aws-cli/2.0.0 Python/3.7.4 Linux/4.14 botocore/2.0.0dev4", ""),
                                   ("aws-cli/1.27.0 Python/3.9", "1")):
            with self.subTest(version=version):
                p = self.run_with("my-sso", FAKE_AWS_VERSION=version, FAKE_AWS_VERSION_ON_STDERR=on_stderr)
                self.assertEqual(p.returncode, 1, self.out(p))
                self.assertIn(version.split()[0], p.stderr)
                self.assertIn("older than 2.9.0", p.stderr)
                self.assertIn(self.INSTALL_URL, p.stderr)
                self.assertFalse((self.prefix / "bin" / "ham").exists())

    def test_the_first_version_with_the_command_and_every_later_one_pass(self) -> None:
        self.aws_on_path()
        for version in ("aws-cli/2.9.0 Python/3.11.0", "aws-cli/2.9.19 Python/3.11.0", "aws-cli/2.10.0 Python/3.11.0",
                        "aws-cli/2.37.2 Python/3.13.9 Linux/6.1.0 exe/x86_64.amzn.2023", "aws-cli/3.0.0 Python/3.14", "aws-cli/2.100.1"):
            with self.subTest(version=version):
                p = self.run_with("my-sso", FAKE_AWS_VERSION=version)
                self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_version_line_setup_cannot_read_is_a_warning_not_a_refusal(self) -> None:
        self.aws_on_path()
        for line in ("something else entirely", "", "aws-cli/"):
            with self.subTest(line=line):
                p = self.run_with("my-sso", FAKE_AWS_VERSION=line)
                self.assertEqual(p.returncode, 0, self.out(p))
                warned = [ln for ln in p.stdout.splitlines() if ln.startswith("warn") and "AWS CLI" in ln]
                self.assertEqual(len(warned), 1, p.stdout)
                self.assertIn("2.9.0", warned[0])

    def test_ham_aws_cli_names_the_program_setup_checks(self) -> None:
        elsewhere = self.tmp / "elsewhere"
        elsewhere.mkdir()
        self.write_stub("my-aws", STUB_AWS, elsewhere)
        p = self.run_with("my-sso", HAMH_AWS_CLI=str(elsewhere / "my-aws"))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(set(self.aws_calls()), {"--version"})
        # one that does not exist is named, and the aws on PATH is not used instead
        self.aws_on_path()
        gone = str(elsewhere / "no-such-aws")
        q = self.run_with("my-sso", HAMH_AWS_CLI=gone)
        self.assertEqual(q.returncode, 1, self.out(q))
        self.assertIn(gone, q.stderr)
        self.assertIn("HAMH_AWS_CLI", q.stderr)

    def test_static_key_and_credential_process_profiles_never_start_the_aws_cli(self) -> None:
        self.aws_on_path()
        (self.home / ".aws" / "credentials").write_text("[team]\naws_access_key_id = " + KEY_ID + "\naws_secret_access_key = made-up\n")
        for profile in ("work", "team"):
            p = self.run_with(profile)
            self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.aws_calls(), [])

    def test_the_helper_profile_text_is_gone(self) -> None:
        self.aws_on_path()
        out = ""
        for profile in ("my-sso", "jump"):
            p = self.run_with(profile)
            out += self.out(p)
        q = self.run_with("my-sso", "--dry-run")
        out += self.out(q)
        for gone in ("for-ham", "export-credentials", "bridge", "has not been tried", "cannot use"):
            self.assertNotIn(gone, out)
        self.assertNotIn("for-ham", (self.home / ".aws" / "config").read_text())


class ProfileKindAgreementCase(unittest.TestCase):
    """ham (crates/ham/src/awsprofile.rs) and setup.sh (`profile_kind`) hold one definition of the profile kinds. Both are run over the same
    fixture files, `fixtures/aws-profiles/`: `crates/ham/tests/aws_profile_kinds.rs` runs the Rust function over them and this case runs
    the shell function, each against the expected kind in `kinds.tsv`."""

    FIXTURE = REPO / "fixtures" / "aws-profiles"

    @staticmethod
    def function(text: str, name: str) -> str:
        m = re.search(rf"(?ms)^{name}\(\) \{{\n.*?^\}}\n", text)
        if not m:
            raise AssertionError(f"setup.sh has no function {name}")
        return m.group(0)

    def kind(self, tmp: Path, name: str, config: Path, credentials: Path) -> str:
        text = SCRIPT.read_text()
        script = tmp / "kind.sh"
        script.write_text(self.function(text, "pick_aws_files") + self.function(text, "profile_kind") +
                          'AWS_CFG_FILE=$1\nAWS_CRED_FILE=$2\nprofile_kind "$3"\n')
        p = subprocess.run([BASH, str(script), str(config), str(credentials), name], capture_output=True, text=True, timeout=limit(30),  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
                           env={"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C", "HOME": str(tmp)})
        self.assertEqual(p.returncode, 0, p.stderr)
        return p.stdout.strip()

    def test_setup_sh_gives_every_profile_of_the_shared_fixture_the_kind_the_tsv_names(self) -> None:
        rows = [ln.split("\t") for ln in (self.FIXTURE / "kinds.tsv").read_text().splitlines() if ln.strip()]
        self.assertGreaterEqual(len(rows), 20)
        with tempfile.TemporaryDirectory(prefix="setup-sh-kinds-", dir=os.environ.get("TMPDIR")) as td:
            tmp = Path(td)
            for name, want in rows:
                with self.subTest(profile=name):
                    self.assertEqual(self.kind(tmp, name, self.FIXTURE / "aws-config", self.FIXTURE / "aws-credentials"), want)

    def test_no_aws_files_means_missing(self) -> None:
        with tempfile.TemporaryDirectory(prefix="setup-sh-kinds-", dir=os.environ.get("TMPDIR")) as td:
            tmp = Path(td)
            self.assertEqual(self.kind(tmp, "any", tmp / "no-config", tmp / "no-credentials"), "missing")

    def test_the_fixture_covers_every_kind(self) -> None:
        kinds = {ln.split("\t")[1] for ln in (self.FIXTURE / "kinds.tsv").read_text().splitlines() if ln.strip()}
        self.assertEqual(kinds, {"credential_process", "static", "sso", "assume", "unknown", "missing"})


class RegionCase(SetupBase):
    """S-10: ham signs for one Region and cannot guess it. When setup.sh will run ham doctor, a Region must come from --region, the ham config,
    AWS_REGION or the AWS profile, and setup.sh checks that before it changes anything."""

    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"
    NO_REGION = "[profile work]\ncredential_process = /usr/bin/env helper print\n"

    def setUp(self) -> None:
        super().setUp()
        (self.home / ".aws" / "config").write_text(self.NO_REGION)

    def install(self, *extra: str, **env: str) -> subprocess.CompletedProcess:
        return self.run_setup("--from", str(self.make_artifact()), "--profile", "work", "--endpoint", self.ENDPOINT, "--yes", *extra, **env)

    def test_missing_region_stops_before_install(self) -> None:
        before = tree_state(self.home)
        p = self.install()
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("no AWS Region was found", p.stderr)
        self.assertIn("--region", p.stderr)
        self.assertIn("Nothing was changed", p.stderr)
        self.assertFalse((self.prefix / "bin" / "ham").exists())
        self.assertEqual(self.log_lines(), [])
        self.assertEqual(before, tree_state(self.home))

    def test_the_failure_names_the_region_in_the_endpoint_host(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--profile", "work", "--yes",
                           "--endpoint", "https://abc123.execute-api.us-west-2.amazonaws.com/direct")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("--region us-west-2", p.stderr)

    def test_a_region_from_the_option_satisfies_the_check(self) -> None:
        p = self.install("--region", "us-east-1")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_region_from_aws_region_satisfies_the_check(self) -> None:
        p = self.install(AWS_REGION="eu-west-1")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_region_in_the_profile_satisfies_the_check(self) -> None:
        (self.home / ".aws" / "config").write_text(self.NO_REGION + "region = eu-west-1\n")
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_region_in_the_ham_config_satisfies_the_check(self) -> None:
        self.config_file().parent.mkdir(parents=True)
        self.config_file().write_text('region = "eu-west-1"\n')
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_the_region_of_the_default_profile_counts_when_no_profile_is_named(self) -> None:
        (self.home / ".aws" / "config").write_text("[default]\nregion = eu-west-1\ncredential_process = x\n")
        p = self.run_setup("--from", str(self.make_artifact()), "--endpoint", self.ENDPOINT, "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_the_region_of_another_profile_does_not_count(self) -> None:
        (self.home / ".aws" / "config").write_text(self.NO_REGION + "\n[profile other]\nregion = eu-west-1\ncredential_process = x\n")
        p = self.install()
        self.assertEqual(p.returncode, 1, self.out(p))

    def test_without_an_endpoint_no_region_is_needed(self) -> None:
        # ham doctor does not run, so nothing needs a Region yet
        p = self.run_setup("--from", str(self.make_artifact()), "--profile", "work", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_a_dry_run_reports_the_missing_region_too(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--profile", "work", "--endpoint", self.ENDPOINT, "--dry-run")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("no AWS Region was found", p.stderr)

    def test_doctor_exit_without_a_fail_line_relays_hams_own_text(self) -> None:
        said = "ham: AWS credentials: no Region: set region in config.toml, AWS_REGION, or the profile's region"
        p = self.install("--region", "us-east-1", FAKE_DOCTOR=said, FAKE_DOCTOR_RC="1")
        self.assertEqual(p.returncode, 3, self.out(p))
        warned = [ln for ln in p.stdout.splitlines() if ln.startswith("warn") and "without a FAIL line" in ln]
        self.assertEqual(len(warned), 1, p.stdout)
        self.assertIn("no Region", warned[0])


class AccountRuleHintCase(SetupBase):
    """S-11: the identity hint names the account rule, and an endpoint that does not end in /direct draws a warning."""

    HOST = "https://abc123.execute-api.us-east-1.amazonaws.com"

    def install(self, endpoint: str, **env: str) -> subprocess.CompletedProcess:
        self.runs = getattr(self, "runs", 0) + 1
        return self.run_setup("--from", str(self.make_artifact(f"art{self.runs}")), "--endpoint", endpoint, "--yes", **env)

    def test_identity_failure_names_the_account_rule(self) -> None:
        doctor = "FAIL identity HTTP 403: Forbidden\nPASS api ok\nPASS catalog ok"
        p = self.install(self.HOST + "/direct", FAKE_DOCTOR=doctor)
        self.assertEqual(p.returncode, 3, self.out(p))
        text = self.out(p)
        self.assertIn("accepts callers of its own AWS account only", text)
        self.assertIn("a role in that account", text)
        self.assertIn("sign in again", text)  # the expiry advice stays, in ordinary words

    def test_endpoint_without_slash_direct_warns(self) -> None:
        for bad in (self.HOST + "/", self.HOST + "/v1", self.HOST + "/direct/", self.HOST):
            with self.subTest(endpoint=bad):
                p = self.install(bad)
                self.assertEqual(p.returncode, 0, self.out(p))
                warned = [ln for ln in p.stdout.splitlines() if ln.startswith("warn") and "/direct" in ln]
                self.assertEqual(len(warned), 1, p.stdout)
                self.assertIn("does not end with /direct", warned[0])
                self.assertIn("403", warned[0])

    def test_endpoint_with_slash_direct_does_not_warn(self) -> None:
        p = self.install(self.HOST + "/direct")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("does not end with /direct", p.stdout)

    def test_the_endpoint_warning_also_shows_in_a_dry_run(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--endpoint", self.HOST + "/v1", "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("does not end with /direct", p.stdout)


class PlanAndVersionTextCase(SetupBase):
    """S-13: text that went stale or was missing: the rules claim, the installed version, the WARN versions line and the cost of a build."""

    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"

    def test_plan_does_not_claim_nothing_is_overwritten(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("nothing is overwritten", p.stdout)
        self.assertIn("replaces only an unedited earlier default or pack file", p.stdout)

    def test_the_script_never_says_that_rules_init_never_overwrites(self) -> None:
        text = SCRIPT.read_text()
        for stale in ("never overwrites", "nothing is overwritten", "without overwriting"):
            self.assertNotIn(stale, text)

    def test_setup_prints_the_installed_version(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("ham v9.99", p.stdout)
        self.assertEqual(self.log_lines().count("--version"), 1, self.log_lines())
        self.assertLess(self.log_lines().index("models"), self.log_lines().index("--version"))

    def test_a_dry_run_announces_the_version_check_and_runs_nothing(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--dry-run")
        self.assertIn("would run: ham --version", p.stdout)
        self.assertEqual(self.log_lines(), [])

    def test_a_ham_that_cannot_print_its_version_is_a_warning_only(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--yes", FAKE_VERSION_RC="2")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("warn  ham --version", p.stdout)

    def test_doctor_versions_warning_is_explained_in_one_sentence(self) -> None:
        doctor = ("PASS identity arn ok\nPASS api ok\nWARN versions ham v4.05 | backend v4.02; backend older than this ham\n"
                  "PASS catalog ok\nPASS git-host ok\nPASS allowlist ok")
        p = self.run_setup("--from", str(self.make_artifact()), "--endpoint", self.ENDPOINT, "--yes", FAKE_DOCTOR=doctor)
        self.assertEqual(p.returncode, 0, self.out(p))
        explained = [ln for ln in p.stdout.splitlines() if ln.startswith("warn") and "versions:" in ln]
        self.assertEqual(len(explained), 1, p.stdout)
        self.assertIn("advice", explained[0])
        self.assertIn("different versions", explained[0])
        self.assertEqual(explained[0].count(". "), 0, "one sentence")

    def test_no_versions_warning_no_explanation(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--endpoint", self.ENDPOINT, "--yes")
        self.assertNotIn("versions:", p.stdout)

    def test_build_plan_states_time_and_size(self) -> None:
        repo = BuildCase.make_checkout(self)
        self.write_stub("cargo", STUB_CARGO)
        p = self.run_setup("--build", "--dry-run", script=repo / "setup.sh", STUB_HAM_SOURCE=str(self.ham_source))
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = p.stdout.split("== Plan", 1)[1].split("\n== ", 1)[0]
        self.assertIn("about 11 minutes", plan)
        self.assertIn("3 GB", plan)
        self.assertIn("rustup", plan)

    def test_a_plan_without_a_build_does_not_mention_the_build_cost(self) -> None:
        p = self.run_setup("--from", str(self.make_artifact()), "--dry-run")
        self.assertNotIn("11 minutes", p.stdout)


KEY_ID = "AKIA" + "EXAMPLEEXAMPLE0"  # built from pieces: a made-up key id for a child environment, never a real credential


class ConfigReadCase(SetupBase):
    """S-14: setup.sh reads the ham config the way ham does (ham config get when a ham exists, a tolerant file reader before), and blames a
    config file ham cannot read on the file, not on the binary."""

    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"
    COMMENTED = ('awsProfile = "work" # my work profile\n'
                 "  apiEndpoint = '" + ENDPOINT + "'   # single quotes, indented\n"
                 'region = "us-east-1"\n')

    def setUp(self) -> None:
        super().setUp()
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)  # no [default] profile: only the ham config names one
        self.config_file().parent.mkdir(parents=True)

    def test_commented_config_key_is_read(self) -> None:
        self.config_file().write_text(self.COMMENTED)
        self.ham_on_path()
        p = self.run_setup("--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("config get aws_profile", self.log_lines())
        self.assertEqual(self.log_lines().count("doctor"), 1, "the commented apiEndpoint line must be found, so doctor runs")

    def test_commented_config_key_is_read_before_a_ham_is_installed(self) -> None:
        self.config_file().write_text(self.COMMENTED)
        p = self.run_setup("--from", str(self.make_artifact()), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.log_lines().count("doctor"), 1)

    def test_a_dry_run_reads_the_file_and_runs_no_ham(self) -> None:
        self.config_file().write_text(self.COMMENTED)
        self.ham_on_path()
        p = self.run_setup("--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.log_lines(), [])

    def test_bad_config_key_is_blamed_on_the_file(self) -> None:
        self.config_file().write_text('bogusKey = "x"\n')
        p = self.run_setup("--from", str(self.make_artifact()), "--profile", "work", "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn(str(self.config_file()), p.stderr)
        self.assertIn("unknown key 'bogusKey'", p.stderr)
        self.assertIn("config file", p.stderr)
        self.assertIn("binary itself runs", p.stderr)
        self.assertNotIn("does not run here", p.stderr)

    def test_a_config_file_ham_cannot_read_stops_the_run_before_anything_changes(self) -> None:
        self.config_file().write_text('bogusKey = "x"\n')
        self.ham_on_path()
        before = tree_state(self.home)
        p = self.run_setup("--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("cannot read its config file", p.stderr)
        self.assertIn(str(self.config_file()), p.stderr)
        self.assertIn("Nothing was changed", p.stderr)
        self.assertEqual(before, tree_state(self.home))
        self.assertNotIn("models", self.log_lines())
        self.assertNotIn("rules init", self.log_lines())

    def test_a_key_that_is_not_set_is_not_a_bad_file(self) -> None:
        self.config_file().write_text('downloadDir = "/x"\n')
        self.ham_on_path()
        p = self.run_setup("--yes", "--profile", "work")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("cannot read its config file", p.stderr)


class CredentialOrderCase(SetupBase):
    """S-14: setup.sh checks the credentials in the order ham uses them: the profile in the ham config (which --profile writes), then the
    AWS_* keys, then AWS_PROFILE or default."""

    def keys(self) -> dict:
        return {"AWS_ACCESS_KEY_ID": KEY_ID, "AWS_SECRET_ACCESS_KEY": "made-up"}

    def seed_config(self, profile: str) -> None:
        self.config_file().parent.mkdir(parents=True)
        self.config_file().write_text(f'awsProfile = "{profile}"\n')

    def test_the_ham_config_profile_is_checked_before_the_aws_keys(self) -> None:
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)
        self.seed_config("gone")
        p = self.run_setup("--from", str(self.make_artifact()), "--yes", **self.keys())
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("gone", p.stderr)
        self.assertIn("not found", p.stderr)

    def test_the_ham_config_profile_wins_over_aws_profile(self) -> None:
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)
        self.seed_config("work")
        p = self.run_setup("--from", str(self.make_artifact()), "--yes", AWS_PROFILE="nope")
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_the_aws_keys_win_over_aws_profile(self) -> None:
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)
        p = self.run_setup("--from", str(self.make_artifact()), "--yes", AWS_PROFILE="nope", **self.keys())
        self.assertEqual(p.returncode, 0, self.out(p))

    def test_aws_profile_is_checked_when_there_are_no_keys_and_no_config_profile(self) -> None:
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)
        p = self.run_setup("--from", str(self.make_artifact()), "--yes", AWS_PROFILE="nope")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("nope", p.stderr)

    def test_the_profile_option_wins_over_the_ham_config_profile(self) -> None:
        (self.home / ".aws" / "config").write_text(AWS_CONFIG_WORK_ONLY)
        self.seed_config("gone")
        p = self.run_setup("--from", str(self.make_artifact()), "--profile", "work", "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))


# What setup.sh may not print for a person outside the team: an AWS account number or an e-mail address.
INTERNAL_TERMS = [
    r"\b\d{12}\b", r"[\w.+-]+@[\w-]+\.(?:com|org|net)\b",
]


ACCOUNT_NUMBER = r"\b\d{12}\b"
HASH_LABEL = re.compile(r"(sha-?256|sha1|digest|template|hash|commit|checksum|revision)[^\n]*$", re.I)


def internal_names_in(text: str) -> list:
    found = set()
    for pat in INTERNAL_TERMS:
        for m in re.finditer(pat, text, re.I):
            # The first 12 characters of a SHA-256 can be all digits; a number after a hash label on its line is a hash display, not an account.
            if pat == ACCOUNT_NUMBER and HASH_LABEL.search(text[text.rfind("\n", 0, m.start()) + 1:m.start()]):
                continue
            found.add(m.group(0))
    return sorted(found)


class AccountNumberDetectorCase(unittest.TestCase):
    """The first 12 characters of a SHA-256 are all digits for about 1 hash in 280, so a hash display can look like an account number. The detector
    reads a hash display as a hash and an account number as an account number."""

    # Made at run time: a literal run of 12 digits in a test file is what the public-export scan reads as an account id.
    DIGITS = "".join(str((i * 7 + 4) % 10) for i in range(12))

    def test_a_hash_display_made_of_digits_is_not_an_account_number(self) -> None:
        d = self.DIGITS
        for line in (f"ok    uploaded source/b94c300f.zip (version v39, SHA-256 {d})",
                     f"ok    CDK bootstrap template of 1234 bytes, SHA-256 {d}, at s3://bucket/key",
                     f"deploy digest {d}", f"template {d}", f"ok    built from commit {d}", f"checksum {d} matches"):
            self.assertEqual(internal_names_in(line), [], line)

    def test_an_account_number_is_still_found(self) -> None:
        d = self.DIGITS
        for line in (f"account {d}, Region us-east-1", f"arn:aws:iam::{d}:user/x", f"ok    SHA-256 abcdef123456\naccount {d}"):
            self.assertEqual(internal_names_in(line), [d], line)


class NoInternalNamesCase(SetupBase):
    """setup.sh prints nothing that names a tool, host, account or profile of its authors: it is read by people outside the team, whose
    profiles are ordinary AWS CLI profiles. The cases run the failing and the passing paths with stubs and scan everything they print, and the
    script text is scanned as well (comments and help text included)."""

    ENDPOINT = "https://abc123.execute-api.us-east-1.amazonaws.com/direct"

    def test_the_script_text_holds_no_internal_name(self) -> None:
        self.assertEqual(internal_names_in(SCRIPT.read_text()), [])

    def test_nothing_setup_sh_prints_on_any_path_holds_an_internal_name(self) -> None:
        cfg = self.home / ".aws" / "config"
        sso_config = ("[profile my-sso]\nsso_session = corp\nsso_account_id = 1\n\n[profile jump]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = my-sso\n"
                      "\n[profile bare]\nregion = us-east-1\n\n[profile work]\nregion = us-east-1\ncredential_process = x\n")
        artifact = str(self.make_artifact())
        scenarios = []  # (label, args, environment, setup of the home)

        def home(config: str | None):
            def apply() -> None:
                if config is None:
                    cfg.unlink(missing_ok=True)
                else:
                    cfg.write_text(config)
            return apply

        def aws(version: str | None):
            def apply() -> None:
                for f in (self.stub / "aws",):
                    if f.exists():
                        f.unlink()
                if version is not None:
                    self.aws_on_path()
            return apply

        run = lambda label, args, cfg_text=sso_config, aws_version="aws-cli/2.37.2", **env: scenarios.append((label, args, env, cfg_text, aws_version))  # noqa: E731
        run("help", ["--help"])
        run("no aws files", ["--from", artifact, "--yes"], None)
        run("no aws files with an endpoint", ["--from", artifact, "--endpoint", self.ENDPOINT, "--yes"], None)
        run("no default profile", ["--from", artifact, "--yes"])
        run("missing named profile", ["--from", artifact, "--profile", "nope", "--yes"])
        run("profile without credentials", ["--from", artifact, "--profile", "bare", "--yes"])
        run("sso, no aws cli", ["--from", artifact, "--profile", "my-sso", "--yes"], aws_version=None)
        run("assume role, no aws cli", ["--from", artifact, "--profile", "jump", "--yes"], aws_version=None)
        run("sso, old aws cli", ["--from", artifact, "--profile", "my-sso", "--yes"], aws_version="aws-cli/1.18.147")
        run("sso, unreadable version", ["--from", artifact, "--profile", "my-sso", "--yes"], aws_version="")
        run("sso, ok", ["--from", artifact, "--profile", "my-sso", "--yes"])
        run("assume role, ok, dry run", ["--from", artifact, "--profile", "jump", "--dry-run"])
        run("no region", ["--from", artifact, "--profile", "work", "--endpoint", self.ENDPOINT, "--yes"])
        run("endpoint without /direct", ["--from", artifact, "--profile", "work", "--region", "us-east-1", "--yes",
                                         "--endpoint", "https://abc123.execute-api.us-east-1.amazonaws.com"])
        run("success with an endpoint", ["--from", artifact, "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT, "--yes"])
        run("no endpoint", ["--from", artifact, "--profile", "work", "--yes"])
        run("identity fails", ["--from", artifact, "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT, "--yes"],
            FAKE_DOCTOR="FAIL identity HTTP 403\nPASS api ok\nPASS catalog ok")
        run("api fails", ["--from", artifact, "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT, "--yes"],
            FAKE_DOCTOR="PASS identity arn ok\nFAIL api no\nPASS catalog ok")
        run("catalog fails", ["--from", artifact, "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT, "--yes"],
            FAKE_DOCTOR="PASS identity arn ok\nPASS api ok\nFAIL catalog no")
        run("doctor exits without a FAIL line", ["--from", artifact, "--profile", "work", "--region", "us-east-1", "--endpoint", self.ENDPOINT, "--yes"],
            FAKE_DOCTOR="something went wrong", FAKE_DOCTOR_RC="1")
        run("no binary", ["--profile", "work", "--yes"])
        run("no such --from", ["--from", str(self.tmp / "nowhere"), "--profile", "work", "--yes"])
        run("build without a checkout", ["--build", "--profile", "work", "--yes"])
        run("unknown option", ["--nope"])
        run("bad endpoint", ["--endpoint", "nonsense"])
        seen = ""
        for label, args, env, cfg_text, aws_version in scenarios:
            home(cfg_text)()
            aws(aws_version)()
            if aws_version is not None:
                env = dict(env, FAKE_AWS_VERSION=aws_version)
            p = self.run_setup(*args, **env)
            text = self.out(p)
            self.assertNotEqual(text.strip(), "", label)
            seen += text
            self.assertEqual(internal_names_in(text), [], f"{label}:\n{text}")
        self.assertGreater(len(seen), 4000, "the scan should have read a lot of text")

    def test_the_no_git_and_root_refusals_hold_no_internal_name(self) -> None:
        (self.stub / "git").unlink()
        p = self.run_setup("--from", str(self.make_artifact()), "--profile", "work", "--yes")
        self.assertEqual(internal_names_in(self.out(p)), [])
        self.write_stub("id", "#!/bin/sh\necho 0\n")
        q = self.run_setup("--yes")
        self.assertEqual(q.returncode, 1, self.out(q))
        self.assertEqual(internal_names_in(self.out(q)), [])


class StaticCase(unittest.TestCase):
    """What the file itself must be: portable bash 3.2, no deletes, no network fetch, no root, no secret reads."""

    text = SCRIPT.read_text() if SCRIPT.is_file() else ""

    def test_the_script_exists_and_is_executable(self) -> None:
        self.assertTrue(SCRIPT.is_file(), "setup.sh is missing")
        self.assertTrue(os.access(SCRIPT, os.X_OK), "setup.sh is not executable")
        self.assertTrue(self.text.startswith("#!/usr/bin/env bash\n"))

    def test_the_syntax_parses(self) -> None:
        p = subprocess.run([BASH, "-n", str(SCRIPT)], capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
        self.assertEqual(p.returncode, 0, p.stderr)

    def test_no_bash_4_features(self) -> None:
        banned = [r"declare\s+-A", r"\bmapfile\b", r"\breadarray\b", r"\$\{[A-Za-z_]+(,,|\^\^|,|\^)\}", r"&>>", r"\|&",
                  r"\bcoproc\b", r"\bwait\s+-n\b", r"\bdeclare\s+-n\b", r"\blocal\s+-n\b", r"\[\[.*=~", r"\$\{!"]
        for pat in banned:
            self.assertIsNone(re.search(pat, self.text), pat)

    def test_it_never_deletes_downloads_or_asks_for_root(self) -> None:
        code = "\n".join(ln for ln in self.text.splitlines() if not ln.lstrip().startswith("#"))
        for pat in (r"(^|[^\w.-])rm\s", r"\bsudo\b", r"\bcurl\b", r"\bwget\b", r"\|\s*(ba)?sh\b", r"\beval\b", r"\bsu\s"):
            self.assertIsNone(re.search(pat, code, re.M), pat)

    def test_it_never_prints_or_stores_credential_values(self) -> None:
        code = "\n".join(ln for ln in self.text.splitlines() if not ln.lstrip().startswith("#"))
        for pat in (r"(printf|echo|say|note|warn|fail|die).*\$\{?AWS_(SECRET_ACCESS_KEY|SESSION_TOKEN|ACCESS_KEY_ID)",
                    r"\$\{?AWS_SESSION_TOKEN", r">>?\s*\S*\.aws"):
            self.assertIsNone(re.search(pat, code), pat)

    def test_it_embeds_no_release_url_or_checksum(self) -> None:
        self.assertIsNone(re.search(r"https?://(?!127\.0\.0\.1|localhost)[a-z0-9.-]+\.[a-z]{2,}/\S*\.(tar|tgz|zip|gz)", self.text))
        self.assertIsNone(re.search(r"\b[0-9a-f]{64}\b", self.text))

    def test_no_account_ids_or_endpoints_are_baked_in(self) -> None:
        self.assertIsNone(re.search(r"\b\d{12}\b", self.text))
        self.assertIsNone(re.search(r"execute-api\.[a-z0-9-]+\.amazonaws\.com/[a-z]", self.text))

    @unittest.skipUnless(shutil.which("shellcheck"), "shellcheck is not installed")
    def test_shellcheck_is_clean(self) -> None:
        p = subprocess.run(["shellcheck", "-s", "bash", str(SCRIPT)], capture_output=True, text=True, timeout=limit(120))  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(p.returncode, 0, p.stdout)

    @unittest.skipUnless(BASH3, "no bash 3.x on this host")
    def test_the_syntax_parses_with_bash_3(self) -> None:
        p = subprocess.run([BASH3, "-n", str(SCRIPT)], capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
        self.assertEqual(p.returncode, 0, p.stderr)


if __name__ == "__main__":
    unittest.main()
