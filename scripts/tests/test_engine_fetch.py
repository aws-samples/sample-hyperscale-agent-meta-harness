"""The adapters of scripts/engine-fetch: git-tag, git-path-bundle and release-asset.

No network. The git sources are local repositories made here (a tag, a patch series, a second repository for an input) and the release assets are
local files, so every pin can be wrong on purpose. The cases hold the exit-code classes of the module (moved, hash, transport, rebase, destination,
lint), the verification that comes before anything is read, the safe handling of archives, the retry of a download and the atomic destination.
"""

from __future__ import annotations

import contextlib
import hashlib
import io
import os
import stat
import subprocess  # nosec B404: the tests start git and the entry point with argument lists, never a shell
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from typing import Dict, List, Optional
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts" / "lib"))
import engine_fetch as ef  # noqa: E402
import engine_source as es  # noqa: E402

ENV = {**os.environ, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@x",
       "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@x", "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z", "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z"}


def git(cwd: Path, *args: str) -> str:
    p = subprocess.run(["git", *args], cwd=str(cwd), env=ENV, capture_output=True, text=True)  # nosec B603: argument list, no shell, program is git; B607: bare name of a standard tool looked up on PATH, as the code under test does
    assert p.returncode == 0, (args, p.stderr)
    return p.stdout.strip()


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


class Repo:
    """A local git repository with files, one commit, an annotated tag v1 and (optionally) a second commit that is a patch of the first."""

    def __init__(self, base: Path, name: str, files: Dict[str, bytes], tag: Optional[str] = "v1"):
        self.path = base / name
        self.path.mkdir(parents=True)
        git(self.path, "init", "-q", ".")
        git(self.path, "config", "uploadpack.allowAnySHA1InWant", "true")
        for rel, data in files.items():
            p = self.path / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(data)
        git(self.path, "add", ".")
        git(self.path, "commit", "-q", "-m", "one")
        self.commit = git(self.path, "rev-parse", "HEAD")
        self.tag = tag
        self.tag_object = ""
        if tag:
            git(self.path, "tag", "-a", "-m", "release", tag)
            self.tag_object = git(self.path, "rev-parse", tag)

    def blob(self, rel: str) -> str:
        return git(self.path, "rev-parse", "%s:%s" % (self.commit, rel))


def tag_source(name: str, repo: Repo, patches_lock: str = "") -> str:
    extra = 'patches_lock = "%s"\n' % patches_lock if patches_lock else ""
    return ('schema = "1"\nname = "%s"\nkind = "git-tag"\nlicense = "Apache-2.0"\nrecipe = "none"\n\n[source]\nurl = "%s"\ntag = "%s"\ntag_object = "%s"\ncommit = "%s"\n%s'
            % (name, repo.path, repo.tag, repo.tag_object, repo.commit, extra))


def bundle_source(name: str, repo: Repo, files: Dict[str, str], inputs: Optional[List[str]] = None, inputs_repo: Optional[Repo] = None) -> str:
    text = ('schema = "1"\nname = "%s"\nkind = "git-path-bundle"\nlicense = "Apache-2.0"\nrecipe = "none"\n\n[source]\nurl = "%s"\ncommit = "%s"\n'
            % (name, repo.path, repo.commit))
    for rel, dest in files.items():
        data = (repo.path / rel).read_bytes()
        text += '\n[[files]]\npath = "%s"\nblob = "%s"\nsha256 = "%s"\nas = "%s"\n' % (rel, repo.blob(rel), sha(data), dest)
    for rel in inputs or []:
        assert inputs_repo is not None
        data = (inputs_repo.path / rel).read_bytes()
        text += '\n[[inputs]]\nfrom = "up"\npath = "%s"\nblob = "%s"\nsha256 = "%s"\nas = "up/%s"\n' % (rel, inputs_repo.blob(rel), sha(data), rel)
    return text


def load(text: str, stem: str) -> es.Source:
    raw = es.parse(text)
    problems = es.validate(raw, stem, allow_local_url=True)
    assert not problems, problems
    return es.Source(Path(stem + ".toml"), raw)


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="ef-test-")
        self.addCleanup(self._tmp.cleanup)
        self.tmp = Path(self._tmp.name)
        self.root = self.tmp / "repo-root"
        self.root.mkdir()
        self.lines = []  # type: List[str]

    def fetcher(self, *sources: es.Source, **kw) -> "ef.Fetcher":
        return ef.Fetcher(self.root, {s.name: s for s in sources}, allow_local=True, log=self.lines.append, **kw)

    def assertFetchError(self, code: int, fn, *args, **kw) -> str:
        with self.assertRaises(ef.FetchError) as cm:
            fn(*args, **kw)
        self.assertEqual(cm.exception.code, code, cm.exception.message)
        return cm.exception.message


