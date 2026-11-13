"""The build recipes, the plugin lock, `check`, `update` and the command line of scripts/engine-fetch.

A world is built in a temporary directory: local git repositories for the plugin and for Cline, a local archive that holds a fake esbuild, the real
recipe files of the repository and a plugin lock made by the recipe itself. The fake esbuild is a small program that records how it was started
(arguments, working directory, environment, the layout it found) and writes a bundle that depends on its inputs, so each rule of the recipe can be
broken on purpose: another import, another input hash, a warning, another file in the lock. No network and no real esbuild; the real one is pinned
by hash in engine/sources/esbuild.toml and exercised by the live check of the image build.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import shutil
import subprocess  # nosec B404: the tests start git and the entry point with argument lists, never a shell
import sys
import tarfile
import tempfile
import unittest
from pathlib import Path
from typing import Dict, List, Optional

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts" / "lib"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import engine_fetch as ef  # noqa: E402
import engine_recipes as er  # noqa: E402
import engine_source as es  # noqa: E402
from test_engine_fetch import ENV, Repo, git, load, sha, tag_source  # noqa: E402

FAKE_ESBUILD = r'''#!/usr/bin/env python3
import json, os, sys
try:
    mode = open(os.path.realpath(__file__) + ".mode").read().strip()
except OSError:
    mode = ""
cwd = os.getcwd()
files = sorted(os.path.relpath(os.path.join(d, f), cwd) for d, _, fs in os.walk(cwd) for f in fs)
log = {"argv": sys.argv[1:], "cwd": cwd, "env": sorted(os.environ), "files": files}
open(os.path.join(os.path.dirname(cwd), "esbuild-call.json"), "w").write(json.dumps(log))
inputs = [f for f in files if f not in ("tsconfig.json", "package.json", "LICENSE", "cline/LICENSE")]
if mode == "extra-import":
    inputs.append("node_modules/other/index.js")
if mode == "missing-input":
    inputs.remove("stubs/zod.mjs")
outfile = next((a.split("=", 1)[1] for a in sys.argv if a.startswith("--outfile=")), "../index.js")
if "sdk-module" in outfile:
    open(outfile, "wb").write(b"// fake sdk module\n" + open("cline/shared/src/tools/create.ts", "rb").read())
    sys.exit(1 if mode == "fail" else 0)
data = b"// fake bundle\n" + b"".join(open(f, "rb").read() for f in inputs)
if mode == "different":
    data += b"// other\n"
open("../index.js", "wb").write(data)
open("../meta.json", "w").write(json.dumps({"inputs": {f: {"bytes": 1} for f in inputs}}))
if mode == "warning":
    sys.stderr.write("warning: something changed\n")
sys.exit(1 if mode == "fail" else 0)
'''


def make_esbuild(base: Path) -> Path:
    p = base / "fake-esbuild"
    p.write_text(FAKE_ESBUILD)
    p.chmod(0o755)
    return p


def tgz_with(member: str, data: bytes, base: Path, name: str = "esbuild.tgz") -> Path:
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz") as tf:
        ti = tarfile.TarInfo(member)
        ti.size = len(data)
        ti.mode = 0o755
        tf.addfile(ti, io.BytesIO(data))
    p = base / name
    p.write_bytes(buf.getvalue())
    return p


LICENSE_TEXT = b"Apache License\nVersion 2.0, January 2004\n"


class World:
    """A repository root with its sources, recipe, plugin lock and the local upstreams."""

    def __init__(self, base: Path):
        self.base = base
        self.root = base / "root"
        self.root.mkdir()
        self.plugin = Repo(base, "plugins", {"plugins/goal/index.ts": b'import { createTool } from "@cline/sdk";\nexport default {};\n', "LICENSE": LICENSE_TEXT}, tag=None)
        self.cline = Repo(base, "cline", {"sdk/packages/shared/src/tools/create.ts": b"export function createTool(c) { return c; }\n", "LICENSE": LICENSE_TEXT})
        self.fake = make_esbuild(base)
        self.member = FAKE_ESBUILD.encode()
        self.archive = tgz_with("package/bin/esbuild", self.member, base)
        shutil.copytree(str(ROOT / "engine" / "sources" / "recipes"), str(self.root / "engine" / "sources" / "recipes"))
        self.sources_dir = self.root / "engine" / "sources"
        self.write_sources()
        (self.root / "engine" / "plugins" / "harness-rounds").mkdir(parents=True)
        (self.root / "engine" / "plugins" / "harness-rounds" / "index.js").write_text("export default {};\n")
        (self.root / "engine" / "plugins" / "harness-rounds" / "package.json").write_text('{"name":"harness-rounds"}\n')

    def set_mode(self, mode: str) -> None:
        """How the fake esbuild misbehaves (it reads this file beside itself; the recipe gives it a clean environment)."""
        Path(str(self.fake) + ".mode").write_text(mode)

    def write_sources(self) -> None:
        d = self.sources_dir
        (d / "cline.toml").write_text(tag_source("cline", self.cline).replace('recipe = "none"', 'recipe = "cline-fork"'))
        goal = ('schema = "1"\nname = "goal-plugin"\nkind = "git-path-bundle"\nlicense = "Apache-2.0"\nrecipe = "esbuild-bundle"\n\n[source]\nurl = "%s"\ncommit = "%s"\n'
                % (self.plugin.path, self.plugin.commit))
        for rel, dest in (("plugins/goal/index.ts", "goal-index.ts"), ("LICENSE", "LICENSE")):
            goal += '\n[[files]]\npath = "%s"\nblob = "%s"\nsha256 = "%s"\nas = "%s"\n' % (rel, self.plugin.blob(rel), sha((self.plugin.path / rel).read_bytes()), dest)
        for rel, dest in (("sdk/packages/shared/src/tools/create.ts", "cline/shared/src/tools/create.ts"), ("LICENSE", "cline/LICENSE")):
            goal += '\n[[inputs]]\nfrom = "cline"\npath = "%s"\nblob = "%s"\nsha256 = "%s"\nas = "%s"\n' % (rel, self.cline.blob(rel), sha((self.cline.path / rel).read_bytes()), dest)
        (d / "goal-plugin.toml").write_text(goal)
        (d / "esbuild.toml").write_text(
            'schema = "1"\nname = "esbuild"\nkind = "release-asset"\nlicense = "MIT"\nrecipe = "none"\nversion = "0.25.12"\n\n[assets.linux-x64]\nurl = "%s"\nsha256 = "%s"\nmember = "package/bin/esbuild"\nmember_sha256 = "%s"\n\n[assets.linux-arm64]\nurl = "%s"\nsha256 = "%s"\nmember = "package/bin/esbuild"\nmember_sha256 = "%s"\n'
            % (self.archive, sha(self.archive.read_bytes()), sha(self.member), self.archive, sha(self.archive.read_bytes()), sha(self.member)))
        (d / "opencode.toml").write_text(
            'schema = "1"\nname = "opencode"\nkind = "release-asset"\nlicense = "MIT"\nrecipe = "none"\nversion = "1.0.0"\n\n[assets.linux-arm64]\nurl = "https://example.invalid/oc.tgz"\nsha256 = "%s"\nmember = "opencode"\nmember_sha256 = "%s"\n'
            % ("1" * 64, "2" * 64))
        (self.root / "engine" / "cline").mkdir(exist_ok=True)
        (self.root / "engine" / "cline" / "lock.toml").write_text('schema = "1"\n\n[upstream]\nurl = "%s"\ntag = "v1"\ncommit = "%s"\n' % (self.cline.path, self.cline.commit))
        (self.root / "engine" / "lock.toml").write_text(
            'schema = "1"\n\n[cline]\ntag = "v1"\ncommit = "%s"\n\n[opencode]\nversion = "1.0.0"\nasset = "https://example.invalid/oc.tgz"\nassetSha256 = "%s"\nbinarySha256 = "%s"\n'
            % (self.cline.commit, "1" * 64, "2" * 64))

    def registry(self) -> Dict[str, es.Source]:
        return es.load_all(self.sources_dir, allow_local_url=True)

    def fetcher(self, registry: Optional[Dict[str, es.Source]] = None) -> "ef.Fetcher":
        return ef.Fetcher(self.root, registry or self.registry(), allow_local=True, log=lambda s: None)

    def build_lock(self) -> Dict[str, object]:
        """The plugin lock the recipe itself makes (refresh mode), with the observer files of the tree."""
        reg = self.registry()
        built = er.build_goal_plugin(self.root, reg, self.fetcher(reg), self.base / "refresh-out", "linux-x64", esbuild=self.fake, refresh=True)
        files = {"goal/" + k: v for k, v in built["files"].items()}
        for rel in ("index.js", "package.json"):
            files["harness-rounds/" + rel] = sha((self.root / "engine" / "plugins" / "harness-rounds" / rel).read_bytes())
        lock = {"version": 1, "files": files, "sources": {er.PLUGIN: built["record"]}}
        er.write_lock(self.root, lock)
        return lock

    def git_init(self) -> None:
        git(self.root, "init", "-q", ".")
        git(self.root, "add", "-A")


class Base(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="er-test-")
        self.addCleanup(self._tmp.cleanup)
        self.base = Path(self._tmp.name)
        self.w = World(self.base)

    def assertFetchError(self, code: int, fn, *args, **kw) -> str:
        with self.assertRaises(ef.FetchError) as cm:
            fn(*args, **kw)
        self.assertEqual(cm.exception.code, code, cm.exception.message)
        return cm.exception.message

    def build(self, dest: Optional[Path] = None, **kw):
        reg = self.w.registry()
        return er.build_goal_plugin(self.w.root, reg, self.w.fetcher(reg), dest or (self.base / "out"), "linux-x64", **{"esbuild": self.w.fake, **kw})


class BuildCase(Base):
    def test_the_folder_is_made_and_equals_the_lock_the_recipe_made(self) -> None:
        self.w.build_lock()
        info = self.build()
        self.assertEqual(sorted(info["files"]), sorted(er.SHIPPED_GOAL))
        folder = self.base / "out" / "goal"
        self.assertEqual(sorted(p.name for p in folder.iterdir()), sorted(er.SHIPPED_GOAL))
        self.assertEqual(er.verify_goal_plugin(self.w.root, self.base / "out")["files"], 5)
        self.assertTrue((self.base / "out" / "goal-plugin.receipt.json").is_file())

    def test_the_arguments_the_layout_and_the_environment_are_what_the_recipe_says(self) -> None:
        # The fake records its call next to the layout; keep the scratch directory for this case by pointing the fetcher's scratch at a fixed place.
        seen = {}  # type: Dict[str, object]
        orig = er.run_esbuild

        def spy(esbuild, layout, work):
            out = orig(esbuild, layout, work)
            seen.update(json.loads((work / "esbuild-call.json").read_text()))
            return out

        er.run_esbuild = spy
        try:
            self.w.build_lock()
            seen.clear()
            self.build()
        finally:
            er.run_esbuild = orig
        self.assertEqual(seen["argv"], er.ESBUILD_ARGS)
        self.assertTrue(str(seen["cwd"]).endswith("/src"))
        self.assertEqual(seen["env"], ["HOME", "LC_ALL", "PATH", "TMPDIR"], "no NODE_PATH, no inherited variable")
        want = ["cline/shared/src/parse/zod.ts", "cline/shared/src/tools/create.ts", "goal-index.ts", "package.json", "sdk.mjs", "stubs/zod.mjs", "tsconfig.json"]
        self.assertEqual([f for f in seen["files"] if f != "LICENSE"], want, "the layout is fixed; the second licence file is gone before esbuild runs")

    def test_createtool_is_laid_out_verbatim_from_the_fetched_cline_source(self) -> None:
        seen = {}  # type: Dict[str, bytes]
        orig = er.run_esbuild

        def spy(esbuild, layout, work):
            seen["create"] = (layout / "cline/shared/src/tools/create.ts").read_bytes()
            seen["sdk"] = (layout / "sdk.mjs").read_bytes()
            return orig(esbuild, layout, work)

        er.run_esbuild = spy
        try:
            self.w.build_lock()
            self.build()
        finally:
            er.run_esbuild = orig
        self.assertEqual(seen["create"], (self.w.cline.path / "sdk/packages/shared/src/tools/create.ts").read_bytes())
        self.assertIn(b'export { createTool } from "./cline/shared/src/tools/create.ts"', seen["sdk"])

    def test_the_sdk_module_is_written_beside_the_folder_only_when_asked_and_is_not_in_the_lock(self) -> None:
        self.w.build_lock()
        self.build()
        self.assertFalse((self.base / "out" / "sdk-module.mjs").exists())
        self.build(dest=self.base / "out2", sdk_module=True)
        mod = (self.base / "out2" / "sdk-module.mjs").read_bytes()
        self.assertIn(b"createTool", mod, "the module is made from the fetched create.ts")
        self.assertEqual(er.verify_goal_plugin(self.w.root, self.base / "out2")["files"], 5, "the folder check ignores a file beside goal/")
        self.assertNotIn("sdk-module.mjs", json.dumps(er.read_lock(self.w.root)))

    def test_an_esbuild_that_cannot_be_started_is_a_recipe_error_not_a_traceback(self) -> None:
        self.w.build_lock()
        self.assertFetchError(ef.RECIPE, self.build, esbuild=self.base / "no-such-esbuild")
        plain = self.base / "not-executable"
        plain.write_text("#!/bin/sh\n")
        plain.chmod(0o644)
        self.assertFetchError(ef.RECIPE, self.build, esbuild=plain)
        self.assertFalse((self.base / "out").exists())

    def test_an_empty_plugin_lock_is_a_lint_error(self) -> None:
        (self.w.root / er.LOCK_REL).write_bytes(b"")
        self.assertFetchError(ef.LINT, er.read_lock, self.w.root)
        self.assertFetchError(ef.LINT, self.build, dest=self.base / "out4")

    def test_a_failing_sdk_module_build_publishes_nothing(self) -> None:
        self.w.build_lock()
        self.w.set_mode("fail")
        self.assertFetchError(ef.RECIPE, self.build, dest=self.base / "out3", sdk_module=True)
        self.assertFalse((self.base / "out3").exists())

    def test_a_bundle_that_pulls_in_another_file_fails_the_input_check(self) -> None:
        self.w.build_lock()
        self.w.set_mode("extra-import")
        msg = self.assertFetchError(ef.RECIPE, self.build)
        self.assertIn("node_modules/other/index.js", msg)
        self.assertFalse((self.base / "out").exists())

    def test_a_bundle_that_misses_an_input_fails_the_input_check(self) -> None:
        self.w.build_lock()
        self.w.set_mode("missing-input")
        msg = self.assertFetchError(ef.RECIPE, self.build)
        self.assertIn("stubs/zod.mjs", msg)

    def test_a_warning_on_stderr_and_a_failing_exit_both_fail_the_build(self) -> None:
        self.w.build_lock()
        for mode in ("warning", "fail"):
            self.w.set_mode(mode)
            self.assertFetchError(ef.RECIPE, self.build, dest=self.base / ("out-" + mode))

    def test_a_bundle_that_differs_from_the_lock_is_an_output_error_naming_the_file(self) -> None:
        self.w.build_lock()
        self.w.set_mode("different")
        msg = self.assertFetchError(ef.OUTPUT, self.build)
        self.assertIn("goal/index.js", msg)
        self.assertIn("goal/PROVENANCE", msg, "the provenance carries the bundle hash, so it differs too")
        self.assertFalse((self.base / "out").exists(), "nothing is published when a file differs")

    def test_a_pinned_input_that_changed_in_the_lock_is_an_output_error(self) -> None:
        lock = self.w.build_lock()
        lock["files"]["goal/LICENSE"] = "0" * 64  # type: ignore[index]
        er.write_lock(self.w.root, lock)
        self.assertIn("goal/LICENSE", self.assertFetchError(ef.OUTPUT, self.build))

    def test_a_file_the_lock_lists_and_the_build_does_not_make_is_an_output_error(self) -> None:
        lock = self.w.build_lock()
        lock["files"]["goal/EXTRA"] = "0" * 64  # type: ignore[index]
        er.write_lock(self.w.root, lock)
        self.assertIn("goal/EXTRA", self.assertFetchError(ef.OUTPUT, self.build))

    def test_the_two_licence_texts_must_be_the_same_bytes(self) -> None:
        self.w.build_lock()
        (self.w.cline.path / "LICENSE").write_bytes(LICENSE_TEXT + b"changed\n")
        git(self.w.cline.path, "commit", "-q", "-am", "licence")
        self.w.cline.commit = git(self.w.cline.path, "rev-parse", "HEAD")
        git(self.w.cline.path, "tag", "-f", "-a", "-m", "again", "v1")
        self.w.cline.tag_object = git(self.w.cline.path, "rev-parse", "v1")
        self.w.write_sources()
        self.assertIn("licence", self.assertFetchError(ef.HASH, self.build).lower())

    def test_the_pinned_esbuild_is_fetched_when_none_is_given(self) -> None:
        # the archive of the esbuild source holds the fake; the member hash is checked, then it runs
        self.w.build_lock()
        info = self.build(esbuild=None)
        self.assertEqual(sorted(info["files"]), sorted(er.SHIPPED_GOAL))

    def test_a_wrong_esbuild_member_hash_stops_before_it_runs(self) -> None:
        self.w.build_lock()
        text = (self.w.sources_dir / "esbuild.toml").read_text().replace(sha(self.w.member), "9" * 64)
        (self.w.sources_dir / "esbuild.toml").write_text(text)
        self.assertFetchError(ef.HASH, self.build, esbuild=None)

    def test_without_a_lock_a_build_that_is_not_a_refresh_is_a_lint_error(self) -> None:
        self.assertFetchError(ef.LINT, self.build)


class RenderCase(Base):
    def test_the_rendered_files_hold_the_facts_of_the_source_files_and_nothing_else(self) -> None:
        # The upstream repositories of the world are local directories (git needs a place to fetch from); the facts a real source file holds are https URLs.
        facts = dict(er.plugin_facts(self.w.registry()), plugin_repository="https://github.com/cline/plugins", sdk_repository="https://github.com/cline/cline")
        prov = er.render_provenance(facts, {"recipe_sha256": "a" * 64, "metafile_sha256": "b" * 64, "bundle_sha256": "c" * 64, "bundle_bytes": 10}).decode()
        for needle in ("plugin_commit=" + self.w.plugin.commit, "sdk_commit=" + self.w.cline.commit, "sdk_tag=v1", "license=Apache-2.0", "bundler=esbuild 0.25.12", "bundle_bytes=10",
                       "plugin_repository=https://github.com/cline/plugins", "sdk_repository=https://github.com/cline/cline"):
            self.assertIn(needle, prov)
        self.assertNotRegex(prov, r"(?i)linux|x64|arm64|/tmp|/local|[0-9]{4}-[0-9]{2}-[0-9]{2}T")
        self.assertTrue(prov.endswith("\n") and "\r" not in prov)
        notice = er.render_template(self.w.root, "plugin-notice.tmpl", facts).decode()
        self.assertIn(self.w.plugin.commit, notice)
        self.assertIn(self.w.cline.commit, notice)
        self.assertNotIn("{", notice, "every placeholder is filled")

    def test_the_manifest_template_is_byte_for_byte_the_manifest_the_engine_reads(self) -> None:
        facts = er.plugin_facts(self.w.registry())
        pkg = er.render_template(self.w.root, "plugin-package.tmpl", facts)
        self.assertEqual(sha(pkg), "5e3315a2ef29dcb36f48b81c55c52266b29882f90af8f4f6ca71f4d7c1b60d18")
        doc = json.loads(pkg)
        self.assertEqual(doc["cline"]["plugins"], [{"paths": ["./index.js"], "capabilities": ["commands", "tools", "hooks", "rules"]}])

    def test_the_recipe_digest_changes_with_a_recipe_file_and_with_the_arguments(self) -> None:
        d0 = er.recipe_digest(self.w.root)
        (self.w.root / er.RECIPE_REL / "zod-stub.mjs").write_text("export const z = {};\n")
        d1 = er.recipe_digest(self.w.root)
        self.assertNotEqual(d0, d1)
        old = list(er.ESBUILD_ARGS)
        er.ESBUILD_ARGS.append("--minify")
        try:
            self.assertNotEqual(er.recipe_digest(self.w.root), d1)
        finally:
            er.ESBUILD_ARGS[:] = old

    def test_the_fixed_arguments_do_not_leave_the_bytes_to_defaults(self) -> None:
        args = " ".join(er.ESBUILD_ARGS)
        for needle in ("--charset=ascii", "--legal-comments=eof", "--tree-shaking=true", "--log-level=warning", "--metafile=", "--platform=node", "--format=esm", "--target=node22"):
            self.assertIn(needle, args)
        for banned in ("--minify", "--sourcemap", "--banner", "--footer", "--keep-names"):
            self.assertNotIn(banned, args)


class VerifyCase(Base):
    def fresh(self, name: str) -> Path:
        self.w.build_lock() if not (self.w.root / er.LOCK_REL).exists() else None
        self.build(dest=self.base / name)
        return self.base / name

    def test_a_clean_folder_passes(self) -> None:
        self.assertEqual(er.verify_goal_plugin(self.w.root, self.fresh("v1"))["files"], 5)

    def test_a_changed_file_is_an_output_error(self) -> None:
        out = self.fresh("v2")
        (out / "goal" / "index.js").write_bytes(b"changed")
        self.assertIn("goal/index.js", self.assertFetchError(ef.OUTPUT, er.verify_goal_plugin, self.w.root, out))

    def test_an_extra_file_and_an_extra_directory_are_output_errors(self) -> None:
        out = self.fresh("v3")
        (out / "goal" / "extra.js").write_bytes(b"x")
        self.assertIn("extra.js", self.assertFetchError(ef.OUTPUT, er.verify_goal_plugin, self.w.root, out))
        out2 = self.fresh("v4")
        (out2 / "goal" / "node_modules").mkdir()
        self.assertFetchError(ef.OUTPUT, er.verify_goal_plugin, self.w.root, out2)

    def test_a_missing_file_is_an_output_error(self) -> None:
        out = self.fresh("v5")
        (out / "goal" / "NOTICE").rename(out / "goal" / "NOTICE.moved")
        self.assertFetchError(ef.OUTPUT, er.verify_goal_plugin, self.w.root, out)

    def test_a_link_is_refused_even_when_its_target_has_the_right_bytes(self) -> None:
        out = self.fresh("v6")
        (out / "goal" / "NOTICE").rename(out / "goal" / "NOTICE.keep")
        os.symlink(str(out / "goal" / "NOTICE.keep"), str(out / "goal" / "NOTICE"))
        self.assertFetchError(ef.OUTPUT, er.verify_goal_plugin, self.w.root, out)

    def test_a_directory_without_the_goal_folder_is_an_output_error(self) -> None:
        self.w.build_lock()
        self.assertFetchError(ef.OUTPUT, er.verify_goal_plugin, self.w.root, self.base / "nothing")


class CheckCase(Base):
    def prepare(self) -> None:
        self.w.build_lock()
        self.w.git_init()

    def problems(self) -> Dict[str, List[str]]:
        return {name: probs for name, probs in er.check_all(self.w.root, self.w.registry()) if probs}

    def test_a_consistent_world_passes_every_check(self) -> None:
        self.prepare()
        self.assertEqual(self.problems(), {})

    def test_a_cline_pin_that_differs_from_the_older_locks_fails(self) -> None:
        self.prepare()
        text = (self.w.root / "engine" / "lock.toml").read_text().replace('tag = "v1"', 'tag = "v2"')
        (self.w.root / "engine" / "lock.toml").write_text(text)
        probs = self.problems()
        self.assertTrue(any("engine/lock.toml [cline] tag" in p for ps in probs.values() for p in ps), probs)

    def test_an_opencode_pin_that_differs_from_the_engine_lock_fails(self) -> None:
        self.prepare()
        text = (self.w.root / "engine" / "lock.toml").read_text().replace("2" * 64, "3" * 64)
        (self.w.root / "engine" / "lock.toml").write_text(text)
        self.assertTrue(any("opencode" in p for ps in self.problems().values() for p in ps))

    def test_an_input_hash_in_the_source_file_that_the_lock_does_not_record_fails(self) -> None:
        self.prepare()
        p = self.w.sources_dir / "goal-plugin.toml"
        p.write_text(p.read_text().replace(sha((self.w.plugin.path / "plugins/goal/index.ts").read_bytes()), "4" * 64))
        self.assertTrue(any("inputs" in x for ps in self.problems().values() for x in ps))

    def test_an_edit_of_a_recipe_file_without_a_lock_refresh_fails(self) -> None:
        self.prepare()
        (self.w.root / er.RECIPE_REL / "sdk.mjs").write_text("export { createTool } from './elsewhere.ts';\n")
        self.assertTrue(any("recipe_sha256" in x for ps in self.problems().values() for x in ps))

    def test_an_edit_of_the_notice_template_without_a_lock_refresh_fails(self) -> None:
        self.prepare()
        (self.w.root / er.RECIPE_REL / "plugin-notice.tmpl").write_text("changed {plugin_commit}\n")
        self.assertTrue(any("goal/NOTICE" in x for ps in self.problems().values() for x in ps))

    def test_a_changed_observer_file_fails(self) -> None:
        self.prepare()
        (self.w.root / "engine" / "plugins" / "harness-rounds" / "index.js").write_text("export default { changed: true };\n")
        self.assertTrue(any("harness-rounds/index.js" in x for ps in self.problems().values() for x in ps))

    def test_a_file_in_the_lock_outside_the_two_folders_fails(self) -> None:
        self.prepare()
        lock = er.read_lock(self.w.root)
        lock["files"]["other/x.js"] = "0" * 64
        er.write_lock(self.w.root, lock)
        self.assertTrue(any("outside the two plugin folders" in x for ps in self.problems().values() for x in ps))

    def test_a_tracked_third_party_file_fails_the_scan_part(self) -> None:
        self.prepare()
        (self.w.root / "engine" / "plugins" / "goal").mkdir()
        (self.w.root / "engine" / "plugins" / "goal" / "index.js").write_text("// goal-index.ts\nexport default {};\n")
        git(self.w.root, "add", "-A")
        self.assertTrue(any("engine-layout" in x or "bundle-comment" in x for ps in self.problems().values() for x in ps))


class UpdateCase(Base):
    def advance_plugin(self) -> str:
        (self.w.plugin.path / "plugins/goal/index.ts").write_bytes(b'import { createTool } from "@cline/sdk";\nexport default { v: 2 };\n')
        git(self.w.plugin.path, "commit", "-q", "-am", "two")
        return git(self.w.plugin.path, "rev-parse", "HEAD")

    def run_update(self, commit: Optional[str], write: bool = True, esbuild: Optional[Path] = None):
        reg = self.w.registry()
        return er.update_goal_plugin(self.w.root, self.w.sources_dir, reg, self.w.fetcher(reg), commit, "linux-x64", esbuild or self.w.fake, write)

    def test_a_new_commit_rewrites_the_pin_the_hashes_and_the_lock_only_after_the_build_passes(self) -> None:
        self.w.build_lock()
        new = self.advance_plugin()
        before = (self.w.sources_dir / "goal-plugin.toml").read_text()
        res = self.run_update(new)
        self.assertEqual(res["new"], new)
        after = (self.w.sources_dir / "goal-plugin.toml").read_text()
        self.assertIn(new, after)
        self.assertNotEqual(before, after)
        reg = self.w.registry()
        self.assertEqual(reg["goal-plugin"].tables("files")[0]["sha256"], sha((self.w.plugin.path / "plugins/goal/index.ts").read_bytes()))
        # the lock now equals what a plain build makes
        self.build()
        self.assertEqual(er.read_lock(self.w.root)["sources"][er.PLUGIN]["inputs"]["goal-index.ts"], reg["goal-plugin"].tables("files")[0]["sha256"])

    def test_a_dry_run_writes_nothing(self) -> None:
        self.w.build_lock()
        new = self.advance_plugin()
        before = ((self.w.sources_dir / "goal-plugin.toml").read_text(), (self.w.root / er.LOCK_REL).read_text())
        res = self.run_update(new, write=False)
        self.assertFalse(res["written"])
        self.assertEqual(before, ((self.w.sources_dir / "goal-plugin.toml").read_text(), (self.w.root / er.LOCK_REL).read_text()))

    def test_a_build_that_fails_leaves_the_source_file_and_the_lock_as_they_were(self) -> None:
        self.w.build_lock()
        new = self.advance_plugin()
        before = ((self.w.sources_dir / "goal-plugin.toml").read_text(), (self.w.root / er.LOCK_REL).read_text())
        self.w.set_mode("fail")
        self.assertFetchError(ef.RECIPE, self.run_update, new)
        self.assertEqual(before, ((self.w.sources_dir / "goal-plugin.toml").read_text(), (self.w.root / er.LOCK_REL).read_text()))

    def test_a_refresh_with_the_same_pin_rewrites_the_lock_after_a_recipe_change(self) -> None:
        self.w.build_lock()
        (self.w.root / er.RECIPE_REL / "zod-stub.mjs").write_text("export class ZodType {}\nexport const z = { ZodType, extra: 1 };\n")
        self.assertFetchError(ef.OUTPUT, self.build)
        res = self.run_update(None)
        self.assertEqual(res["old"], res["new"])
        self.build()  # the lock matches the new recipe

    def test_a_commit_that_is_not_in_the_repository_changes_nothing(self) -> None:
        self.w.build_lock()
        before = (self.w.sources_dir / "goal-plugin.toml").read_text()
        self.assertFetchError(ef.TRANSPORT, self.run_update, "e" * 40)
        self.assertEqual(before, (self.w.sources_dir / "goal-plugin.toml").read_text())


class CommandLineCase(Base):
    def cli(self, *args: str, allow_local: bool = True, env: Optional[Dict[str, str]] = None):
        e = {**ENV, **(env or {})}
        if allow_local:
            e["ENGINE_FETCH_ALLOW_LOCAL"] = "1"
        return subprocess.run([sys.executable, str(ROOT / "scripts" / "lib" / "engine_fetch.py"), "--root", str(self.w.root), *args], capture_output=True, text=True, env=e)  # nosec B603: argument list, no shell, repository script under test

    def test_list_names_every_source(self) -> None:
        out = self.cli("list")
        self.assertEqual(out.returncode, 0, out.stderr)
        for name in ("cline", "goal-plugin", "esbuild", "opencode"):
            self.assertIn(name, out.stdout)
        self.assertRegex(out.stdout.strip().splitlines()[-1], r"^ENGINE-FETCH PASS list 4 source\(s\)$")

    def test_local_urls_are_a_lint_error_unless_the_test_hook_is_set(self) -> None:
        out = self.cli("list", allow_local=False)
        self.assertEqual(out.returncode, ef.LINT, out.stdout + out.stderr)
        self.assertIn("ENGINE-FETCH FAIL lint", out.stderr)

    def test_fetch_prints_pass_and_a_failure_prints_its_class_and_exits_with_its_code(self) -> None:
        ok = self.cli("fetch", "esbuild", "--dest", str(self.base / "e1"), "--platform", "linux-x64")
        self.assertEqual(ok.returncode, 0, ok.stderr)
        self.assertRegex(ok.stdout.strip().splitlines()[-1], r"^ENGINE-FETCH PASS fetch esbuild kind=release-asset platform=linux-x64 sha256=[0-9a-f]{12} file=esbuild$")
        again = self.cli("fetch", "esbuild", "--dest", str(self.base / "e1"), "--platform", "linux-x64")
        self.assertEqual(again.returncode, ef.DEST, again.stderr)
        self.assertIn("ENGINE-FETCH FAIL destination", again.stderr)
        missing = self.cli("fetch", "nothing", "--dest", str(self.base / "e2"))
        self.assertEqual(missing.returncode, ef.USAGE, missing.stderr)

    def test_a_usage_error_exits_2(self) -> None:
        self.assertEqual(self.cli().returncode, 2)
        self.assertEqual(self.cli("fetch").returncode, 2)
        self.assertEqual(self.cli("frobnicate").returncode, 2)

    def test_build_and_verify_of_the_goal_plugin_through_the_command_line(self) -> None:
        self.w.build_lock()
        built = self.cli("build", "goal-plugin", "--dest", str(self.base / "b1"), "--platform", "linux-x64", "--esbuild", str(self.w.fake))
        self.assertEqual(built.returncode, 0, built.stdout + built.stderr)
        self.assertIn("ENGINE-FETCH PASS build goal-plugin platform=linux-x64 files=5", built.stdout)
        ok = self.cli("verify", "goal-plugin", "--dir", str(self.base / "b1"))
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        (self.base / "b1" / "goal" / "index.js").write_bytes(b"x")
        bad = self.cli("verify", "goal-plugin", "--dir", str(self.base / "b1"))
        self.assertEqual(bad.returncode, ef.OUTPUT, bad.stdout + bad.stderr)
        self.assertIn("ENGINE-FETCH FAIL output", bad.stderr)

    def test_update_of_cline_asks_for_a_rebase_and_of_a_release_asset_is_a_usage_error(self) -> None:
        r = self.cli("update", "cline", "--to-head")
        self.assertEqual(r.returncode, 2, r.stdout + r.stderr)
        self.w.sources_dir.joinpath("cline.toml").write_text(
            tag_source("cline", self.w.cline, "engine/cline/lock.toml").replace('recipe = "none"', 'recipe = "cline-fork"'))
        r = self.cli("update", "cline", "--to-head")
        self.assertEqual(r.returncode, ef.REBASE, r.stdout + r.stderr)
        self.assertIn("rebase", r.stderr)
        e = self.cli("update", "esbuild", "--to-head")
        self.assertEqual(e.returncode, ef.USAGE, e.stdout + e.stderr)

    def test_update_to_head_moves_the_pin_and_prints_the_commits(self) -> None:
        self.w.build_lock()
        (self.w.plugin.path / "plugins/goal/index.ts").write_bytes(b'import { createTool } from "@cline/sdk";\nexport default { v: 3 };\n')
        git(self.w.plugin.path, "commit", "-q", "-am", "three")
        head = git(self.w.plugin.path, "rev-parse", "HEAD")
        out = self.cli("update", "goal-plugin", "--to-head", "--platform", "linux-x64", "--esbuild", str(self.w.fake))
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertRegex(out.stdout.strip().splitlines()[-1], r"^ENGINE-FETCH PASS update goal-plugin [0-9a-f]{12} -> %s written$" % head[:12])
        self.assertIn(head, (self.w.sources_dir / "goal-plugin.toml").read_text())

    def test_check_passes_on_a_consistent_world_and_fails_with_exit_7_on_a_wrong_one(self) -> None:
        self.w.build_lock()
        self.w.git_init()
        ok = self.cli("check")
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        self.assertRegex(ok.stdout.strip().splitlines()[-1], r"^ENGINE-FETCH-CHECK PASS 5 checks$")
        (self.w.root / "engine" / "lock.toml").write_text((self.w.root / "engine" / "lock.toml").read_text().replace('tag = "v1"', 'tag = "v2"'))
        bad = self.cli("check")
        self.assertEqual(bad.returncode, ef.LINT, bad.stdout + bad.stderr)
        self.assertIn("ENGINE-FETCH-CHECK FAIL", bad.stdout)

    def test_the_exit_code_table_is_the_documented_one(self) -> None:
        self.assertEqual((ef.OK, ef.USAGE, ef.TRANSPORT, ef.MOVED, ef.HASH, ef.OUTPUT, ef.LINT, ef.REBASE, ef.DEST, ef.RECIPE), (0, 2, 3, 4, 5, 6, 7, 8, 9, 10))
        doc = (ROOT / "scripts" / "lib" / "engine_fetch.py").read_text()
        for line in ("0 ok; 2 usage; 3 transport", "4 moved", "5 hash", "6 output", "7 lint", "8 rebase", "9 destination", "10 recipe"):
            self.assertIn(line, doc)


class LauncherCase(unittest.TestCase):
    """scripts/engine-fetch is the one verified entry point: a launcher that picks a Python 3.8 or later and runs the module isolated."""

    LAUNCHER = ROOT / "scripts" / "engine-fetch"

    def test_the_launcher_exists_is_executable_and_runs_python_isolated(self) -> None:
        self.assertTrue(self.LAUNCHER.is_file(), "scripts/engine-fetch is missing")
        self.assertTrue(os.access(str(self.LAUNCHER), os.X_OK))
        text = self.LAUNCHER.read_text()
        self.assertIn("lib/python.sh", text)
        self.assertRegex(text, r'exec "\$py" -I "\$here/lib/engine_fetch\.py" "\$@"')

    def test_the_launcher_lists_the_sources_of_the_repository(self) -> None:
        out = subprocess.run([str(self.LAUNCHER), "list"], capture_output=True, text=True, cwd=str(ROOT))  # nosec B603: argument list, no shell, repository script under test
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertRegex(out.stdout.strip().splitlines()[-1], r"^ENGINE-FETCH PASS list 4 source\(s\)$")
        for name in ("cline", "goal-plugin", "esbuild", "opencode"):
            self.assertIn(name, out.stdout)

    def test_the_launcher_passes_the_exit_code_of_a_failure_through(self) -> None:
        out = subprocess.run([str(self.LAUNCHER), "fetch", "nothing", "--dest", os.path.join(tempfile.gettempdir(), "none-%d" % os.getpid())], capture_output=True, text=True, cwd=str(ROOT))  # nosec B603: argument list, no shell, repository script under test
        self.assertEqual(out.returncode, ef.USAGE, out.stdout + out.stderr)
        self.assertIn("ENGINE-FETCH FAIL usage", out.stderr)


class PythonCompatibilityCase(unittest.TestCase):
    """The fetch runs on the build host of the image (Python 3.9 on the CodeBuild image), so the modules parse as Python 3.8 and use no newer library call."""

    MODULES = ("engine_source.py", "engine_fetch.py", "engine_recipes.py", "third_party_scan.py")

    def test_the_modules_parse_as_python_3_8(self) -> None:
        import ast

        for name in self.MODULES:
            ast.parse((ROOT / "scripts" / "lib" / name).read_text(), feature_version=(3, 8))

    def test_the_modules_use_no_standard_library_name_newer_than_3_8(self) -> None:
        banned = (".removeprefix(", ".removesuffix(", "functools.cache(", "functools.cache\n", "import tomllib", "from tomllib", "match ", "zoneinfo", "Path.is_relative_to", ".is_relative_to(")
        for name in self.MODULES:
            text = (ROOT / "scripts" / "lib" / name).read_text()
            for b in banned:
                if b == "match ":
                    self.assertNotRegex(text, r"(?m)^\s+match \w+:", name)
                else:
                    self.assertNotIn(b, text, "%s uses %r" % (name, b))


if __name__ == "__main__":
    unittest.main()
