#!/usr/bin/env python3
"""engine_fetch.py: fetch and verify the pinned sources of `engine/sources/`. The entry point is scripts/engine-fetch.

Third-party engines, harnesses and plugins are fetched at setup from a pinned source and never committed. This module holds the three adapters,
one per source kind, and the commands around them:

  fetch <name> --dest DIR     fetch the pinned source into a new directory and verify every pin. git-tag: a checkout of the tag with the patch
                              series of `patches_lock` applied and its tree checked; git-path-bundle: the pinned paths under their layout names;
                              release-asset: the download and, for an archive, the one named member.
  build <name> --dest DIR     fetch and run the source's build recipe (engine_recipes.py): `esbuild-bundle` makes the goal plugin folder, `cline-fork`
                              runs scripts/build-cline-fork.sh; a source with recipe `none` is fetched.
  verify <name> --dir DIR     re-hash a fetched or built directory against the pins and the plugin lock; extra files fail. A git-tag checkout is held to
                              its series tree and the pinned base commit under the patches (`--no-patches` for one fetched without them) and must be clean.
  update <name> ...           the one command that moves a pin (git-path-bundle sources); never run by a deploy or an image build.
  check                       offline: the source files, their agreement with the older locks, the plugin lock, the repository scan.
  list                        the sources, their kind and pin.

Nothing here resolves a branch or a tag to a newer commit except `update`; a fetch checks what the file pins and fails on a difference. Git runs
with an isolated environment (no system or global config, no hooks, no prompts, no filters or end-of-line conversion; blobs are read with
`git cat-file`). A download is verified before it is read, and an archive member is read by name, never extracted by a pattern.

Exit codes (one table, tested): 0 ok; 2 usage; 3 transport (the host, the reference or the object is not reachable); 4 moved (a tag or a commit is
not what the file pins); 5 hash (a blob, a file, an archive, a patch or a tree differs from its pin); 6 output (a built file differs from the plugin
lock, or an extra file is present); 7 lint (a source file or a cross-file agreement is wrong); 8 rebase (the patch series no longer applies, or a patched
source cannot be re-pinned automatically); 9 destination (not empty); 10 recipe (the build or its tool failed); 11 internal (an error nobody
expected, a defect of this tool: one `ENGINE-FETCH FAIL internal: <type>: <message>` line, and the traceback too when ENGINE_FETCH_DEBUG=1).

Python 3.8+, standard library only. The scratch directories are made by this process (tempfile.mkdtemp) and are the only paths it deletes.
"""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import platform as _platform
import re
import shutil
import stat
import subprocess  # nosec B404: starts git and the recipe tools with argument lists, never through a shell
import sys
import tarfile
import tempfile
import time
import traceback
import urllib.error
import urllib.request
import zipfile
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, List, Optional, Tuple

sys.path.insert(0, str(Path(__file__).resolve().parent))
import engine_source as es  # noqa: E402

OK, USAGE, TRANSPORT, MOVED, HASH, OUTPUT, LINT, REBASE, DEST, RECIPE, INTERNAL = 0, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11
CLASS = {TRANSPORT: "transport", MOVED: "moved", HASH: "hash", OUTPUT: "output", LINT: "lint", REBASE: "rebase", DEST: "destination", RECIPE: "recipe", USAGE: "usage",
         INTERNAL: "internal"}

MAX_MEMBER_BYTES = 1 << 30
CHUNK = 1 << 20


class FetchError(Exception):
    """A failure with an exit code of the table above."""

    def __init__(self, code: int, message: str):
        super().__init__(message)
        self.code = code
        self.message = message


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(CHUNK), b""):
            h.update(chunk)
    return h.hexdigest()


def run(argv: List[str], cwd: Optional[Path] = None, env: Optional[Dict[str, str]] = None, input_bytes: Optional[bytes] = None,
        fail: int = RECIPE) -> "subprocess.CompletedProcess[bytes]":
    """One program, an argument list, no shell. Every call of this module goes through here. A program that cannot be started (not installed, not
    executable) is a FetchError of the class `fail`: TRANSPORT for git, RECIPE for the tools of a build recipe."""
    try:
        return subprocess.run(argv, cwd=str(cwd) if cwd else None, env=env, input=input_bytes, capture_output=True)  # nosec B603: argument list built by this module, no shell, program is git or a pinned recipe tool; B607: bare name of a standard tool such as git looked up on the PATH of the host running this tool  # nosemgrep: dangerous-subprocess-use-audit, dangerous-subprocess-use-tainted-env-args -- argument list built here from validated source-file fields, no shell; the program is git or a recipe tool
    except OSError as e:
        what = "git is not installed or cannot be started" if fail == TRANSPORT else "%s cannot be started" % os.path.basename(argv[0])
        raise FetchError(fail, "%s (%s): %s" % (what, argv[0], e.strerror or e))


