#!/usr/bin/env python3
"""engine_recipes.py: the build recipes behind `engine-fetch build`, the plugin lock, and the offline `check`.

Recipes (the `recipe` field of a source file):
  esbuild-bundle   the goal plugin folder. The plugin source and its licence come from github.com/cline/plugins at the pinned commit; createTool of the
                   Cline SDK comes verbatim from the Cline source (the `[[inputs]]` of engine/sources/goal-plugin.toml). They are laid out in a fixed
                   relative tree beside a few small files we wrote (engine/sources/recipes/goal-plugin/) and bundled by the pinned esbuild binary
                   (engine/sources/esbuild.toml) with fixed arguments, from that tree as the working directory, in a clean environment. esbuild's
                   metafile must list exactly the expected inputs. The five files of the folder (index.js, LICENSE, NOTICE, package.json, PROVENANCE)
                   are then compared with the SHA-256 values of engine/plugins/lock.json; a difference, a missing file or an extra file is an error
                   (exit 6). NOTICE, package.json and PROVENANCE are rendered from tracked data only, so `check` verifies them offline.
  cline-fork       scripts/build-cline-fork.sh (the fork build with its own verification of the commit, the patch series, the tree and the toolchain).
  none             nothing to build; `build` fetches.

The plugin lock engine/plugins/lock.json (version 1) keeps what agentd reads at the start of a run, `files` (the SHA-256 of every shipped file of
both plugin folders), and a `sources` record of the goal plugin build: the bundler, the SHA-256 of its two inputs, the SHA-256 of esbuild's metafile
and of the recipe files. `build` never writes it. `update` is the only writer. The goal files are not in the repository; the image build makes them
(images/codebuild/worker-python.yml) and verifies them against this file before the image is built.

Python 3.8+, standard library only.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import sys
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

sys.path.insert(0, str(Path(__file__).resolve().parent))
import engine_fetch as ef  # noqa: E402
import engine_source as es  # noqa: E402
from engine_fetch import FetchError, sha256_bytes, sha256_file  # noqa: E402

LOCK_REL = "engine/plugins/lock.json"
PLUGIN = "goal-plugin"
RECIPE_REL = "engine/sources/recipes/goal-plugin"

PLUGIN_SOURCE_AS = "goal-index.ts"
SDK_SOURCE_AS = "cline/shared/src/tools/create.ts"
LICENSE_AS = "LICENSE"
CLINE_LICENSE_AS = "cline/LICENSE"

# The recipe files we wrote, and where each lands in the build layout (the working directory of esbuild).
LAYOUT_FILES = {
    "sdk.mjs": "sdk.mjs",
    "zod-stub.mjs": "stubs/zod.mjs",
    "parse-zod-stub.ts": "cline/shared/src/parse/zod.ts",
    "tsconfig.json": "tsconfig.json",
    "scratch-package.tmpl": "package.json",
}
SHIPPED_TEMPLATES = {"plugin-package.tmpl": "package.json", "plugin-notice.tmpl": "NOTICE"}
RECIPE_FILES = sorted(list(LAYOUT_FILES) + list(SHIPPED_TEMPLATES))

# The arguments are fixed and complete: the defaults that matter for the bytes are written out so a new esbuild default cannot change them.
ESBUILD_ARGS = [
    "goal-index.ts", "--bundle", "--platform=node", "--format=esm", "--target=node22",
    "--alias:@cline/sdk=./sdk.mjs", "--alias:zod=./stubs/zod.mjs", "--tsconfig=./tsconfig.json",
    "--charset=ascii", "--legal-comments=eof", "--tree-shaking=true", "--log-level=warning",
    "--metafile=../meta.json", "--outfile=../index.js",
]
# The same layout bundled from sdk.mjs alone: one ES module that exports createTool. The goal-plugin probe loads the plugin's TypeScript
# source against it; it is not shipped and not in the lock.
SDK_MODULE_ARGS = [
    "sdk.mjs", "--bundle", "--platform=node", "--format=esm", "--target=node22", "--alias:zod=./stubs/zod.mjs", "--tsconfig=./tsconfig.json",
    "--charset=ascii", "--legal-comments=eof", "--tree-shaking=true", "--log-level=warning", "--outfile=../sdk-module.mjs",
]
EXPECTED_INPUTS = {"goal-index.ts", "sdk.mjs", "stubs/zod.mjs", "cline/shared/src/tools/create.ts", "cline/shared/src/parse/zod.ts"}
SHIPPED_GOAL = ("index.js", "LICENSE", "NOTICE", "package.json", "PROVENANCE")


# ---- the plugin lock ----------------------------------------------------------------------------


def canonical(lock: Dict[str, Any]) -> str:
    return json.dumps(lock, indent=2, sort_keys=True) + "\n"


def read_lock(root: Path) -> Dict[str, Any]:
    p = Path(root) / LOCK_REL
    try:
        lock = json.loads(p.read_text(encoding="utf-8"))
    except (OSError, ValueError) as e:
        raise FetchError(ef.LINT, "%s: %s" % (LOCK_REL, e))
    if lock.get("version") != 1 or not isinstance(lock.get("files"), dict):
        raise FetchError(ef.LINT, "%s: version 1 and a files object are required" % LOCK_REL)
    return lock


def write_lock(root: Path, lock: Dict[str, Any]) -> None:
    (Path(root) / LOCK_REL).write_text(canonical(lock), encoding="utf-8")


# ---- facts, templates and the rendered files ----------------------------------------------------


def _file(src: es.Source, table: str, dest: str) -> Dict[str, Any]:
    for t in src.tables(table):
        if t["as"] == dest:
            return t
    raise FetchError(ef.LINT, "%s lists no %s as %s" % (src.name, table, dest))


def plugin_facts(registry: Dict[str, es.Source]) -> Dict[str, str]:
    """Everything the notice and the provenance say about the sources, read from the source files only."""
    for need in (PLUGIN, "cline", "esbuild"):
        if need not in registry:
            raise FetchError(ef.LINT, "engine/sources has no %s.toml" % need)
    goal, cline, esb = registry[PLUGIN], registry["cline"], registry["esbuild"]
    plugin = _file(goal, "files", PLUGIN_SOURCE_AS)
    sdk = _file(goal, "inputs", SDK_SOURCE_AS)
    return {
        "plugin_repository": goal.table("source")["url"],
        "plugin_commit": goal.table("source")["commit"],
        "plugin_path": plugin["path"],
        "plugin_sha256": plugin["sha256"],
        "sdk_repository": cline.table("source")["url"],
        "sdk_tag": cline.table("source")["tag"],
        "sdk_commit": cline.table("source")["commit"],
        "sdk_path": sdk["path"],
        "sdk_sha256": sdk["sha256"],
        "license": goal.license,
        "bundler": "esbuild %s" % esb.raw.get("version", "?"),
    }


def recipe_dir(root: Path) -> Path:
    return Path(root) / RECIPE_REL


def recipe_digest(root: Path) -> str:
    """One SHA-256 over the recipe files we wrote and the fixed esbuild arguments."""
    h = hashlib.sha256()
    for name in RECIPE_FILES:
        h.update(("%s %s\n" % (name, sha256_file(recipe_dir(root) / name))).encode())
    h.update(("args %s\n" % " ".join(ESBUILD_ARGS)).encode())
    return h.hexdigest()


def render_template(root: Path, name: str, facts: Dict[str, str]) -> bytes:
    text = (recipe_dir(root) / name).read_text(encoding="utf-8")
    for k, v in facts.items():
        text = text.replace("{%s}" % k, v)
    return text.encode("utf-8")


def render_provenance(facts: Dict[str, str], record: Dict[str, Any]) -> bytes:
    """key=value lines in a fixed order: UTF-8, LF, no time, no host, no path, no architecture."""
    lines = [
        "# Where the goal plugin folder comes from. Rendered by scripts/engine-fetch from engine/sources/goal-plugin.toml and engine/plugins/lock.json; do not edit.",
        "plugin_repository=" + facts["plugin_repository"],
        "plugin_commit=" + facts["plugin_commit"],
        "plugin_path=" + facts["plugin_path"],
        "plugin_sha256=" + facts["plugin_sha256"],
        "sdk_repository=" + facts["sdk_repository"],
        "sdk_tag=" + facts["sdk_tag"],
        "sdk_commit=" + facts["sdk_commit"],
        "sdk_path=" + facts["sdk_path"],
        "sdk_sha256=" + facts["sdk_sha256"],
        "license=" + facts["license"],
        "bundler=" + facts["bundler"],
        "bundler_args=" + " ".join(ESBUILD_ARGS),
        "recipe_sha256=" + record["recipe_sha256"],
        "metafile_sha256=" + record["metafile_sha256"],
        "bundle_sha256=" + record["bundle_sha256"],
        "bundle_bytes=" + str(record["bundle_bytes"]),
    ]
    return ("\n".join(lines) + "\n").encode("utf-8")


def render_goal(root: Path, facts: Dict[str, str], bundle: bytes, license_text: bytes, metafile_sha256: str) -> Dict[str, bytes]:
    """The five files of the goal plugin folder."""
    record = {"recipe_sha256": recipe_digest(root), "metafile_sha256": metafile_sha256, "bundle_sha256": sha256_bytes(bundle), "bundle_bytes": len(bundle)}
    return {
        "index.js": bundle,
        "LICENSE": license_text,
        "NOTICE": render_template(root, "plugin-notice.tmpl", facts),
        "package.json": render_template(root, "plugin-package.tmpl", facts),
        "PROVENANCE": render_provenance(facts, record),
    }


# ---- esbuild-bundle -----------------------------------------------------------------------------


def lay_out(root: Path, layout: Path) -> None:
    for name, where in LAYOUT_FILES.items():
        data = (recipe_dir(root) / name).read_bytes()
        ef.write_file(layout / where, data)


def run_esbuild(esbuild: Path, layout: Path, work: Path) -> Tuple[bytes, bytes]:
    """esbuild in a clean environment with the working directory `layout`; returns the bundle and the metafile bytes."""
    env = {"PATH": "/usr/bin:/bin", "HOME": str(work), "TMPDIR": str(work), "LC_ALL": "C"}
    p = ef.run([str(esbuild), *ESBUILD_ARGS], cwd=layout, env=env)
    if p.returncode != 0 or p.stderr.strip():
        raise FetchError(ef.RECIPE, "esbuild exited %d: %s" % (p.returncode, ef.tail(p.stderr + p.stdout)))
    try:
        bundle = (work / "index.js").read_bytes()
        meta = (work / "meta.json").read_bytes()
    except OSError as e:
        raise FetchError(ef.RECIPE, "esbuild wrote no output: %s" % e)
    return bundle, meta


def run_esbuild_sdk(esbuild: Path, layout: Path, work: Path) -> bytes:
    """The createTool module of the layout as one ES module (esbuild started like run_esbuild)."""
    env = {"PATH": "/usr/bin:/bin", "HOME": str(work), "TMPDIR": str(work), "LC_ALL": "C"}
    p = ef.run([str(esbuild), *SDK_MODULE_ARGS], cwd=layout, env=env)
    if p.returncode != 0 or p.stderr.strip():
        raise FetchError(ef.RECIPE, "esbuild (sdk module) exited %d: %s" % (p.returncode, ef.tail(p.stderr + p.stdout)))
    try:
        return (work / "sdk-module.mjs").read_bytes()
    except OSError as e:
        raise FetchError(ef.RECIPE, "esbuild wrote no sdk module: %s" % e)


def check_metafile(meta: bytes) -> None:
    try:
        inputs = set(json.loads(meta)["inputs"])
    except (ValueError, KeyError, TypeError) as e:
        raise FetchError(ef.RECIPE, "esbuild metafile unreadable: %s" % e)
    if inputs != EXPECTED_INPUTS:
        extra, missing = sorted(inputs - EXPECTED_INPUTS), sorted(EXPECTED_INPUTS - inputs)
        raise FetchError(ef.RECIPE, "esbuild bundled other files than expected (extra %s, missing %s): an import of the plugin or of createTool changed" % (extra, missing))


def build_goal_plugin(root: Path, registry: Dict[str, es.Source], fetcher: "ef.Fetcher", dest: Path, platform: str, esbuild: Optional[Path] = None,
                      lock: Optional[Dict[str, Any]] = None, refresh: bool = False, sdk_module: bool = False) -> Dict[str, Any]:
    """Fetches the inputs, bundles and renders the folder `<dest>/goal/`. Unless `refresh`, every file must equal the plugin lock. Returns the facts of
    the build for the lock record (`update`) and the receipt."""
    root = Path(root)
    lock = lock if lock is not None else (None if refresh else read_lock(root))
    facts = plugin_facts(registry)
    goal = registry[PLUGIN]
    with ef.staged(Path(dest)) as out, fetcher.scratch() as work:
        layout = work / "src"
        fetcher.git_path_bundle(goal, layout)
        if (layout / LICENSE_AS).read_bytes() != (layout / CLINE_LICENSE_AS).read_bytes():
            raise FetchError(ef.HASH, "the licence text of cline/plugins and of cline/cline differ: the notice says they are the same text")
        (layout / CLINE_LICENSE_AS).unlink()
        lay_out(root, layout)
        if esbuild is None:
            got = fetcher.release_asset(registry["esbuild"], work / "esbuild-bin", platform)
            esbuild = work / "esbuild-bin" / got["file"]
        bundle, meta = run_esbuild(Path(esbuild), layout, work)
        check_metafile(meta)
        files = render_goal(root, facts, bundle, (layout / LICENSE_AS).read_bytes(), sha256_bytes(meta))
        if lock is not None:
            compare_files(lock, {"goal/" + k: v for k, v in files.items()})
        folder = out / "goal"
        for name, data in files.items():
            ef.write_file(folder / name, data)
        if sdk_module:
            ef.write_file(out / "sdk-module.mjs", run_esbuild_sdk(Path(esbuild), layout, work))
        record = {
            "bundler": facts["bundler"],
            "inputs": {PLUGIN_SOURCE_AS: facts["plugin_sha256"], SDK_SOURCE_AS: facts["sdk_sha256"]},
            "metafile_sha256": sha256_bytes(meta),
            "bundle_bytes": len(bundle),
            "recipe_sha256": recipe_digest(root),
        }
        ef.write_file(out / "goal-plugin.receipt.json", (canonical({"platform": platform, "esbuild_sha256": sha256_file(Path(esbuild)), "record": record,
                                                                        "files": {k: sha256_bytes(v) for k, v in sorted(files.items())}})).encode())
    return {"name": PLUGIN, "files": {k: sha256_bytes(v) for k, v in files.items()}, "record": record}


def compare_files(lock: Dict[str, Any], produced: Dict[str, bytes]) -> None:
    """The produced files (keys like goal/index.js) against the lock's files for the same folder; any difference is exit 6."""
    want = {k: v for k, v in lock["files"].items() if k.split("/", 1)[0] in {p.split("/", 1)[0] for p in produced}}
    problems = []  # type: List[str]
    for k in sorted(set(want) | set(produced)):
        if k not in produced:
            problems.append("%s is in the lock but was not produced" % k)
        elif k not in want:
            problems.append("%s was produced but the lock does not list it" % k)
        elif sha256_bytes(produced[k]) != want[k]:
            problems.append("%s: built %s, the lock has %s" % (k, sha256_bytes(produced[k])[:12], want[k][:12]))
    if problems:
        raise FetchError(ef.OUTPUT, "; ".join(problems))


