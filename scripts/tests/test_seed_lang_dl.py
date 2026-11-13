"""scripts/seed-lang-dl.sh puts the pinned Maven archive into the language tools download cache from Maven Central, so a build host does not depend on archive.apache.org.

On 2026-10-20 the download of apache-maven from archive.apache.org timed out from the build network in 4 of 4 full rounds (curl exit status 28, 3 to 1 shards a round). Maven Central,
dlcdn.apache.org and downloads.apache.org serve the same bytes (SHA-256 80ffca22...). The installer (images/tools/lang/install.sh, `fetch`) uses a cache file named like the URL's last
segment when its SHA-256 matches the lock, so a verified file in the cache is all it needs. The seed never makes a build fail: any failure leaves the cache as it was and the installer's own
download runs.
"""
import hashlib
import os
import stat
import subprocess  # nosec B404: runs the seed script with an argument list, never through a shell
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SEED = ROOT / "scripts" / "seed-lang-dl.sh"
BODY = b"pinned maven archive stand-in\n" * 40
GOOD = hashlib.sha256(BODY).hexdigest()
NAME = "apache-maven-3.9.16-bin.tar.gz"


def lock_text(sha: str) -> str:
    return (
        "# stand-in lock\n"
        f"golangci-lint 2.14.0 x86_64 {'a' * 64} https://example.invalid/golangci-lint.tar.gz\n"
        f"maven 3.9.16 any {sha} https://archive.apache.org/dist/maven/maven-3/3.9.16/binaries/{NAME}\n"
        f"gradle 9.8.0 any {'b' * 64} https://services.gradle.org/distributions/gradle-9.8.0-bin.zip\n"
    )


