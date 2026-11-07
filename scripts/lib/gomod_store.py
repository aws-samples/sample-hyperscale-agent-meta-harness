#!/usr/bin/env python3
"""gomod_store.py: the operator step behind scripts/gomod-store.sh. It fills and audits the Go module store.

The store is an approved, create-only GOPROXY tree in the S3 bucket `hamh-<stage>-gomod-<account>` (packages stack). No gate can install a
Go module otherwise: the gates run with GOPROXY=off, CodeArtifact has no Go format, and the worker VPC reaches no public proxy. Workers
read the bucket anonymously through the stage's S3 gateway endpoint; this script is the only writer.

usage: gomod-store.sh fill  --stage live|drill|direct --modules FILE [--approved FILE] [--tools FILE] [--out DIR] [--jobs N] [--dry-run]
                            [--go PATH] [--bucket NAME] [--no-assume-role] [--reuse-current]
       gomod-store.sh check --stage live|drill|direct [--manifest FILE] [--bucket NAME] [--no-assume-role] [--quick]

fill, in order (each step prints lines; any failure before the upload leaves the bucket untouched):
 1. Refuses, exit 2, before any download or AWS call: a module line that is not `path@vX.Y.Z[-pre][+meta]` (no `latest`, no branch), a
    module not on the approved list (`--approved`, default engine/rules/deps/go-approved-modules.txt), an upstream other than `direct`
    or an https URL (`--upstream`, default `direct`: Go fetches from version
    control and verifies every module against the checksum database), a checksum database other than sum.golang.org.
 2. PRECHECK line; the account of the credentials must be the selected accounts.json entry's (`GOMOD-FILL FAIL account <id>`, exit 1).
 3. Resolves every root with real Go in its own synthetic consumer module (GOPROXY=<upstream>, GOSUMDB=sum.golang.org, GOFLAGS=-mod=mod,
    GOTOOLCHAIN=local, GOENV=off, GOWORK=off, caches in the work directory): `go get root@version`, every public package of the root as
    a blank import, `go mod tidy`, `go mod download`, `go mod graph`. `go mod tidy` is what the harness's lock step runs, so what it
    downloads is what the lock step will ask the store for: it also loads the TEST imports of a dependency's packages (amqp091-go's
    tests import goleak, so a store with only `go mod download` of the requirements fails the tidy). A second
    consumer imports every root at once, because minimal version selection then picks versions no single root picks.
 4. Harvests `cache/download`: `.info`, `.mod` and `.zip` of every module version that has a zip, and the `.mod` of every version in the
    module graphs (fetched with `go list -m` when Go did not need it). Every stored version must have a checksum-database lookup record,
    and the `h1:` hashes of its zip and go.mod must equal the database's.
 5. Stages the GOPROXY tree in `<out>/stage`: `<escaped path>/@v/<version>.{info,mod,zip}`, and per module `@v/list` (tagged versions
    that have a zip, sorted by semver) and `@latest` (the highest release, else prerelease, else pseudo-version).
 6. OFFLINE-CHECK: fresh module cache, GOPROXY=file://<out>/stage, GOSUMDB=off and the lock step's environment; every consumer (each root
    alone, then all) must pass `go mod tidy` and `go mod download` from a minimal go.mod (`--jobs` consumers at once). `--build-check` adds
    `go build ./...`, reported as a warning only (code that does not compile is not a store defect; slow, off by default). A failure
    here fails the fill with nothing uploaded.
 7. Uploads as the role hamh-<stage>-gomod-filler (`sts assume-role` from the profile of the selected entry; `--no-assume-role` writes with the
    profile itself, for a test bucket): content objects first, each with one create-only put (`If-None-Match: *`, SHA-256 checksum, sha256
    metadata). A key that exists with the same SHA-256 is `exists`; with other bytes it is a `CONFLICT` and the fill fails without
    overwriting anything. Then the index files per module by compare-and-swap (`If-Match` with the ETag just read, `If-None-Match` for a
    new module; the new content is the union of the existing and the new versions). Last, the manifest `_manifests/<UTC>-<nonce>-<sha12>.json`
    (create-only): the sha256 of every object, the hashes the checksum database confirmed, the upstream, the Go version.
 8. `--reuse-current` asks first (after steps 1 and 2, before any root is resolved) whether the store already holds what this fill would put in it: the newest manifest under
    `_manifests/` is read, and when its stage, bucket, roots, tool roots, Go version, upstream and checksum database are those of this fill and its offline check passed, the
    last line is `GOMOD-FILL CURRENT stage=<s> roots=<n> modules=<n> objects=<n> bucket=<b> manifest=<key>` (exit 0): nothing is resolved, staged or uploaded and no manifest is
    written. Otherwise a `REUSE no <what differs>` line says why and the fill goes on as below; an empty store, a manifest that cannot be read and `--dry-run` always fill.
    Without the flag a fill always resolves everything (a person who fills by hand wants that).
 9. Last line `GOMOD-FILL PASS stage=<s> roots=<n> modules=<n> objects=<n> put=<n> exists=<n> bucket=<b> manifest=<key>` (exit 0),
    `GOMOD-FILL DRY-RUN ...` (steps 1 to 6 and `PLAN put <key>` lines; no AWS call), or `GOMOD-FILL FAIL <reason>` (exit 1).

check (`--jobs N` calls at once) reads the bucket as the filler role (or the profile): every object of each manifest (`_manifests/`, or `--manifest FILE`) must exist with
the manifest's SHA-256; every `list` must name only versions that have `.info`, `.mod` and `.zip` and no pseudo-version; `@latest` must be one
of them. Last line `GOMOD-CHECK PASS stage=<s> modules=<n> objects=<n> manifests=<n>` or `GOMOD-CHECK FAIL ...` (exit 1). A bucket with no manifest fails with the fill command
in the line. `check --quick` reads one object of the bucket and nothing else: `GOMOD-CHECK PASS stage=<s> quick=nonempty bucket=<b>` when the bucket holds an object,
`GOMOD-CHECK FAIL empty stage=<s> bucket=<b>: ...` with the fill command when it holds none (exit 1), and any other failure (wrong account, no such bucket, no permission) is
a `GOMOD-CHECK FAIL <reason>` line that does not say "empty". The gate script runs it before the events sample, and scripts/deploy.sh reads the
same one object after the stacks are up.

Test hooks (environment): GOMOD_STORE_AWS_BIN (the aws binary), GOMOD_STORE_GO (the go binary), GOMOD_STORE_UPSTREAM (a `file://` upstream; the
manifest then says `test-upstream`), GOMOD_STORE_SUMDB=off (needs the upstream hook), GOMOD_STORE_TEST_DROP (a staged key to delete before the
offline check; needs the upstream hook). The account, Region and profile come from the environment scripts/lib/aws-target.sh sets (HAMH_ACCOUNT,
HAMH_REGION, HAMH_PROFILE). Python 3.8+.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import datetime
import hashlib
import json
import os
import re
import secrets
import subprocess  # nosec B404: starts programs with argument lists, never through a shell
import sys
import tempfile
import time
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

REPO = Path(__file__).resolve().parents[2]
DEFAULT_APPROVED = REPO / "engine" / "rules" / "deps" / "go-approved-modules.txt"
MANIFEST_PREFIX = "_manifests/"
# The revision of what a fill puts in the store for given inputs (how roots are resolved, staged, checked and uploaded). `fill --reuse-current` treats a manifest of another revision
# as stale, so a change that makes the content differ for the same inputs bumps this number. Manifests written before the field existed count as revision 1.
FILL_VERSION = 1
INDEX_SUFFIXES = ("/@v/list", "/@latest")
SUMDB = "sum.golang.org"
ASSUME_REFRESH_S = 45 * 60
FILL_COMMAND = ("scripts/gomod-store.sh fill --stage {stage} --modules engine/rules/deps/go-approved-modules.txt "
                "--tools scripts/gomod-tool-roots.txt --jobs 8")
CHECK_COMMAND = "scripts/gomod-store.sh check --stage {stage}"


def fill_command(stage: str) -> str:
    return FILL_COMMAND.format(stage=stage)


def check_command(stage: str) -> str:
    return CHECK_COMMAND.format(stage=stage)

MODULE_PATH = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._~-]*(/[A-Za-z0-9._~-]+)+$")
SEMVER = re.compile(  # nosemgrep: regex_dos -- anchored version pattern whose repeated parts are separated by a literal dot, no ambiguous match
    r"^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$")
PSEUDO_TAIL = re.compile(r"(?:^|[-.])[0-9]{14}-[0-9a-f]{12}(?:\+[0-9A-Za-z.-]+)?$")
TRANSIENT = re.compile(
    r"Could not connect to the endpoint URL|Connection was closed|Read timeout|Connect timeout|SlowDown|Throttling|RequestLimitExceeded|"
    r"RequestTimeout|ServiceUnavailable|InternalError|ConditionalRequestConflict|Error when retrieving credentials|"
    r"credential_process|unsupported protocol scheme", re.IGNORECASE)


class ListError(Exception):
    """A refusal before any work: exit 2."""


class Fail(Exception):
    """A failure: `GOMOD-FILL FAIL <message>`, exit 1."""


# ---------------------------------------------------------------- pure functions

def escape_path(path: str) -> str:
    """Go's case encoding of a module path in the proxy protocol and the module cache: each capital becomes `!` and its lower case."""
    return "".join("!" + c.lower() if "A" <= c <= "Z" else c for c in path)


