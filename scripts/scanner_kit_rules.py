#!/usr/bin/env python3
"""scanner_kit_rules.py: build and publish the rules package of the scanner kit.

A coding session has no semgrep, so a finding of the repository's scanner check stays invisible until a delivery is rejected. The scanner kit
(`scanner_kit.py` in the lifecycle-check skill of the lifecycle pack) installs the pinned semgrep and the package `hamh-semgrep-rules` into a virtual
environment and runs the pinned python and bash rules on the files a run changed. The rules are the YAML files of the open-source repository
https://github.com/semgrep/semgrep-rules at one commit. They are third-party content under the Semgrep Rules License, so this repository holds the
pin (`security/scanner-kit.toml`) and this tool, never the rules (`scripts/lib/third_party_scan.py` fails on a copy). An operator builds the package from
the pinned commit and publishes it to the package registry of the stage (a CodeArtifact PyPI repository) that the sessions install from.

  scanner_kit_rules.py build   [--pin FILE] [--source URL_OR_FOLDER] [--out DIR] [--work DIR]
      Fetches the pinned commit (git, over https by default), checks the commit, the tree and the selection of rule files against the pin and
      writes hamh_semgrep_rules-<version>-py3-none-any.whl into DIR. The rule files are the .yaml and .yml files below the pinned folders; test files,
      documents and other folders stay out. The wheel is reproducible: the same pin and the same commit give the same bytes (path order, fixed
      dates and modes, no compression). It prints the name, size and SHA-256 of the wheel. Nothing is written inside this repository: DIR and
      the fetch folder (--work, a new or empty folder; default a fresh folder under $TMPDIR/scanner-kit-rules) are the only places written.
  scanner_kit_rules.py publish --wheel FILE [--apply] [--domain NAME] [--repository NAME]
      A dry run by default: it prints the target (the accounts.json entry that HAMH_AWS_PROFILE names, else the active one; the domain `hamh-direct`
      and the repository `packages` of stage direct) and the wheel, and calls nothing. With --apply it checks that the profile is the entry's
      account, leaves a version that is already there alone, refuses to replace one with other bytes, uploads the wheel to the repository's PyPI
      endpoint with a short-lived CodeArtifact token (Basic authentication, user aws; the token stays in memory and is never printed), reads the
      asset hash back and makes sure the package's upstream origin is blocked, so a public package of the same name can never replace it.

Exit codes: 0 done, 1 a build or publish step refused or failed (the reason is on stderr), 2 a usage error or an unreadable pin. Python 3.8 or later, standard library only.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import subprocess  # nosec B404: starts git and the AWS CLI with argument lists, never through a shell
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, List, Mapping, Optional, Sequence, Tuple

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
sys.path.insert(0, str(HERE / "lib"))
import aws_target  # noqa: E402
import engine_source  # noqa: E402

PACKAGE = "hamh-semgrep-rules"
MODULE = "hamh_semgrep_rules"
LICENCE_NAME = "Semgrep Rules License v1.0"
DEFAULT_PIN = ROOT / "security" / "scanner-kit.toml"
DEFAULT_DOMAIN = "hamh-direct"
DEFAULT_REPOSITORY = "packages"
MAX_RULE_BYTES = 1024 * 1024
TOKEN_SECONDS = 900
TAG = "py3-none-any"

HEX40 = re.compile(r"^[0-9a-f]{40}$")
HEX64 = re.compile(r"^[0-9a-f]{64}$")
VERSION = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
FOLDER = re.compile(r"^[a-z][a-z0-9_-]*$")
RULE_PATH = re.compile(r"^[A-Za-z0-9._@+-]+(?:/[A-Za-z0-9._@+-]+)*$")
URL = re.compile(r"^https://[A-Za-z0-9._~:/?#@!$&'()*+,;=%-]+$")
LICENCE_ID = re.compile(r"^[A-Za-z0-9.+-]+$")
WHEEL_NAME = re.compile(r"^" + MODULE + r"-([0-9]+\.[0-9]+\.[0-9]+)-" + TAG + r"\.whl$")
LOOPBACK = ("127.0.0.1", "localhost", "[::1]", "::1")

TOP_KEYS = {"schema", "name", "version", "license", "description", "source", "rules", "semgrep"}
SOURCE_KEYS = {"url", "commit", "tree"}
RULES_KEYS = {"folders", "files", "bytes", "files_sha256"}

INIT_PY = '''"""The pinned python and bash rules of semgrep-rules for the ham scanner kit.