class GitTagCase(Base):
    def setUp(self) -> None:
        super().setUp()
        self.repo = Repo(self.tmp, "up", {"README": b"hello\n", "src/main.txt": b"main\n"})

    def test_a_tag_is_fetched_checked_out_and_verified(self) -> None:
        src = load(tag_source("eng", self.repo), "eng")
        info = self.fetcher(src).fetch("eng", self.tmp / "out")
        self.assertEqual(info["commit"], self.repo.commit)
        self.assertEqual((self.tmp / "out" / "src" / "main.txt").read_bytes(), b"main\n")
        self.assertEqual(git(self.tmp / "out", "rev-parse", "HEAD"), self.repo.commit)

    def test_a_moved_tag_is_refused_before_anything_is_checked_out(self) -> None:
        src = load(tag_source("eng", self.repo), "eng")
        (self.repo.path / "README").write_bytes(b"changed\n")
        git(self.repo.path, "commit", "-q", "-am", "two")
        git(self.repo.path, "tag", "-f", "-a", "-m", "moved", "v1")
        msg = self.assertFetchError(ef.MOVED, self.fetcher(src).fetch, "eng", self.tmp / "out")
        self.assertIn("v1", msg)
        self.assertFalse((self.tmp / "out").exists(), "a failed fetch leaves the destination as it was")

    def test_a_wrong_pinned_commit_with_the_right_tag_object_is_moved_too(self) -> None:
        text = tag_source("eng", self.repo).replace(self.repo.commit, "f" * 40)
        self.assertFetchError(ef.MOVED, self.fetcher(load(text, "eng")).fetch, "eng", self.tmp / "out")

    def test_an_unreachable_repository_is_a_transport_error(self) -> None:
        text = tag_source("eng", self.repo).replace(str(self.repo.path), str(self.tmp / "nowhere"))
        self.assertFetchError(ef.TRANSPORT, self.fetcher(load(text, "eng")).fetch, "eng", self.tmp / "out")

    def test_a_missing_tag_is_a_transport_error(self) -> None:
        text = tag_source("eng", self.repo).replace('tag = "v1"', 'tag = "v9"')
        self.assertFetchError(ef.TRANSPORT, self.fetcher(load(text, "eng")).fetch, "eng", self.tmp / "out")

    def test_a_destination_that_is_not_empty_is_refused_and_an_empty_one_is_used(self) -> None:
        src = load(tag_source("eng", self.repo), "eng")
        (self.tmp / "full").mkdir()
        (self.tmp / "full" / "x").write_text("x")
        self.assertFetchError(ef.DEST, self.fetcher(src).fetch, "eng", self.tmp / "full")
        (self.tmp / "empty").mkdir()
        self.fetcher(src).fetch("eng", self.tmp / "empty")
        self.assertTrue((self.tmp / "empty" / "README").is_file())

    def test_a_destination_and_files_with_non_ascii_and_spaced_names_work(self) -> None:
        repo = Repo(self.tmp, "up-unicode", {"README": b"hello\n", "src/fïle ñame.txt": b"x\n", "dïr with space/日本語.txt": b"y\n"})
        src = load(tag_source("eng", repo), "eng")
        dest = self.tmp / "dëst dir" / "ñew"
        info = self.fetcher(src).fetch("eng", dest)
        self.assertEqual(info["commit"], repo.commit)
        self.assertEqual((dest / "src" / "fïle ñame.txt").read_bytes(), b"x\n")
        self.assertEqual((dest / "dïr with space" / "日本語.txt").read_bytes(), b"y\n")
        self.assertEqual(self.fetcher(src).verify_fetched("eng", dest)["head"], repo.commit)

    def test_a_remote_url_that_is_not_https_is_refused_without_the_test_hook(self) -> None:
        src = load(tag_source("eng", self.repo), "eng")
        strict = ef.Fetcher(self.root, {"eng": src}, allow_local=False)
        self.assertFetchError(ef.LINT, strict.fetch, "eng", self.tmp / "out")

    def test_verify_accepts_the_checkout_and_names_a_commit_that_is_not_the_pin(self) -> None:
        src = load(tag_source("eng", self.repo), "eng")
        f = self.fetcher(src)
        f.fetch("eng", self.tmp / "out")
        self.assertEqual(f.verify_fetched("eng", self.tmp / "out")["head"], self.repo.commit)
        other = Repo(self.tmp, "other", {"x.txt": b"x\n"}, tag=None)
        self.assertFetchError(ef.MOVED, f.verify_fetched, "eng", other.path)

    def test_verify_of_a_missing_directory_and_of_one_that_is_no_checkout_is_an_output_error(self) -> None:
        f = self.fetcher(load(tag_source("eng", self.repo), "eng"))
        self.assertFetchError(ef.OUTPUT, f.verify_fetched, "eng", self.tmp / "nowhere")
        (self.tmp / "plain").mkdir()
        self.assertFetchError(ef.OUTPUT, f.verify_fetched, "eng", self.tmp / "plain")