def unescape_path(path: str) -> str:
    return re.sub(r"!([a-z])", lambda m: m.group(1).upper(), path)


def _pre_key(pre: Optional[str]):
    if pre is None:
        return (1,)
    return (0, tuple((0, int(i), "") if i.isdigit() else (1, 0, i) for i in pre.split(".")))


def semver_key(v: str):
    """Sort key of a Go version: semver precedence (build metadata such as +incompatible is ignored)."""
    m = SEMVER.match(v)
    if not m:
        raise ValueError(f"not a semantic version: {v}")
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)), _pre_key(m.group(4)))


def is_pseudo(v: str) -> bool:
    return bool(SEMVER.match(v)) and bool(PSEUDO_TAIL.search(v))


def is_prerelease(v: str) -> bool:
    m = SEMVER.match(v)
    return bool(m and m.group(4))


def latest(versions: Iterable[str]) -> Optional[str]:
    """The version `@latest` names: the highest release, else the highest prerelease, else the highest pseudo-version."""
    vs = sorted(set(versions), key=semver_key)
    releases = [v for v in vs if not is_pseudo(v) and not is_prerelease(v)]
    pres = [v for v in vs if not is_pseudo(v) and is_prerelease(v)]
    pseudo = [v for v in vs if is_pseudo(v)]
    for group in (releases, pres, pseudo):
        if group:
            return group[-1]
    return None


def merge_list(existing: Optional[str], versions: Iterable[str]) -> str:
    """The `@v/list` text: the union of the existing lines and `versions`, tagged versions only (a pseudo-version is never listed),
    sorted by semver, one per line. No versions is an empty file."""
    have = [ln.strip() for ln in (existing or "").splitlines() if ln.strip()]
    for v in have:
        if not SEMVER.match(v):
            raise ValueError(f"the existing list has a line that is not a version: {v!r}")
    keep = sorted({v for v in [*have, *versions] if not is_pseudo(v)}, key=semver_key)
    return "".join(v + "\n" for v in keep)