def tail(data: bytes, n: int = 240) -> str:
    return " ".join(data.decode("utf-8", "replace").split())[-n:]


class Git:
    """git with an isolated environment: HOME in a scratch directory, no system or global configuration, no hooks, no prompts."""

    def __init__(self, home: Path, binary: str = "git"):
        self.binary = binary
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(
            HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"), TMPDIR=str(home),
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null", GIT_TERMINAL_PROMPT="0",
        )
        self.prefix = ["-c", "core.hooksPath=/dev/null", "-c", "core.autocrlf=false", "-c", "core.safecrlf=false", "-c", "advice.detachedHead=false"]

    def raw(self, *args: str, cwd: Optional[Path] = None) -> "subprocess.CompletedProcess[bytes]":
        return run([self.binary, *self.prefix, *args], cwd=cwd, env=self.env, fail=TRANSPORT)

    def out(self, *args: str, cwd: Optional[Path] = None, code: int = TRANSPORT) -> str:
        p = self.raw(*args, cwd=cwd)
        if p.returncode != 0:
            raise FetchError(code, "git %s failed: %s" % (" ".join(args[:2]), tail(p.stderr)))
        return p.stdout.decode("utf-8", "replace").strip()

    def blob(self, ref: str, cwd: Path) -> bytes:
        p = self.raw("cat-file", "blob", ref, cwd=cwd)
        if p.returncode != 0:
            raise FetchError(TRANSPORT, "git cat-file blob %s failed: %s" % (ref, tail(p.stderr)))
        return p.stdout


def platform_default() -> str:
    sysname = _platform.system().lower()
    machine = _platform.machine().lower()
    arch = {"x86_64": "x64", "amd64": "x64", "aarch64": "arm64", "arm64": "arm64"}.get(machine, machine)
    return "%s-%s" % (sysname, arch)


def check_destination(dest: Path) -> None:
    if dest.exists() and (not dest.is_dir() or any(dest.iterdir())):
        raise FetchError(DEST, "destination %s exists and is not an empty directory" % dest)


@contextmanager
def staged(dest: Path) -> Iterator[Path]:
    """A directory next to `dest` that becomes `dest` only when the block ends without an error; a failure leaves `dest` as it was."""
    dest = Path(dest)
    check_destination(dest)
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = Path(tempfile.mkdtemp(prefix=".%s.partial-" % dest.name, dir=str(dest.parent)))
    try:
        yield tmp
        # mkdtemp makes a private directory (0700); the destination gets the mode a new directory has under the umask of the caller.
        umask = os.umask(0)
        os.umask(umask)
        os.chmod(str(tmp), 0o777 & ~umask)
        if dest.exists():
            dest.rmdir()
        os.rename(str(tmp), str(dest))
    except BaseException:
        shutil.rmtree(str(tmp), ignore_errors=True)
        raise


def content_length(resp: Any) -> Optional[int]:
    """The Content-Length an answer announced, None when there is none or it is not a number."""
    headers = getattr(resp, "headers", None)
    raw = headers.get("Content-Length") if headers is not None else None
    try:
        n = int(raw)
    except (TypeError, ValueError):
        return None
    return n if n >= 0 else None


def download(url: str, dest: Path, want_sha256: str, allow_local: bool = False, retries: int = 3, opener: Optional[Callable[..., Any]] = None,
             sleep: Callable[[float], None] = time.sleep) -> int:
    """Streams `url` to `dest` and returns its size. The SHA-256 is checked before anything reads the file; a difference removes it (exit 5).
    Only https is fetched (file:// and a plain path only when `allow_local`, the test hook). A cut or malformed answer (a chunked body that ends early,
    a body shorter than its Content-Length, a status line that is not HTTP) is a transport error like any other: it is retried with a growing pause,
    the length is checked before the hash so a flaky connection is never blamed on the pin, and a failed download leaves no partial file."""
    if url.startswith("/"):
        url = "file://" + url
    if not (url.startswith("https://") or (allow_local and url.startswith("file://"))):
        raise FetchError(LINT, "only https URLs are fetched (got %s)" % url[:60])
    open_url = opener or urllib.request.urlopen
    last = ""
    for attempt in range(retries):
        try:
            h = hashlib.sha256()
            size = 0
            with open_url(url, timeout=60) as resp, open(dest, "wb") as f:  # nosec B310: the scheme was checked above (https, or file:// for the test hook)  # nosemgrep: dynamic-urllib-use-detected -- the URL is a pinned field of a source file and the scheme is checked above
                for chunk in iter(lambda: resp.read(CHUNK), b""):
                    h.update(chunk)
                    f.write(chunk)
                    size += len(chunk)
                announced = content_length(resp)
            if announced is not None and size != announced:
                raise OSError("short read: %d of %d bytes" % (size, announced))
            if h.hexdigest() != want_sha256:
                dest.unlink()
                raise FetchError(HASH, "sha256 of %s is %s, the source file pins %s" % (url.rsplit("/", 1)[-1], h.hexdigest(), want_sha256))
            return size
        except FetchError:
            raise
        except urllib.error.HTTPError as e:
            last = "HTTP %s" % e.code
            if e.code < 500 and e.code != 429:
                break
        except (urllib.error.URLError, http.client.HTTPException, OSError, TimeoutError) as e:
            last = str(getattr(e, "reason", e)) or type(e).__name__
        if attempt + 1 < retries:
            sleep(2 ** attempt)
    if dest.exists():
        dest.unlink()
    raise FetchError(TRANSPORT, "download of %s failed: %s" % (url, last))