class SeriesCase(Base):
    """The patch series of `patches_lock`: file hashes, test-only patches, the tree."""

    def setUp(self) -> None:
        super().setUp()
        self.repo = Repo(self.tmp, "up", {"a.txt": b"one\n", "b.txt": b"b\n"})
        # a second clone makes two commits (a test-only commit then the fix) and exports them as patches
        work = self.tmp / "work"
        git(self.tmp, "clone", "-q", str(self.repo.path), str(work))
        (work / "a.txt").write_bytes(b"one\ntwo test\n")
        git(work, "commit", "-q", "-am", "test: a failing test")
        self.tdd_commit = git(work, "rev-parse", "HEAD")
        self.tdd_tree = git(work, "rev-parse", "HEAD^{tree}")
        git(work, "reset", "-q", "--hard", "HEAD~1")
        (work / "a.txt").write_bytes(b"one\ntwo\n")
        git(work, "commit", "-q", "-am", "fix: make a.txt right")
        self.tree = git(work, "rev-parse", "HEAD^{tree}")
        self.patches = self.root / "engine" / "x" / "patches"
        (self.patches / "tdd").mkdir(parents=True)
        out = git(work, "format-patch", "-1", "--stdout", "HEAD")
        self.patch_name = "0001-fix-make-a-txt-right.patch"
        (self.patches / self.patch_name).write_text(out + "\n")
        # the test-only patch: made on the tag commit, applied before patch 1
        git(work, "checkout", "-q", "-b", "side", self.repo.commit)
        (work / "a.txt").write_bytes(b"one\ntwo test\n")
        git(work, "commit", "-q", "-am", "test: a failing test")
        self.tdd_tree = git(work, "rev-parse", "HEAD^{tree}")
        (self.patches / "tdd" / "0001-test-only.patch").write_text(git(work, "format-patch", "-1", "--stdout", "HEAD") + "\n")
        self.write_lock()

    def write_lock(self, tree: Optional[str] = None, patch_sha: Optional[str] = None, tdd_tree: Optional[str] = None) -> None:
        p = self.patches / self.patch_name
        t = self.patches / "tdd" / "0001-test-only.patch"
        (self.root / "engine" / "x" / "lock.toml").write_text(
            'schema = "1"\n\n[series]\ntree = "%s"\n\n[[patches]]\nfile = "%s"\nsha256 = "%s"\n\n[[tdd]]\nfile = "tdd/0001-test-only.patch"\nsha256 = "%s"\ncommit = "%s"\nparent = "%s"\ntree = "%s"\n'
            % (tree or self.tree, self.patch_name, patch_sha or sha(p.read_bytes()), sha(t.read_bytes()), self.tdd_commit, self.repo.commit, tdd_tree or self.tdd_tree))

    def source(self) -> es.Source:
        return load(tag_source("eng", self.repo, "engine/x/lock.toml"), "eng")

    def test_the_series_is_applied_and_its_tree_checked(self) -> None:
        info = self.fetcher(self.source()).fetch("eng", self.tmp / "out")
        self.assertEqual(info["series_tree"], self.tree)
        self.assertEqual(info["patches"], 1)
        self.assertEqual((self.tmp / "out" / "a.txt").read_bytes(), b"one\ntwo\n")

    def test_the_patches_can_be_left_out(self) -> None:
        info = self.fetcher(self.source()).fetch("eng", self.tmp / "out", patches=False)
        self.assertNotIn("series_tree", info)
        self.assertEqual((self.tmp / "out" / "a.txt").read_bytes(), b"one\n")

    def test_a_patch_that_differs_from_its_hash_is_refused(self) -> None:
        self.write_lock(patch_sha="0" * 64)
        msg = self.assertFetchError(ef.HASH, self.fetcher(self.source()).fetch, "eng", self.tmp / "out")
        self.assertIn(self.patch_name, msg)

    def test_a_series_that_gives_another_tree_is_refused(self) -> None:
        self.write_lock(tree="a" * 40)
        self.assertFetchError(ef.HASH, self.fetcher(self.source()).fetch, "eng", self.tmp / "out")

    def test_a_test_only_patch_that_gives_another_tree_is_refused(self) -> None:
        self.write_lock(tdd_tree="b" * 40)
        msg = self.assertFetchError(ef.HASH, self.fetcher(self.source()).fetch, "eng", self.tmp / "out")
        self.assertIn("test-only", msg)

    def test_a_patch_that_no_longer_applies_asks_for_a_rebase(self) -> None:
        # upstream moved the line the patch changes; the tag is the same, so the patch is applied to other text
        p = self.patches / self.patch_name
        text = p.read_text().replace("\n one\n", "\n zzz\n")
        p.write_text(text)
        self.write_lock()
        self.assertFetchError(ef.REBASE, self.fetcher(self.source()).fetch, "eng", self.tmp / "out")

    def test_a_missing_patch_file_is_a_hash_error(self) -> None:
        (self.patches / self.patch_name).rename(self.patches / "elsewhere.patch")
        self.assertFetchError(ef.HASH, self.fetcher(self.source()).fetch, "eng", self.tmp / "out")

    # ---- verify: a patched checkout is held to the series tree and to the pinned base commit ----

    def fetched(self, patches: bool = True) -> Path:
        out = self.tmp / "out"
        self.fetcher(self.source()).fetch("eng", out, patches=patches)
        return out

    def test_verify_accepts_what_fetch_made_and_reports_the_series(self) -> None:
        info = self.fetcher(self.source()).verify_fetched("eng", self.fetched())
        self.assertEqual((info["series_tree"], info["patches"], info["head"] != self.repo.commit), (self.tree, 1, True))

    def test_verify_of_an_unrelated_git_tree_is_a_hash_error(self) -> None:
        other = Repo(self.tmp, "other", {"x.txt": b"x\n"}, tag=None)
        msg = self.assertFetchError(ef.HASH, self.fetcher(self.source()).verify_fetched, "eng", other.path)
        self.assertIn("series", msg)

    def test_verify_of_a_series_on_another_base_commit_is_a_moved_error(self) -> None:
        out = self.fetched()
        moved = load(tag_source("eng", self.repo, "engine/x/lock.toml").replace(self.repo.commit, "f" * 40), "eng")
        self.assertFetchError(ef.MOVED, self.fetcher(moved).verify_fetched, "eng", out)

    def test_verify_of_a_checkout_without_the_series_fails_unless_the_patches_were_left_out_on_purpose(self) -> None:
        f = self.fetcher(self.source())
        plain = self.fetched(patches=False)
        self.assertFetchError(ef.HASH, f.verify_fetched, "eng", plain)
        self.assertEqual(f.verify_fetched("eng", plain, patches=False)["head"], self.repo.commit)
        patched = self.tmp / "patched"
        f.fetch("eng", patched)
        self.assertFetchError(ef.MOVED, f.verify_fetched, "eng", patched, patches=False)

    def test_verify_rejects_a_changed_file_an_extra_file_and_a_subdirectory_of_a_checkout(self) -> None:
        f = self.fetcher(self.source())
        out = self.fetched()
        (out / "a.txt").write_bytes(b"changed\n")
        self.assertFetchError(ef.OUTPUT, f.verify_fetched, "eng", out)
        git(out, "checkout", "--", "a.txt")
        self.assertEqual(f.verify_fetched("eng", out)["patches"], 1)
        (out / "extra.txt").write_text("x")
        self.assertFetchError(ef.OUTPUT, f.verify_fetched, "eng", out)
        (out / "extra.txt").unlink()
        (out / "sub").mkdir()
        self.assertFetchError(ef.OUTPUT, f.verify_fetched, "eng", out / "sub")

    def test_the_command_line_fails_when_verify_is_pointed_at_an_unrelated_checkout(self) -> None:
        sources = self.tmp / "sources"
        sources.mkdir()
        (sources / "eng.toml").write_text(tag_source("eng", self.repo, "engine/x/lock.toml"))
        other = Repo(self.tmp, "other", {"x.txt": b"x\n"}, tag=None)
        err = io.StringIO()
        with contextlib.redirect_stderr(err), contextlib.redirect_stdout(io.StringIO()):
            old = os.environ.get("ENGINE_FETCH_ALLOW_LOCAL")
            os.environ["ENGINE_FETCH_ALLOW_LOCAL"] = "1"
            try:
                rc = ef.main(["--root", str(self.root), "--sources", str(sources), "verify", "eng", "--dir", str(other.path)])
            finally:
                if old is None:
                    del os.environ["ENGINE_FETCH_ALLOW_LOCAL"]
                else:
                    os.environ["ENGINE_FETCH_ALLOW_LOCAL"] = old
        self.assertEqual(rc, ef.HASH, err.getvalue())
        self.assertIn("ENGINE-FETCH FAIL hash", err.getvalue())


