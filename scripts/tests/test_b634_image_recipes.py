"""The pins that the image scan found vulnerable stay fixed.

Offline checks of the build recipes: the base image, certifi and pip of the gate tool set, and gitleaks built from source on a current Go toolchain.
"""

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def read(rel):
    return (ROOT / rel).read_text(encoding="utf-8")


def version_tuple(v):
    return tuple(int(x) for x in v.split("."))


class BaseImage(unittest.TestCase):
    def test_the_three_dockerfiles_start_from_one_base_and_the_arm64_pins_follow_it(self):
        digests = set()
        for rel in ("images/worker-python/Dockerfile", "images/delivery/Dockerfile", "images/lang-test/Dockerfile"):
            m = re.search(r"^FROM public\.ecr\.aws/amazonlinux/amazonlinux:2023@(sha256:[0-9a-f]{64})$", read(rel), re.M)
            self.assertIsNotNone(m, rel)
            digests.add(m.group(1))
        self.assertEqual(len(digests), 1, digests)
        # The arm64 child manifest of tools.lock is the one scripts/lib/arm64_sysroot.py uses; they name the same image.
        lock = re.search(r'^al2023_arm64 = "public\.ecr\.aws/amazonlinux/amazonlinux@(sha256:[0-9a-f]{64})"$', read("tools.lock"), re.M)
        sysroot = re.search(r'^IMAGE = "public\.ecr\.aws/amazonlinux/amazonlinux@(sha256:[0-9a-f]{64})"$', read("scripts/lib/arm64_sysroot.py"), re.M)
        self.assertIsNotNone(lock)
        self.assertIsNotNone(sysroot)
        self.assertEqual(lock.group(1), sysroot.group(1))
        self.assertNotEqual(next(iter(digests)), "sha256:06da5a3362eda00c5114227ac81abe56dd9b943395553b7c64a310457f4d9e2b", "the base of the 2026-10 scan")

    def test_the_service_packages_are_the_ones_of_the_newer_base(self):
        lock = read("images/services/services.lock")
        self.assertNotIn("11.4.12-1.amzn2023.0.1", lock)
        self.assertIn("mariadb114-server 11.4.13-1.amzn2023.0.2", lock)
        self.assertNotIn("11.4.12", read("images/lang-test/Dockerfile"))


class GateToolSet(unittest.TestCase):
    def test_certifi_is_past_the_critical_advisory_and_pip_carries_a_fixed_certifi(self):
        text = read("images/tools/tools.txt")
        certifi = re.search(r"^certifi==(\d+\.\d+\.\d+) ", text, re.M)
        self.assertIsNotNone(certifi)
        self.assertGreaterEqual(version_tuple(certifi.group(1)), (2026, 10, 19))
        pip_in = re.search(r"^pip==(\S+)$", read("images/tools/tools.in"), re.M).group(1)
        pip_txt = re.search(r"^pip==(\S+) ", text, re.M).group(1)
        self.assertEqual(pip_in, pip_txt)
        for name, floor in (("filelock", (4, 2, 1)), ("msgpack", (1, 2, 3)), ("urllib3", (2, 8, 0))):
            m = re.search(rf"^{name}==(\d+\.\d+\.\d+) ", text, re.M)
            self.assertIsNotNone(m, name)
            self.assertGreaterEqual(version_tuple(m.group(1)), floor, name)
        # pip 26.2.1 vendored certifi 2026.6.17, the second critical finding; 26.3 vendors 2026.10.23.
        self.assertGreaterEqual(version_tuple(pip_in), (26, 3))


class Gitleaks(unittest.TestCase):
    def test_both_recipes_build_gitleaks_from_source_and_pin_the_same_result(self):
        shas = set()
        for rel in ("images/codebuild/worker-python.yml", "images/codebuild/delivery.yml"):
            text = read(rel)
            self.assertIn("images/tools/build-gitleaks.sh", text, rel)
            self.assertNotIn("GITLEAKS_URL", text, rel)
            self.assertNotIn("GITLEAKS_ARCHIVE_SHA256", text, rel)
            m = re.search(r"^\s+GITLEAKS_SHA256: ([0-9a-f]{64})$", text, re.M)
            self.assertIsNotNone(m, rel)
            shas.add(m.group(1))
        self.assertEqual(len(shas), 1, "one build, one hash")

    def test_the_source_build_is_the_version_of_tools_lock_on_a_go_past_the_critical_advisory(self):
        lock = read("images/tools/gitleaks.lock")
        row = re.search(r"^gitleaks (\S+) module (\S+)$", lock, re.M)
        self.assertIsNotNone(row)
        tools = next(b for b in read("tools.lock").split("[[tool]]") if 'name = "gitleaks"' in b)
        self.assertIn(f'version = "{row.group(1)}"', tools)
        gos = re.findall(r"^go (\d+\.\d+\.\d+) (x86_64|aarch64) ([0-9a-f]{64}) (\S+)$", lock, re.M)
        self.assertEqual(sorted(g[1] for g in gos), ["aarch64", "x86_64"])
        for ver, arch, _sha, url in gos:
            # CVE-2025-68121 is fixed in Go 1.24.13, 1.25.7 and 1.26.0-rc.3; the standard library rows of the 2026-10 scan are fixed in 1.27.3.
            self.assertGreaterEqual(version_tuple(ver), (1, 27, 3))
            self.assertTrue(url.startswith(f"https://go.dev/dl/go{ver}.linux-"), url)

    def test_the_build_script_checks_the_go_archive_and_leaves_no_vcs_stamp(self):
        text = read("images/tools/build-gitleaks.sh")
        self.assertIn("fetch_pinned", text)
        self.assertIn("-trimpath", text)
        self.assertIn("-buildvcs=false", text)
        self.assertIn("CGO_ENABLED=0", text)


if __name__ == "__main__":
    unittest.main()