def member_bytes(archive: Path, member: str) -> Tuple[bytes, bool]:
    """The bytes of the one regular file `member` in a tar (any compression) or zip archive, and whether it is executable. The archive is not
    extracted: a link, a directory, a repeated name, an oversized member or a missing member is an error."""
    if tarfile.is_tarfile(str(archive)):
        with tarfile.open(str(archive), "r:*") as tf:
            found = [m for m in tf.getmembers() if m.name == member]
            if len(found) != 1:
                raise FetchError(HASH, "archive has %d entries named %s (one expected)" % (len(found), member))
            m = found[0]
            if not m.isreg() or m.size > MAX_MEMBER_BYTES:
                raise FetchError(HASH, "archive entry %s is not a regular file within the size limit" % member)
            f = tf.extractfile(m)
            if f is None:
                raise FetchError(HASH, "archive entry %s cannot be read" % member)
            return f.read(), bool(m.mode & 0o111)
    if zipfile.is_zipfile(str(archive)):
        with zipfile.ZipFile(str(archive)) as zf:
            found_z = [i for i in zf.infolist() if i.filename == member]
            if len(found_z) != 1:
                raise FetchError(HASH, "archive has %d entries named %s (one expected)" % (len(found_z), member))
            info = found_z[0]
            mode = (info.external_attr >> 16) & 0xFFFF
            if info.filename.endswith("/") or stat.S_ISLNK(mode) or info.file_size > MAX_MEMBER_BYTES:
                raise FetchError(HASH, "archive entry %s is not a regular file within the size limit" % member)
            return zf.read(info), bool(mode & 0o111)
    raise FetchError(HASH, "%s is neither a tar nor a zip archive" % archive.name)


def write_file(path: Path, data: bytes, executable: bool = False) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    os.chmod(str(path), 0o755 if executable else 0o644)