class PathBundleCase(Base):
    def setUp(self) -> None:
        super().setUp()
        self.plugin = Repo(self.tmp, "plugins", {"plugins/goal/index.ts": b"export const x = 1;\n", "LICENSE": b"Apache text\n", "other.txt": b"o\n"}, tag=None)
        self.cline = Repo(self.tmp, "cline", {"sdk/create.ts": b"export function createTool() {}\n"})

    def sources(self, **kw) -> List[es.Source]:
        text = bundle_source("goal", self.plugin, {"plugins/goal/index.ts": "goal-index.ts", "LICENSE": "LICENSE"}, ["sdk/create.ts"], self.cline)
        return [load(text, "goal"), load(tag_source("up", self.cline), "up")]

    def test_the_pinned_paths_land_under_their_layout_names_and_an_input_comes_from_the_other_source(self) -> None:
        info = self.fetcher(*self.sources()).fetch("goal", self.tmp / "out")
        self.assertEqual((self.tmp / "out" / "goal-index.ts").read_bytes(), b"export const x = 1;\n")
        self.assertEqual((self.tmp / "out" / "LICENSE").read_bytes(), b"Apache text\n")
        self.assertEqual((self.tmp / "out" / "up" / "sdk" / "create.ts").read_bytes(), b"export function createTool() {}\n")
        self.assertEqual(sorted(info["files"]), ["LICENSE", "goal-index.ts", "up/sdk/create.ts"])
        self.assertFalse((self.tmp / "out" / "other.txt").exists(), "only the pinned paths are written")

    def test_a_blob_that_differs_from_its_pin_is_refused(self) -> None:
        text = bundle_source("goal", self.plugin, {"plugins/goal/index.ts": "goal-index.ts"})
        text = text.replace(self.plugin.blob("plugins/goal/index.ts"), "c" * 40)
        msg = self.assertFetchError(ef.HASH, self.fetcher(load(text, "goal")).fetch, "goal", self.tmp / "out")
        self.assertIn("blob", msg)
        self.assertFalse((self.tmp / "out").exists())

    def test_a_sha256_that_differs_from_its_pin_is_refused_even_with_the_right_blob(self) -> None:
        text = bundle_source("goal", self.plugin, {"LICENSE": "LICENSE"}).replace(sha(b"Apache text\n"), "d" * 64)
        msg = self.assertFetchError(ef.HASH, self.fetcher(load(text, "goal")).fetch, "goal", self.tmp / "out")
        self.assertIn("sha256", msg)

    def test_a_commit_that_is_not_in_the_repository_is_a_transport_error(self) -> None:
        text = bundle_source("goal", self.plugin, {"LICENSE": "LICENSE"}).replace(self.plugin.commit, "e" * 40)
        self.assertFetchError(ef.TRANSPORT, self.fetcher(load(text, "goal")).fetch, "goal", self.tmp / "out")

    def test_a_path_that_is_not_in_the_commit_is_refused(self) -> None:
        text = bundle_source("goal", self.plugin, {"LICENSE": "LICENSE"}).replace('path = "LICENSE"', 'path = "NOPE"')
        self.assertFetchError(ef.HASH, self.fetcher(load(text, "goal")).fetch, "goal", self.tmp / "out")

    def test_an_input_that_names_no_git_tag_source_is_a_lint_error(self) -> None:
        text = bundle_source("goal", self.plugin, {"LICENSE": "LICENSE"}, ["sdk/create.ts"], self.cline)
        self.assertFetchError(ef.LINT, self.fetcher(load(text, "goal")).fetch, "goal", self.tmp / "out")

    def test_verify_rehashes_the_files_and_rejects_extras(self) -> None:
        f = self.fetcher(*self.sources())
        f.fetch("goal", self.tmp / "out")
        self.assertEqual(f.verify_fetched("goal", self.tmp / "out")["files"], 3)
        (self.tmp / "out" / "LICENSE").write_bytes(b"changed\n")
        self.assertFetchError(ef.HASH, f.verify_fetched, "goal", self.tmp / "out")
        (self.tmp / "out" / "LICENSE").write_bytes(b"Apache text\n")
        (self.tmp / "out" / "extra.js").write_bytes(b"x")
        self.assertFetchError(ef.OUTPUT, f.verify_fetched, "goal", self.tmp / "out")


def tar_bytes(members: Dict[str, bytes], links: Optional[Dict[str, str]] = None, mode: int = 0o755) -> bytes:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        for name, data in members.items():
            ti = tarfile.TarInfo(name)
            ti.size = len(data)
            ti.mode = mode
            tf.addfile(ti, io.BytesIO(data))
        for name, target in (links or {}).items():
            ti = tarfile.TarInfo(name)
            ti.type = tarfile.SYMTYPE
            ti.linkname = target
            tf.addfile(ti)
    return buf.getvalue()


def asset_source(name: str, archive: Path, member: Optional[str] = None, member_data: Optional[bytes] = None, platform: str = "linux-x64", sha_override: Optional[str] = None) -> es.Source:
    text = 'schema = "1"\nname = "%s"\nkind = "release-asset"\nlicense = "MIT"\nrecipe = "none"\n\n[assets.%s]\nurl = "%s"\nsha256 = "%s"\n' % (name, platform, archive, sha_override or sha(archive.read_bytes()))
    if member:
        text += 'member = "%s"\nmember_sha256 = "%s"\n' % (member, sha(member_data or b""))
    return load(text, name)


