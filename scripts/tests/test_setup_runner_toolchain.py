"""Tests for scripts/setup/runner-toolchain.sh, the toolchain step of the setup runner.

The script installs the pinned build tools on the runner, a Linux x86_64 CodeBuild container: the tools of tools.lock that the deploy builds with (the rustup
installer, cargo-lambda, cargo-zigbuild and zig, each checked against its SHA-256 by scripts/lib/tools_lock.py), the Rust toolchain and targets of tools.lock through
rustup, the Go toolchain of images/tools/lang/lang.lock (checked against its SHA-256 here), and a running Docker daemon (scripts/node.sh runs Node, pnpm and the CDK in a
container). It writes <tools>/env.sh for the caller to read and ends with `TOOLCHAIN PASS ...` or `TOOLCHAIN FAIL <step> <detail>`.
These cases run it in a made-up tree with a stand-in for tools_lock.py and fake rustup, rustc, docker and dockerd; the Go archive is a real tar file read through a
file URL.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import subprocess  # nosec B404: tests start the scripts under test with argument lists, never a shell
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from typing import Dict, List

import setup_runner_support as sup
from setup_waits import limit

RUST = "1.98.1"
GO = "1.27.1"
ZIG = "0.16.0"
ZIGBUILD = "0.23.4"
LAMBDA = "1.9.2"
AWSCLI = "2.37.2"

FAKE_TOOLS_LOCK = r'''#!__PY__
import json, os, stat, sys
args = sys.argv[1:]
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(json.dumps({"tool": "tools_lock", "argv": args, "roots": os.environ.get("TOOLS_LOCK_ROOTS")}) + "\n")
vals = {"rust.toolchain": "__RUST__"}
if args[0] == "get":
    print(vals.get(args[-1], ""))
elif args[0] == "list":
    for n, v in (("rustup-init", "1.29.1"), ("cargo-lambda", "__LAMBDA__"), ("cargo-zigbuild", "__ZIGBUILD__"), ("zig", "__ZIG__"), ("awscli", "__AWSCLI__"), ("jq", "1.8.2")):
        print(n, v)
elif args[0] == "install":
    dest = args[args.index("--dest") + 1]
    os.makedirs(dest + "/bin", exist_ok=True)
    if os.environ.get("FAKE_INSTALL") == "fail":
        print("TOOL FAIL zig sha256 want=aa got=bb")
        sys.exit(4)
    for name in ("rustup-init", "cargo-lambda", "cargo-zigbuild", "zig"):
        p = dest + "/bin/" + name
        body = "#!/bin/sh\necho %s \"$@\" >> \"$FAKE_LOG.tools\"\n" % name
        if name == "rustup-init":
            body = ("#!/bin/sh\necho rustup-init \"$@\" >> \"$FAKE_LOG.tools\"\nmkdir -p \"$CARGO_HOME/bin\"\n"
                    "cat > \"$CARGO_HOME/bin/rustc\" <<'EOS'\n#!/bin/sh\necho \"rustc %s (abc 2026-09-01)\"\nEOS\nchmod +x \"$CARGO_HOME/bin/rustc\"\n"
                    "printf '#!/bin/sh\\necho rustup \"$@\" >> \"$FAKE_LOG.tools\"\\nexit 0\\n' > \"$CARGO_HOME/bin/rustup\"\nchmod +x \"$CARGO_HOME/bin/rustup\"\n") % os.environ.get("FAKE_RUSTC", "__RUST__")
        open(p, "w").write(body)
        os.chmod(p, 0o755)
    inst = dest + "/pkgs/awscli-__AWSCLI__/aws/install"
    os.makedirs(os.path.dirname(inst), exist_ok=True)
    open(inst, "w").write("#!/bin/sh\necho install \"$@\" >> \"$FAKE_LOG.tools\"\n"
                          "while [ $# -gt 0 ]; do [ \"$1\" = --bin-dir ] && bin=$2; shift; done\n"
                          "mkdir -p \"$bin\"\nprintf '#!/bin/sh\\necho aws-cli/%s Python/3.13 Linux\\n' \"${FAKE_AWSCLI:-__AWSCLI__}\" > \"$bin/aws\"\nchmod +x \"$bin/aws\"\n")
    os.chmod(inst, 0o755)
    print("TOOL OK rustup-init 1.29.1")
'''

FAKE_DOCKER = r'''#!/bin/sh
echo "docker $*" >> "$FAKE_LOG.tools"
if [ "$1" = info ]; then
  [ -f "$FAKE_DOCKER_UP" ] && exit 0
  exit 1
fi
exit 0
'''

FAKE_DOCKERD = r'''#!/bin/sh
echo "dockerd $*" >> "$FAKE_LOG.tools"
[ "${FAKE_DOCKERD_BROKEN:-0}" = 1 ] && exit 1
touch "$FAKE_DOCKER_UP"
sleep 30
'''


def go_archive(version: str = GO, arch: str = "linux-amd64") -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tar:
        script = f"#!/bin/sh\necho go version go{version} linux/amd64\n".encode()
        info = tarfile.TarInfo("go/bin/go")
        info.size = len(script)
        info.mode = 0o755
        tar.addfile(info, io.BytesIO(script))
        info = tarfile.TarInfo("go/VERSION")
        data = f"go{version}\n".encode()
        info.size = len(data)
        tar.addfile(info, io.BytesIO(data))
    return buf.getvalue()


class Rig:
    def __init__(self, test: unittest.TestCase, sha_ok: bool = True, with_row: bool = True) -> None:
        self.test = test
        self.tmp = Path(tempfile.mkdtemp(prefix="runner-toolchain-"))
        test.addCleanup(shutil.rmtree, self.tmp, True)
        self.tree = self.tmp / "src"
        self.tools = self.tmp / "tools"
        self.bin = self.tmp / "bin"
        self.log = self.tmp / "log"
        for d in (self.tree, self.bin):
            d.mkdir(parents=True)
        shutil.copytree(sup.REPO / "scripts" / "setup", self.tree / "scripts" / "setup", ignore=shutil.ignore_patterns("__pycache__"))
        (self.tree / "scripts" / "lib").mkdir(parents=True)
        shutil.copyfile(sup.REPO / "scripts" / "lib" / "python.sh", self.tree / "scripts" / "lib" / "python.sh")
        lock = self.tree / "scripts" / "lib" / "tools_lock.py"
        lock.write_text(FAKE_TOOLS_LOCK.replace("__PY__", sys.executable).replace("__RUST__", RUST).replace("__LAMBDA__", LAMBDA)
                        .replace("__ZIGBUILD__", ZIGBUILD).replace("__ZIG__", ZIG).replace("__AWSCLI__", AWSCLI))
        lock.chmod(0o755)
        (self.tree / "tools.lock").write_text(
            'schema = "1"\n\n[rust]\ntoolchain = "%s"\ncomponents = ["rustfmt", "clippy"]\ntargets = ["aarch64-apple-darwin", "x86_64-unknown-linux-musl"]\n' % RUST)
        self.archive = go_archive()
        self.archive_path = self.tmp / f"go{GO}.linux-amd64.tar.gz"
        self.archive_path.write_bytes(self.archive)
        sha = hashlib.sha256(self.archive).hexdigest() if sha_ok else "0" * 64
        rows = ""
        if with_row:
            for arch in ("x86_64", "aarch64"):
                rows += f"go {GO} {arch} {sha} file://{self.archive_path}\n"
        (self.tree / "images" / "tools" / "lang").mkdir(parents=True)
        (self.tree / "images" / "tools" / "lang" / "lang.lock").write_text("# made-up lock\njdk 21 x86_64 " + "1" * 64 + " https://example.invalid/jdk.tgz\n" + rows)
        for name, body in (("docker", FAKE_DOCKER), ("dockerd", FAKE_DOCKERD)):
            p = self.bin / name
            p.write_text(body)
            p.chmod(0o755)
        self.docker_up = self.tmp / "docker-up"

    def env(self, **extra: str) -> Dict[str, str]:
        env = {
            "PATH": f"{self.bin}:{os.environ.get('PATH', '/usr/bin:/bin')}", "HOME": str(self.tmp / "home"), "TMPDIR": str(self.tmp / "tmp"),
            "HAMH_PYTHON": sys.executable, "FAKE_LOG": str(self.log), "FAKE_DOCKER_UP": str(self.docker_up),
            "RUNNER_TOOLCHAIN_DOCKER_WAIT": "5",
        }
        (self.tmp / "home").mkdir(exist_ok=True)
        (self.tmp / "tmp").mkdir(exist_ok=True)
        env.update(extra)
        return env

    def run(self, **extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(self.tree / "scripts/setup/runner-toolchain.sh"), str(self.tools)], cwd=self.tree, env=self.env(**extra),  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                              capture_output=True, text=True, timeout=limit(120))

    def log_lines(self, suffix: str = "") -> List[str]:
        p = Path(str(self.log) + suffix)
        return p.read_text().splitlines() if p.is_file() else []

    def lock_calls(self) -> List[dict]:
        return [json.loads(ln) for ln in self.log_lines()]


def last_line(proc: subprocess.CompletedProcess) -> str:
    return [ln for ln in proc.stdout.splitlines() if ln.strip()][-1]


class Base(unittest.TestCase):
    def setUp(self) -> None:
        if not sup.TOOLCHAIN_SCRIPT.is_file():
            self.fail("scripts/setup/runner-toolchain.sh does not exist")


class InstallCase(Base):
    def test_installs_the_pinned_tools_rust_go_and_starts_docker(self) -> None:
        rig = Rig(self)
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"TOOLCHAIN PASS rust={RUST} zig={ZIG} cargo-zigbuild={ZIGBUILD} cargo-lambda={LAMBDA} go={GO} awscli={AWSCLI}")
        install = [c for c in rig.lock_calls() if c["argv"][0] == "install"][0]
        self.assertEqual(install["argv"][install["argv"].index("--only") + 1], "rustup-init,cargo-lambda,cargo-zigbuild,zig,awscli")
        self.assertEqual(install["argv"][install["argv"].index("--dest") + 1], str(rig.tools))
        self.assertEqual(install["argv"][install["argv"].index("--lock") + 1], str(rig.tree / "tools.lock"))
        self.assertEqual(install["roots"], str(rig.tools), "tools_lock.py installs only below the tools folder")

    def test_rustup_gets_the_toolchain_components_and_targets_of_the_lock(self) -> None:
        rig = Rig(self)
        rig.run()
        calls = [ln for ln in rig.log_lines(".tools") if ln.startswith("rustup-init")]
        self.assertEqual(len(calls), 1)
        self.assertIn(f"--default-toolchain {RUST}", calls[0])
        self.assertIn("-y --no-modify-path --profile minimal", calls[0])
        self.assertIn("-c rustfmt,clippy", calls[0])
        self.assertIn("-t aarch64-apple-darwin,x86_64-unknown-linux-musl", calls[0])

    def test_the_pinned_aws_cli_is_installed_under_the_tools_folder_and_first_on_the_path(self) -> None:
        rig = Rig(self)
        rig.run()
        installs = [ln for ln in rig.log_lines(".tools") if ln.startswith("install")]
        self.assertEqual(installs, [f"install --install-dir {rig.tools}/aws-cli --bin-dir {rig.tools}/bin --update"])
        text = (rig.tools / "env.sh").read_text()
        self.assertTrue([ln for ln in text.splitlines() if ln.startswith("export PATH=")][0].startswith(f"export PATH={rig.tools}/bin:"))

    def test_go_is_unpacked_from_the_checked_archive(self) -> None:
        rig = Rig(self)
        rig.run()
        self.assertTrue(os.access(rig.tools / "go" / "bin" / "go", os.X_OK))
        self.assertEqual((rig.tools / "go" / "VERSION").read_text().strip(), f"go{GO}")

    def test_env_file(self) -> None:
        rig = Rig(self)
        rig.run()
        text = (rig.tools / "env.sh").read_text()
        for needle in (f"export HAMH_TOOLS={rig.tools}", f"export RUSTUP_HOME={rig.tools}/rustup", f"export CARGO_HOME={rig.tools}/cargo",
                       f"{rig.tools}/bin", f"{rig.tools}/cargo/bin", f"{rig.tools}/go/bin"):
            self.assertIn(needle, text)
        self.assertNotIn("AWS_", text, "no credential or profile in a file")

    def test_docker_daemon_is_started_when_it_is_down_and_left_alone_when_up(self) -> None:
        rig = Rig(self)
        rig.run()
        self.assertTrue(any(ln.startswith("dockerd") for ln in rig.log_lines(".tools")))
        rig = Rig(self)
        rig.docker_up.write_text("")
        rig.run()
        self.assertFalse(any(ln.startswith("dockerd") for ln in rig.log_lines(".tools")))

    def test_a_second_run_downloads_and_installs_nothing_again(self) -> None:
        rig = Rig(self)
        rig.run()
        before = len([ln for ln in rig.log_lines(".tools") if ln.startswith("rustup-init")])
        self.archive_gone(rig)
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len([ln for ln in rig.log_lines(".tools") if ln.startswith("rustup-init")]), before)

    def test_a_second_run_installs_the_pinned_components_and_targets_again(self) -> None:
        rig = Rig(self)
        rig.run()
        self.assertEqual([ln for ln in rig.log_lines(".tools") if ln.startswith("rustup ")], [], "the first install is rustup-init's own")
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        calls = [ln for ln in rig.log_lines(".tools") if ln.startswith("rustup ")]
        self.assertEqual(calls, [f"rustup toolchain install {RUST} --profile minimal -c rustfmt,clippy -t aarch64-apple-darwin,x86_64-unknown-linux-musl",
                                 f"rustup default {RUST}"], "an interrupted install gets the missing component or target")

    def archive_gone(self, rig: Rig) -> None:
        rig.archive_path.write_bytes(b"gone")


class TemplateOnlyCase(Base):
    """RUNNER_TOOLCHAIN_ONLY=template: the build that prints the CDK bootstrap template (SETUP_ACTION=bootstrap-template of runner-build.sh) needs the pinned AWS CLI and a
    Docker daemon for scripts/node.sh, and nothing else; the Rust toolchain and Go take minutes it does not need."""

    def test_only_the_aws_cli_is_installed_and_the_daemon_is_started(self) -> None:
        rig = Rig(self)
        proc = rig.run(RUNNER_TOOLCHAIN_ONLY="template")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"TOOLCHAIN PASS awscli={AWSCLI}")
        install = [c for c in rig.lock_calls() if c["argv"][0] == "install"][0]
        self.assertEqual(install["argv"][install["argv"].index("--only") + 1], "awscli")
        self.assertEqual([ln for ln in rig.log_lines(".tools") if ln.startswith("rustup")], [], "no Rust toolchain")
        self.assertFalse((rig.tools / "go").exists(), "no Go toolchain")
        self.assertTrue(any(ln.startswith("dockerd") for ln in rig.log_lines(".tools")))

    def test_the_env_file_has_the_tools_folder_and_the_path_but_no_rust(self) -> None:
        rig = Rig(self)
        rig.run(RUNNER_TOOLCHAIN_ONLY="template")
        text = (rig.tools / "env.sh").read_text()
        self.assertIn(f"export HAMH_TOOLS={rig.tools}", text)
        self.assertIn(f"{rig.tools}/bin", text)
        for word in ("RUSTUP_HOME", "CARGO_HOME", "/go/bin"):
            self.assertNotIn(word, text)

    def test_a_machine_without_a_go_row_is_no_problem(self) -> None:
        rig = Rig(self, with_row=False)
        proc = rig.run(RUNNER_TOOLCHAIN_ONLY="template")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)

    def test_another_value_is_a_usage_error_before_anything_is_installed(self) -> None:
        rig = Rig(self)
        proc = rig.run(RUNNER_TOOLCHAIN_ONLY="everything")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("RUNNER_TOOLCHAIN_ONLY", proc.stderr)
        self.assertEqual(rig.lock_calls(), [])

    def test_an_aws_cli_of_another_version_still_fails(self) -> None:
        rig = Rig(self)
        proc = rig.run(RUNNER_TOOLCHAIN_ONLY="template", FAKE_AWSCLI="2.0.1")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("TOOLCHAIN FAIL awscli "), proc.stdout)


class FailureCase(Base):
    def test_tools_that_do_not_match_their_checksum(self) -> None:
        rig = Rig(self)
        proc = rig.run(FAKE_INSTALL="fail")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("TOOLCHAIN FAIL tools "), proc.stdout)
        self.assertIn("sha256", last_line(proc))

    def test_an_aws_cli_of_another_version(self) -> None:
        rig = Rig(self)
        proc = rig.run(FAKE_AWSCLI="2.0.1")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("TOOLCHAIN FAIL awscli "), proc.stdout)
        self.assertIn(AWSCLI, last_line(proc))

    def test_rustc_of_another_version(self) -> None:
        rig = Rig(self)
        proc = rig.run(FAKE_RUSTC="1.70.0")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("TOOLCHAIN FAIL rust "), proc.stdout)
        self.assertIn("1.70.0", last_line(proc))

    def test_go_archive_with_another_checksum(self) -> None:
        rig = Rig(self, sha_ok=False)
        proc = rig.run()
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("TOOLCHAIN FAIL go "), proc.stdout)
        self.assertIn("sha256", last_line(proc))
        self.assertFalse((rig.tools / "go" / "bin" / "go").exists(), "an archive that fails the check is not unpacked")

    def test_no_go_row_for_this_machine(self) -> None:
        rig = Rig(self, with_row=False)
        proc = rig.run()
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("TOOLCHAIN FAIL go "), proc.stdout)
        self.assertIn("lang.lock", last_line(proc))

    def test_docker_that_does_not_come_up(self) -> None:
        rig = Rig(self)
        proc = rig.run(FAKE_DOCKERD_BROKEN="1", RUNNER_TOOLCHAIN_DOCKER_WAIT="2")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("TOOLCHAIN FAIL docker "), proc.stdout)

    def test_a_wait_that_is_not_a_number(self) -> None:
        rig = Rig(self)
        proc = rig.run(RUNNER_TOOLCHAIN_DOCKER_WAIT="soon")
        self.assertEqual(proc.returncode, 2)
        self.assertIn("RUNNER_TOOLCHAIN_DOCKER_WAIT", proc.stderr)
        self.assertEqual(rig.lock_calls(), [], "nothing is installed before the usage error")

    def test_a_tools_folder_with_a_space_gives_an_env_file_that_sources(self) -> None:
        rig = Rig(self)
        rig.tools = rig.tmp / "tools folder"
        proc = rig.run()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        out = subprocess.run(["bash", "-c", f'. "{rig.tools}/env.sh" && printf "%s\\n%s\\n" "$HAMH_TOOLS" "$CARGO_HOME"'], capture_output=True, text=True, timeout=limit(30))  # nosec B603: argument list, shell is a standard interpreter and the text is a constant with a path; B607: bash from PATH as the scripts under test do
        self.assertEqual(out.stdout.splitlines(), [str(rig.tools), f"{rig.tools}/cargo"], out.stderr)

    def test_a_missing_tools_folder_argument(self) -> None:
        rig = Rig(self)
        proc = subprocess.run(["bash", str(rig.tree / "scripts/setup/runner-toolchain.sh")], cwd=rig.tree, env=rig.env(), capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual(proc.returncode, 2)
        self.assertIn("usage", proc.stderr)


if __name__ == "__main__":
    unittest.main()