class Fetcher:
    """The adapters. `registry` maps source names to Source (an [[inputs]] table names another source); `root` is the repository root."""

    def __init__(self, root: Path, registry: Dict[str, es.Source], allow_local: bool = False, git_bin: str = "git",
                 opener: Optional[Callable[..., Any]] = None, sleep: Callable[[float], None] = time.sleep, log: Callable[[str], None] = print):
        self.root = Path(root)
        self.registry = registry
        self.allow_local = allow_local
        self.git_bin = git_bin
        self.opener = opener
        self.sleep = sleep
        self.log = log

    # ---- helpers ----------------------------------------------------------------------------

    @contextmanager
    def scratch(self) -> Iterator[Path]:
        d = Path(tempfile.mkdtemp(prefix="engine-fetch-"))
        try:
            yield d
        finally:
            shutil.rmtree(str(d), ignore_errors=True)

    def _url(self, url: str) -> str:
        if url.startswith("/"):
            if not self.allow_local:
                raise FetchError(LINT, "only https URLs are fetched (got %s)" % url[:60])
            return url
        if url.startswith("https://") or (self.allow_local and url.startswith("file://")):
            return url
        raise FetchError(LINT, "only https URLs are fetched (got %s)" % url[:60])

    def _repo(self, git: Git, home: Path, url: str, name: str) -> Path:
        repo = home / name
        repo.mkdir(parents=True)
        git.out("init", "-q", str(repo))
        git.out("remote", "add", "origin", self._url(url), cwd=repo)
        return repo

    def _fetch_commit(self, git: Git, repo: Path, commit: str) -> None:
        for filt in (["--filter=blob:none"], []):
            p = git.raw("fetch", "-q", "--depth", "1", *filt, "origin", commit, cwd=repo)
            if p.returncode == 0:
                break
        else:
            raise FetchError(TRANSPORT, "git fetch of commit %s failed: %s" % (commit[:12], tail(p.stderr)))
        head = git.out("rev-parse", "FETCH_HEAD", cwd=repo)
        if head != commit:
            raise FetchError(MOVED, "fetching %s gave commit %s" % (commit, head))

    # ---- git-tag ----------------------------------------------------------------------------

    def git_tag(self, src: es.Source, dest: Path, patches: bool = True) -> Dict[str, Any]:
        s = src.table("source")
        url, tag, tag_object, commit = s["url"], s["tag"], s["tag_object"], s["commit"]
        with staged(dest) as out, self.scratch() as home:
            git = Git(home, self.git_bin)
            repo = out
            git.out("init", "-q", str(repo))
            git.out("remote", "add", "origin", self._url(url), cwd=repo)
            p = git.raw("fetch", "-q", "--depth", "1", "origin", "refs/tags/%s:refs/tags/%s" % (tag, tag), cwd=repo)
            if p.returncode != 0:
                raise FetchError(TRANSPORT, "git fetch of tag %s failed: %s" % (tag, tail(p.stderr)))
            got_tag = git.out("rev-parse", "refs/tags/%s" % tag, cwd=repo)
            got_commit = git.out("rev-parse", "refs/tags/%s^{commit}" % tag, cwd=repo)
            if got_tag != tag_object or got_commit != commit:
                raise FetchError(MOVED, "tag %s now points at object %s commit %s; the source file pins object %s commit %s"
                                 % (tag, got_tag[:12], got_commit[:12], tag_object[:12], commit[:12]))
            git.out("checkout", "-q", "--detach", commit, cwd=repo)
            info = {"name": src.name, "kind": "git-tag", "commit": commit, "tag_object": tag_object}  # type: Dict[str, Any]
            lock = s.get("patches_lock")
            if lock and patches:
                info.update(self.apply_series(git, repo, self.root / lock, commit))
        return info

    def series_lock(self, lock_path: Path) -> Tuple[List[Dict[str, Any]], List[Dict[str, Any]], str]:
        """The patches, the test-only patches and the series tree of a patch lock (engine/cline/lock.toml)."""
        try:
            lock = es.parse(lock_path.read_text(encoding="utf-8"), str(lock_path))
        except (OSError, UnicodeDecodeError, es.SourceError) as e:
            raise FetchError(LINT, "patch lock %s: %s" % (lock_path, e))
        series = lock.get("patches") or []
        tdd = lock.get("tdd") or []
        want_tree = (lock.get("series") or {}).get("tree", "")
        if not series or not re.match(r"^[0-9a-f]{40}$", want_tree):
            raise FetchError(LINT, "patch lock %s lists no patches or no series tree" % lock_path)
        return series, tdd, want_tree

    def apply_series(self, git: Git, repo: Path, lock_path: Path, commit: str) -> Dict[str, Any]:
        """Applies the patch series of the lock (engine/cline/lock.toml) onto `commit`: each patch file is checked against its SHA-256, each
        test-only patch must apply to the series before its number and give its recorded tree, and the whole series must give `series.tree`."""
        series, tdd, want_tree = self.series_lock(lock_path)
        pdir = lock_path.parent / "patches"
        tdd_at = {}  # type: Dict[int, Dict[str, Any]]
        for t in tdd:
            tdd_at[int(Path(t["file"]).name.split("-")[0]) - 1] = t

        def checked(entry: Dict[str, Any]) -> Path:
            path = pdir / entry["file"]
            if not path.is_file():
                raise FetchError(HASH, "patch %s is missing" % entry["file"])
            if sha256_file(path) != entry["sha256"]:
                raise FetchError(HASH, "sha256 of patch %s is %s, the lock pins %s" % (entry["file"], sha256_file(path), entry["sha256"]))
            return path

        ident = ["-c", "user.name=engine-fetch", "-c", "user.email=engine-fetch@localhost"]

        def am(path: Path) -> None:
            p = git.raw(*ident, "am", "--quiet", "--committer-date-is-author-date", str(path), cwd=repo)
            if p.returncode != 0:
                git.raw("am", "--abort", cwd=repo)
                raise FetchError(REBASE, "git am of %s failed: %s" % (path.name, tail(p.stderr)))

        git.out("checkout", "-q", "-b", "harness-series", cwd=repo)
        for i, entry in enumerate(series):
            t = tdd_at.get(i)
            if t is not None:
                tpath = checked(t)
                git.out("checkout", "-q", "-b", "tdd-check-%d" % i, cwd=repo)
                am(tpath)
                got = git.out("rev-parse", "HEAD^{tree}", cwd=repo)
                if got != t["tree"]:
                    raise FetchError(HASH, "test-only patch %s gives tree %s, the lock pins %s" % (t["file"], got[:12], t["tree"][:12]))
                git.out("checkout", "-q", "harness-series", cwd=repo)
                git.out("branch", "-q", "-D", "tdd-check-%d" % i, cwd=repo)
            am(checked(entry))
        applied = git.out("rev-list", "--count", "%s..HEAD" % commit, cwd=repo)
        if applied != str(len(series)):
            raise FetchError(REBASE, "git am applied %s commits, the lock lists %d" % (applied, len(series)))
        tree = git.out("rev-parse", "HEAD^{tree}", cwd=repo)
        if tree != want_tree:
            raise FetchError(HASH, "the series gives tree %s, the lock pins %s" % (tree, want_tree))
        return {"series_tree": tree, "patches": len(series), "series_head": git.out("rev-parse", "HEAD", cwd=repo)}

    # ---- git-path-bundle --------------------------------------------------------------------

    def _entries(self, src: es.Source) -> List[Tuple[str, str, Dict[str, Any]]]:
        """(url, commit, entry) for every [[files]] and [[inputs]] table; an input takes the url and commit of the source it names."""
        s = src.table("source")
        out = [(s["url"], s["commit"], dict(t)) for t in src.tables("files")]
        for t in src.tables("inputs"):
            other = self.registry.get(t["from"])
            if other is None or other.kind != "git-tag":
                raise FetchError(LINT, "input %s of %s names %r, which is not a git-tag source" % (t["path"], src.name, t["from"]))
            o = other.table("source")
            out.append((o["url"], o["commit"], dict(t)))
        return out

    def git_path_bundle(self, src: es.Source, dest: Path) -> Dict[str, Any]:
        groups = {}  # type: Dict[Tuple[str, str], List[Dict[str, Any]]]
        for url, commit, entry in self._entries(src):
            groups.setdefault((url, commit), []).append(entry)
        files = {}  # type: Dict[str, str]
        with staged(dest) as out, self.scratch() as home:
            git = Git(home, self.git_bin)
            for n, ((url, commit), entries) in enumerate(sorted(groups.items())):
                repo = self._repo(git, home, url, "r%d" % n)
                self._fetch_commit(git, repo, commit)
                for e in entries:
                    blob = git.out("rev-parse", "%s:%s" % (commit, e["path"]), cwd=repo, code=HASH)
                    if blob != e["blob"]:
                        raise FetchError(HASH, "%s at %s has blob %s, the source file pins %s" % (e["path"], commit[:12], blob, e["blob"]))
                    data = git.blob(blob, repo)
                    got = sha256_bytes(data)
                    if got != e["sha256"]:
                        raise FetchError(HASH, "sha256 of %s at %s is %s, the source file pins %s" % (e["path"], commit[:12], got, e["sha256"]))
                    write_file(out / e["as"], data)
                    files[e["as"]] = got
        return {"name": src.name, "kind": "git-path-bundle", "commit": src.table("source")["commit"], "files": files}

    # ---- release-asset ----------------------------------------------------------------------

    def release_asset(self, src: es.Source, dest: Path, platform: Optional[str] = None) -> Dict[str, Any]:
        plat = platform or platform_default()
        a = src.asset(plat)
        if not a:
            raise FetchError(LINT, "%s has no asset for platform %s" % (src.name, plat))
        with staged(dest) as out, self.scratch() as home:
            archive = home / "asset"
            size = download(a["url"], archive, a["sha256"], allow_local=self.allow_local, opener=self.opener, sleep=self.sleep)
            if "member" in a:
                data, executable = member_bytes(archive, a["member"])
                got = sha256_bytes(data)
                if got != a["member_sha256"]:
                    raise FetchError(HASH, "sha256 of %s in the archive is %s, the source file pins %s" % (a["member"], got, a["member_sha256"]))
                name = a["member"].rsplit("/", 1)[-1]
                write_file(out / name, data, executable)
                return {"name": src.name, "kind": "release-asset", "platform": plat, "file": name, "sha256": got, "archive_sha256": a["sha256"], "archive_bytes": size}
            name = a["url"].rsplit("/", 1)[-1] or "asset"
            write_file(out / name, archive.read_bytes())
            return {"name": src.name, "kind": "release-asset", "platform": plat, "file": name, "sha256": a["sha256"], "archive_bytes": size}

    # ---- dispatch ---------------------------------------------------------------------------

    def fetch(self, name: str, dest: Path, platform: Optional[str] = None, patches: bool = True) -> Dict[str, Any]:
        src = self.registry.get(name)
        if src is None:
            raise FetchError(USAGE, "no source named %r (the sources: %s)" % (name, ", ".join(sorted(self.registry)) or "none"))
        if src.kind == "git-tag":
            return self.git_tag(src, dest, patches)
        if src.kind == "git-path-bundle":
            return self.git_path_bundle(src, dest)
        return self.release_asset(src, dest, platform)

    def verify_fetched(self, name: str, directory: Path, platform: Optional[str] = None, patches: bool = True) -> Dict[str, Any]:
        """Re-hashes a directory `fetch` made: the layout files of a git-path-bundle, the member of a release-asset, and for a git-tag checkout its
        commits and its tree (the series tree of `patches_lock` and the pinned base commit under the patches; `patches=False` for a checkout fetched
        without them), with no changed or extra file. Nothing is trusted from a receipt."""
        src = self.registry.get(name)
        if src is None:
            raise FetchError(USAGE, "no source named %r" % name)
        directory = Path(directory)
        if not directory.is_dir():
            raise FetchError(OUTPUT, "%s is not a directory" % directory)
        if src.kind == "git-path-bundle":
            want = {t["as"]: t["sha256"] for t in src.tables("files") + src.tables("inputs")}
            found = {p.relative_to(directory).as_posix() for p in directory.rglob("*") if p.is_file() and not p.is_symlink()}
            if found != set(want):
                raise FetchError(OUTPUT, "files %s differ from the source file's %s" % (sorted(found ^ set(want)), sorted(want)))
            for rel, sha in sorted(want.items()):
                if sha256_file(directory / rel) != sha:
                    raise FetchError(HASH, "sha256 of %s differs from the source file" % rel)
            return {"name": name, "files": len(want)}
        if src.kind == "release-asset":
            plat = platform or platform_default()
            a = src.asset(plat)
            if not a:
                raise FetchError(LINT, "%s has no asset for platform %s" % (src.name, plat))
            sha = a.get("member_sha256") or a.get("sha256")
            files = [p for p in directory.iterdir() if p.is_file()]
            if len(files) != 1 or sha256_file(files[0]) != sha:
                raise FetchError(HASH, "%s does not hold the one file with sha256 %s" % (directory, sha))
            return {"name": name, "files": 1}
        return self.verify_checkout(src, directory, patches)

    def verify_checkout(self, src: es.Source, directory: Path, patches: bool) -> Dict[str, Any]:
        s = src.table("source")
        lock = s.get("patches_lock") if patches else None
        with self.scratch() as home:
            git = Git(home, self.git_bin)
            top = git.out("rev-parse", "--show-toplevel", cwd=directory, code=OUTPUT)
            if Path(top).resolve() != directory.resolve():
                raise FetchError(OUTPUT, "%s is not the top directory of a git checkout (its repository starts at %s)" % (directory, top))
            head = git.out("rev-parse", "HEAD", cwd=directory, code=OUTPUT)
            info = {"name": src.name, "head": head}  # type: Dict[str, Any]
            if lock:
                series, _tdd, want_tree = self.series_lock(self.root / lock)
                tree = git.out("rev-parse", "HEAD^{tree}", cwd=directory, code=OUTPUT)
                if tree != want_tree:
                    raise FetchError(HASH, "the tree of %s is %s, the series of %s pins %s" % (directory, tree, lock, want_tree))
                base = git.raw("rev-parse", "HEAD~%d" % len(series), cwd=directory)
                base_commit = base.stdout.decode("utf-8", "replace").strip() if base.returncode == 0 else ""
                if base_commit != s["commit"]:
                    raise FetchError(MOVED, "the %d patches of %s sit on commit %s, the source file pins %s" % (len(series), directory, base_commit or "(none)", s["commit"]))
                info.update(series_tree=tree, patches=len(series))
            elif head != s["commit"]:
                raise FetchError(MOVED, "HEAD of %s is %s, the source file pins %s" % (directory, head, s["commit"]))
            changed = git.out("status", "--porcelain", "--untracked-files=all", "--ignored", cwd=directory, code=OUTPUT)
            if changed:
                raise FetchError(OUTPUT, "%s holds changed or extra files: %s" % (directory, "; ".join(changed.splitlines()[:5])))
        return info