class ReleaseAssetCase(Base):
    def test_an_archive_member_is_read_by_name_checked_and_written_with_its_mode(self) -> None:
        binary = b"\x7fELF fake binary\n"
        arc = self.tmp / "pkg.tgz"
        arc.write_bytes(tar_bytes({"package/bin/esbuild": binary, "package/README.md": b"r\n"}))
        info = self.fetcher(asset_source("esb", arc, "package/bin/esbuild", binary)).fetch("esb", self.tmp / "out", "linux-x64")
        self.assertEqual(info["sha256"], sha(binary))
        out = self.tmp / "out" / "esbuild"
        self.assertEqual(out.read_bytes(), binary)
        self.assertTrue(os.access(str(out), os.X_OK))
        self.assertEqual([p.name for p in (self.tmp / "out").iterdir()], ["esbuild"], "nothing else of the archive is extracted")

    def test_an_archive_with_another_sha256_is_refused_before_it_is_read(self) -> None:
        arc = self.tmp / "pkg.tgz"
        arc.write_bytes(tar_bytes({"package/bin/esbuild": b"x"}))
        src = asset_source("esb", arc, "package/bin/esbuild", b"x", sha_override="0" * 64)
        msg = self.assertFetchError(ef.HASH, self.fetcher(src).fetch, "esb", self.tmp / "out", "linux-x64")
        self.assertIn("sha256", msg)
        self.assertFalse((self.tmp / "out").exists())

    def test_a_member_with_another_sha256_is_refused(self) -> None:
        arc = self.tmp / "pkg.tgz"
        arc.write_bytes(tar_bytes({"package/bin/esbuild": b"real"}))
        src = asset_source("esb", arc, "package/bin/esbuild", b"other")
        self.assertFetchError(ef.HASH, self.fetcher(src).fetch, "esb", self.tmp / "out", "linux-x64")

    def test_a_missing_member_a_link_and_a_repeated_name_are_refused(self) -> None:
        arc = self.tmp / "a.tgz"
        arc.write_bytes(tar_bytes({"package/other": b"x"}))
        self.assertFetchError(ef.HASH, self.fetcher(asset_source("esb", arc, "package/bin/esbuild", b"x")).fetch, "esb", self.tmp / "o1", "linux-x64")
        link = self.tmp / "l.tgz"
        link.write_bytes(tar_bytes({}, links={"package/bin/esbuild": "/bin/sh"}))
        self.assertFetchError(ef.HASH, self.fetcher(asset_source("esb", link, "package/bin/esbuild", b"x")).fetch, "esb", self.tmp / "o2", "linux-x64")
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz") as tf:
            for data in (b"first", b"second"):
                ti = tarfile.TarInfo("package/bin/esbuild")
                ti.size = len(data)
                tf.addfile(ti, io.BytesIO(data))
        dup = self.tmp / "d.tgz"
        dup.write_bytes(buf.getvalue())
        self.assertFetchError(ef.HASH, self.fetcher(asset_source("esb", dup, "package/bin/esbuild", b"first")).fetch, "esb", self.tmp / "o3", "linux-x64")

    def test_an_empty_archive_and_a_zero_byte_file_are_hash_errors_not_crashes(self) -> None:
        empty_tar = self.tmp / "empty.tgz"
        empty_tar.write_bytes(tar_bytes({}))
        self.assertFetchError(ef.HASH, self.fetcher(asset_source("esb", empty_tar, "package/bin/esbuild", b"x")).fetch, "esb", self.tmp / "o1", "linux-x64")
        zero = self.tmp / "zero.tgz"
        zero.write_bytes(b"")
        self.assertFetchError(ef.HASH, self.fetcher(asset_source("esb", zero, "package/bin/esbuild", b"x")).fetch, "esb", self.tmp / "o2", "linux-x64")
        empty_zip = self.tmp / "empty.zip"
        with zipfile.ZipFile(str(empty_zip), "w"):
            pass
        self.assertFetchError(ef.HASH, self.fetcher(asset_source("esb", empty_zip, "package/bin/esbuild", b"x")).fetch, "esb", self.tmp / "o3", "linux-x64")
        for name in ("o1", "o2", "o3"):
            self.assertFalse((self.tmp / name).exists(), "nothing is published")

    def test_a_zip_member_is_read_and_a_zip_link_is_refused(self) -> None:
        z = self.tmp / "p.zip"
        with zipfile.ZipFile(str(z), "w") as zf:
            zf.writestr("bun/bun", b"bun binary")
            info = zipfile.ZipInfo("bun/link")
            info.create_system = 3
            info.external_attr = (0o120777 << 16)
            zf.writestr(info, "/bin/sh")
        info = self.fetcher(asset_source("bun", z, "bun/bun", b"bun binary")).fetch("bun", self.tmp / "out", "linux-x64")
        self.assertEqual(info["sha256"], sha(b"bun binary"))
        self.assertFetchError(ef.HASH, self.fetcher(asset_source("bun", z, "bun/link", b"/bin/sh")).fetch, "bun", self.tmp / "out2", "linux-x64")

    def test_a_plain_file_asset_is_kept_under_its_name(self) -> None:
        f = self.tmp / "tool-1.0.bin"
        f.write_bytes(b"plain tool")
        src = asset_source("tool", f)
        info = self.fetcher(src).fetch("tool", self.tmp / "out", "linux-x64")
        self.assertEqual((self.tmp / "out" / "tool-1.0.bin").read_bytes(), b"plain tool")
        self.assertEqual(info["sha256"], sha(b"plain tool"))

    def test_a_platform_without_an_asset_is_a_lint_error_and_any_is_the_fallback(self) -> None:
        f = self.tmp / "t.bin"
        f.write_bytes(b"t")
        self.assertFetchError(ef.LINT, self.fetcher(asset_source("tool", f, platform="linux-arm64")).fetch, "tool", self.tmp / "o", "linux-x64")
        self.fetcher(asset_source("tool", f, platform="any")).fetch("tool", self.tmp / "o2", "linux-x64")
        self.assertTrue((self.tmp / "o2" / "t.bin").is_file())

    def test_an_unknown_source_is_a_usage_error(self) -> None:
        self.assertFetchError(ef.USAGE, self.fetcher().fetch, "nothing", self.tmp / "o")

    def test_verify_checks_the_one_file_and_a_platform_without_an_asset_is_a_lint_error_not_a_hash_error(self) -> None:
        binary = b"\x7fELF fake binary\n"
        arc = self.tmp / "pkg.tgz"
        arc.write_bytes(tar_bytes({"package/bin/esbuild": binary}))
        f = self.fetcher(asset_source("esb", arc, "package/bin/esbuild", binary))
        f.fetch("esb", self.tmp / "out", "linux-x64")
        self.assertEqual(f.verify_fetched("esb", self.tmp / "out", "linux-x64")["files"], 1)
        (self.tmp / "out" / "esbuild").write_bytes(b"other")
        self.assertFetchError(ef.HASH, f.verify_fetched, "esb", self.tmp / "out", "linux-x64")
        (self.tmp / "empty").mkdir()
        self.assertFetchError(ef.HASH, f.verify_fetched, "esb", self.tmp / "empty", "linux-x64")
        msg = self.assertFetchError(ef.LINT, f.verify_fetched, "esb", self.tmp / "empty", "linux-arm64")
        self.assertIn("linux-arm64", msg)
        self.assertNotIn("None", msg)
        self.assertFetchError(ef.OUTPUT, f.verify_fetched, "esb", self.tmp / "nowhere", "linux-x64")