Third-party content under the Semgrep Rules License (LICENSE.txt); RULES-PIN.txt names the commit it was taken from.
"""

import os

RULES_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "rules")
'''


class PinError(Exception):
    """A pin file that is not exact."""


class Refused(Exception):
    """A build or publish step that stops with a reason."""


class UsageError(Exception):
    """A request the tool cannot read."""


@dataclass(frozen=True)
class Pin:
    name: str
    version: str
    license: str
    description: str
    url: str
    commit: str
    tree: str
    folders: Tuple[str, ...]
    files: int
    bytes: int
    files_sha256: str
    semgrep_version: str


def _table(doc: dict, key: str, allowed: set, where: str) -> dict:
    t = doc.get(key)
    if not isinstance(t, dict):
        raise PinError("%s: the table [%s] is missing" % (where, key))
    extra = sorted(set(t) - allowed)
    if extra:
        raise PinError("%s: [%s] has an unknown key: %s" % (where, key, ", ".join(extra)))
    missing = sorted(allowed - set(t))
    if missing:
        raise PinError("%s: [%s] lacks: %s" % (where, key, ", ".join(missing)))
    return t


def parse_pin(text: str, where: str = "pin") -> Pin:
    """The pin of the rules package from the text of its file; every field is checked and a partial parse never passes."""
    try:
        doc = engine_source.parse(text, where)
    except engine_source.SourceError as exc:
        raise PinError(str(exc)) from None
    extra = sorted(set(doc) - TOP_KEYS)
    if extra:
        raise PinError("%s: unknown key: %s" % (where, ", ".join(extra)))
    if doc.get("schema") != "1":
        raise PinError('%s: schema must be "1"' % where)
    for key in ("name", "version", "license", "description"):
        if not isinstance(doc.get(key), str) or not doc[key]:
            raise PinError("%s: %s must be a non-empty string" % (where, key))
    if doc["name"] != PACKAGE:
        raise PinError("%s: name must be %s" % (where, PACKAGE))
    if not VERSION.match(doc["version"]):
        raise PinError("%s: version must be X.Y.Z" % where)
    if not LICENCE_ID.match(doc["license"]):
        raise PinError("%s: license must be an SPDX id or a LicenseRef id" % where)
    source = _table(doc, "source", SOURCE_KEYS, where)
    if not isinstance(source["url"], str) or not URL.match(source["url"]):
        raise PinError("%s: source.url must be an https url" % where)
    for key in ("commit", "tree"):
        if not isinstance(source[key], str) or not HEX40.match(source[key]):
            raise PinError("%s: source.%s must be a full 40-character lower-case hex id, never a branch or an abbreviation" % (where, key))
    rules = _table(doc, "rules", RULES_KEYS, where)
    folders = rules["folders"]
    if not isinstance(folders, list) or not folders or not all(isinstance(f, str) and FOLDER.match(f) for f in folders) or len(set(folders)) != len(folders):
        raise PinError("%s: rules.folders must be a non-empty list of distinct top-level folder names" % where)
    for key in ("files", "bytes"):
        if not isinstance(rules[key], int) or isinstance(rules[key], bool) or rules[key] <= 0:
            raise PinError("%s: rules.%s must be a positive number" % (where, key))
    if not isinstance(rules["files_sha256"], str) or not HEX64.match(rules["files_sha256"]):
        raise PinError("%s: rules.files_sha256 must be 64 lower-case hex characters" % where)
    semgrep = _table(doc, "semgrep", {"version"}, where)
    if not isinstance(semgrep["version"], str) or not VERSION.match(semgrep["version"]):
        raise PinError("%s: semgrep.version must be X.Y.Z" % where)
    return Pin(name=doc["name"], version=doc["version"], license=doc["license"], description=doc["description"], url=source["url"], commit=source["commit"],
               tree=source["tree"], folders=tuple(folders), files=rules["files"], bytes=rules["bytes"], files_sha256=rules["files_sha256"],
               semgrep_version=semgrep["version"])


def read_pin(path: Path) -> Pin:
    try:
        text = Path(path).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as exc:
        raise PinError("%s: %s" % (path, exc)) from None
    return parse_pin(text, str(path))


# ------------------------------------------------------------------------------------------------------------------ build
def _git(work: Path, *args: str, check: bool = True) -> "subprocess.CompletedProcess":
    env = dict(os.environ)
    env["GIT_TERMINAL_PROMPT"] = "0"
    done = subprocess.run(["git", "-C", str(work), *args], capture_output=True, env=env)  # nosec B603: argument list, no shell, program is git; B607: bare name of a standard tool such as git looked up on PATH
    if check and done.returncode != 0:
        raise Refused("git %s failed: %s" % (args[0], _last_line(done.stderr.decode("utf-8", "replace")) or "exit %d" % done.returncode))
    return done


def _last_line(text: str, limit: int = 200) -> str:
    lines = [ln.strip() for ln in text.splitlines() if ln.strip()]
    return lines[-1][:limit] if lines else ""


def default_source(pin: Pin, option: Optional[str]) -> str:
    return option or pin.url


def default_work(env: Optional[Mapping[str, str]] = None) -> Path:
    """A fresh folder name under $TMPDIR/scanner-kit-rules (not created here)."""
    env = os.environ if env is None else env
    base = Path(env.get("TMPDIR") or tempfile.gettempdir())
    return base / "scanner-kit-rules" / ("fetch-%s-%d" % (time.strftime("%Y%m%d-%H%M%S"), os.getpid()))


def manifest(rules: Mapping[str, bytes]) -> Tuple[int, int, str]:
    """(number of files, total bytes, digest): the digest is the SHA-256 of the lines `<path> TAB <SHA-256 of the file>` in path order."""
    lines = "".join("%s\t%s\n" % (p, hashlib.sha256(rules[p]).hexdigest()) for p in sorted(rules))
    return len(rules), sum(len(b) for b in rules.values()), hashlib.sha256(lines.encode("utf-8")).hexdigest()


@dataclass
class Fetched:
    commit: str
    tree: str
    rules: Dict[str, bytes]
    licence: bytes


def fetch_source(pin: Pin, source: str, work: Path) -> Fetched:
    """The rule files and the licence text of the pinned commit, read from the git objects (nothing is checked out or run).

    Refuses a commit or a tree that differs from the pin, a rule file that is a link or not plain text, a pinned folder with no rule file, a source with
    no LICENSE file and a selection (number of files, bytes, digest) that differs from the pin.
    """
    work = Path(work)
    if work.exists() and (not work.is_dir() or any(work.iterdir())):
        raise Refused("--work must be a new or empty folder: %s" % work)
    work.mkdir(parents=True, exist_ok=True)
    where = source
    if "://" not in source:
        folder = Path(source)
        if not folder.is_dir():
            raise Refused("the source %s is neither a url nor a folder" % source)
        where = folder.resolve().as_uri()
    _git(work, "init", "-q", ".")
    got = _git(work, "fetch", "-q", "--depth", "1", "--no-tags", where, pin.commit, check=False)
    if got.returncode != 0:
        raise Refused("could not fetch commit %s from %s: %s" % (pin.commit, source, _last_line(got.stderr.decode("utf-8", "replace")) or "exit %d" % got.returncode))
    commit = _git(work, "rev-parse", "FETCH_HEAD").stdout.decode("ascii").strip()
    if commit != pin.commit:
        raise Refused("the source gave commit %s and the pin says %s" % (commit, pin.commit))
    tree = _git(work, "rev-parse", "FETCH_HEAD^{tree}").stdout.decode("ascii").strip()
    if tree != pin.tree:
        raise Refused("the tree of commit %s is %s and the pin says %s" % (commit, tree, pin.tree))
    licence_blob = _git(work, "cat-file", "blob", "FETCH_HEAD:LICENSE", check=False)
    if licence_blob.returncode != 0 or not licence_blob.stdout:
        raise Refused("the source has no LICENSE file at commit %s" % commit)
    listing = _git(work, "ls-tree", "-r", "-z", "--full-tree", "FETCH_HEAD", "--", *pin.folders).stdout
    rules: Dict[str, bytes] = {}
    for record in listing.split(b"\0"):
        if not record:
            continue
        meta, _, raw_path = record.partition(b"\t")
        mode, kind, sha = meta.decode("ascii").split(" ")
        path = raw_path.decode("utf-8", "strict")
        if not path.endswith((".yaml", ".yml")):
            continue
        if mode == "120000":
            raise Refused("the rule file %s is a link; a link is never followed" % path)
        if kind != "blob" or mode not in ("100644", "100755"):
            continue
        if not RULE_PATH.match(path):
            raise Refused("the rule file %s has a name outside the plain set of characters" % path)
        data = _git(work, "cat-file", "blob", sha).stdout
        if len(data) > MAX_RULE_BYTES or b"\0" in data:
            raise Refused("the rule file %s is not plain text under %d bytes" % (path, MAX_RULE_BYTES))
        try:
            data.decode("utf-8")
        except UnicodeDecodeError:
            raise Refused("the rule file %s is not UTF-8 text" % path) from None
        rules[path] = data
    for folder in pin.folders:
        if not any(p.startswith(folder + "/") for p in rules):
            raise Refused("no rule file in the pinned folder %s" % folder)
    n, size, digest = manifest(rules)
    if (n, size, digest) != (pin.files, pin.bytes, pin.files_sha256):
        raise Refused("the selection differs from the pin: files %d (pin %d), bytes %d (pin %d), manifest %s (pin %s)"
                      % (n, pin.files, size, pin.bytes, digest, pin.files_sha256))
    return Fetched(commit=commit, tree=tree, rules=rules, licence=licence_blob.stdout)


def pin_note(pin: Pin, fetched: Fetched) -> str:
    n, size, digest = manifest(fetched.rules)
    return (
        "%s %s\n"
        "source: %s\n"
        "commit: %s\n"
        "tree: %s\n"
        "folders: %s\n"
        "files: %d\n"
        "bytes: %d\n"
        "manifest: %s\n"
        "semgrep: %s\n"
        "license: %s (LICENSE.txt in this folder). Third-party content; the ham repository does not contain it.\n"
    ) % (pin.name, pin.version, pin.url, fetched.commit, fetched.tree, ", ".join(pin.folders), n, size, digest, pin.semgrep_version, LICENCE_NAME)


def metadata_text(pin: Pin) -> str:
    return (
        "Metadata-Version: 2.1\n"
        "Name: %s\n"
        "Version: %s\n"
        "Summary: Pinned python and bash rules of semgrep-rules for the ham scanner kit\n"
        "License: %s (see %s/LICENSE.txt)\n"
        "Requires-Python: >=3.8\n"
    ) % (pin.name, pin.version, LICENCE_NAME, MODULE)


WHEEL_TEXT = "Wheel-Version: 1.0\nGenerator: scanner_kit_rules\nRoot-Is-Purelib: true\nTag: " + TAG + "\n"


def _record_digest(data: bytes) -> str:
    return "sha256=" + base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode("ascii")


def wheel_files(pin: Pin, fetched: Fetched) -> Dict[str, bytes]:
    dist = "%s-%s.dist-info" % (MODULE, pin.version)
    files = {
        MODULE + "/__init__.py": INIT_PY.encode("ascii"),
        MODULE + "/LICENSE.txt": fetched.licence,
        MODULE + "/RULES-PIN.txt": pin_note(pin, fetched).encode("utf-8"),
        dist + "/METADATA": metadata_text(pin).encode("utf-8"),
        dist + "/WHEEL": WHEEL_TEXT.encode("ascii"),
    }
    for path, data in fetched.rules.items():
        files["%s/rules/%s" % (MODULE, path)] = data
    return files


def build_wheel(pin: Pin, fetched: Fetched, out_dir: Path) -> Path:
    """Writes the wheel (entries in path order, the RECORD last, fixed dates and modes, stored not compressed) and returns its path."""
    files = wheel_files(pin, fetched)
    dist = "%s-%s.dist-info" % (MODULE, pin.version)
    record = "".join("%s,%s,%d\n" % (p, _record_digest(files[p]), len(files[p])) for p in sorted(files)) + "%s/RECORD,,\n" % dist
    files[dist + "/RECORD"] = record.encode("utf-8")
    names = sorted(p for p in files if not p.endswith("/RECORD")) + [dist + "/RECORD"]
    out_dir = Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    final = out_dir / ("%s-%s-%s.whl" % (MODULE, pin.version, TAG))
    partial = out_dir / (".%s.partial-%d" % (final.name, os.getpid()))
    with zipfile.ZipFile(str(partial), "w", zipfile.ZIP_STORED) as z:
        for name in names:
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_STORED
            info.create_system = 3
            info.external_attr = 0o100644 << 16
            z.writestr(info, files[name])
    os.replace(str(partial), str(final))
    return final


def cmd_build(args: argparse.Namespace, env: Mapping[str, str], out) -> int:
    pin = read_pin(Path(args.pin))
    work = Path(args.work) if args.work else default_work(env)
    fetched = fetch_source(pin, default_source(pin, args.source), work)
    n, size, digest = manifest(fetched.rules)
    out.write("SCANNER-KIT-RULES source commit %s tree %s\n" % (fetched.commit, fetched.tree))
    out.write("SCANNER-KIT-RULES rules files=%d bytes=%d manifest=%s\n" % (n, size, digest))
    out_dir = Path(args.out) if args.out else work.parent / "dist"
    wheel = build_wheel(pin, fetched, out_dir)
    data = wheel.read_bytes()
    out.write("SCANNER-KIT-RULES WHEEL %s bytes=%d sha256=%s\n" % (wheel, len(data), hashlib.sha256(data).hexdigest()))
    out.write("SCANNER-KIT-RULES next: python3 scripts/scanner_kit_rules.py publish --wheel %s   (a dry run; add --apply to publish)\n" % wheel)
    out.write("SCANNER-KIT-RULES PASS\n")
    return 0


# ---------------------------------------------------------------------------------------------------------------- publish
class Aws:
    """The AWS CLI of one accounts.json entry: every call names the profile and the Region."""

    def __init__(self, target, env: Mapping[str, str]) -> None:
        self.target = target
        self.env = dict(env)
        self.env["AWS_PAGER"] = ""

    def run(self, args: Sequence[str], timeout: int = 120) -> Tuple[int, str, str]:
        cmd = ["aws", "--profile", self.target.profile, "--region", self.target.region, *args]
        try:
            done = subprocess.run(cmd, capture_output=True, env=self.env, timeout=timeout)  # nosec B603: argument list, no shell, program is the AWS CLI; B607: bare name of a standard tool such as aws looked up on PATH  # nosemgrep: dangerous-subprocess-use-audit -- argument list built here from the profile, the Region and fixed AWS CLI words, no shell; the program is the AWS CLI
        except FileNotFoundError:
            raise Refused("the AWS CLI (aws) is not installed") from None
        except subprocess.TimeoutExpired:
            raise Refused("the AWS CLI did not answer in %d seconds" % timeout) from None
        return done.returncode, done.stdout.decode("utf-8", "replace"), done.stderr.decode("utf-8", "replace")

    def json(self, args: Sequence[str], what: str) -> dict:
        rc, so, se = self.run([*args, "--output", "json"])
        if rc != 0:
            raise Refused("%s failed: %s" % (what, _last_line(se) or "exit %d" % rc))
        try:
            doc = json.loads(so)
        except ValueError:
            raise Refused("%s gave no JSON" % what) from None
        if not isinstance(doc, dict):
            raise Refused("%s gave an unexpected answer" % what)
        return doc


def wheel_identity(wheel: Path) -> Tuple[str, str, str]:
    """(version, name of the file, SHA-256) of a wheel of this package; a UsageError for anything else."""
    wheel = Path(wheel)
    if not wheel.is_file():
        raise UsageError("no such wheel: %s" % wheel)
    m = WHEEL_NAME.match(wheel.name)
    if not m:
        raise UsageError("the wheel must be named %s-X.Y.Z-%s.whl, not %s" % (MODULE, TAG, wheel.name))
    try:
        with zipfile.ZipFile(str(wheel)) as z:
            meta = z.read("%s-%s.dist-info/METADATA" % (MODULE, m.group(1))).decode("utf-8")
    except (zipfile.BadZipFile, KeyError, OSError) as exc:
        raise UsageError("%s is not a wheel of this package: %s" % (wheel.name, exc)) from None
    fields = dict(ln.split(": ", 1) for ln in meta.splitlines() if ": " in ln)
    if fields.get("Name") != PACKAGE or fields.get("Version") != m.group(1):
        raise UsageError("%s: the METADATA says %s %s" % (wheel.name, fields.get("Name"), fields.get("Version")))
    return m.group(1), wheel.name, hashlib.sha256(wheel.read_bytes()).hexdigest()


def _assets(aws: Aws, domain: str, repository: str, version: str) -> Optional[Dict[str, str]]:
    """{file name: SHA-256} of the assets of the package version, or None when the version is not in the repository."""
    rc, so, se = aws.run(["codeartifact", "list-package-version-assets", "--domain", domain, "--domain-owner", aws.target.account,
                          "--repository", repository, "--format", "pypi", "--package", PACKAGE, "--package-version", version, "--output", "json"])
    if rc != 0:
        if "ResourceNotFoundException" in se:
            return None
        raise Refused("list-package-version-assets failed: %s" % (_last_line(se) or "exit %d" % rc))
    try:
        assets = json.loads(so).get("assets", [])
        return {a["name"]: a.get("hashes", {}).get("SHA-256", "") for a in assets}
    except (ValueError, KeyError, TypeError, AttributeError):
        raise Refused("list-package-version-assets gave an unexpected answer") from None


def _endpoint_ok(url: str) -> bool:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme == "https" and parts.hostname:
        return True
    return parts.scheme == "http" and (parts.hostname or "") in LOOPBACK


def _field(name: str, value: str, boundary: str) -> bytes:
    return ('--%s\r\nContent-Disposition: form-data; name="%s"\r\n\r\n%s\r\n' % (boundary, name, value)).encode("utf-8")


def upload(endpoint: str, token: str, wheel: Path, version: str, sha256: str) -> Tuple[int, str]:
    """The legacy upload request that twine sends (multipart form, Basic authentication with the user aws and the token as the password)."""
    data = Path(wheel).read_bytes()
    boundary = "----hamhkit" + hashlib.sha256(os.urandom(16)).hexdigest()[:24]
    fields = [(":action", "file_upload"), ("protocol_version", "1"), ("metadata_version", "2.1"), ("name", PACKAGE), ("version", version),
              ("summary", "Pinned python and bash rules of semgrep-rules for the ham scanner kit"), ("license", LICENCE_NAME),
              ("requires_python", ">=3.8"), ("filetype", "bdist_wheel"), ("pyversion", "py3"), ("sha256_digest", sha256),
              ("blake2_256_digest", hashlib.blake2b(data, digest_size=32).hexdigest())]
    body = b"".join(_field(k, v, boundary) for k, v in fields)
    body += ('--%s\r\nContent-Disposition: form-data; name="content"; filename="%s"\r\nContent-Type: application/octet-stream\r\n\r\n'
             % (boundary, Path(wheel).name)).encode("utf-8") + data + ("\r\n--%s--\r\n" % boundary).encode("ascii")
    auth = base64.b64encode(("aws:" + token).encode("utf-8")).decode("ascii")
    request = urllib.request.Request(endpoint, data=body, method="POST", headers={
        "Authorization": "Basic " + auth, "Content-Type": "multipart/form-data; boundary=" + boundary, "User-Agent": "scanner_kit_rules/1"})
    try:
        with urllib.request.urlopen(request, timeout=300) as answer:  # nosec B310: the scheme is https, or http on the loopback address in a test (checked by _endpoint_ok)  # nosemgrep: dynamic-urllib-use-detected -- the URL is the repository endpoint that the AWS CLI returned and _endpoint_ok accepted https, or loopback in a test
            return answer.status, answer.read(2000).decode("utf-8", "replace")
    except urllib.error.HTTPError as exc:
        return exc.code, exc.read(2000).decode("utf-8", "replace")
    except (urllib.error.URLError, OSError) as exc:
        raise Refused("the upload did not reach the repository: %s" % (getattr(exc, "reason", None) or exc)) from None


def cmd_publish(args: argparse.Namespace, env: Mapping[str, str], out, err) -> int:
    version, name, sha = wheel_identity(Path(args.wheel))
    size = Path(args.wheel).stat().st_size
    try:
        target = aws_target.select(env)
    except aws_target.TargetError as exc:
        raise UsageError(str(exc)) from None
    domain, repository = args.domain, args.repository
    plan = ["profile %s account %s region %s" % (target.profile, target.account, target.region), "domain %s repository %s" % (domain, repository),
            "package %s %s" % (PACKAGE, version), "file %s bytes=%d sha256=%s" % (name, size, sha)]
    if not args.apply:
        out.write("SCANNER-KIT-RULES DRY RUN publish: nothing is sent and no AWS call is made\n")
        for line in plan:
            out.write("SCANNER-KIT-RULES %s\n" % line)
        out.write("SCANNER-KIT-RULES to publish, run the same command again with --apply\n")
        return 0
    aws = Aws(target, env)
    rc, so, se = aws.run(["configure", "get", "cli_history"])
    if rc == 0 and so.strip().lower() == "enabled":
        raise Refused("the AWS CLI history is on for the profile %s (cli_history = enabled): the CLI would record the token in its history file. Turn it off first: "
                      "aws configure set cli_history disabled --profile %s" % (target.profile, target.profile))
    if rc not in (0, 1) or (rc == 1 and (so.strip() or se.strip())):
        raise Refused("could not read the cli_history setting of the profile %s" % target.profile)
    who = aws.json(["sts", "get-caller-identity"], "sts get-caller-identity")
    if who.get("Account") != target.account:
        raise Refused("the profile %s is account %s and accounts.json says %s; nothing was sent" % (target.profile, who.get("Account"), target.account))
    have = _assets(aws, domain, repository, version)
    if have is not None:
        if have.get(name) == sha:
            out.write("SCANNER-KIT-RULES ALREADY-PUBLISHED %s %s sha256=%s\n" % (PACKAGE, version, sha))
            return 0
        raise Refused("version %s of %s is already in %s/%s with sha256 %s and this wheel is %s; a published version never changes: raise the version in the pin and build again"
                      % (version, PACKAGE, domain, repository, ",".join(sorted(h for h in have.values() if h)) or "none", sha))
    grant = aws.json(["codeartifact", "get-authorization-token", "--domain", domain, "--domain-owner", target.account, "--duration-seconds", str(TOKEN_SECONDS)],
                     "get-authorization-token")
    token = grant.get("authorizationToken")
    if not isinstance(token, str) or not token:
        raise Refused("get-authorization-token gave no token")
    ep = aws.json(["codeartifact", "get-repository-endpoint", "--domain", domain, "--domain-owner", target.account, "--repository", repository, "--format", "pypi"],
                  "get-repository-endpoint")
    endpoint = ep.get("repositoryEndpoint")
    if not isinstance(endpoint, str) or not _endpoint_ok(endpoint):
        raise Refused("the repository endpoint is not https: nothing was sent")
    status, body = upload(endpoint, token, Path(args.wheel), version, sha)
    if not 200 <= status < 300:
        raise Refused("the repository answered %d: %s" % (status, body.replace(token, "<token>")[:200].strip()))
    after = _assets(aws, domain, repository, version)
    if after is None or after.get(name) != sha:
        raise Refused("could not read back %s %s with sha256 %s from %s/%s after the upload" % (PACKAGE, version, sha, domain, repository))
    info = aws.json(["codeartifact", "describe-package", "--domain", domain, "--domain-owner", target.account, "--repository", repository, "--format", "pypi",
                     "--package", PACKAGE], "describe-package")
    restrictions = ((info.get("package") or {}).get("originConfiguration") or {}).get("restrictions") or {}
    if restrictions.get("upstream") != "BLOCK":
        aws.json(["codeartifact", "put-package-origin-configuration", "--domain", domain, "--domain-owner", target.account, "--repository", repository,
                  "--format", "pypi", "--package", PACKAGE, "--restrictions", "publish=ALLOW,upstream=BLOCK"], "put-package-origin-configuration")
        out.write("SCANNER-KIT-RULES origin: the upstream of %s is now blocked\n" % PACKAGE)
    out.write("SCANNER-KIT-RULES PUBLISHED %s %s sha256=%s\n" % (PACKAGE, version, sha))
    return 0


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="scanner_kit_rules.py", description="Build and publish the rules package of the scanner kit.", allow_abbrev=False)
    sub = p.add_subparsers(dest="command", required=True)
    b = sub.add_parser("build", help="fetch the pinned commit of the rules and write the wheel")
    b.add_argument("--pin", default=str(DEFAULT_PIN), help="the pin file (default: security/scanner-kit.toml)")
    b.add_argument("--source", default=None, help="a git url or a folder (default: the url of the pin)")
    b.add_argument("--out", default=None, help="the folder for the wheel (default: dist beside the fetch folder)")
    b.add_argument("--work", default=None, help="a new or empty folder for the fetch (default: a fresh folder under $TMPDIR/scanner-kit-rules)")
    pub = sub.add_parser("publish", help="upload the wheel to the stage's CodeArtifact PyPI repository (a dry run unless --apply)")
    pub.add_argument("--wheel", required=True, help="the wheel that build wrote")
    pub.add_argument("--apply", action="store_true", help="really publish (the default prints the plan and calls nothing)")
    pub.add_argument("--domain", default=DEFAULT_DOMAIN, help="the CodeArtifact domain of the stage (default: %s)" % DEFAULT_DOMAIN)
    pub.add_argument("--repository", default=DEFAULT_REPOSITORY, help="the repository sessions install from (default: %s)" % DEFAULT_REPOSITORY)
    return p


def main(argv: Optional[Sequence[str]] = None, env: Optional[Mapping[str, str]] = None, out=None, err=None) -> int:
    out = out or sys.stdout
    err = err or sys.stderr
    env = os.environ if env is None else env
    args = parser().parse_args(argv)
    try:
        if args.command == "build":
            return cmd_build(args, env, out)
        return cmd_publish(args, env, out, err)
    except PinError as exc:
        err.write("scanner_kit_rules.py: %s\n" % exc)
        return 2
    except UsageError as exc:
        err.write("scanner_kit_rules.py: %s\n" % exc)
        return 2
    except Refused as exc:
        err.write("scanner_kit_rules.py: refused: %s\n" % exc)
        return 1


if __name__ == "__main__":
    sys.exit(main())