def resolve_ref(url: str, ref: str, git_bin: str = "git", allow_local: bool = False) -> str:
    """The commit a reference (`HEAD`, a branch or a tag; a tag is peeled) has now. Used by `update` only."""
    with tempfile.TemporaryDirectory(prefix="engine-fetch-ref-") as home:
        git = Git(Path(home), git_bin)
        if not (url.startswith("https://") or (allow_local and (url.startswith("file://") or url.startswith("/")))):
            raise FetchError(LINT, "only https URLs are fetched (got %s)" % url[:60])
        patterns = [ref] if ref == "HEAD" else [ref, "refs/heads/%s" % ref, "refs/tags/%s" % ref, "refs/tags/%s^{}" % ref]
        out = git.out("ls-remote", url, *patterns)
        rows = [ln.split("\t") for ln in out.splitlines() if "\t" in ln]
        if not rows:
            raise FetchError(TRANSPORT, "%s has no reference %s" % (url, ref))
        peeled = [r for r in rows if r[1].endswith("^{}")]
        pick = peeled[0] if peeled else rows[0]
        if not re.match(r"^[0-9a-f]{40}$", pick[0]):
            raise FetchError(TRANSPORT, "unexpected ls-remote answer: %s" % out[:80])
        return pick[0]


def load_registry(root: Path, sources_dir: Optional[Path] = None, allow_local: bool = False) -> Dict[str, es.Source]:
    d = Path(sources_dir) if sources_dir else Path(root) / "engine" / "sources"
    try:
        return es.load_all(d, allow_local_url=allow_local)
    except es.SourceError as e:
        raise FetchError(LINT, "; ".join(e.problems[:6]))