class DownloadCase(Base):
    class Resp:
        def __init__(self, data: bytes, headers: Optional[Dict[str, str]] = None):
            self.buf = io.BytesIO(data)
            self.headers = headers or {}

        def read(self, n: int = -1) -> bytes:
            return self.buf.read(n)

        def __enter__(self):
            return self

        def __exit__(self, *a) -> None:
            return None

    def test_a_transient_failure_is_retried_with_a_growing_pause_then_succeeds(self) -> None:
        import urllib.error

        calls = []
        pauses = []

        def opener(url, timeout=0):
            calls.append(url)
            if len(calls) < 3:
                raise urllib.error.URLError("temporary")
            return self.Resp(b"payload")

        size = ef.download("https://example.invalid/a", self.tmp / "a", sha(b"payload"), opener=opener, sleep=pauses.append)
        self.assertEqual((size, len(calls), pauses), (7, 3, [1, 2]))

    def test_a_server_error_is_retried_and_a_not_found_is_not(self) -> None:
        import urllib.error

        n = []

        def not_found(url, timeout=0):
            n.append(1)
            raise urllib.error.HTTPError(url, 404, "nf", {}, None)

        self.assertFetchError(ef.TRANSPORT, ef.download, "https://example.invalid/a", self.tmp / "a", "0" * 64, opener=not_found, sleep=lambda s: None)
        self.assertEqual(len(n), 1)
        m = []

        def boom(url, timeout=0):
            m.append(1)
            raise urllib.error.HTTPError(url, 503, "busy", {}, None)

        self.assertFetchError(ef.TRANSPORT, ef.download, "https://example.invalid/a", self.tmp / "b", "0" * 64, opener=boom, sleep=lambda s: None)
        self.assertEqual(len(m), 3)

    def test_a_hash_mismatch_is_final_not_retried(self) -> None:
        n = []

        def opener(url, timeout=0):
            n.append(1)
            return self.Resp(b"payload")

        self.assertFetchError(ef.HASH, ef.download, "https://example.invalid/a", self.tmp / "a", "0" * 64, opener=opener, sleep=lambda s: None)
        self.assertEqual(len(n), 1)

    # A cut or malformed answer is a failure of the connection: it is retried, then it ends as a transport error (exit 3), and it never blames the pin.

    def retries_then_fails_as_transport(self, opener, want: str = "0" * 64) -> str:
        calls = []
        pauses = []

        def counting(url, timeout=0):
            calls.append(url)
            return opener(url, timeout)

        dest = self.tmp / "a"
        msg = self.assertFetchError(ef.TRANSPORT, ef.download, "https://example.invalid/a", dest, want, opener=counting, sleep=pauses.append)
        self.assertEqual((len(calls), pauses), (3, [1, 2]), "three attempts with a growing pause")
        self.assertFalse(dest.exists(), "a failed download leaves no partial file")
        return msg

    def test_a_chunked_body_cut_before_its_last_chunk_is_retried_and_ends_as_a_transport_error(self) -> None:
        import http.client

        class Cut(self.Resp):
            def read(self, n: int = -1) -> bytes:
                data = super().read(n)
                if not data:
                    raise http.client.IncompleteRead(b"")
                return data

        msg = self.retries_then_fails_as_transport(lambda url, timeout: Cut(b"partial"))
        self.assertIn("download of", msg)

    def test_a_status_line_that_is_not_http_is_retried_and_ends_as_a_transport_error(self) -> None:
        import http.client

        def garbage(url, timeout):
            raise http.client.BadStatusLine("garbage")

        self.retries_then_fails_as_transport(garbage)

    def test_a_body_shorter_than_its_content_length_is_a_transport_error_not_a_wrong_pin(self) -> None:
        msg = self.retries_then_fails_as_transport(lambda url, timeout: self.Resp(b"payload", {"Content-Length": "100"}), want=sha(b"the-whole-payload"))
        self.assertNotIn("pins", msg, "a flaky connection is not blamed on the source file")

    def test_a_short_read_is_tried_again_and_the_second_try_can_succeed(self) -> None:
        calls = []
        pauses = []

        def opener(url, timeout=0):
            calls.append(url)
            return self.Resp(b"pay" if len(calls) == 1 else b"payload", {"Content-Length": "7"})

        size = ef.download("https://example.invalid/a", self.tmp / "a", sha(b"payload"), opener=opener, sleep=pauses.append)
        self.assertEqual((size, len(calls), pauses), (7, 2, [1]))

    def test_a_complete_body_with_a_wrong_hash_is_still_a_hash_error_when_the_length_is_right(self) -> None:
        n = []

        def opener(url, timeout=0):
            n.append(1)
            return self.Resp(b"payload", {"Content-Length": "7"})

        self.assertFetchError(ef.HASH, ef.download, "https://example.invalid/a", self.tmp / "a", "0" * 64, opener=opener, sleep=lambda s: None)
        self.assertEqual(len(n), 1)

    def test_a_content_length_that_is_not_a_number_is_ignored(self) -> None:
        def opener(url, timeout=0):
            return self.Resp(b"payload", {"Content-Length": "seven"})

        self.assertEqual(ef.download("https://example.invalid/a", self.tmp / "a", sha(b"payload"), opener=opener, sleep=lambda s: None), 7)

    def test_a_zero_byte_answer_is_a_hash_error_for_a_pin_that_names_content(self) -> None:
        n = []

        def opener(url, timeout=0):
            n.append(1)
            return self.Resp(b"", {"Content-Length": "0"})

        msg = self.assertFetchError(ef.HASH, ef.download, "https://example.invalid/a", self.tmp / "a", sha(b"real content"), opener=opener, sleep=lambda s: None)
        self.assertIn("pins", msg)
        self.assertEqual(len(n), 1)
        self.assertFalse((self.tmp / "a").exists())

    def test_only_https_is_fetched(self) -> None:
        self.assertFetchError(ef.LINT, ef.download, "http://example.invalid/a", self.tmp / "a", "0" * 64)
        self.assertFetchError(ef.LINT, ef.download, "ftp://example.invalid/a", self.tmp / "a", "0" * 64)
        self.assertFetchError(ef.LINT, ef.download, "file:///etc/hosts", self.tmp / "a", "0" * 64)