def verify_goal_plugin(root: Path, directory: Path, lock: Optional[Dict[str, Any]] = None) -> Dict[str, Any]:
    """The folder `<directory>/goal/` against the plugin lock: every file present with its SHA-256, nothing else, no link, no sub-folder."""
    lock = lock if lock is not None else read_lock(root)
    folder = Path(directory) / "goal"
    if not folder.is_dir():
        raise FetchError(ef.OUTPUT, "%s is not a directory" % folder)
    found = {}  # type: Dict[str, bytes]
    for p in sorted(folder.rglob("*")):
        rel = p.relative_to(Path(directory)).as_posix()
        if p.is_symlink() or p.is_dir():
            raise FetchError(ef.OUTPUT, "%s is a link or a directory; the folder holds plain files only" % rel)
        found[rel] = p.read_bytes()
    compare_files(lock, found)
    return {"name": PLUGIN, "files": len(found)}


# ---- cline-fork ---------------------------------------------------------------------------------


def build_cline_fork(root: Path, dest: Path, target: Optional[str], work_dir: Optional[Path]) -> Dict[str, Any]:
    """scripts/build-cline-fork.sh, which verifies the commit, the series, the tree and the toolchain itself and prints FORK-BUILD PASS."""
    script = Path(root) / "scripts" / "build-cline-fork.sh"
    work = Path(work_dir) if work_dir else Path(os.environ.get("CLINE_FORK_WORK_DIR", ""))
    if not str(work):
        raise FetchError(ef.USAGE, "the Cline fork build needs --work-dir or CLINE_FORK_WORK_DIR (a directory under the workspace tmp)")
    argv = ["bash", str(script), "--work-dir", str(work), "--out", str(dest)]
    if target:
        argv += ["--target", target]
    p = ef.run(argv, cwd=Path(root))
    lines = p.stdout.decode("utf-8", "replace").strip().splitlines()
    last = lines[-1] if lines else ""
    if p.returncode != 0 or not last.startswith("FORK-BUILD PASS"):
        raise FetchError(ef.RECIPE, "build-cline-fork.sh exited %d: %s" % (p.returncode, ef.tail(p.stderr) or last))
    return {"name": "cline", "last_line": last}