def say(line: str) -> None:
    print(line)


# ---- commands -----------------------------------------------------------------------------------


def _pin(src: es.Source) -> str:
    s = src.table("source")
    if src.kind == "git-tag":
        return "%s %s" % (s.get("tag"), str(s.get("commit"))[:12])
    if src.kind == "git-path-bundle":
        return str(s.get("commit"))[:12]
    return " ".join("%s:%s" % (p, str(t.get("sha256"))[:12]) for p, t in sorted(src.table("assets").items()))


def cmd_list(args: argparse.Namespace, root: Path, registry: Dict[str, es.Source], fetcher: Fetcher) -> int:
    for name in sorted(registry):
        src = registry[name]
        say("%-12s %-16s %-14s %-12s %s" % (name, src.kind, src.recipe, src.license, _pin(src)))
    say("ENGINE-FETCH PASS list %d source(s)" % len(registry))
    return OK


def cmd_fetch(args: argparse.Namespace, root: Path, registry: Dict[str, es.Source], fetcher: Fetcher) -> int:
    info = fetcher.fetch(args.name, Path(args.dest), args.platform, patches=not args.no_patches)
    say("ENGINE-FETCH PASS fetch %s %s" % (args.name, _summary(info)))
    return OK


def _summary(info: Dict[str, Any]) -> str:
    keep = ("kind", "platform", "commit", "head", "series_tree", "sha256", "file", "patches")
    out = []
    for k in keep:
        v = info.get(k)
        if v is not None:
            out.append("%s=%s" % (k, str(v)[:12] if k in ("commit", "head", "series_tree", "sha256") else v))
    if "files" in info and isinstance(info["files"], dict):
        out.append("files=%d" % len(info["files"]))
    return " ".join(out)