def read_module_list(path: Path) -> List[str]:
    """`path@version` lines of a module list file: comments (`#`) and blank lines dropped, each line checked, duplicates removed."""
    out: List[str] = []
    seen = set()
    for n, raw in enumerate(Path(path).read_text().splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        where = f"{path}:{n}"
        if line.count("@") != 1 or len(line.split()) != 1:
            raise ListError(f"{where}: {raw.strip()!r} is not <module path>@<version>")
        mod, ver = line.split("@")
        if not MODULE_PATH.match(mod):
            raise ListError(f"{where}: {mod!r} is not a module path")
        if not SEMVER.match(ver):
            raise ListError(f"{where}: {ver!r} is not an exact version (vX.Y.Z); a module list names no `latest`, branch or range")
        if line not in seen:
            seen.add(line)
            out.append(line)
    return out


def check_approved(requested: Sequence[str], approved: Sequence[str]) -> None:
    ok = set(approved)
    bad = [m for m in requested if m not in ok]
    if bad:
        raise ListError("not on the approved list: " + ", ".join(bad))


def check_upstream(value: str, env: Dict[str, str]) -> str:
    hook = env.get("GOMOD_STORE_UPSTREAM")
    if hook and value == hook and hook.startswith("file://"):
        return value
    if value == "direct" or (re.fullmatch(r"https://[A-Za-z0-9._~:/-]+", value or "") and "," not in value and "|" not in value):
        return value
    raise ListError(f"upstream {value!r} must be `direct` or one https URL (no `off`, no fallback list, no `,direct`)")


def sumdb(env: Dict[str, str]) -> str:
    v = env.get("GOMOD_STORE_SUMDB") or SUMDB
    if v not in (SUMDB, "off"):
        raise ListError(f"GOMOD_STORE_SUMDB={v!r}: only {SUMDB} is accepted (off is a test hook)")
    return v


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_b64(data: bytes) -> str:
    return base64.b64encode(hashlib.sha256(data).digest()).decode()


def h1_of_gomod(data: bytes) -> str:
    """Go's `h1:` hash of a go.mod file (dirhash.Hash1 over the one file named go.mod)."""
    summary = f"{sha256_hex(data)}  go.mod\n".encode()
    return "h1:" + base64.b64encode(hashlib.sha256(summary).digest()).decode()


def now() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def _strings(value: object) -> Optional[List[str]]:
    """`value` sorted when it is a list of strings, else None (a manifest is read from a bucket that others may have written)."""
    if not isinstance(value, list) or not all(isinstance(v, str) for v in value):
        return None
    return sorted(value)


def _count(manifest: dict, key: str) -> int:
    value = manifest.get(key)
    return len(value) if isinstance(value, list) else 0


def manifest_covers(manifest: object, *, stage: str, bucket: str, roots: Sequence[str], tools: Sequence[str], go_version: str, upstream: str,
                    sumdb_value: str) -> Optional[str]:
    """None when `manifest` was written by a fill with these inputs, so the store already holds what this fill would put in it (the store is create-only: an object a
    manifest lists stays); else the first thing that differs, in words. A manifest that is no object, or lacks a field, differs."""
    if not isinstance(manifest, dict):
        return "the newest manifest is not an object"
    if manifest.get("kind") != "gomod-fill":
        return f"the newest manifest is no fill manifest (kind {manifest.get('kind')!r})"
    if manifest.get("schema") != 1:
        return f"the schema of the newest manifest is {manifest.get('schema')!r}, not 1"
    if manifest.get("stage") != stage:
        return f"the stage differs ({manifest.get('stage')!r})"
    if manifest.get("bucket") != bucket:
        return f"the bucket differs ({manifest.get('bucket')!r})"
    if manifest.get("fillVersion", 1) != FILL_VERSION:
        return f"the fill version differs ({manifest.get('fillVersion')!r})"
    if _strings(manifest.get("roots")) != sorted(roots):
        return "the roots differ"
    if _strings(manifest.get("toolRoots")) != sorted(tools):
        return "the tool roots differ"
    if manifest.get("go") != go_version:
        return f"the Go version differs ({manifest.get('go')!r})"
    if manifest.get("upstream") != upstream:
        return f"the upstream differs ({manifest.get('upstream')!r})"
    if manifest.get("sumdb") != sumdb_value:
        return f"the checksum database differs ({manifest.get('sumdb')!r})"
    if not isinstance(manifest.get("offlineCheck"), dict) or manifest["offlineCheck"].get("status") != "pass":
        return "the offline check of the newest manifest did not pass"
    return None


def newest_manifest(bucket: "Bucket", scratch: Path) -> Tuple[Optional[Tuple[str, object]], str]:
    """The newest fill manifest of the store (the names start with the UTC time of the fill) and its parsed text, or (None, why) when the store has none, the listing or the
    read does not work, or the text is no JSON."""
    try:
        keys = sorted(k for k in bucket.list_keys(MANIFEST_PREFIX) if k.endswith(".json"))
        if not keys:
            return None, "the store holds no manifest"
        got = bucket.get(keys[-1], scratch)
        if got is None:
            return None, f"{keys[-1]} cannot be read"
        return (keys[-1], json.loads(got[0])), ""
    except (Fail, ValueError) as e:
        return None, f"the newest manifest cannot be read ({e})"


def say(msg: str) -> None:
    print(msg, flush=True)


# ---------------------------------------------------------------- Go

class GoTool:
    def __init__(self, go_bin: str, upstream: str, sumdb_value: str, work: Path, test_mode: bool) -> None:
        self.go_bin, self.upstream, self.sumdb, self.work, self.test_mode = go_bin, upstream, sumdb_value, work, test_mode
        self.cache = work / "cache"
        self.download = self.cache / "mod" / "cache" / "download"

    def _env(self, cache: Path, proxy: str, sumdb_value: str, vcs: str) -> Dict[str, str]:
        home = cache / "home"
        for d in (home, cache / "tmp", cache / "build", cache / "mod", cache / "path"):
            d.mkdir(parents=True, exist_ok=True)
        env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(home), "TMPDIR": str(cache / "tmp"), "LC_ALL": "C",
            "GOPROXY": proxy, "GOSUMDB": sumdb_value, "GONOSUMDB": "", "GOPRIVATE": "", "GONOPROXY": "", "GOINSECURE": "",
            "GOFLAGS": "-mod=mod -modcacherw", "GOENV": "off", "GOWORK": "off", "GOTOOLCHAIN": "local", "GOTELEMETRY": "off",
            "GOVCS": vcs, "GOAUTH": "off", "CGO_ENABLED": "0", "GO111MODULE": "on",
            "GOCACHE": str(cache / "build"), "GOMODCACHE": str(cache / "mod"), "GOPATH": str(cache / "path"),
            "GIT_TERMINAL_PROMPT": "0", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
        }
        for k in ("SSL_CERT_FILE", "SSL_CERT_DIR", "HTTPS_PROXY", "HTTP_PROXY", "NO_PROXY", "https_proxy", "http_proxy", "no_proxy"):
            if k in os.environ:
                env[k] = os.environ[k]
        return env

    def fetch_env(self) -> Dict[str, str]:
        """Resolving: the upstream, the checksum database on, version control fetches by git only."""
        return self._env(self.cache, self.upstream, self.sumdb, "*:git")

    def lock_env(self, cache: Path, store: Path) -> Dict[str, str]:
        """The environment of the harness's lock step: the store alone, no checksum database."""
        return self._env(cache, "file://" + str(store), "off", "*:off")

    def run(self, args: Sequence[str], cwd: Path, env: Dict[str, str], timeout: int = 1800) -> subprocess.CompletedProcess:
        return subprocess.run([self.go_bin, *args], cwd=str(cwd), env=env, capture_output=True, text=True, timeout=timeout)  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell

    def version(self) -> str:
        p = subprocess.run([self.go_bin, "version"], capture_output=True, text=True, env={"PATH": os.environ.get("PATH", ""), "GOTOOLCHAIN": "local", "GOENV": "off"})  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
        return p.stdout.strip() or "unknown"


def tail(text: str, n: int = 6) -> str:
    lines = [ln for ln in (text or "").strip().splitlines() if ln.strip()]
    return " | ".join(ln.strip() for ln in lines[-n:])[:900]


def find_go(explicit: Optional[str], env: Dict[str, str]) -> str:
    cands = [explicit, env.get("GOMOD_STORE_GO"),
             os.path.join(env.get("HARNESS_GATES_LANG_TOOLS", "/nonexistent"), "go/bin/go"),
             os.path.join(env.get("HAMH_TOOLS") or os.path.join(env.get("XDG_CACHE_HOME") or os.path.join(env.get("HOME", "/nonexistent"), ".cache"), "ham"), "gate-tools-lang/go/bin/go")]
    for c in cands:
        if c and os.access(c, os.X_OK):
            return c
    for d in env.get("PATH", "").split(os.pathsep):
        if d and os.access(os.path.join(d, "go"), os.X_OK):
            return os.path.join(d, "go")
    raise Fail("no Go toolchain: pass --go, set GOMOD_STORE_GO, or install the gate tool set (scripts/gates-lang-tools.sh)")


class Consumer:
    """One synthetic module that imports a root (or all roots): what the fill resolves and what the offline check replays."""

    def __init__(self, name: str, roots: List[str], directory: Path, tools: Sequence[str] = ()) -> None:
        self.name, self.roots, self.dir = name, roots, directory
        # The roots among `roots` that are tools: modules whose main package an agent runs with `go run <module>@<version>`.
        self.tools: List[str] = [r for r in roots if r in set(tools)]
        self.tool_packages: List[str] = [module_of(r) for r in self.tools]
        self.imports: List[str] = []
        self.go_line = "go 1.21"
        self.graph: List[Tuple[str, str]] = []
        self.packages = 0


def _consumer_gomod(module: str, go_line: str, requires: Sequence[str], tools: Sequence[str] = ()) -> str:
    """A consumer's go.mod: one require per root and, for a tool root, a `tool` directive for its main package, so that `go mod tidy`
    keeps the module and downloads what building the tool needs (a blank import cannot name a main package)."""
    req = "".join(f"require {r.replace('@', ' ')}\n" for r in requires)
    tool = "".join(f"tool {t}\n" for t in tools)
    return f"module {module}\n\n{go_line}\n\n{req}" + (f"\n{tool}" if tool else "")


def _main_go(imports: Sequence[str]) -> str:
    body = "".join(f'\t_ "{i}"\n' for i in imports)
    return "package main\n\n" + (f"import (\n{body})\n\n" if imports else "") + "func main() {}\n"


def public_packages(listing: str, module: str) -> List[str]:
    """Import paths of `go list -e -f '{{.ImportPath}}|{{.Name}}|{{if .Error}}E{{end}}'` worth importing from a consumer: importable
    (not main, no internal, testdata or vendor element) and loadable on this platform."""
    out = []
    for line in listing.splitlines():
        parts = line.split("|")
        if len(parts) != 3:
            continue
        ip, name, err = parts
        if err or name == "main" or not name:
            continue
        rel = ip[len(module):] if ip.startswith(module) else ip
        if any(seg in ("internal", "testdata", "vendor") or seg.startswith(("_", ".")) for seg in rel.split("/")):
            continue
        out.append(ip)
    return sorted(set(out))