class SeedLangDlCase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(dir=os.environ.get("TMPDIR"))
        self.dir = Path(self._tmp.name)
        self.cache = self.dir / "lang-dl"
        self.bin = self.dir / "bin"
        self.bin.mkdir()
        self.calls = self.dir / "curl-calls.txt"

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def fake_curl(self, body: bytes | None, exit_status: int = 0) -> None:
        """A curl that logs its URL argument and writes `body` to the -o file, or fails with `exit_status`."""
        payload = self.dir / "payload.bin"
        if body is not None:
            payload.write_bytes(body)
        script = (
            "#!/usr/bin/env bash\n"
            'out=""; url=""\n'
            'while [ $# -gt 0 ]; do case "$1" in -o) out=$2; shift 2 ;; -*) shift ;; *) url=$1; shift ;; esac; done\n'
            f'echo "$url" >> "{self.calls}"\n'
            f'[ {exit_status} -eq 0 ] || exit {exit_status}\n'
            f'cp "{payload}" "$out"\n'
        )
        path = self.bin / "curl"
        path.write_text(script)
        path.chmod(path.stat().st_mode | stat.S_IXUSR)

    def run_seed(self, lock_sha: str = GOOD) -> subprocess.CompletedProcess:
        lock = self.dir / "lang.lock"
        lock.write_text(lock_text(lock_sha))
        env = {"PATH": f"{self.bin}:/usr/bin:/bin", "HOME": str(self.dir), "TMPDIR": str(self.dir)}
        return subprocess.run(["/bin/bash", str(SEED), str(lock), str(self.cache)], capture_output=True, text=True, env=env, timeout=60)  # nosec B603: argument list, no shell, the program is /bin/bash and the script is this repository own

    def urls(self) -> list:
        return self.calls.read_text().split() if self.calls.exists() else []

    def test_it_puts_the_verified_archive_into_the_cache_from_maven_central(self) -> None:
        self.fake_curl(BODY)
        done = self.run_seed()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual((self.cache / NAME).read_bytes(), BODY)
        self.assertEqual(self.urls()[0], f"https://repo.maven.apache.org/maven2/org/apache/maven/apache-maven/3.9.16/{NAME}")
        self.assertIn("SEED lang-dl maven 3.9.16 from repo.maven.apache.org", done.stdout)

    def test_it_does_nothing_when_the_cache_already_holds_the_pinned_file(self) -> None:
        self.cache.mkdir()
        (self.cache / NAME).write_bytes(BODY)
        self.fake_curl(BODY)
        done = self.run_seed()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.urls(), [], "no download when the cache file matches the lock")
        self.assertIn("SEED lang-dl maven 3.9.16 already cached", done.stdout)

    def test_a_download_with_the_wrong_sha256_never_takes_the_cache_name(self) -> None:
        self.fake_curl(b"an error page, not the archive\n")
        done = self.run_seed()
        self.assertEqual(done.returncode, 0, "a failed seed never fails the build")
        self.assertFalse((self.cache / NAME).exists(), "the pin is never relaxed")
        self.assertIn("SEED lang-dl maven 3.9.16 skipped: SHA-256 of the mirror copy differs", done.stdout)

    def test_a_failing_transfer_leaves_the_installer_to_do_its_own_download(self) -> None:
        self.fake_curl(None, exit_status=28)
        done = self.run_seed()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertFalse((self.cache / NAME).exists())
        self.assertIn("SEED lang-dl maven 3.9.16 skipped: transfer failed (curl exit status 28)", done.stdout)

    def test_a_stale_cache_file_is_replaced_by_a_verified_one(self) -> None:
        self.cache.mkdir()
        (self.cache / NAME).write_bytes(b"truncated")
        self.fake_curl(BODY)
        done = self.run_seed()
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual((self.cache / NAME).read_bytes(), BODY)

    def test_a_lock_without_a_maven_row_is_left_alone(self) -> None:
        self.fake_curl(BODY)
        lock = self.dir / "lang.lock"
        lock.write_text("gosec 2.29.0 x86_64 " + "c" * 64 + " https://example.invalid/gosec.tar.gz\n")
        env = {"PATH": f"{self.bin}:/usr/bin:/bin", "HOME": str(self.dir), "TMPDIR": str(self.dir)}
        done = subprocess.run(["/bin/bash", str(SEED), str(lock), str(self.cache)], capture_output=True, text=True, env=env, timeout=60)  # nosec B603: argument list, no shell, the program is /bin/bash and the script is this repository own
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertEqual(self.urls(), [])

    def test_the_gates_script_runs_the_seed_before_the_installer(self) -> None:
        gates = (ROOT / "scripts" / "gates-lang-tools.sh").read_text()
        self.assertIn('"$here/seed-lang-dl.sh" "$lang/lang.lock" "$tmp_root/lang-dl"', gates)
        self.assertLess(gates.index("seed-lang-dl.sh"), gates.index('bash "$lang/install.sh"'))

    def test_the_real_lock_names_maven_central_so_the_image_build_does_not_need_archive_apache_org(self) -> None:
        # The worker image build downloads Maven inside `docker build`, where the seed above does not run. On 2026-10-24 archive.apache.org timed out
        # from the CodeBuild network in two image builds in a row (curl exit status 28, four attempts, 925 s each), and the deploy waited for it.
        # Maven Central serves the same file (SHA-256 80ffca22...) and is the host of the lock's jacoco, findsecbugs and ktfmt rows.
        rows = [
            line.split()
            for line in (ROOT / "images" / "tools" / "lang" / "lang.lock").read_text().splitlines()
            if line.startswith("maven ")
        ]
        self.assertEqual(len(rows), 1, rows)
        _tool, version, arch, sha, url = rows[0]
        self.assertEqual((version, arch), ("3.9.16", "any"))
        self.assertEqual(sha, "80ffca22aed9e8b9713a232f3394fd81d7f20322df75efdb2b047dbd3e3a23bb")
        self.assertEqual(
            url,
            "https://repo.maven.apache.org/maven2/org/apache/maven/apache-maven/3.9.16/apache-maven-3.9.16-bin.tar.gz",
        )
        self.assertEqual(url.rsplit("/", 1)[1], NAME)


if __name__ == "__main__":
    unittest.main()