def cmd_build(args: argparse.Namespace, root: Path, registry: Dict[str, es.Source], fetcher: Fetcher) -> int:
    import engine_recipes as er

    src = registry.get(args.name)
    if src is None:
        raise FetchError(USAGE, "no source named %r (the sources: %s)" % (args.name, ", ".join(sorted(registry))))
    platform = args.platform or platform_default()
    if src.recipe == "esbuild-bundle":
        info = er.build_goal_plugin(root, registry, fetcher, Path(args.dest), platform, esbuild=Path(args.esbuild) if args.esbuild else None,
                                    sdk_module=args.with_sdk_module)
        say("ENGINE-FETCH PASS build %s platform=%s files=%d (every file equals engine/plugins/lock.json)%s"
            % (args.name, platform, len(info["files"]), " sdk-module.mjs written" if args.with_sdk_module else ""))
    elif src.recipe == "cline-fork":
        info = er.build_cline_fork(root, Path(args.dest), args.target or platform, Path(args.work) if args.work else None)
        say("ENGINE-FETCH PASS build %s %s" % (args.name, info["last_line"]))
    else:
        info = fetcher.fetch(args.name, Path(args.dest), platform)
        say("ENGINE-FETCH PASS build %s (recipe none: fetched) %s" % (args.name, _summary(info)))
    return OK


def cmd_verify(args: argparse.Namespace, root: Path, registry: Dict[str, es.Source], fetcher: Fetcher) -> int:
    import engine_recipes as er

    src = registry.get(args.name)
    if src is None:
        raise FetchError(USAGE, "no source named %r" % args.name)
    if src.recipe == "esbuild-bundle":
        info = er.verify_goal_plugin(root, Path(args.dir))
    else:
        info = fetcher.verify_fetched(args.name, Path(args.dir), args.platform, patches=not args.no_patches)
    say("ENGINE-FETCH PASS verify %s %s" % (args.name, _summary(info) or "ok"))
    return OK


def cmd_update(args: argparse.Namespace, root: Path, registry: Dict[str, es.Source], fetcher: Fetcher) -> int:
    import engine_recipes as er

    src = registry.get(args.name)
    if src is None:
        raise FetchError(USAGE, "no source named %r" % args.name)
    if src.kind == "git-tag" and src.table("source").get("patches_lock"):
        raise FetchError(REBASE, "%s carries a patch series: moving it is a rebase (engine/cline/rebase.sh <tag>), not an update" % args.name)
    if src.name != er.PLUGIN:
        raise FetchError(USAGE, "this command moves the pin of the goal plugin only; edit %s by hand and run `engine-fetch fetch` to check the new pin" % src.path.name)
    url = src.table("source")["url"]
    commit = None  # type: Optional[str]
    if args.to_head:
        commit = resolve_ref(url, "HEAD", fetcher.git_bin, fetcher.allow_local)
    elif args.to_tag:
        commit = resolve_ref(url, args.to_tag, fetcher.git_bin, fetcher.allow_local)
    elif args.to_commit:
        if not es.HEX40.match(args.to_commit):
            raise FetchError(USAGE, "--to-commit takes a full 40-hex commit")
        commit = args.to_commit
    sources_dir = Path(args.sources) if args.sources else root / "engine" / "sources"
    res = er.update_goal_plugin(root, sources_dir, registry, fetcher, commit, args.platform or platform_default(),
                                Path(args.esbuild) if args.esbuild else None, write=not args.dry_run)
    for c in res["changes"]:
        say("  " + c)
    say("ENGINE-FETCH PASS update %s %s -> %s %s" % (res["name"], res["old"][:12], res["new"][:12], "written" if res["written"] else "dry run: nothing written"))
    return OK