class RefCase(Base):
    def test_head_and_a_tag_resolve_to_commits_and_an_annotated_tag_is_peeled(self) -> None:
        repo = Repo(self.tmp, "up", {"a": b"a\n"})
        self.assertEqual(ef.resolve_ref(str(repo.path), "HEAD", allow_local=True), repo.commit)
        self.assertEqual(ef.resolve_ref(str(repo.path), "v1", allow_local=True), repo.commit)
        self.assertNotEqual(repo.tag_object, repo.commit)
        self.assertFetchError(ef.TRANSPORT, ef.resolve_ref, str(repo.path), "nope", allow_local=True)
        self.assertFetchError(ef.LINT, ef.resolve_ref, str(repo.path), "HEAD")


class DestinationCase(Base):
    def test_staged_keeps_the_destination_until_the_block_ends(self) -> None:
        dest = self.tmp / "d"
        with self.assertRaises(RuntimeError):
            with ef.staged(dest) as out:
                (out / "f").write_text("x")
                raise RuntimeError("boom")
        self.assertFalse(dest.exists())
        self.assertEqual([p.name for p in self.tmp.iterdir() if p.name.startswith(".d.partial")], [], "the partial directory is gone")
        with ef.staged(dest) as out:
            (out / "f").write_text("x")
        self.assertEqual((dest / "f").read_text(), "x")

    def test_the_destination_has_the_mode_of_a_new_directory_under_the_umask_not_the_private_mode_of_the_scratch_directory(self) -> None:
        for mask in (0o022, 0o077, 0o002):
            with self.subTest(umask=oct(mask)):
                old = os.umask(mask)
                try:
                    dest = self.tmp / ("d%o" % mask)
                    with ef.staged(dest) as out:
                        (out / "f").write_text("x")
                    mode = stat.S_IMODE(dest.stat().st_mode)
                finally:
                    os.umask(old)
                self.assertEqual(mode, 0o777 & ~mask)