# ---- check --------------------------------------------------------------------------------------


def check_all(root: Path, registry: Dict[str, es.Source]) -> List[Tuple[str, List[str]]]:
    """(check name, problems) for every offline check. A check with no problem passed."""
    root = Path(root)
    out = []  # type: List[Tuple[str, List[str]]]

    def lock_toml(rel: str) -> Dict[str, Any]:
        return es.parse((root / rel).read_text(encoding="utf-8"), rel)

    p1 = []  # type: List[str]
    cline = registry.get("cline")
    if cline is None:
        p1.append("engine/sources/cline.toml is missing")
    else:
        s = cline.table("source")
        for rel, table, keys in (("engine/cline/lock.toml", "upstream", (("url", "url"), ("tag", "tag"), ("commit", "commit"))),
                                 ("engine/lock.toml", "cline", (("tag", "tag"), ("commit", "commit")))):
            try:
                t = lock_toml(rel).get(table, {})
            except (OSError, es.SourceError) as e:
                p1.append("%s: %s" % (rel, e))
                continue
            for mine, theirs in keys:
                if t.get(theirs) != s.get(mine):
                    p1.append("%s [%s] %s is %r, engine/sources/cline.toml has %r" % (rel, table, theirs, t.get(theirs), s.get(mine)))
        pl = s.get("patches_lock")
        if pl and not (root / pl).is_file():
            p1.append("patches_lock %s does not exist" % pl)
    out.append(("cline pin agrees with engine/cline/lock.toml and engine/lock.toml", p1))

    p2 = []  # type: List[str]
    oc = registry.get("opencode")
    if oc is None:
        p2.append("engine/sources/opencode.toml is missing")
    else:
        try:
            lk = lock_toml("engine/lock.toml").get("opencode", {})
        except (OSError, es.SourceError) as e:
            lk = {}
            p2.append("engine/lock.toml: %s" % e)
        a = oc.asset("linux-arm64")
        for mine, theirs in ((a.get("url"), lk.get("asset")), (a.get("sha256"), lk.get("assetSha256")), (a.get("member_sha256"), lk.get("binarySha256")),
                             (oc.raw.get("version"), lk.get("version"))):
            if mine != theirs:
                p2.append("opencode.toml has %r, engine/lock.toml [opencode] has %r" % (mine, theirs))
    out.append(("opencode pin agrees with engine/lock.toml", p2))

    p3 = []  # type: List[str]
    esb = registry.get("esbuild")
    for plat in ("linux-x64", "linux-arm64"):
        if esb is None or not esb.table("assets").get(plat):
            p3.append("esbuild.toml has no %s asset" % plat)
    goal = registry.get(PLUGIN)
    if goal is not None:
        for t in goal.tables("inputs"):
            o = registry.get(t["from"])
            if o is None or o.kind != "git-tag":
                p3.append("input %s of goal-plugin names %r, which is not a git-tag source" % (t["path"], t["from"]))
    out.append(("goal plugin inputs and the bundler resolve", p3))

    p4 = []  # type: List[str]
    try:
        lock = read_lock(root)
        facts = plugin_facts(registry)
        rec = (lock.get("sources") or {}).get(PLUGIN) or {}
        want_inputs = {PLUGIN_SOURCE_AS: facts["plugin_sha256"], SDK_SOURCE_AS: facts["sdk_sha256"]}
        if rec.get("inputs") != want_inputs:
            p4.append("lock sources.%s.inputs is %s, the source files pin %s" % (PLUGIN, rec.get("inputs"), want_inputs))
        if rec.get("bundler") != facts["bundler"]:
            p4.append("lock sources.%s.bundler is %r, esbuild.toml says %r" % (PLUGIN, rec.get("bundler"), facts["bundler"]))
        if rec.get("recipe_sha256") != recipe_digest(root):
            p4.append("lock sources.%s.recipe_sha256 differs from the recipe files: run engine-fetch update --refresh" % PLUGIN)
        lic = _file(registry[PLUGIN], "files", LICENSE_AS)["sha256"]
        if lock["files"].get("goal/LICENSE") != lic:
            p4.append("lock files goal/LICENSE is not the pinned LICENSE sha256")
        record = {"recipe_sha256": rec.get("recipe_sha256", ""), "metafile_sha256": rec.get("metafile_sha256", ""),
                  "bundle_sha256": lock["files"].get("goal/index.js", ""), "bundle_bytes": rec.get("bundle_bytes", 0)}
        rendered = {"goal/NOTICE": render_template(root, "plugin-notice.tmpl", facts), "goal/package.json": render_template(root, "plugin-package.tmpl", facts),
                    "goal/PROVENANCE": render_provenance(facts, dict(record, bundle_bytes=rec.get("bundle_bytes", 0)))}
        for k, data in rendered.items():
            if sha256_bytes(data) != lock["files"].get(k):
                p4.append("%s rendered from the tracked data has sha256 %s, the lock has %s" % (k, sha256_bytes(data)[:12], str(lock["files"].get(k))[:12]))
        for k, want in sorted(lock["files"].items()):
            if k.startswith("harness-rounds/"):
                f = Path(root) / "engine" / "plugins" / k
                if not f.is_file() or sha256_file(f) != want:
                    p4.append("%s differs from the lock" % k)
        tracked_rounds = sorted(p.relative_to(Path(root) / "engine" / "plugins").as_posix() for p in (Path(root) / "engine" / "plugins" / "harness-rounds").rglob("*") if p.is_file())
        if tracked_rounds != sorted(k for k in lock["files"] if k.startswith("harness-rounds/")):
            p4.append("harness-rounds files %s are not exactly the lock's" % tracked_rounds)
        stray = sorted(k for k in lock["files"] if not k.startswith(("goal/", "harness-rounds/")))
        if stray:
            p4.append("lock lists files outside the two plugin folders: %s" % stray)
        goal_files = sorted(k for k in lock["files"] if k.startswith("goal/"))
        if goal_files != sorted("goal/" + n for n in SHIPPED_GOAL):
            p4.append("lock goal files %s are not exactly the five shipped files" % goal_files)
    except (FetchError, OSError, KeyError) as e:
        p4.append(getattr(e, "message", str(e)))
    out.append(("plugin lock agrees with the sources, the recipe and the tracked files", p4))

    p5 = []  # type: List[str]
    try:
        import third_party_scan as scan

        p5 = [f.line() for f in scan.scan_tree(root)]
    except (RuntimeError, ImportError) as e:
        p5.append(str(e))
    out.append(("no third-party engine or plugin source is tracked", p5))
    return out