def cmd_check(args: argparse.Namespace, root: Path, registry: Dict[str, es.Source], fetcher: Fetcher) -> int:
    import engine_recipes as er

    return er.main_check(root, registry)


def build_parser() -> argparse.ArgumentParser:
    ap = argparse.ArgumentParser(prog="engine-fetch", description="Fetch, verify and build the pinned third-party sources of engine/sources/ (exit codes: see engine_fetch.py).")
    ap.add_argument("--root", default=str(Path(__file__).resolve().parents[2]), help="repository root")
    ap.add_argument("--sources", default=None, help="directory of the source files (default engine/sources under the root)")
    sub = ap.add_subparsers(dest="cmd")
    sub.required = True
    sub.add_parser("list", help="the sources, their kind and pin")
    p = sub.add_parser("fetch", help="fetch and verify one source into a new directory")
    p.add_argument("name")
    p.add_argument("--dest", required=True)
    p.add_argument("--platform", default=None)
    p.add_argument("--no-patches", action="store_true", help="a git-tag source: leave the patch series out")
    p = sub.add_parser("build", help="fetch and run the build recipe of a source")
    p.add_argument("name")
    p.add_argument("--dest", required=True)
    p.add_argument("--platform", default=None)
    p.add_argument("--esbuild", default=None, help="an esbuild binary to use instead of the pinned one (tests)")
    p.add_argument("--with-sdk-module", action="store_true", help="goal plugin: also write <dest>/sdk-module.mjs, createTool as one ES module (for the goal-plugin probe)")
    p.add_argument("--target", default=None, help="cline-fork: linux-x64 or linux-arm64")
    p.add_argument("--work", default=None, help="cline-fork: scratch root (default CLINE_FORK_WORK_DIR)")
    p = sub.add_parser("verify", help="re-hash a fetched or built directory against the pins and the plugin lock")
    p.add_argument("name")
    p.add_argument("--dir", required=True)
    p.add_argument("--platform", default=None)
    p.add_argument("--no-patches", action="store_true", help="a git-tag source: the checkout was fetched without the patch series")
    p = sub.add_parser("update", help="move a pin (the only command that does)")
    p.add_argument("name")
    g = p.add_mutually_exclusive_group()
    g.add_argument("--to-head", action="store_true", help="the commit HEAD of the source repository has now")
    g.add_argument("--to-tag", default=None)
    g.add_argument("--to-commit", default=None)
    p.add_argument("--dry-run", action="store_true", help="build and report; write nothing")
    p.add_argument("--platform", default=None)
    p.add_argument("--esbuild", default=None)
    sub.add_parser("check", help="offline: source files, agreement with the older locks, the plugin lock, the repository scan")
    return ap


COMMANDS = {"list": cmd_list, "fetch": cmd_fetch, "build": cmd_build, "verify": cmd_verify, "update": cmd_update, "check": cmd_check}


def main(argv: Optional[List[str]] = None) -> int:
    args = build_parser().parse_args(argv)
    root = Path(args.root).resolve()
    allow_local = os.environ.get("ENGINE_FETCH_ALLOW_LOCAL") == "1"
    try:
        registry = load_registry(root, Path(args.sources) if args.sources else None, allow_local)
        fetcher = Fetcher(root, registry, allow_local=allow_local, log=say)
        return COMMANDS[args.cmd](args, root, registry, fetcher)
    except FetchError as e:
        print("ENGINE-FETCH FAIL %s: %s" % (CLASS.get(e.code, str(e.code)), e.message), file=sys.stderr)
        return e.code
    except Exception as e:  # an error nobody expected is a defect of this tool: one line and a defined code, the traceback on request
        print("ENGINE-FETCH FAIL internal: %s: %s" % (type(e).__name__, tail(str(e).encode("utf-8", "replace"))), file=sys.stderr)
        if os.environ.get("ENGINE_FETCH_DEBUG") == "1":
            traceback.print_exc()
        return INTERNAL


if __name__ == "__main__":
    import engine_fetch as _module  # the file runs as __main__; the commands and the error classes live in the imported module

    sys.exit(_module.main())