def module_of(root: str) -> str:
    return root.split("@", 1)[0]


def resolve_consumer(go: GoTool, c: Consumer) -> None:
    """Steps a to f of the fill for one consumer; raises Fail with the reason."""
    c.dir.mkdir(parents=True, exist_ok=True)
    (c.dir / "go.mod").write_text(_consumer_gomod(f"ham.invalid/fill/{c.name}", "go 1.21", []))
    env = go.fetch_env()
    libraries = [r for r in c.roots if r not in c.tools]
    for flags, group in (([], libraries), (["-tool"], c.tools)):
        if not group:
            continue
        p = go.run(["get", *flags, *group], c.dir, env)
        if p.returncode != 0:
            raise Fail(f"go get {' '.join(flags + group) if len(group) < 4 else str(len(group)) + ' roots'}: {tail(p.stderr or p.stdout)}")
    imports: List[str] = []
    for root in libraries:
        mod = module_of(root)
        p = go.run(["list", "-e", "-f", "{{.ImportPath}}|{{.Name}}|{{if .Error}}E{{end}}", mod + "/..."], c.dir, env)
        if p.returncode != 0 and not p.stdout.strip():
            raise Fail(f"go list {mod}/...: {tail(p.stderr)}")
        imports.extend(public_packages(p.stdout, mod))
    c.imports = sorted(set(imports))
    c.packages = len(c.imports)
    (c.dir / "main.go").write_text(_main_go(c.imports))
    for step in (["mod", "tidy"], ["mod", "download"]):
        p = go.run(step, c.dir, env)
        if p.returncode != 0:
            raise Fail(f"go {' '.join(step)}: {tail(p.stderr or p.stdout)}")
    text = (c.dir / "go.mod").read_text()
    m = re.search(r"^go\s+(\S+)\s*$", text, re.M)
    if m:
        c.go_line = f"go {m.group(1)}"
    p = go.run(["mod", "graph"], c.dir, env)
    if p.returncode != 0:
        raise Fail(f"go mod graph: {tail(p.stderr)}")
    nodes = set()
    for ln in p.stdout.splitlines():
        for side in ln.split():
            if "@" in side:
                path, ver = side.rsplit("@", 1)
                if path not in ("go", "toolchain") and SEMVER.match(ver):
                    nodes.add((path, ver))
    c.graph = sorted(nodes)


class Harvest:
    """What the module cache holds after the resolves, as GOPROXY objects."""

    def __init__(self) -> None:
        self.files: Dict[Tuple[str, str], Dict[str, Path]] = {}

    @staticmethod
    def scan(download: Path) -> "Harvest":
        h = Harvest()
        for at_v in download.rglob("@v"):
            if not at_v.is_dir():
                continue
            rel = at_v.parent.relative_to(download)
            if rel.parts and rel.parts[0] in ("sumdb", "vcs"):
                continue
            path = unescape_path(rel.as_posix())
            for f in at_v.iterdir():
                for ext in ("info", "mod", "zip", "ziphash"):
                    if f.name.endswith("." + ext):
                        ver = f.name[: -len(ext) - 1]
                        if SEMVER.match(ver):
                            h.files.setdefault((path, ver), {})[ext] = f
        return h


def complete_graph_mods(go: GoTool, harvest: Harvest, nodes: Iterable[Tuple[str, str]], cwd: Path, jobs: int) -> List[str]:
    """Fetch, with `go list -m`, the go.mod and info of graph nodes Go did not need and the info of every version that has a zip
. Returns what could not be fetched."""
    missing = sorted({n for n in set(nodes) if "mod" not in harvest.files.get(n, {})}
                     | {k for k, f in harvest.files.items() if "zip" in f and "info" not in f})
    env = go.fetch_env()
    gaps: List[str] = []

    def one(n: Tuple[str, str]) -> Optional[str]:
        p = go.run(["list", "-m", "-json", f"{n[0]}@{n[1]}"], cwd, env, timeout=600)
        return None if p.returncode == 0 else f"{n[0]}@{n[1]}: {tail(p.stderr)}"

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        for r in ex.map(one, missing):
            if r:
                gaps.append(r)
    return gaps


def lookup_h1s(download: Path, path: str, ver: str) -> Dict[str, str]:
    """The `h1:` hashes the checksum database answered for a module version, from the lookup record Go wrote after verifying it."""
    f = download / "sumdb" / SUMDB / "lookup" / f"{escape_path(path)}@{ver}"
    out: Dict[str, str] = {}
    if not f.exists():
        return out
    for ln in f.read_text().splitlines():
        parts = ln.split()
        if len(parts) == 3 and parts[0] == path and parts[2].startswith("h1:"):
            out["mod" if parts[1].endswith("/go.mod") else "zip"] = parts[2]
    return out


class Staged:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.content: Dict[str, Path] = {}        # key -> staged file (create-only objects)
        self.index: Dict[str, Tuple[str, List[str]]] = {}   # key -> (kind, versions with a zip)
        self.modules: List[dict] = []


def stage_tree(go: GoTool, harvest: Harvest, nodes: Iterable[Tuple[str, str]], out: Path, sumdb_value: str) -> Staged:
    """Copies the store set into `out`, checks each stored version against the checksum database, and derives the index files."""
    st = Staged(out)
    zipped = {k for k, f in harvest.files.items() if "zip" in f}
    with_mod = {n for n in nodes if "mod" in harvest.files.get(n, {})}
    keep = sorted(zipped | with_mod)
    by_path: Dict[str, List[str]] = {}
    for path, ver in keep:
        files = harvest.files[(path, ver)]
        if "mod" not in files:
            raise Fail(f"{path}@{ver} has a zip and no go.mod in the module cache")
        rec = {"path": path, "version": ver, "zip": "zip" in files}
        gomod = files["mod"].read_bytes()
        rec["modH1"] = h1_of_gomod(gomod)
        if "zip" in files:
            if "info" not in files:
                raise Fail(f"{path}@{ver} has a zip and no info in the module cache")
            zh = files["ziphash"].read_text().strip() if "ziphash" in files else ""
            if not zh.startswith("h1:"):
                raise Fail(f"{path}@{ver}: no ziphash in the module cache")
            rec["zipH1"] = zh
            by_path.setdefault(path, []).append(ver)
        if sumdb_value != "off":
            h = lookup_h1s(go.download, path, ver)
            if "mod" not in h:
                raise Fail(f"{path}@{ver}: the checksum database did not verify its go.mod (no lookup record)")
            if h["mod"] != rec["modH1"]:
                raise Fail(f"{path}@{ver}: go.mod hash {rec['modH1']} differs from the checksum database's {h['mod']}")
            if "zip" in files:
                if "zip" not in h:
                    raise Fail(f"{path}@{ver}: the checksum database did not verify its zip (no lookup record)")
                if h["zip"] != rec["zipH1"]:
                    raise Fail(f"{path}@{ver}: zip hash {rec['zipH1']} differs from the checksum database's {h['zip']}")
            rec["verified"] = SUMDB
        else:
            rec["verified"] = "off"
        st.modules.append(rec)
        base = escape_path(path) + "/@v/"
        for ext in ("info", "mod", "zip"):
            if ext in files:
                key = f"{base}{ver}.{ext}"
                dst = out / key
                dst.parent.mkdir(parents=True, exist_ok=True)
                dst.write_bytes(files[ext].read_bytes())
                st.content[key] = dst
    for path, vs in sorted(by_path.items()):
        top = latest(vs)
        base = escape_path(path)
        st.index[f"{base}/@v/list"] = ("list", vs)
        st.index[f"{base}/@latest"] = ("latest", [top] if top else [])
        text = merge_list(None, vs)
        (out / f"{base}/@v/list").write_text(text)
        if top:
            (out / f"{base}/@latest").write_bytes((out / f"{base}/@v/{top}.info").read_bytes())
    return st


