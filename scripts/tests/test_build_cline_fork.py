"""Tests for scripts/build-cline-fork.sh and engine/cline/lock.toml.

Run on a Linux machine with the tools pinned in `tools.lock` (see scripts/tests/AGENTS.md):

    CLINE_FORK_WORK_DIR=<workspace tmp>/<label>/work \
      uv run --no-project --python 3.12 python -m unittest discover -s scripts/tests -v

The verify-only cases clone the pinned upstream tag from GitHub. Nothing is written outside
CLINE_FORK_WORK_DIR, and each case checks that the HOME it passes stays empty.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import tempfile
import tomllib
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts" / "build-cline-fork.sh"
CLINE_DIR = REPO / "engine" / "cline"
LOCK = CLINE_DIR / "lock.toml"
PATCHES = CLINE_DIR / "patches"

# Pins of the Cline series (2026-10-16): cli-v3.0.71 plus 15 patches (0012 and 0013 are the Bedrock additions for GLM 5.3, 0014 keeps Bedrock tool-use inputs free of blank keys, 0015 raises the run_commands command limit from 30 s to 10 min). The numbering restarts at 0001 because the old
# 0004 (harness system prompt) and 0010 (refusal reason) serve the old multi-agent roles and 0014 is upstream since cli-v3.0.66;
# old -> new: 0001-0003 same, 0005-0009 -> 0004-0008, 0011-0013 -> 0009-0011. The values are computed from the rebased series by
# engine/cline/lock.toml's generator (see engine/cline/AGENTS.md, "Rebase onto a new upstream CLI release").
UPSTREAM_COMMIT = "1a5c3f84e0933268a9dba14a55a6383a6578414d"
TAG_OBJECT = "7418804b1460929f3ff701bcccc8184fb69c8e26"
SERIES_TREE = "f8990b6745402aec9bf0458611c763a1a7e710f8"
PATCH_SHA256 = {
    "0001-feat-llms-route-Bedrock-Mantle-models-through-SigV4-.patch":
        "607b134cd5964dd2a3c3a907aa567a06a9057c2bfa6dfaf3cf3b387cc9089960",
    "0002-build-cli-compile-telemetry-out-unless-the-build-opt.patch":
        "f511133521cde912b584321a8374631032629572b78ae04cc082f9bb986c135c",
    "0003-fix-llms-advance-Bedrock-prompt-cache-checkpoints-th.patch":
        "3028597fa1c1504bd7ead08f61227c6721a56df44b809a92758b1f8e0e2ecd14",
    "0004-fix-cli-fail-loud-on-an-ACP-model-pin-instead-of-fal.patch":
        "15eefe12d96149a028f3f408a5ed95790f9fc0d856d0ca0b31e12f23d7659520",
    "0005-fix-cli-honor-data-dir-under-ACP-and-run-the-local-b.patch":
        "d9e5f06c06c480f79155babf4b2499b6730abfce21f4b57bf50133da0c0f6492",
    "0006-fix-cli-never-send-the-ACP-placeholder-key-as-a-bear.patch":
        "6ec43198df1fb3e41e602a6fcbc49753ef816dabfa859cf26e8448e616a0d257",
    "0007-fix-cli-report-the-turn-s-token-usage-in-the-ACP-ses.patch":
        "a602a7833511baf2906940c5a8896834de1c58839783e8df90493fff16bf5877",
    "0008-fix-core-keep-providers.json-maxTokens-when-no-per-t.patch":
        "fe1c677a2c89903e8d20df3f0ebbaf9b19c40e7c943650a105949c9888a258c4",
    "0009-fix-cli-carry-a-provider-failure-s-structure-into-th.patch":
        "b167df4fef441011cdf267dd674246cee97f8904a240e854720cce64f7f121ec",
    "0010-feat-cli-compact-the-context-under-ACP-from-provider.patch":
        "3bbfb70c6e1a8e03dc355ac5d68be74bae00b2bfc2672442b73c848072d063d3",
    "0011-fix-llms-send-a-Mantle-model-that-cannot-turn-reason.patch":
        "9c6db934767912390fa0279de3e7f75b30373fbaff8a81388f728d53b0a3a937",
    "0012-fix-llms-send-GLM-5.3-on-Bedrock-Converse-its-reason.patch":
        "9acce3f8cec53e00d68f304f7ad40a28ef73e25275c23956a57c13a72d8045ba",
    "0013-fix-llms-keep-Bedrock-tool-call-ids-unique-across-co.patch":
        "6579c95056f670313dc873afb46e52b1ed10e0dbc241f4c14a47e6139256e525",
    "0014-fix-llms-drop-blank-keys-from-Bedrock-tool-use-input.patch":
        "fedf8db1a363db3b3c33116912e9e38ea293c8622ce26b7a31e9b7b204477f15",
    "0015-fix-core-raise-the-default-command-limit-to-10-minut.patch":
        "831ab011cab700e5cb1ed0b4d96e37d3dc6f533d26397300e46591c05c93e50b",
}
# Test-only patches: file -> (sha256, test-only commit, parent, tree after git am
# onto the tag plus the preceding series patches).
TDD_PATCHES = {
    "tdd/0003-test-only.patch": (
        "06f17449c763996c9423d1882c1b2ec70b0d6ed7494b445ecc286ad18c0e1147",
        "6133019314854d41893ae3cea731390e3b89e4dc",
        "f8bc968034e4d95eb2afe5b817c762f7eab4188a",
        "06d564b067de2a67b3adf53038b451c47203f8d9"),
    "tdd/0004-test-only.patch": (
        "92a45396a9b8cb8ce5f5b7971328f5f8968fe85a254177e7e8b58cf11d7f9d98",
        "be83eedff3e52273dc9be32afc19b35b60dd8a52",
        "53f4bf70babc578835f529a322c1f5d101e229bc",
        "3f57be40923f68946800f7ca71e0fffceab1cab6"),
    "tdd/0005-test-only.patch": (
        "57c15a4a3579d87a05765828632648f0d7b51657c1425d19ab7da5328d6f9c13",
        "c2e8d02f4dd8c53226d165ed978de9c07d332767",
        "82b73e10fcff2e4510203211354859efd5a72bbf",
        "a10a672ac043f8beef7ca5e26bc40959fdbe53b0"),
    "tdd/0006-test-only.patch": (
        "d568bff52296dd61c84693a93f8cccf1d3acfa5a1c9fdc59b9e91bea2a622629",
        "8de23a18b9d4c74cc04abb8cca82fbcc106f0bf5",
        "2aa5e747fa35058bb27e7f632839ddc2073710f7",
        "d49aac556737f4aad937621269237cfc004ebcd7"),
    "tdd/0007-test-only.patch": (
        "fc60ce1aa2ed9aea0e102e7f31d22b62dfdbcb303985ab58be9cacfd0b3243e4",
        "cf365163a2a9faa996b45a732cbce0d3b438c5d7",
        "7738ee22fda87d09f9fd4cfe9cab5b9f3eba1758",
        "4ea36a9e6e745a524e2388e60d5346565443e28a"),
    "tdd/0008-test-only.patch": (
        "e3f6a3e825e86aadfc1c2c4c35ab03129ddf6a4f02f100fa71b1f8336fcff472",
        "d738cd600f0c470bdab1ee8fe657d555a97fc66a",
        "d02e4ae1d80e00ef84e394104adb221459085ec7",
        "4c8fd7670bf07fc09ab4558f24a0cf55e18d2bef"),
    "tdd/0009-test-only.patch": (
        "f88c4cd554d53279372eb63e46ccab1da92e16bdd3d258cf2edcb06ffcb0f5d6",
        "c4d5a318bcd8f6b6099cd793a73802a6a145107a",
        "2a4c3dae08941551cabedd42f437c14016baac28",
        "4f1d743299202e7ce7800b83545003aa5e408a72"),
    "tdd/0010-test-only.patch": (
        "b68b279277d3d49b51240516ef33a01b004d45479f50081b18fce8d637db96b9",
        "faca9a11b373d5861b1869101bd0afb1f783b76c",
        "90f07f61880c12ee69700653b4cff8b5d6993881",
        "a1df2714723d21e2d139aa078de572ce660c0e9c"),
    "tdd/0011-test-only.patch": (
        "c3b1f1ea561ca5641bb9da12ccea59b910b41fa8cd7a9f9e82a5dfb8d0008734",
        "53a039df2d9195e281c6d1fcfde5f57c04bca8d8",
        "a62e2e19f60bea00b6525464d77c30427473d132",
        "750d95efc4b9eda30a8378491e62c4bc96657cd4"),
    "tdd/0012-test-only.patch": (
        "09b47f0f9ea7b558d72d2190bb33b5d59f824a4e6b6e97671698fec45e06d85e",
        "ceecb8adad72b0fafe1e51772626bbc2d4415f77",
        "8fa0bdbe380f4761ff91700fad1a50a8d9476572",
        "1f5b3ac7da892e60cfb6e88aa25e30c295f865b9"),
    "tdd/0013-test-only.patch": (
        "6684bfe25bd8b12a0c8cf27ed978e4fc525077737b6d5d4db8a6102489bb58cf",
        "6205439fc233929ca1abe9f5ee852d928a2aaea5",
        "f8a28129df26011ee69515a7fe77a8d0cb324d66",
        "92cb7aa9c2b788117cfc7e225de98d3261d19524"),
    "tdd/0014-test-only.patch": (
        "21b98e1977aca44eabc621cd8ef72d69f2007e1d4658c3835680e0f5688a3148",
        "320c04855dd94315e7954f97597d46c9994f3944",
        "125b15f7a5cf7c72b4f5c1929c0374fa7230fb57",
        "f919306e7299d2de215894619dba22ea7bae6e21"),
    "tdd/0015-test-only.patch": (
        "1092d6d2d640b450c54ffdb33a41397cc7a7fee14d53f49ffe004c159aee7164",
        "f8d2de76d353bae72fc292fd6e9dc65815db59c2",
        "46733ed73dadbef5e3b3fa1396126c0c7370a776",
        "a9a46ef099c1866e3089d2375eb3270c710c6c5e"),
}

EXIT_USAGE = 2
EXIT_LOCK = 3
EXIT_PATCH_SHA = 4
EXIT_COMMIT = 5
EXIT_TREE = 7

WORK_ROOT: Path


def setUpModule() -> None:  # noqa: N802 (unittest hook name)
    global WORK_ROOT
    value = os.environ.get("CLINE_FORK_WORK_DIR", "")
    if not value:
        raise RuntimeError("set CLINE_FORK_WORK_DIR to a workspace tmp directory")
    WORK_ROOT = Path(value)
    WORK_ROOT.mkdir(parents=True, exist_ok=True)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


class Case(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix=f"{self._testMethodName}-", dir=WORK_ROOT))
        self.home = self.tmp / "sentinel-home"
        self.home.mkdir()
        # The accounts file these cases select from: an active entry (us-east-1) and a history entry (us-west-2), both placeholders.
        self.accounts = self.tmp / "accounts.json"
        self.accounts.write_text(json.dumps({
            "schemaVersion": 1, "selector": "HAMH_AWS_PROFILE",
            "accounts": [
                {"account": "111122223333", "region": "us-west-2", "profile": "history-profile", "role": "history",
                 "azs": {"direct": ["us-west-2a"]}, "azIds": {"direct": ["usw2-az1"]}, "invocationLogging": "read-only"},
                {"account": "123456789012", "region": "us-east-1", "profile": "my-profile", "role": "active",
                 "azs": {"direct": ["us-east-1a"]}, "azIds": {"direct": ["use1-az1"]}, "invocationLogging": "read-only"},
            ]}), encoding="utf-8")

    def tearDown(self) -> None:
        self.assertEqual(sorted(os.listdir(self.home)), [], "script wrote into HOME")
        shutil.rmtree(self.tmp, ignore_errors=True)

    def copy_cline_dir(self) -> Path:
        dest = self.tmp / "cline"
        shutil.copytree(CLINE_DIR, dest)
        return dest

    def run_script(self, *args: str, env_extra: dict | None = None,
                   timeout: int = 900) -> subprocess.CompletedProcess:
        env = {
            "PATH": os.environ["PATH"],
            "HOME": str(self.home),
            "CLINE_FORK_WORK_DIR": str(self.tmp / "work"),
            # The accounts file entry these cases were written against: the history entry of the file above.
            "HAMH_ACCOUNTS_FILE": str(self.accounts),
            "HAMH_AWS_PROFILE": "history-profile",
        }
        env.update(env_extra or {})
        return subprocess.run(  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
            ["bash", str(SCRIPT), *args],
            cwd=self.tmp, env=env, capture_output=True, text=True, timeout=timeout,
        )

    def assert_fails(self, result: subprocess.CompletedProcess, code: int, pattern: str) -> None:
        output = result.stdout + result.stderr
        self.assertEqual(result.returncode, code, output)
        self.assertRegex(result.stderr, pattern, output)


class LockFile(unittest.TestCase):
    def test_lock_is_toml_with_the_proven_pins(self) -> None:
        lock = tomllib.loads(LOCK.read_text(encoding="utf-8"))
        self.assertEqual(lock["schema"], "1")
        self.assertEqual(lock["upstream"]["url"], "https://github.com/cline/cline.git")
        self.assertEqual(lock["upstream"]["tag"], "cli-v3.0.71")
        self.assertEqual(lock["upstream"]["commit"], UPSTREAM_COMMIT)
        self.assertEqual(lock["series"]["tree"], SERIES_TREE)
        self.assertEqual(
            [(p["file"], p["sha256"]) for p in lock["patches"]], list(PATCH_SHA256.items()))
        self.assertEqual(
            [(t["file"], (t["sha256"], t["commit"], t["parent"], t["tree"])) for t in lock["tdd"]],
            list(TDD_PATCHES.items()))
        self.assertRegex(lock["toolchain"]["build_image"], r"@sha256:[0-9a-f]{64}$")

    def test_patch_files_match_lock_and_nothing_is_unlisted(self) -> None:
        on_disk = sorted(p.name for p in PATCHES.glob("*.patch"))
        self.assertEqual(on_disk, sorted(PATCH_SHA256))
        for name, digest in PATCH_SHA256.items():
            self.assertEqual(sha256(PATCHES / name), digest, name)
        tdd_on_disk = sorted(f"tdd/{p.name}" for p in (PATCHES / "tdd").iterdir())
        self.assertEqual(tdd_on_disk, sorted(TDD_PATCHES))
        for name, (digest, commit, parent, tree) in TDD_PATCHES.items():
            data = (PATCHES / name).read_text(encoding="utf-8")
            self.assertEqual(sha256(PATCHES / name), digest, name)
            self.assertIn(f"X-Harness-Test-Only: commit={commit} parent={parent} tree={tree}\n", data)

    def test_help_documents_the_gate_line(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            "FORK-BUILD PASS series=<n> tree=<sha12> bin=<sha12> telemetry=off leaks=0", text)
        self.assertIn("--no-gate", text)


class VerifyOnly(Case):
    def test_passes_on_real_lock_and_patches(self) -> None:
        out = self.tmp / "out"
        result = self.run_script("--verify-only", "--out", str(out))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(f"ok commit {UPSTREAM_COMMIT}", result.stdout)
        self.assertIn(f"ok tree {SERIES_TREE}", result.stdout)
        for name, (_digest, _commit, _parent, tree) in TDD_PATCHES.items():
            self.assertIn(f"ok tdd {name} tree {tree}", result.stdout)
        self.assertRegex(result.stdout, r"(?m)^VERIFY PASS$")
        meta = json.loads((out / "build-metadata.json").read_text(encoding="utf-8"))
        self.assertEqual(meta["mode"], "verify-only")
        self.assertEqual(meta["upstream"]["commit"], UPSTREAM_COMMIT)
        self.assertEqual(meta["series"]["tree"], SERIES_TREE)
        self.assertEqual(
            [(p["file"], p["sha256"]) for p in meta["patches"]], list(PATCH_SHA256.items()))
        self.assertEqual([t["file"] for t in meta["tdd"]], list(TDD_PATCHES))
        self.assertFalse((out / "cline").exists(), "verify-only must not build")
        self.assertEqual(list((self.tmp / "work" / "runs").glob("*/src")), [],
                         "the scratch clone must be removed on exit")

    def test_tampered_patch_byte_fails_before_any_clone(self) -> None:
        cline = self.copy_cline_dir()
        target = cline / "patches" / "0002-build-cli-compile-telemetry-out-unless-the-build-opt.patch"
        data = bytearray(target.read_bytes())
        middle = len(data) // 2
        data[middle] = ord("X") if data[middle] != ord("X") else ord("Y")
        target.write_bytes(bytes(data))
        result = self.run_script("--verify-only", "--lock", str(cline / "lock.toml"))
        self.assert_fails(result, EXIT_PATCH_SHA,
                          r"SHA-256 mismatch for 0002-build-cli-compile-telemetry-out")
        self.assertIn(PATCH_SHA256[target.name], result.stderr)
        self.assertIn(sha256(target), result.stderr)
        self.assertNotIn("cloning ", result.stdout + result.stderr)

    def test_wrong_commit_fails(self) -> None:
        cline = self.copy_cline_dir()
        lock = cline / "lock.toml"
        lock.write_text(lock.read_text(encoding="utf-8").replace(UPSTREAM_COMMIT, TAG_OBJECT),
                        encoding="utf-8")
        result = self.run_script("--verify-only", "--lock", str(lock))
        self.assert_fails(result, EXIT_COMMIT, r"commit mismatch")
        self.assertIn(TAG_OBJECT, result.stderr)
        self.assertIn(UPSTREAM_COMMIT, result.stderr)

    def test_wrong_tree_fails(self) -> None:
        cline = self.copy_cline_dir()
        lock = cline / "lock.toml"
        wrong = "0" * 40
        lock.write_text(lock.read_text(encoding="utf-8").replace(SERIES_TREE, wrong),
                        encoding="utf-8")
        result = self.run_script("--verify-only", "--lock", str(lock))
        self.assert_fails(result, EXIT_TREE, r"tree mismatch")
        self.assertIn(SERIES_TREE, result.stderr)


    def test_tampered_test_only_patch_fails_before_any_clone(self) -> None:
        cline = self.copy_cline_dir()
        target = cline / "patches" / "tdd" / "0005-test-only.patch"
        target.write_bytes(target.read_bytes() + b"\n")
        result = self.run_script("--verify-only", "--lock", str(cline / "lock.toml"))
        self.assert_fails(result, EXIT_PATCH_SHA, r"SHA-256 mismatch for tdd/0005-test-only\.patch")
        self.assertNotIn("cloning ", result.stdout + result.stderr)

    def test_wrong_test_only_tree_fails(self) -> None:
        cline = self.copy_cline_dir()
        lock = cline / "lock.toml"
        good = TDD_PATCHES["tdd/0006-test-only.patch"][3]
        lock.write_text(lock.read_text(encoding="utf-8").replace(good, "1" * 40), encoding="utf-8")
        result = self.run_script("--verify-only", "--lock", str(lock))
        self.assert_fails(result, EXIT_TREE, r"test-only tree mismatch for tdd/0006-test-only\.patch")
        self.assertIn(good, result.stderr)


# Fake docker for the gated-build record case: no container runs. `image inspect` succeeds,
# `cat /etc/passwd` prints a root entry, each gate step prints a vitest summary, and the build step
# prints what build.ts prints with telemetry off and writes an ELF stub for $FAKE_FORK_TARGET.
FAKE_DOCKER = r'''#!/usr/bin/env bash
case "$1" in image|pull) exit 0 ;; run) shift ;; *) echo "fake docker: $*" >&2; exit 9 ;; esac
args=("$@"); wd=""
for ((i = 0; i < ${#args[@]}; i++)); do [ "${args[i]}" = -w ] && wd=${args[i+1]}; done
cmd=${args[${#args[@]}-1]}
[ -n "${FAKE_DOCKER_LOG:-}" ] && printf '%s\t%s\n' "$wd" "$cmd" >>"$FAKE_DOCKER_LOG"
case "$cmd" in
  /etc/passwd) echo "root:x:0:0:root:/root:/bin/bash" ;;
  *script/build.ts*)
    bin="$wd/apps/cli/dist/cli-$FAKE_FORK_TARGET/bin"; mkdir -p "$bin"
    case $FAKE_FORK_TARGET in linux-arm64) m='\267\000' ;; *) m='\076\000' ;; esac
    printf "\177ELF\002\001\001\000\000\000\000\000\000\000\000\000\002\000$m" > "$bin/cline"
    echo "1.3.13"
    echo "Building @cline/cli v3.0.65 (telemetry: off)"
    echo "Telemetry off: no build-shell telemetry endpoint, header or key values in cli-$FAKE_FORK_TARGET" ;;
  *) echo " Tests  7 passed (7)" ;;
esac
'''
FAKE_CURL = r'''#!/usr/bin/env bash
out=""; prev=""
for a in "$@"; do [ "$prev" = -o ] && out=$a; prev=$a; done
cp "$FAKE_BUN_ZIP" "$out"
'''
HEADER = ("schemaVersion", "kind", "createdAt", "host", "account", "region", "stage", "codeSha", "sourceTree",
          "deploymentDigest", "imageDigest", "criteriaSha256", "result", "inputs", "measurements", "runIds", "notes")


class GatedBuildRecord(Case):
    """A gated build writes the record R24 reads (scripts/verify/lib/r24.py docstring):
    measurements.{arch, baseline: {<suite>: "ran"}, suites: {patched: "green"}, telemetry: "off",
    leaks, binarySha256}, stamped with the --image-digest it is recorded against."""

    def fake_env(self) -> tuple[dict, str]:
        import zipfile
        host = "arm64" if os.uname().machine in ("aarch64", "arm64") else "x64"
        target = "linux-x64" if host == "arm64" else "linux-arm64"  # cross: the stub never runs
        bins = self.tmp / "fake-bin"
        bins.mkdir()
        for name, text in (("docker", FAKE_DOCKER), ("curl", FAKE_CURL)):
            (bins / name).write_text(text)
            (bins / name).chmod(0o755)
        bun_zip = self.tmp / "bun.zip"
        with zipfile.ZipFile(bun_zip, "w") as zf:
            info = zipfile.ZipInfo(f"bun-linux-{'aarch64' if host == 'arm64' else 'x64'}/bun")
            info.external_attr = 0o755 << 16
            zf.writestr(info, "#!/bin/sh\necho 1.3.13\n")
        return ({"PATH": f"{bins}:{os.environ['PATH']}", "FAKE_BUN_ZIP": str(bun_zip),
                 "FAKE_FORK_TARGET": target}, target)

    def fake_lock(self) -> Path:
        cline = self.copy_cline_dir()
        lock = cline / "lock.toml"
        digest = sha256(self.tmp / "bun.zip")
        text = re.sub(r'(?m)^(bun_linux_(x64|aarch64)_zip_sha256) = "[0-9a-f]{64}"', rf'\1 = "{digest}"',
                      lock.read_text(encoding="utf-8"))
        lock.write_text(text, encoding="utf-8")
        return lock

    def test_the_record_names_the_build_account_and_region(self) -> None:
        # A CodeBuild build records its own account and Region (its build ARN); elsewhere the
        # record names the selected accounts file entry (the active entry when HAMH_AWS_PROFILE is unset).
        env, target = self.fake_env()
        lock = self.fake_lock()
        for name, extra, want in (
                ("codebuild", {"CODEBUILD_BUILD_ARN": "arn:aws:codebuild:us-east-1:123456789012:build/hamh-live-image-build:u"},
                 ("123456789012", "us-east-1")),
                ("active", {"HAMH_AWS_PROFILE": ""}, ("123456789012", "us-east-1"))):
            with self.subTest(name=name):
                out, rec_dir = self.tmp / f"out-{name}", self.tmp / f"records-{name}"
                result = self.run_script("--target", target, "--lock", str(lock), "--out", str(out),
                                         "--record-dir", str(rec_dir), "--image-digest", "a" * 64, env_extra={**env, **extra})
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                rec = json.loads(sorted(rec_dir.glob("*.json"))[0].read_text())
                self.assertEqual((rec["account"], rec["region"]), want)

    def test_a_gated_build_writes_the_r24_record(self) -> None:
        env, target = self.fake_env()
        lock = self.fake_lock()
        out, rec_dir, image = self.tmp / "out", self.tmp / "records", "a" * 64
        result = self.run_script("--target", target, "--lock", str(lock), "--out", str(out),
                                 "--record-dir", str(rec_dir), "--image-digest", image, env_extra=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        lines = result.stdout.strip().splitlines()
        self.assertRegex(lines[-1], r"^FORK-BUILD PASS series=15 tree=f8990b674540 bin=[0-9a-f]{12} telemetry=off leaks=0$")
        arch = target.split("-")[1]
        files = sorted(rec_dir.glob("*.json"))
        self.assertEqual(len(files), 1, files)
        self.assertRegex(files[0].name, rf"^\d{{4}}-\d{{2}}-\d{{2}}-cline-fork-build-{arch}-\d{{8}}T\d{{6}}Z-\d+\.json$")
        self.assertIn(f"FORK-BUILD RECORD {files[0]}", lines)
        self.assertEqual((out / files[0].name).read_bytes(), files[0].read_bytes(), "the out dir keeps a copy")
        rec = json.loads(files[0].read_text())
        self.assertEqual([k for k in HEADER if k not in rec], [])
        head = subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
        self.assertEqual((rec["schemaVersion"], rec["kind"], rec["result"], rec["imageDigest"], rec["codeSha"]),
                         (1, "cline-fork-build", "pass", image, head or None))
        self.assertEqual((rec["account"], rec["region"]), ("111122223333", "us-west-2"))
        m = rec["measurements"]
        suites = ["llms", "core", "shared", "agents", "cli", "types"]
        self.assertEqual(m["arch"], arch)
        self.assertEqual(m["baseline"], {s: "ran" for s in suites})
        self.assertEqual(m["suites"], {"baseline": "green", "patched": "green"})
        self.assertEqual(m["passed"], {"baseline": {s: 7 for s in suites}, "patched": {s: 7 for s in suites}})
        self.assertEqual((m["telemetry"], m["leaks"]), ("off", 0))
        self.assertEqual(m["binarySha256"], sha256(out / "cline"))
        self.assertEqual((m["seriesTree"], m["patchCount"], m["lastLine"]), (SERIES_TREE, len(PATCH_SHA256), lines[-1]))
        self.assertEqual(rec["inputs"]["target"], target)
        self.assertEqual(rec["inputs"]["lockSha256"], sha256(lock))

    def test_the_core_suite_runs_its_files_one_at_a_time_on_both_trees(self) -> None:
        # That start-up runs cleanupStaleDetachedCommandLogs() over the shared TMPDIR and retires the test's live
        # directory, whose process start token the test fakes. The same test failed in 5 of 15 gate runs on the
        # unpatched tag (2026-10-01), so no patch causes it. The gate keeps every test and every criterion and runs
        # the @cline/core step's files one at a time, on the unpatched tag and on the patched tree alike; the record
        # names that.
        env, target = self.fake_env()
        lock = self.fake_lock()
        log = self.tmp / "docker.log"
        out, rec_dir = self.tmp / "out", self.tmp / "records"
        result = self.run_script("--target", target, "--lock", str(lock), "--out", str(out),
                                 "--record-dir", str(rec_dir), "--image-digest", "a" * 64,
                                 env_extra={**env, "FAKE_DOCKER_LOG": str(log)})
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        steps: dict[tuple[str, str], str] = {}
        for line in log.read_text().splitlines():
            wd, _, cmd = line.partition("\t")
            tree = "baseline" if wd.endswith("/base") else "patched" if wd.endswith("/src") else None
            if tree and cmd.startswith("set -e; bun -F "):
                steps[(tree, cmd.split("bun -F ", 1)[1].split()[0])] = cmd.removeprefix("set -e; ")
        suites = ["@cline/llms", "@cline/core", "@cline/shared", "@cline/agents", "@cline/cli"]
        for tree in ("baseline", "patched"):
            with self.subTest(tree=tree):
                self.assertEqual(sorted(p for t, p in steps if t == tree), sorted(suites), steps)
                self.assertEqual(steps[(tree, "@cline/core")], "bun -F @cline/core test:unit --no-file-parallelism")
                for pkg in suites:
                    if pkg != "@cline/core":
                        self.assertNotIn("--no-file-parallelism", steps[(tree, pkg)])
        rec = json.loads(sorted(rec_dir.glob("*.json"))[0].read_text())
        self.assertEqual(rec["measurements"]["gateSchedule"], {"core": "file-parallelism-off"})

    def test_record_dir_needs_a_gated_build(self) -> None:
        for flags in (["--verify-only"], ["--no-gate"]):
            with self.subTest(flags=flags):
                result = self.run_script(*flags, "--record-dir", str(self.tmp / "records"))
                self.assert_fails(result, EXIT_USAGE, r"--record-dir needs a gated build")
                self.assertFalse((self.tmp / "records").exists())

    def test_image_digest_must_be_64_hex(self) -> None:
        result = self.run_script("--verify-only", "--image-digest", "sha256:abc")
        self.assert_fails(result, EXIT_USAGE, r"--image-digest must be 64 lowercase hex")


class Guards(Case):
    def test_unlisted_patch_fails(self) -> None:
        cline = self.copy_cline_dir()
        (cline / "patches" / "0003-unlisted.patch").write_text("From x\n", encoding="utf-8")
        result = self.run_script("--verify-only", "--lock", str(cline / "lock.toml"))
        self.assert_fails(result, EXIT_LOCK, r"unlisted patch 0003-unlisted\.patch")

    def test_unknown_lock_key_fails(self) -> None:
        cline = self.copy_cline_dir()
        lock = cline / "lock.toml"
        lock.write_text(lock.read_text(encoding="utf-8").replace(
            "[series]\n", "[series]\nextra = \"x\"\n"), encoding="utf-8")
        result = self.run_script("--verify-only", "--lock", str(lock))
        self.assert_fails(result, EXIT_LOCK, r"unknown key series\.extra")

    def test_unlisted_test_only_patch_fails(self) -> None:
        cline = self.copy_cline_dir()
        (cline / "patches" / "tdd" / "0004-extra.patch").write_text("From x\n", encoding="utf-8")
        result = self.run_script("--verify-only", "--lock", str(cline / "lock.toml"))
        self.assert_fails(result, EXIT_LOCK, r"unlisted test-only patch tdd/0004-extra\.patch")

    def test_test_only_patch_for_a_missing_series_patch_fails(self) -> None:
        cline = self.copy_cline_dir()
        lock = cline / "lock.toml"
        text = lock.read_text(encoding="utf-8")
        past = f"{len(PATCH_SHA256) + 1:04d}"  # one past the last series patch
        (cline / "patches" / "tdd" / "0007-test-only.patch").rename(cline / "patches" / "tdd" / f"{past}-test-only.patch")
        lock.write_text(text.replace("tdd/0007-test-only.patch", f"tdd/{past}-test-only.patch"), encoding="utf-8")
        result = self.run_script("--verify-only", "--lock", str(lock))
        self.assert_fails(result, EXIT_LOCK, rf"tdd/{past}-test-only\.patch.*no series patch {past}")

    def test_no_gate_is_accepted(self) -> None:
        result = self.run_script("--verify-only", "--no-gate", "--lock", str(LOCK))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_refuses_tmp_work_dir(self) -> None:
        result = self.run_script("--verify-only", "--work-dir", "/tmp/cline-fork-test")  # nosec B108: system temp path as test data that is refused, missing or compared as text, nothing is created there
        self.assert_fails(result, EXIT_USAGE, r"refusing work dir /tmp/cline-fork-test")
        self.assertFalse(Path("/tmp/cline-fork-test").exists())  # nosec B108: system temp path as test data that is refused, missing or compared as text, nothing is created there

    def test_refuses_home_work_dir(self) -> None:
        result = self.run_script("--verify-only", "--work-dir", str(self.home / ".cache" / "x"))
        self.assert_fails(result, EXIT_USAGE, r"refusing work dir")

    def test_refuses_telemetry_opt_in(self) -> None:
        result = self.run_script("--verify-only", env_extra={"CLINE_BUILD_TELEMETRY": "on"})
        self.assert_fails(result, EXIT_USAGE, r"telemetry-off only")

    def test_rejects_unknown_target(self) -> None:
        result = self.run_script("--target", "darwin-arm64")
        self.assert_fails(result, EXIT_USAGE, r"--target must be linux-x64 or linux-arm64")

    def test_requires_work_dir(self) -> None:
        result = self.run_script("--verify-only", env_extra={"CLINE_FORK_WORK_DIR": ""})
        self.assert_fails(result, EXIT_USAGE, r"CLINE_FORK_WORK_DIR")


if __name__ == "__main__":
    unittest.main(verbosity=2)
