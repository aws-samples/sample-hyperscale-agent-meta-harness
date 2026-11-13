"""Tests for scripts/lib/agentd_build.py: the one recipe that builds the worker binaries.

The production worker image carries an `agentd` that `scripts/lib/deploy.py` cross-builds with `cargo zigbuild
--release -p agentd --target aarch64-unknown-linux-gnu.2.34`. Needs the `agentd` the fake-acp local qualification runs
to be that binary, so deploy and the `fake-acp` CI step build through this one module. A fake cargo stands in for the
toolchain; nothing here builds Rust.

The contract:
- the argv is the deploy's, the environment carries the selection and the CARGO_HOME remap and none of the variables
  that change what the compiler emits (wrappers, other rustflags, incremental);
- the pins the recipe asserts equal tools.lock and rust-toolchain.toml;
- `prove` builds root A in place (the deploy layout) and root B from a copy of the working tree with a fresh
  CARGO_HOME, prints both hashes and passes only when they are equal and neither binary embeds a host path;
- the recipe digest moves with every element of the recipe.
"""

from __future__ import annotations

import hashlib
import json
import os
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import agentd_build as ab  # noqa: E402
import aws_target  # noqa: E402

CLI = REPO / "scripts" / "lib" / "agentd_build.py"
TARGET = aws_target.module_target()


def git(cwd: Path, *args: str) -> str:
    env = dict(os.environ, GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com", GIT_COMMITTER_NAME="t",
               GIT_COMMITTER_EMAIL="t@example.com", GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1")
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True, env=env).stdout.strip()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do