def offline_check(go: GoTool, consumers: Sequence[Consumer], stage_dir: Path, work: Path, build: bool = False,
                  jobs: int = 4) -> Tuple[List[str], List[str]]:
    """Replays each consumer against the staged tree with the lock step's environment. Returns (failures, warnings). Each consumer has its
    own module cache, so nothing one of them downloaded helps another; `build` adds `go build ./...` (warning only: code that does not
    compile is not a store defect) with a build cache shared by all."""
    cache = work / "verify-cache"
    shared_build = cache / "gocache"

    def one(c: Consumer) -> Tuple[Optional[str], Optional[str]]:
        d = work / "verify" / c.name
        d.mkdir(parents=True, exist_ok=True)
        (d / "go.mod").write_text(_consumer_gomod(f"ham.invalid/verify/{c.name}", c.go_line, c.roots, c.tool_packages))
        (d / "main.go").write_text(_main_go(c.imports))
        env = go.lock_env(cache / c.name, stage_dir)
        for step in (["mod", "tidy"], ["mod", "download"]):
            p = go.run(step, d, env)
            if p.returncode != 0:
                return f"{c.name} ({', '.join(c.roots) if len(c.roots) < 4 else str(len(c.roots)) + ' roots'}): go {' '.join(step)}: {tail(p.stderr or p.stdout)}", None
        if build:
            shared_build.mkdir(parents=True, exist_ok=True)
            p = go.run(["build", "./..."], d, dict(env, GOPROXY="off", GOCACHE=str(shared_build)))
            if p.returncode != 0:
                return None, f"{c.name}: go build ./...: {tail(p.stderr or p.stdout, 3)}"
        return None, None

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs)) as ex:
        results = list(ex.map(one, consumers))
    return [f for f, _ in results if f], [w for _, w in results if w]


# ---------------------------------------------------------------- AWS

class Aws:
    def __init__(self, aws_bin: str, region: str, profile: str, sleep=time.sleep) -> None:
        self.aws_bin, self.region, self.profile, self.sleep = aws_bin, region, profile, sleep
        self.creds: Optional[dict] = None
        self.role_arn: Optional[str] = None
        self.assumed_at = 0.0
        self.base = dict(os.environ)
        for k in ("AWS_REGION", "AWS_DEFAULT_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_BEARER_TOKEN_BEDROCK"):
            self.base.pop(k, None)
        self.base.update(AWS_PROFILE=profile, AWS_PAGER="", AWS_RETRY_MODE="adaptive", AWS_MAX_ATTEMPTS="10")

    def _env(self) -> Dict[str, str]:
        env = dict(self.base)
        if self.creds:
            # Assumed-role credentials live only in the child environment: never in an argument, an output line or a file.
            env.pop("AWS_PROFILE", None)
            env.update(AWS_ACCESS_KEY_ID=self.creds["AccessKeyId"], AWS_SECRET_ACCESS_KEY=self.creds["SecretAccessKey"],
                       AWS_SESSION_TOKEN=self.creds["SessionToken"])
        return env

    def call(self, args: Sequence[str], attempts: int = 5) -> Tuple[int, str, str]:
        self._refresh()
        rc, out, err = 1, "", ""
        for i in range(attempts):
            p = subprocess.run([self.aws_bin, "--output", "json", "--region", self.region, *args], capture_output=True, text=True, env=self._env())  # nosec B603: argument list built here, no shell, program is git, the interpreter or a configured binary  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from its own options and paths, no shell
            rc, out, err = p.returncode, p.stdout, p.stderr
            if rc == 0 or not TRANSIENT.search(err) or i == attempts - 1:
                break
            self.sleep(min(30.0, (2 ** i) + 0.5))
        return rc, out, err

    def assume(self, role_arn: str) -> None:
        self.role_arn = role_arn
        self.creds = None
        rc, out, err = self.call(["sts", "assume-role", "--role-arn", role_arn, "--role-session-name", "gomod-fill-" + secrets.token_hex(3)])
        if rc != 0:
            raise Fail(f"assume-role {role_arn.rsplit('/', 1)[-1]}: {tail(err, 2)}")
        self.creds = json.loads(out)["Credentials"]
        self.assumed_at = time.time()

    def _refresh(self) -> None:
        if self.role_arn and self.creds and time.time() - self.assumed_at > ASSUME_REFRESH_S:
            arn = self.role_arn
            self.creds = None
            rc, out, err = self.call(["sts", "assume-role", "--role-arn", arn, "--role-session-name", "gomod-fill-" + secrets.token_hex(3)])
            if rc != 0:
                raise Fail(f"assume-role refresh: {tail(err, 2)}")
            self.creds = json.loads(out)["Credentials"]
            self.assumed_at = time.time()


CONTENT_TYPES = {"info": "application/json", "mod": "text/plain", "zip": "application/zip", "list": "text/plain", "latest": "application/json"}


def content_type(key: str) -> str:
    if key.endswith("/@latest"):
        return CONTENT_TYPES["latest"]
    if key.endswith("/@v/list"):
        return CONTENT_TYPES["list"]
    if key.endswith(".json"):
        return "application/json"
    return CONTENT_TYPES.get(key.rsplit(".", 1)[-1], "application/octet-stream")