def main_check(root: Path, registry: Dict[str, es.Source]) -> int:
    results = check_all(root, registry)
    bad = 0
    for name, problems in results:
        if problems:
            bad += 1
            print("ENGINE-FETCH-CHECK FAIL %s" % name)
            for p in problems:
                print("  " + p)
        else:
            print("ok %s" % name)
    if bad:
        print("ENGINE-FETCH-CHECK FAIL %d of %d check(s)" % (bad, len(results)))
        return ef.LINT
    print("ENGINE-FETCH-CHECK PASS %d checks" % len(results))
    return ef.OK


# ---- update -------------------------------------------------------------------------------------


def update_goal_plugin(root: Path, sources_dir: Path, registry: Dict[str, es.Source], fetcher: "ef.Fetcher", commit: Optional[str], platform: str,
                       esbuild: Optional[Path], write: bool) -> Dict[str, Any]:
    """Moves the goal plugin to `commit` (None: keeps the pin and refreshes the lock after a change of the recipe files). The new commit is fetched, the
    paths are measured, the whole build runs in a scratch directory without the lock, and only after it passes are the source file and the lock
    rewritten (when `write`). A build that fails changes nothing."""
    goal = registry[PLUGIN]
    path = goal.path
    text = path.read_text(encoding="utf-8")
    old_commit = goal.table("source")["commit"]
    new_commit = commit or old_commit
    changes = []  # type: List[str]
    if new_commit != old_commit:
        text = es.rewrite(text, "source", "commit", new_commit)
        changes.append("commit %s -> %s" % (old_commit[:12], new_commit[:12]))
        with fetcher.scratch() as home:
            git = ef.Git(home, fetcher.git_bin)
            repo = fetcher._repo(git, home, goal.table("source")["url"], "m")
            fetcher._fetch_commit(git, repo, new_commit)
            for i, t in enumerate(goal.tables("files")):
                blob = git.out("rev-parse", "%s:%s" % (new_commit, t["path"]), cwd=repo, code=ef.HASH)
                sha = sha256_bytes(git.blob(blob, repo))
                if blob != t["blob"]:
                    text = es.rewrite(text, "files", "blob", blob, index=i)
                    changes.append("%s blob %s -> %s" % (t["path"], t["blob"][:12], blob[:12]))
                if sha != t["sha256"]:
                    text = es.rewrite(text, "files", "sha256", sha, index=i)
                    changes.append("%s sha256 %s -> %s" % (t["path"], t["sha256"][:12], sha[:12]))
    with fetcher.scratch() as scratch:
        sdir = scratch / "sources"
        sdir.mkdir()
        for name, src in registry.items():
            (sdir / (name + ".toml")).write_text(text if name == PLUGIN else src.path.read_text(encoding="utf-8"), encoding="utf-8")
        try:
            new_registry = es.load_all(sdir, allow_local_url=fetcher.allow_local)
        except es.SourceError as e:
            raise FetchError(ef.LINT, "the rewritten source file is not valid: %s" % "; ".join(e.problems[:3]))
        nf = ef.Fetcher(fetcher.root, new_registry, allow_local=fetcher.allow_local, git_bin=fetcher.git_bin, opener=fetcher.opener, sleep=fetcher.sleep, log=fetcher.log)
        built = build_goal_plugin(root, new_registry, nf, scratch / "out", platform, esbuild=esbuild, refresh=True)
    lock = read_lock(root)
    new_lock = json.loads(json.dumps(lock))
    for k in [k for k in new_lock["files"] if k.startswith("goal/")]:
        del new_lock["files"][k]
    for rel, sha in built["files"].items():
        new_lock["files"]["goal/" + rel] = sha
    new_lock.setdefault("sources", {})[PLUGIN] = built["record"]
    if canonical(new_lock) != canonical(lock):
        changes.append("%s files and sources record" % LOCK_REL)
    if write:
        if text != path.read_text(encoding="utf-8"):
            path.write_text(text, encoding="utf-8")
        write_lock(root, new_lock)
    return {"name": PLUGIN, "old": old_commit, "new": new_commit, "changes": changes, "written": write}