FAKE_CARGO = r'''#!/usr/bin/env python3
"""A fake cargo: logs argv and the environment it was given, and writes the binaries a zigbuild would."""
import json, os, sys
from pathlib import Path
log = Path(os.environ["FAKE_CARGO_LOG"])
with log.open("a") as f:
    f.write(json.dumps({"argv": sys.argv[1:], "cwd": os.getcwd(), "env": {k: os.environ.get(k) for k in (
        "CARGO_HOME", "CARGO_TARGET_DIR", "RUSTFLAGS", "RUSTC_WRAPPER", "HAMH_AWS_PROFILE", "AWS_PROFILE", "CARGO_INCREMENTAL")}}) + "\n")
if sys.argv[1] == "fetch":
    mode = os.environ.get("FAKE_CARGO_MODE", "same")
    once = Path(os.environ["FAKE_CARGO_LOG"] + ".fetch-once")
    if mode == "fetch-fails" or (mode == "fetch-fails-once" and not once.exists()):
        once.write_text("x")
        print("error: failed to download from https://index.crates.io: HTTP 503", file=sys.stderr)
        sys.exit(101)
    Path(os.environ["CARGO_HOME"], "registry").mkdir(parents=True, exist_ok=True)
    sys.exit(0)
if sys.argv[1] == "zigbuild":
    out = Path(os.environ["CARGO_TARGET_DIR"]) / "aarch64-unknown-linux-gnu" / "release"
    out.mkdir(parents=True, exist_ok=True)
    mode = os.environ.get("FAKE_CARGO_MODE", "same")
    if mode == "hang":
        import time; time.sleep(600)
    body = b"\x7fELF-agentd Region " + os.environ["FAKE_REGION"].encode() + b" " + Path(os.getcwd(), "crates/agentd/src/main.rs").read_bytes()
    if mode == "cargo-home-leak":          # the registry path reaches the binary unless the remap is applied
        body += os.environ["CARGO_HOME"].encode()
    if mode == "varies-with-home":
        body += os.environ["CARGO_HOME"].encode() + b"#" + os.environ.get("RUSTFLAGS", "").encode()
    for name in sys.argv:
        pass
    (out / "agentd").write_bytes(body)
    aws = Path(os.environ["CARGO_TARGET_DIR"]) / "aarch64-unknown-linux-gnu" / "release" / "build" / "harness-protocol-0" / "out"
    aws.mkdir(parents=True, exist_ok=True)
    p = os.environ.get("HAMH_AWS_PROFILE", "")
    (aws / "aws_target.rs").write_text('pub const E2E_ACCOUNT: &str = "%s";\npub const E2E_REGION: &str = "%s";\npub const E2E_PROFILE: &str = "%s";\n' % (
        os.environ["FAKE_ACCOUNT"], os.environ["FAKE_REGION"], p))
    sys.exit(0)
sys.exit(2)
'''


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="agentd-build-"))
        self.cargo = self.tmp / "fake-cargo"
        self.cargo.write_text(FAKE_CARGO)
        self.cargo.chmod(0o755)
        self.log = self.tmp / "cargo.log"
        self.repo = self.tmp / "repo"
        (self.repo / "crates/agentd/src").mkdir(parents=True)
        (self.repo / "crates/agentd/src/main.rs").write_text("fn main() {}\n")
        (self.repo / "Cargo.lock").write_text("# lock\n")
        (self.repo / "tools.lock").write_text((REPO / "tools.lock").read_text())
        (self.repo / "rust-toolchain.toml").write_text((REPO / "rust-toolchain.toml").read_text())
        (self.repo / "accounts.json").write_text((REPO / "accounts.json").read_text())
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "add", ".")
        git(self.repo, "commit", "-q", "-m", "base")
        self.env = {k: v for k, v in os.environ.items() if k not in ("RUSTFLAGS", "RUSTC_WRAPPER", "CARGO_HOME", "HAMH_AWS_PROFILE")}
        self.env.update(AGENTD_BUILD_CARGO=str(self.cargo), AGENTD_BUILD_SLOT="", FAKE_CARGO_LOG=str(self.log),
                        FAKE_ACCOUNT=TARGET.account, FAKE_REGION=TARGET.region, CARGO_HOME=str(self.tmp / "cargo-home"),
                        AGENTD_BUILD_SKIP_TOOL_CHECK="1")
        (self.tmp / "cargo-home").mkdir()

    def calls(self) -> list[dict]:
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.is_file() else []

    def cli(self, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run([sys.executable, str(CLI), *args], capture_output=True, text=True, env={**self.env, **(env or {})})  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool


class RecipeCase(Base):
    def test_argv_is_the_deploys(self) -> None:
        self.assertEqual(ab.zigbuild_args(), ["zigbuild", "--release", "--locked", "-p", "agentd", "--target",
                                              "aarch64-unknown-linux-gnu.2.34"])
        self.assertEqual(ab.zigbuild_args(["harness-evaluator"]),
                         ["zigbuild", "--release", "--locked", "-p", "harness-evaluator", "--target", "aarch64-unknown-linux-gnu.2.34"])

    def test_rustflags_remap_cargo_home(self) -> None:
        self.assertEqual(ab.rustflags("/x/cargo"), "--remap-path-prefix=/x/cargo=/cargo-home")

    def test_environment_carries_the_selection_and_drops_compiler_changers(self) -> None:
        base = {"PATH": "/bin", "RUSTFLAGS": "-C target-cpu=native", "CARGO_ENCODED_RUSTFLAGS": "x", "RUSTC_WRAPPER": "/usr/bin/sccache",
                "RUSTC_WORKSPACE_WRAPPER": "w", "CARGO_INCREMENTAL": "1", "CARGO_BUILD_RUSTFLAGS": "y", "HOME": "/h", "CARGO_HOME": "/c"}
        env = ab.build_env(base, target_dir=Path("/t"), profile="ham-acct-a01")
        self.assertEqual(env["CARGO_TARGET_DIR"], "/t")
        self.assertEqual((env["HAMH_AWS_PROFILE"], env["AWS_PROFILE"]), ("ham-acct-a01", "ham-acct-a01"))
        self.assertEqual(env["RUSTFLAGS"], "--remap-path-prefix=/c=/cargo-home")
        self.assertEqual(env["CARGO_INCREMENTAL"], "0")
        for gone in ("CARGO_ENCODED_RUSTFLAGS", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_RUSTFLAGS"):
            self.assertNotIn(gone, env)
        self.assertEqual((env["PATH"], env["HOME"]), ("/bin", "/h"))

    def test_cargo_home_defaults_to_home_dot_cargo(self) -> None:
        env = ab.build_env({"HOME": "/h"}, target_dir=Path("/t"), profile="p")
        self.assertEqual(env["RUSTFLAGS"], "--remap-path-prefix=/h/.cargo=/cargo-home")

    def test_pins_equal_the_locks(self) -> None:
        lock = tomllib.loads((REPO / "tools.lock").read_text())
        tools = {t["name"]: t["version"] for t in lock["tool"]}
        self.assertEqual(ab.ZIG_VERSION, tools["zig"])
        self.assertEqual(ab.CARGO_ZIGBUILD_VERSION, tools["cargo-zigbuild"])
        self.assertEqual(ab.RUSTC_VERSION, lock["rust"]["toolchain"])
        self.assertEqual(ab.RUSTC_VERSION, tomllib.loads((REPO / "rust-toolchain.toml").read_text())["toolchain"]["channel"])

    def test_deploy_builds_only_through_this_module(self) -> None:
        text = (REPO / "scripts" / "lib" / "deploy.py").read_text()
        self.assertIn("agentd_build.zigbuild_args", text)
        self.assertIn("agentd_build.build_env", text)
        self.assertNotIn('"--release", *packages, "--target"', text)

    def test_recipe_digest_is_stable_and_moves_with_every_element(self) -> None:
        d = ab.recipe_digest(self.repo, profile="ham-acct-a01")
        self.assertRegex(d, r"^[0-9a-f]{64}$")
        self.assertEqual(d, ab.recipe_digest(self.repo, profile="ham-acct-a01"))
        self.assertNotEqual(d, ab.recipe_digest(self.repo, profile="ham-acct-b02"))
        rec = ab.recipe(self.repo, profile="ham-acct-a01")
        for key in ("argv", "rustflags", "target", "zig", "cargoZigbuild", "rustc", "profile", "packages"):
            self.assertIn(key, rec)
        self.assertEqual(d, hashlib.sha256(json.dumps(rec, sort_keys=True, separators=(",", ":")).encode()).hexdigest())

    def test_path_leaks_names_the_host_paths_a_binary_embeds(self) -> None:
        b = self.tmp / "bin"
        leak = "/home/u/cargo"  # a home folder of a build host
        b.write_bytes(b"xx" + leak.encode() + b"/registry/src/aws-1/src/a.rsyy")
        self.assertEqual(ab.path_leaks(b, [leak, "/nope", ""]), [leak])
        self.assertEqual(ab.path_leaks(b, []), [])


class BuildCase(Base):
    def test_build_runs_the_recipe_in_the_deploy_layout(self) -> None:
        p = self.cli("build", "--root", str(self.repo))
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        (call,) = self.calls()
        self.assertEqual(call["argv"], ab.zigbuild_args())
        want = self.repo / "target" / "deploy" / f"{TARGET.account}-{TARGET.region}"
        self.assertEqual(call["env"]["CARGO_TARGET_DIR"], str(want))
        self.assertEqual(call["env"]["HAMH_AWS_PROFILE"], TARGET.profile)
        self.assertEqual(call["env"]["RUSTFLAGS"], f"--remap-path-prefix={self.tmp / 'cargo-home'}=/cargo-home")
        self.assertIsNone(call["env"]["RUSTC_WRAPPER"])
        line = next(ln for ln in p.stdout.splitlines() if ln.startswith("AGENTD-BUILD "))
        sha = hashlib.sha256((want / "aarch64-unknown-linux-gnu" / "release" / "agentd").read_bytes()).hexdigest()
        self.assertIn(f"sha256={sha}", line)

    def test_build_refuses_a_binary_built_for_another_selection(self) -> None:
        p = self.cli("build", "--root", str(self.repo), env={"FAKE_REGION": "ap-south-1"})
        self.assertEqual(p.returncode, 1, p.stdout)
        self.assertIn("AGENTD-BUILD FAIL", p.stdout)

    def test_a_build_that_hangs_is_killed_at_the_deadline(self) -> None:
        p = self.cli("build", "--root", str(self.repo), env={"FAKE_CARGO_MODE": "hang", "AGENTD_BUILD_TIMEOUT_S": "1"})
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertIn("AGENTD-BUILD FAIL root=A cargo zigbuild timed out after 1", p.stdout)

    def test_a_wrong_zig_is_refused_before_any_build(self) -> None:
        zig = self.tmp / "zig"
        zig.write_text("#!/bin/sh\necho 0.15.0\n")
        zig.chmod(0o755)
        p = self.cli("build", "--root", str(self.repo), env={"AGENTD_BUILD_ZIG": str(zig), "AGENTD_BUILD_SKIP_TOOL_CHECK": "0"})
        self.assertEqual(p.returncode, 2, p.stdout + p.stderr)
        self.assertIn("zig 0.15.0", p.stdout + p.stderr)
        self.assertEqual(self.calls(), [])


class ProveCase(Base):
    def prove(self, mode: str = "same") -> subprocess.CompletedProcess:
        return self.cli("prove", "--root", str(self.repo), "--work", str(self.tmp / "work"), env={"FAKE_CARGO_MODE": mode})

    def lines(self, p: subprocess.CompletedProcess) -> dict[str, list[str]]:
        out: dict[str, list[str]] = {}
        for ln in p.stdout.splitlines():
            out.setdefault(ln.split(" ", 1)[0], []).append(ln)
        return out

    def test_two_roots_build_the_same_bytes(self) -> None:
        p = self.prove()
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        ln = self.lines(p)
        a, b = ln["AGENTD-BUILD"]
        self.assertIn("root=A", a)
        self.assertIn("root=B", b)
        sha = a.split("sha256=")[1].split()[0]
        self.assertEqual(sha, b.split("sha256=")[1].split()[0])
        self.assertEqual(ln["AGENTD-PRODUCTION-SHA256"], [f"AGENTD-PRODUCTION-SHA256 {sha} source=rebuild-B"])
        self.assertEqual(ln["AGENTD-EQUAL"], ["AGENTD-EQUAL PASS"])
        self.assertRegex(ln["AGENTD-RECIPE"][0], r"digest=[0-9a-f]{64}")
        calls = self.calls()
        fetch = [c for c in calls if c["argv"][0] == "fetch"]
        self.assertEqual(len(fetch), 1)
        self.assertEqual(fetch[0]["argv"], ["fetch", "--locked"])
        zig = [c for c in calls if c["argv"][0] == "zigbuild"]
        self.assertEqual(len(zig), 2)
        self.assertNotEqual(zig[0]["cwd"], zig[1]["cwd"])
        self.assertNotEqual(zig[0]["env"]["CARGO_HOME"], zig[1]["env"]["CARGO_HOME"])
        self.assertNotEqual(zig[0]["env"]["CARGO_TARGET_DIR"], zig[1]["env"]["CARGO_TARGET_DIR"])

    def test_root_b_is_a_copy_of_the_working_tree_including_edits(self) -> None:
        (self.repo / "crates/agentd/src/main.rs").write_text("fn main() { /* dirty */ }\n")
        (self.repo / "crates/agentd/src/new.rs").write_text("// untracked\n")
        p = self.prove()
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        b = next(c for c in self.calls() if c["argv"][0] == "zigbuild" and c["cwd"].startswith(str(self.tmp / "work")))
        self.assertIn("dirty", Path(b["cwd"], "crates/agentd/src/main.rs").read_text())
        self.assertTrue(Path(b["cwd"], "crates/agentd/src/new.rs").is_file())
        self.assertFalse(Path(b["cwd"], ".git").exists(), "a copy of the files, not a clone of the history")

    def test_a_build_that_depends_on_cargo_home_fails_the_equality(self) -> None:
        p = self.prove("varies-with-home")
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        ln = self.lines(p)
        self.assertEqual(ln["AGENTD-EQUAL"], ["AGENTD-EQUAL FAIL a-and-b-differ"])
        self.assertNotIn("AGENTD-PRODUCTION-SHA256", ln)

    def test_a_staged_copy_that_differs_from_root_a_fails_the_proof(self) -> None:
        # The copy is made right after the equality check; a copy whose bytes differ from the hashed binary must never be staged
        # as the production rebuild (review of the implemented code, 2026-10-03).
        real = ab.shutil.copy2

        def corrupt(src, dst, *a, **k):
            out = real(src, dst, *a, **k)
            if Path(dst).name == "agentd" and Path(dst).parent.name == "stage":
                Path(dst).chmod(0o755)
                Path(dst).write_bytes(b"tampered")
            return out

        ab.shutil.copy2 = corrupt
        try:
            code, lines = ab.prove(self.repo, self.tmp / "work", env={**self.env, "FAKE_CARGO_MODE": "same"}, out_dir=self.tmp / "stage")
        finally:
            ab.shutil.copy2 = real
        self.assertEqual(code, 1, "\n".join(lines))
        self.assertIn("AGENTD-EQUAL FAIL staged-differs", lines)

    def test_a_registry_fetch_that_fails_once_is_retried(self) -> None:
        p = self.cli("prove", "--root", str(self.repo), "--work", str(self.tmp / "work"),
                     env={"FAKE_CARGO_MODE": "fetch-fails-once", "AGENTD_BUILD_RETRY_SLEEP": "0"})
        self.assertEqual(p.returncode, 0, p.stdout + p.stderr)
        self.assertEqual(len([c for c in self.calls() if c["argv"][0] == "fetch"]), 2)

    def test_a_registry_fetch_that_never_works_fails_after_three_attempts(self) -> None:
        p = self.cli("prove", "--root", str(self.repo), "--work", str(self.tmp / "work"),
                     env={"FAKE_CARGO_MODE": "fetch-fails", "AGENTD_BUILD_RETRY_SLEEP": "0"})
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertEqual(len([c for c in self.calls() if c["argv"][0] == "fetch"]), 3)
        self.assertIn("AGENTD-BUILD FAIL root=B cargo fetch --locked", p.stdout)

    def test_a_binary_that_embeds_a_host_path_fails(self) -> None:
        p = self.prove("cargo-home-leak")
        self.assertEqual(p.returncode, 1, p.stdout + p.stderr)
        self.assertRegex(self.lines(p)["AGENTD-LEAKS"][0], r"root=A FAIL .*cargo-home")


if __name__ == "__main__":
    unittest.main()