class Bucket:
    def __init__(self, aws: Aws, name: str, account: str) -> None:
        self.aws, self.name, self.account = aws, name, account

    def _base(self, key: str) -> List[str]:
        return ["--bucket", self.name, "--key", key, "--expected-bucket-owner", self.account]

    def put_create_only(self, key: str, path: Path, data: bytes, scratch: Path) -> str:
        """Returns `put`, `exists` (same SHA-256) or `conflict`; raises Fail on any other refusal."""
        sha = sha256_hex(data)
        args = ["s3api", "put-object", *self._base(key), "--body", str(path), "--if-none-match", "*", "--checksum-algorithm", "SHA256",
                "--metadata", f"sha256={sha}", "--content-type", content_type(key)]
        rc, out, err = self.aws.call(args)
        if rc == 0:
            return "put"
        if "PreconditionFailed" in err:
            return self.compare_existing(key, data, scratch)
        raise Fail(f"put {key}: {tail(err, 2)}")

    def compare_existing(self, key: str, data: bytes, scratch: Path) -> str:
        """The key exists. `exists` when it holds these bytes (the stored SHA-256 checksum says so; an object put without that checksum
        algorithm is read and compared), or, for a `.info`, the same Version and Time (Origin says where this Go fetched the module from
        and differs between upstreams); else `conflict`. Nothing is ever overwritten."""
        head = self.head(key)
        if head is None:
            raise Fail(f"{key}: the create-only put was refused and the key cannot be read back")
        if head.get("ChecksumSHA256") == sha256_b64(data):
            return "exists"
        if head.get("ChecksumSHA256") and not key.endswith(".info"):
            return "conflict"
        got = self.get(key, scratch)
        if got is None:
            raise Fail(f"{key}: the create-only put was refused and the key cannot be read back")
        if got[0] == data:
            return "exists"
        if key.endswith(".info"):
            try:
                a, b = json.loads(got[0]), json.loads(data)
                if a.get("Version") == b.get("Version") and a.get("Time") == b.get("Time"):
                    return "exists"
            except (ValueError, AttributeError):
                pass
        return "conflict"

    def head(self, key: str) -> Optional[dict]:
        rc, out, err = self.aws.call(["s3api", "head-object", *self._base(key), "--checksum-mode", "ENABLED"])
        if rc == 0:
            return json.loads(out)
        if "404" in err or "Not Found" in err or "NoSuchKey" in err:
            return None
        raise Fail(f"head {key}: {tail(err, 2)}")

    def get(self, key: str, scratch: Path) -> Optional[Tuple[bytes, str]]:
        dst = scratch / ("get-" + secrets.token_hex(4))
        rc, out, err = self.aws.call(["s3api", "get-object", *self._base(key), str(dst)])
        if rc == 0:
            return dst.read_bytes(), json.loads(out)["ETag"]
        if "NoSuchKey" in err or "404" in err or "Not Found" in err:
            return None
        raise Fail(f"get {key}: {tail(err, 2)}")

    def put_index(self, key: str, new_text: str, merge, scratch: Path, tries: int = 6) -> str:
        """Compare-and-swap: merges `new_text` into what the bucket holds and writes with If-Match (If-None-Match for a new key).
        Returns `put` or `unchanged`."""
        for _ in range(tries):
            cur = self.get(key, scratch)
            try:
                merged = merge(cur[0].decode() if cur else None, new_text)
            except ValueError as e:
                raise Fail(f"{key}: {e}; the store is not changed")
            if cur is not None and merged == cur[0].decode():
                return "unchanged"
            dst = scratch / ("idx-" + secrets.token_hex(4))
            dst.write_text(merged)
            cond = ["--if-match", cur[1]] if cur else ["--if-none-match", "*"]
            rc, out, err = self.aws.call(["s3api", "put-object", *self._base(key), "--body", str(dst), *cond, "--checksum-algorithm", "SHA256",
                                          "--metadata", f"sha256={sha256_hex(merged.encode())}", "--content-type", content_type(key)])
            if rc == 0:
                return "put"
            if "PreconditionFailed" in err or "NoSuchKey" in err:
                continue
            raise Fail(f"put {key}: {tail(err, 2)}")
        raise Fail(f"put {key}: the index changed under us {tries} times")

    def has_objects(self) -> bool:
        """One page of one object: does the bucket hold anything? A failure of the call (no such bucket, no permission) raises Fail, so it is never read as empty."""
        rc, out, err = self.aws.call(["s3api", "list-objects-v2", "--bucket", self.name, "--expected-bucket-owner", self.account, "--max-items", "1"])
        if rc != 0:
            raise Fail(f"list {self.name}: {tail(err, 2)}")
        doc = json.loads(out) if out.strip() else {}
        return bool(doc.get("Contents"))

    def list_keys(self, prefix: str = "") -> List[str]:
        args = ["s3api", "list-objects-v2", "--bucket", self.name, "--expected-bucket-owner", self.account]
        if prefix:
            args += ["--prefix", prefix]
        rc, out, err = self.aws.call(args)
        if rc != 0:
            raise Fail(f"list {prefix or self.name}: {tail(err, 2)}")
        doc = json.loads(out) if out.strip() else {}
        return [o["Key"] for o in (doc.get("Contents") or [])]


# ---------------------------------------------------------------- commands

def settings(args: argparse.Namespace, env: Dict[str, str]) -> dict:
    account, region = env.get("HAMH_ACCOUNT", ""), env.get("HAMH_REGION", "")
    if not account or not region:
        raise ListError("HAMH_ACCOUNT and HAMH_REGION are not set: run through scripts/gomod-store.sh (it selects the accounts.json entry)")
    bucket = args.bucket or f"hamh-{args.stage}-gomod-{account}"
    if not bucket.startswith(f"hamh-{args.stage}-gomod-"):
        raise ListError(f"--bucket {bucket}: a store bucket is named hamh-{args.stage}-gomod-*")
    return {"account": account, "region": region, "profile": env.get("HAMH_PROFILE") or env.get("AWS_PROFILE", ""), "bucket": bucket,
            "role": f"arn:aws:iam::{account}:role/hamh-{args.stage}-gomod-filler", "aws_bin": env.get("GOMOD_STORE_AWS_BIN", "aws")}


def connect(args: argparse.Namespace, st: dict) -> Bucket:
    aws = Aws(st["aws_bin"], st["region"], st["profile"])
    rc, out, err = aws.call(["sts", "get-caller-identity"])
    if rc != 0:
        raise Fail(f"sts get-caller-identity: {tail(err, 2)}")
    got = json.loads(out).get("Account", "")
    if got != st["account"]:
        raise Fail(f"account {got} is not the selected entry's ({st['account']}); nothing was changed")
    if not args.no_assume_role:
        aws.assume(st["role"])
    return Bucket(aws, st["bucket"], st["account"])