class FailureClassCase(Base):
    """A failure that is not one of the classes of the table is one line and a defined exit code, never a traceback with exit 1 (review of 2026-10-23)."""

    def sources_dir(self, *, text: Optional[str] = None, raw: Optional[bytes] = None, name: str = "eng") -> Path:
        d = self.tmp / "sources"
        d.mkdir(exist_ok=True)
        if raw is not None:
            (d / (name + ".toml")).write_bytes(raw)
        else:
            repo = Repo(self.tmp, "up", {"README": b"hello\n"})
            (d / (name + ".toml")).write_text(text or tag_source(name, repo))
        return d

    def cli(self, *argv: str, local: bool = True):
        """ef.main in this process; returns (exit code, stdout, stderr)."""
        out, err = io.StringIO(), io.StringIO()
        old = os.environ.get("ENGINE_FETCH_ALLOW_LOCAL")
        if local:
            os.environ["ENGINE_FETCH_ALLOW_LOCAL"] = "1"
        try:
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                rc = ef.main(list(argv))
        finally:
            if old is None:
                os.environ.pop("ENGINE_FETCH_ALLOW_LOCAL", None)
            else:
                os.environ["ENGINE_FETCH_ALLOW_LOCAL"] = old
        return rc, out.getvalue(), err.getvalue()

    def test_git_that_is_not_installed_is_a_transport_error_not_a_traceback(self) -> None:
        repo = Repo(self.tmp, "up", {"README": b"hello\n"})
        src = load(tag_source("eng", repo), "eng")
        f = ef.Fetcher(self.root, {"eng": src}, allow_local=True, git_bin="no-such-git-for-this-test")
        msg = self.assertFetchError(ef.TRANSPORT, f.fetch, "eng", self.tmp / "out")
        self.assertIn("git", msg)
        self.assertIn("not", msg)
        self.assertFetchError(ef.TRANSPORT, ef.resolve_ref, str(repo.path), "HEAD", git_bin="no-such-git-for-this-test", allow_local=True)
        self.assertFalse((self.tmp / "out").exists())

    def test_the_command_line_without_git_on_the_path_ends_in_one_line_and_exit_3(self) -> None:
        d = self.sources_dir()
        empty = self.tmp / "empty-path"
        empty.mkdir()
        env = {**os.environ, "PATH": str(empty), "ENGINE_FETCH_ALLOW_LOCAL": "1"}
        out = subprocess.run([sys.executable, "-I", str(ROOT / "scripts" / "lib" / "engine_fetch.py"), "--root", str(self.root), "--sources", str(d),
                              "fetch", "eng", "--dest", str(self.tmp / "out")], env=env, capture_output=True, text=True)  # nosec B603: argument list, no shell, repository script under test
        self.assertEqual(out.returncode, ef.TRANSPORT, out.stdout + out.stderr)
        self.assertNotIn("Traceback", out.stderr)
        self.assertRegex(out.stderr.strip(), r"^ENGINE-FETCH FAIL transport: git is not installed")

    def test_a_source_file_that_is_not_utf8_is_a_lint_error(self) -> None:
        d = self.sources_dir(raw=b'schema = "1"\nname = "eng"\n# \xff\xfe not text\n')
        with self.assertRaises(es.SourceError):
            es.load(d / "eng.toml")
        rc, out, err = self.cli("--root", str(self.root), "--sources", str(d), "list")
        self.assertEqual(rc, ef.LINT, err)
        self.assertRegex(err.strip(), r"^ENGINE-FETCH FAIL lint: ")

    def test_an_empty_source_file_is_a_lint_error(self) -> None:
        d = self.sources_dir(raw=b"")
        rc, out, err = self.cli("--root", str(self.root), "--sources", str(d), "list")
        self.assertEqual(rc, ef.LINT, err)
        self.assertIn("eng", err)

    def test_a_patch_lock_that_is_not_utf8_or_is_empty_is_a_lint_error(self) -> None:
        repo = Repo(self.tmp, "up", {"README": b"hello\n"})
        (self.root / "engine" / "x").mkdir(parents=True)
        lock = self.root / "engine" / "x" / "lock.toml"
        f = self.fetcher(load(tag_source("eng", repo, "engine/x/lock.toml"), "eng"))
        for raw in (b'schema = "1"\n# \xff\xfe\n', b""):
            lock.write_bytes(raw)
            self.assertFetchError(ef.LINT, f.fetch, "eng", self.tmp / ("out%d" % len(raw)))

    def test_an_error_nobody_expected_is_one_line_with_a_defined_exit_code_and_the_traceback_on_request(self) -> None:
        d = self.sources_dir()
        with mock.patch.object(ef.Fetcher, "fetch", side_effect=RuntimeError("boom")):
            rc, out, err = self.cli("--root", str(self.root), "--sources", str(d), "fetch", "eng", "--dest", str(self.tmp / "out"))
        self.assertEqual(rc, ef.INTERNAL, err)
        self.assertEqual(err.strip(), "ENGINE-FETCH FAIL internal: RuntimeError: boom")
        self.assertNotIn("Traceback", err)
        old = os.environ.get("ENGINE_FETCH_DEBUG")
        os.environ["ENGINE_FETCH_DEBUG"] = "1"
        try:
            with mock.patch.object(ef.Fetcher, "fetch", side_effect=RuntimeError("boom")):
                rc, out, err = self.cli("--root", str(self.root), "--sources", str(d), "fetch", "eng", "--dest", str(self.tmp / "out2"))
        finally:
            if old is None:
                del os.environ["ENGINE_FETCH_DEBUG"]
            else:
                os.environ["ENGINE_FETCH_DEBUG"] = old
        self.assertEqual(rc, ef.INTERNAL)
        self.assertIn("Traceback", err)
        self.assertIn("ENGINE-FETCH FAIL internal: RuntimeError: boom", err)

    def test_the_exit_codes_of_the_table_are_distinct_and_each_has_a_name(self) -> None:
        codes = [ef.OK, ef.USAGE, ef.TRANSPORT, ef.MOVED, ef.HASH, ef.OUTPUT, ef.LINT, ef.REBASE, ef.DEST, ef.RECIPE, ef.INTERNAL]
        self.assertEqual(len(set(codes)), len(codes))
        self.assertEqual(ef.INTERNAL, 11)
        for c in codes[1:]:
            self.assertIn(c, ef.CLASS)

    def test_no_command_of_the_real_repository_ends_in_a_traceback_when_it_is_given_nonsense(self) -> None:
        missing = str(self.tmp / "nowhere")
        full = self.tmp / "full"
        full.mkdir()
        (full / "x").write_text("x")
        cases = [
            (("fetch", "nothing", "--dest", missing), ef.USAGE),
            (("build", "nothing", "--dest", missing), ef.USAGE),
            (("verify", "nothing", "--dir", missing), ef.USAGE),
            (("verify", "goal-plugin", "--dir", missing), ef.OUTPUT),
            (("verify", "esbuild", "--dir", missing), ef.OUTPUT),
            (("verify", "cline", "--dir", missing), ef.OUTPUT),
            (("update", "nothing", "--to-head"), ef.USAGE),
            (("update", "cline", "--to-head"), ef.REBASE),
            (("update", "esbuild", "--to-head"), ef.USAGE),
            (("update", "goal-plugin", "--to-commit", "not-a-commit"), ef.USAGE),
            (("fetch", "esbuild", "--dest", str(full)), ef.DEST),
        ]
        for argv, want in cases:
            with self.subTest(argv=" ".join(argv)):
                rc, out, err = self.cli("--root", str(ROOT), *argv, local=False)
                self.assertEqual(rc, want, out + err)
                self.assertNotIn("Traceback", err)
                self.assertTrue(err.startswith("ENGINE-FETCH FAIL "), err)


if __name__ == "__main__":
    unittest.main()