def fresh_dir(path: Optional[str], env: Dict[str, str]) -> Path:
    if path:
        d = Path(path)
        if d.exists() and any(d.iterdir()):
            raise ListError(f"--out {d} is not empty: a fill works in a fresh directory")
    else:
        base = Path(env.get("TMPDIR") or tempfile.gettempdir()) / "gomod-fill"
        base.mkdir(parents=True, exist_ok=True)
        d = Path(tempfile.mkdtemp(prefix=datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ-"), dir=str(base)))
    d.mkdir(parents=True, exist_ok=True)
    return d


def cmd_fill(args: argparse.Namespace, env: Dict[str, str]) -> int:
    started = now()
    st = settings(args, env)
    requested = read_module_list(Path(args.modules))
    approved_path = Path(args.approved or DEFAULT_APPROVED)
    approved = read_module_list(approved_path)
    if not requested:
        raise ListError(f"{args.modules} names no module")
    check_approved(requested, approved)
    # Tool roots (`--tools FILE`, the reviewed scripts/gomod-tool-roots.txt): modules an agent runs with `go run <module>@<version>`.
    tools = read_module_list(Path(args.tools)) if args.tools else []
    both = sorted(set(tools) & set(requested))
    if both:
        raise ListError("both a library root and a tool root: " + ", ".join(both))
    hook = env.get("GOMOD_STORE_UPSTREAM")
    upstream = check_upstream(hook or args.upstream, env)
    test_mode = bool(hook and upstream == hook)
    sdb = sumdb(env)
    if sdb == "off" and not test_mode:
        raise ListError("the checksum database stays on: GOMOD_STORE_SUMDB=off is a test hook that needs GOMOD_STORE_UPSTREAM")
    out = fresh_dir(args.out, env)
    go_bin = find_go(args.go, env)
    go = GoTool(go_bin, upstream, sdb, out, test_mode)
    roots = sorted({*requested, *tools})
    say(f"PRECHECK stage={args.stage} account={st['account']} bucket={st['bucket']} approved={sha256_hex(approved_path.read_bytes())[:12]} "
        f"modules={sha256_hex(Path(args.modules).read_bytes())[:12]} roots={len(roots)} tools={len(tools)} upstream={'test-upstream' if test_mode else upstream} "
        f"sumdb={sdb} go={go.version()} out={out}")
    bucket: Optional[Bucket] = None
    if not args.dry_run:
        bucket = connect(args, st)
        say(f"ACCOUNT ok {st['account']} writer={'profile ' + st['profile'] if args.no_assume_role else 'role hamh-' + args.stage + '-gomod-filler'}")

    if args.reuse_current and bucket is not None:
        scratch = out / "scratch"
        scratch.mkdir(exist_ok=True)
        held, why = newest_manifest(bucket, scratch)
        if held is not None:
            why = manifest_covers(held[1], stage=args.stage, bucket=st["bucket"], roots=roots, tools=tools, go_version=go.version(),
                                  upstream="test-upstream" if test_mode else upstream, sumdb_value=sdb) or ""
        if held is not None and not why:
            m = held[1]
            say(f"GOMOD-FILL CURRENT stage={args.stage} roots={len(roots)} modules={_count(m, 'modules')} objects={_count(m, 'objects') + _count(m, 'indexes')} "
                f"bucket={st['bucket']} manifest={held[0]}")
            return 0
        say(f"REUSE no {why}")

    consumers = [Consumer(f"r{i}", [r], out / "consumers" / f"r{i}", tools) for i, r in enumerate(roots)]
    # The `all` consumer holds the library roots only: a tool is run on its own (`go run <module>@<version>`, the versions its go.mod
    # requires), so its own consumer is the one that matters, and two versions of one tool cannot share a go.mod.
    libraries = sorted(set(roots) - set(tools))
    if libraries:
        consumers.append(Consumer("all", libraries, out / "consumers" / "all"))
    failures: List[str] = []

    def resolve(c: Consumer) -> Optional[str]:
        try:
            resolve_consumer(go, c)
            return None
        except Fail as e:
            return f"{c.name} ({', '.join(c.roots) if len(c.roots) < 4 else str(len(c.roots)) + ' roots'}): {e}"
        except subprocess.TimeoutExpired as e:
            return f"{c.name}: timed out: {e}"

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as ex:
        for c, err in zip(consumers, ex.map(resolve, consumers)):
            if err:
                failures.append(err)
                say(f"ROOT FAIL {err}")
            else:
                say(f"ROOT OK {c.name} {','.join(c.roots) if len(c.roots) < 4 else str(len(c.roots)) + ' roots'} packages={c.packages} graph={len(c.graph)}")
    if failures:
        say(f"GOMOD-FILL FAIL {len(failures)} of {len(consumers)} consumers did not resolve; nothing was uploaded")
        return 1
    nodes = sorted({n for c in consumers for n in c.graph})
    harvest = Harvest.scan(go.download)
    gaps = complete_graph_mods(go, harvest, nodes, consumers[-1].dir, args.jobs)
    harvest = Harvest.scan(go.download)
    for g in gaps:
        say(f"WARN graph go.mod not fetched: {g}")
    try:
        staged = stage_tree(go, harvest, nodes, out / "stage", sdb)
    except Fail as e:
        say(f"GOMOD-FILL FAIL {e}; nothing was uploaded")
        return 1
    drop = env.get("GOMOD_STORE_TEST_DROP")
    if drop and test_mode and (out / "stage" / drop).exists():
        # Test hook: a staged object the lock step needs goes missing, which the offline check must notice (renamed, not removed).
        (out / "stage" / drop).rename(out / "stage" / (drop + ".dropped"))
    say(f"MODULES {len(staged.modules)} (zip {sum(1 for m in staged.modules if m['zip'])}) content={len(staged.content)} index={len(staged.index)}")
    fails, warns = offline_check(go, consumers, out / "stage", out, build=args.build_check, jobs=args.jobs)
    for w in warns:
        say(f"WARN {w}")
    if fails:
        for f in fails:
            say(f"OFFLINE-CHECK FAIL {f}")
        say("GOMOD-FILL FAIL the staged tree does not serve the lock step; nothing was uploaded")
        return 1
    say(f"OFFLINE-CHECK PASS consumers={len(consumers)} warnings={len(warns)}")

    objects: List[dict] = []
    for key, path in sorted(staged.content.items()):
        data = path.read_bytes()
        objects.append({"key": key, "sha256": sha256_hex(data), "bytes": len(data)})
    indexes = [{"key": k, "sha256": sha256_hex((out / "stage" / k).read_bytes()), "bytes": (out / "stage" / k).stat().st_size} for k in sorted(staged.index)
               if (out / "stage" / k).exists()]
    manifest = {
        "schema": 1, "kind": "gomod-fill", "fillVersion": FILL_VERSION, "stage": args.stage, "account": st["account"], "region": st["region"], "bucket": st["bucket"],
        "startedAt": started, "go": go.version(), "upstream": "test-upstream" if test_mode else upstream, "sumdb": sdb,
        "approvedSha256": sha256_hex(approved_path.read_bytes()), "modulesSha256": sha256_hex(Path(args.modules).read_bytes()),
        "roots": roots, "toolRoots": sorted(tools), "modules": staged.modules, "objects": objects, "indexes": indexes,
        "offlineCheck": {"status": "pass", "consumers": len(consumers), "build": "checked" if args.build_check else "skipped", "warnings": warns},
        "gaps": gaps,
    }
    if args.dry_run:
        for o in objects:
            say(f"PLAN put {o['key']} sha256={o['sha256']} bytes={o['bytes']}")
        for i in indexes:
            say(f"PLAN index {i['key']}")
        manifest["finishedAt"] = now()
        (out / "manifest.json").write_text(json.dumps(manifest, indent=1, sort_keys=True) + "\n")
        say(f"GOMOD-FILL DRY-RUN stage={args.stage} roots={len(roots)} modules={len(staged.modules)} objects={len(objects) + len(indexes)} manifest={out / 'manifest.json'}")
        return 0

    assert bucket is not None
    scratch = out / "scratch"
    scratch.mkdir(exist_ok=True)
    results: Dict[str, str] = {}

    def upload(o: dict) -> Tuple[str, str]:
        p = out / "stage" / o["key"]
        return o["key"], bucket.put_create_only(o["key"], p, p.read_bytes(), scratch)

    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as ex:
            for key, verdict in ex.map(upload, objects):
                results[key] = verdict
    except Fail as e:
        say(f"OBJECT FAIL {e}")
        say(f"GOMOD-FILL FAIL {e}")
        return 1
    conflicts = [k for k, v in results.items() if v == "conflict"]
    for o in objects:
        verdict = results[o["key"]]
        say(f"{'CONFLICT' if verdict == 'conflict' else 'OBJECT ' + verdict} {o['key']} sha256={o['sha256']} bytes={o['bytes']}")
    if conflicts:
        say(f"GOMOD-FILL FAIL {len(conflicts)} object(s) exist with other bytes and were not overwritten: {', '.join(conflicts[:5])}")
        return 1
    # Index files last: a reader never sees a list that names a version whose objects are not yet there. Each key is its own
    # compare-and-swap, so they go in parallel.
    def index_one(item: Tuple[str, Tuple[str, List[str]]]) -> Tuple[str, str]:
        key, (kind, versions) = item
        if kind == "list":
            return key, bucket.put_index(key, merge_list(None, versions), lambda cur, new: merge_list(cur, new.splitlines()), scratch)
        return key, bucket.put_index(key, (out / "stage" / key).read_text(), _merge_latest, scratch)

    try:
        with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, args.jobs)) as ex:
            for key, verdict in ex.map(index_one, sorted(staged.index.items())):
                results[key] = "exists" if verdict == "unchanged" else "put"
                say(f"INDEX {verdict} {key}")
    except Fail as e:
        say(f"GOMOD-FILL FAIL {e}")
        return 1
    manifest["finishedAt"] = now()
    text = json.dumps(manifest, indent=1, sort_keys=True) + "\n"
    (out / "manifest.json").write_text(text)
    mkey = f"{MANIFEST_PREFIX}{datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%SZ')}-{secrets.token_hex(2)}-{manifest['modulesSha256'][:12]}.json"
    try:
        verdict = bucket.put_create_only(mkey, out / "manifest.json", text.encode(), scratch)
    except Fail as e:
        say(f"GOMOD-FILL FAIL {e}")
        return 1
    if verdict != "put":
        say(f"GOMOD-FILL FAIL manifest {mkey}: {verdict}")
        return 1
    put = sum(1 for v in results.values() if v == "put")
    exists = sum(1 for v in results.values() if v == "exists")
    say(f"MANIFEST {mkey} sha256={sha256_hex(text.encode())}")
    say(f"GOMOD-FILL PASS stage={args.stage} roots={len(roots)} modules={len(staged.modules)} objects={len(results)} put={put} exists={exists} "
        f"bucket={st['bucket']} manifest={mkey}")
    return 0


def _merge_latest(cur: Optional[str], new: str) -> str:
    """`@latest` keeps the higher of the stored and the new version (the index only moves forward)."""
    if cur is None:
        return new
    try:
        cur_v, new_v = json.loads(cur)["Version"], json.loads(new)["Version"]
    except (ValueError, KeyError):
        return new
    return new if latest([cur_v, new_v]) == new_v and new_v != cur_v else cur


def cmd_check(args: argparse.Namespace, env: Dict[str, str]) -> int:
    st = settings(args, env)
    bucket = connect(args, st)
    if args.quick:
        if bucket.has_objects():
            say(f"GOMOD-CHECK PASS stage={args.stage} quick=nonempty bucket={st['bucket']}")
            return 0
        say(f"GOMOD-CHECK FAIL empty stage={args.stage} bucket={st['bucket']}: the store holds no object, so a session finds no Go module and every Go request is a 404; "
            f"fill it with: {fill_command(args.stage)} ; then audit it with: {check_command(args.stage)}")
        return 1
    scratch = Path(tempfile.mkdtemp(prefix="gomod-check-", dir=env.get("TMPDIR") or None))
    problems: List[str] = []
    manifests: List[Tuple[str, dict]] = []
    if args.manifest:
        manifests.append((args.manifest, json.loads(Path(args.manifest).read_text())))
    else:
        for key in bucket.list_keys(MANIFEST_PREFIX):
            got = bucket.get(key, scratch)
            if got:
                manifests.append((key, json.loads(got[0])))
    if not manifests:
        say(f"GOMOD-CHECK FAIL no manifest under {MANIFEST_PREFIX} in {st['bucket']}; fill the store with: {fill_command(args.stage)}")
        return 1
    checked = 0
    jobs = max(1, args.jobs)
    todo = [(name, o) for name, m in manifests for o in m.get("objects", [])]

    def verify(item: Tuple[str, dict]) -> Optional[str]:
        name, o = item
        head = bucket.head(o["key"])
        if head is None:
            return f"missing {o['key']} (manifest {name})"
        if o["key"].endswith(".info"):
            return None   # version and time only (an older fill may differ in its Origin); go.sum covers the zip and the go.mod
        if head.get("ChecksumSHA256") != base64.b64encode(bytes.fromhex(o["sha256"])).decode():
            # An object put without the SHA-256 checksum algorithm has none: compare its bytes.
            got = bucket.get(o["key"], scratch)
            if got is None or sha256_hex(got[0]) != o["sha256"]:
                return f"sha256 differs {o['key']} (manifest {name})"
        return None

    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as ex:
        for r in ex.map(verify, todo):
            checked += 1
            if r:
                problems.append(r)
    keys = set(bucket.list_keys())
    index_keys = sorted(k for k in keys if k.endswith("/@v/list"))

    def audit(key: str) -> List[str]:
        out: List[str] = []
        base = key[: -len("/@v/list")]
        got = bucket.get(key, scratch)
        versions = [ln.strip() for ln in (got[0].decode() if got else "").splitlines() if ln.strip()]
        for v in versions:
            if is_pseudo(v):
                out.append(f"{unescape_path(base)}: list names the pseudo-version {v}")
            for ext in ("info", "mod", "zip"):
                if f"{base}/@v/{v}.{ext}" not in keys:
                    out.append(f"{unescape_path(base)}: list names {v} and {base}/@v/{v}.{ext} is missing")
        lat = f"{base}/@latest"
        if lat in keys:
            g = bucket.get(lat, scratch)
            try:
                lv = json.loads(g[0])["Version"] if g else ""
            except (ValueError, KeyError):
                lv = ""
            if lv and f"{base}/@v/{lv}.zip" not in keys:
                out.append(f"{unescape_path(base)}: @latest names {lv} and its zip is missing")
        return out

    modules = len(index_keys)
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as ex:
        for found in ex.map(audit, index_keys):
            problems.extend(found)
    for p in problems:
        say(f"CHECK FAIL {p}")
    if problems:
        say(f"GOMOD-CHECK FAIL stage={args.stage} problems={len(problems)} bucket={st['bucket']}")
        return 1
    say(f"GOMOD-CHECK PASS stage={args.stage} modules={modules} objects={checked} manifests={len(manifests)} bucket={st['bucket']}")
    return 0


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="gomod-store.sh", description="Fill and audit the Go module store.")
    sub = p.add_subparsers(dest="cmd", required=True)
    for name in ("fill", "check"):
        s = sub.add_parser(name)
        s.add_argument("--stage", required=True, choices=["live", "drill", "direct"])
        s.add_argument("--bucket")
        s.add_argument("--no-assume-role", action="store_true")
    f = sub.choices["fill"]
    f.add_argument("--modules", required=True)
    f.add_argument("--approved")
    f.add_argument("--tools", help="a module list of tool roots (main packages an agent runs with `go run <module>@<version>`)")
    f.add_argument("--upstream", default="direct")
    f.add_argument("--out")
    f.add_argument("--jobs", type=int, default=4)
    f.add_argument("--go")
    f.add_argument("--dry-run", action="store_true")
    f.add_argument("--build-check", action="store_true", help="also `go build ./...` each offline consumer (warnings only; slow)")
    f.add_argument("--reuse-current", action="store_true",
                   help="end with GOMOD-FILL CURRENT, resolving and uploading nothing, when the newest manifest of the store was written by a fill with these inputs")
    c = sub.choices["check"]
    c.add_argument("--manifest")
    c.add_argument("--jobs", type=int, default=8)
    c.add_argument("--quick", action="store_true", help="read one object only: pass when the bucket holds one, fail with the fill command when it is empty")
    return p


def main(argv: Optional[Sequence[str]] = None, env: Optional[Dict[str, str]] = None) -> int:
    env = dict(os.environ if env is None else env)
    try:
        args = parser().parse_args(argv)
    except SystemExit as e:
        return 2 if e.code not in (0, None) else 0
    try:
        if args.cmd == "fill":
            return cmd_fill(args, env)
        return cmd_check(args, env)
    except ListError as e:
        print(f"gomod-store: refused: {e}", file=sys.stderr)
        return 2
    except Fail as e:
        say(f"GOMOD-{'FILL' if args.cmd == 'fill' else 'CHECK'} FAIL {e}")
        return 1


if __name__ == "__main__":
    sys.exit(main())
