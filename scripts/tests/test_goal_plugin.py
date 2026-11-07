"""The goal plugin and the observer that ship in the worker image.

Cline's own goal plugin runs the goal loop by default. Its folder is not in the repository: `scripts/engine-fetch build goal-plugin` fetches the plugin
source and its licence from cline/plugins and createTool from the Cline source at their pins, bundles them with the pinned esbuild and checks the five
files against `engine/plugins/lock.json` before the image is built (images/codebuild/worker-python.yml). The harness's own observer
(`engine/plugins/harness-rounds/index.js`) stays in the tree. These cases hold the tracked files and the lock to each other, the lock to the sources and
the recipe (offline), the observer to its two hooks, the image build to the fetch, the verification and the 14 checks of the plugin's state machine, and
the engine pin to the Cline tag. The bundle itself is checked by Node when `HAMH_GOAL_BUNDLE_DIR` names a folder `engine-fetch build` made.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess  # nosec B404: the tests start Node through the repository script with an argument list, never a shell
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts" / "lib"))
import engine_recipes as er  # noqa: E402
import engine_source as es  # noqa: E402

PLUGINS = ROOT / "engine" / "plugins"
OBSERVER = PLUGINS / "harness-rounds"
BUILD = PLUGINS / "build"
DOCKERFILE = (ROOT / "images" / "worker-python" / "Dockerfile").read_text()
BUILDSPEC = (ROOT / "images" / "codebuild" / "worker-python.yml").read_text()

SHIPPED = {
    "goal/LICENSE",
    "goal/NOTICE",
    "goal/PROVENANCE",
    "goal/index.js",
    "goal/package.json",
    "harness-rounds/index.js",
    "harness-rounds/package.json",
}
TRACKED = {"harness-rounds/index.js", "harness-rounds/package.json"}


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def lock() -> dict:
    return json.loads((PLUGINS / "lock.json").read_text())


def node(*args: str) -> subprocess.CompletedProcess:
    """Runs Node in the pinned container (scripts/node.sh) from the repository root; the unit scripts keep their scratch files in the container."""
    return subprocess.run(  # noqa: S603  # nosec B603: argument list, repository script, no shell
        [str(ROOT / "scripts" / "node.sh"), "node", *args],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=600,
        check=False,
    )


def unit_results(out: subprocess.CompletedProcess) -> dict:
    text = out.stdout
    start = text.index("{")
    return json.loads(text[start:])


class TrackedFilesCase(unittest.TestCase):
    def test_the_tree_holds_the_observer_alone_and_no_goal_plugin_file(self) -> None:
        found = {p.relative_to(PLUGINS).as_posix() for p in OBSERVER.rglob("*") if p.is_file()}
        self.assertEqual(found, TRACKED)
        self.assertFalse((PLUGINS / "goal").exists(), "the goal plugin is fetched at build, never committed")
        self.assertFalse((BUILD / "sdk-stub.mjs").exists())
        self.assertFalse((BUILD / "build.sh").exists(), "engine-fetch replaced the rebuild script")
        self.assertEqual([p for p in PLUGINS.rglob("node_modules")], [])
        # The cost on the 1,780 MiB image is about 16 KB (the fit study): no layer of megabytes may hide in here.
        total = sum((PLUGINS / f).stat().st_size for f in TRACKED) + lock()["sources"][er.PLUGIN]["bundle_bytes"]
        self.assertLess(total, 40_000, total)

    def test_lock_json_lists_every_shipped_file_and_holds_the_tracked_ones_to_their_sha256(self) -> None:
        doc = lock()
        self.assertEqual(doc["version"], 1)
        self.assertEqual(set(doc["files"]), SHIPPED)
        for rel in TRACKED:
            self.assertEqual(sha256(PLUGINS / rel), doc["files"][rel], rel)
        for rel in SHIPPED - TRACKED:
            self.assertRegex(doc["files"][rel], r"^[0-9a-f]{64}$", rel)
        # the tool that writes the tracked entries agrees with the tree
        out = subprocess.run(  # noqa: S603  # nosec B603: argument list, repository script, no shell
            [sys.executable, str(BUILD / "lock.py")], capture_output=True, text=True, check=False
        )
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        self.assertIn("GOAL-PLUGIN-LOCK PASS", out.stdout)

    def test_the_lock_records_the_two_inputs_the_bundler_and_the_recipe_of_the_goal_plugin(self) -> None:
        rec = lock()["sources"][er.PLUGIN]
        registry = es.load_all(ROOT / "engine" / "sources")
        facts = er.plugin_facts(registry)
        self.assertEqual(rec["inputs"], {er.PLUGIN_SOURCE_AS: facts["plugin_sha256"], er.SDK_SOURCE_AS: facts["sdk_sha256"]})
        self.assertEqual(rec["bundler"], "esbuild 0.25.12")
        self.assertEqual(rec["recipe_sha256"], er.recipe_digest(ROOT))
        self.assertRegex(rec["metafile_sha256"], r"^[0-9a-f]{64}$")
        self.assertGreater(rec["bundle_bytes"], 1000)

    def test_the_plugin_lock_agrees_with_the_sources_the_recipe_and_the_rendered_files_offline(self) -> None:
        problems = dict(er.check_all(ROOT, es.load_all(ROOT / "engine" / "sources")))
        name = "plugin lock agrees with the sources, the recipe and the tracked files"
        self.assertEqual(problems[name], [], problems[name])

    def test_the_manifests_load_the_bundle_and_the_observer_as_plugins(self) -> None:
        facts = er.plugin_facts(es.load_all(ROOT / "engine" / "sources"))
        goal = json.loads(er.render_template(ROOT, "plugin-package.tmpl", facts))
        self.assertEqual(goal["name"], "goal")
        self.assertEqual(goal["type"], "module")
        self.assertEqual(goal["cline"]["plugins"], [{"paths": ["./index.js"], "capabilities": ["commands", "tools", "hooks", "rules"]}])
        obs = json.loads((OBSERVER / "package.json").read_text())
        self.assertEqual(obs["name"], "harness-rounds")
        self.assertEqual(obs["type"], "module")
        self.assertEqual(obs["cline"]["plugins"], [{"paths": ["./index.js"], "capabilities": ["hooks"]}])

    def test_the_notice_and_the_provenance_name_both_upstream_sources_and_the_licence(self) -> None:
        facts = er.plugin_facts(es.load_all(ROOT / "engine" / "sources"))
        notice = er.render_template(ROOT, "plugin-notice.tmpl", facts).decode()
        for needle in (facts["plugin_commit"], facts["sdk_commit"], facts["sdk_tag"], "Apache License 2.0", "This file was changed", "Cline Bot Inc."):
            self.assertIn(needle, notice)
        self.assertEqual(facts["license"], "Apache-2.0")

    def test_the_observer_catches_every_write_error_and_calls_nothing_else(self) -> None:
        text = (OBSERVER / "index.js").read_text()
        self.assertEqual(text.count("try {"), 2)
        for forbidden in ("fetch(", "http", "child_process", "spawn", "exec(", "process.exit"):
            self.assertNotIn(forbidden, text)

    def test_no_telemetry_or_network_call_is_in_the_observer(self) -> None:
        text = (OBSERVER / "index.js").read_text()
        for forbidden in ("fetch(", "node:http", "node:https", "node:net", "child_process", "XMLHttpRequest", "WebSocket"):
            self.assertNotIn(forbidden, text)

    def test_the_cline_tag_the_source_file_pins_is_the_tag_of_the_engine_series(self) -> None:
        # A pin bump of the Cline fork changes cline.toml, which `engine-fetch check` holds to engine/cline/lock.toml. The createTool input is pinned by
        # git blob at the Cline commit, so a bump that changes create.ts fails the fetch until the input is re-pinned (no manual comparison of a stand-in).
        registry = es.load_all(ROOT / "engine" / "sources")
        text = (ROOT / "engine" / "cline" / "lock.toml").read_text()
        self.assertEqual(registry["cline"].table("source")["tag"], es.parse(text)["upstream"]["tag"])
        sdk = [t for t in registry[er.PLUGIN].tables("inputs") if t["as"] == er.SDK_SOURCE_AS]
        self.assertEqual(len(sdk), 1)
        self.assertEqual(sdk[0]["from"], "cline")


class BundleCase(unittest.TestCase):
    """The fetched bundle: checked when `HAMH_GOAL_BUNDLE_DIR` names the output folder of `engine-fetch build goal-plugin` (the image build always does)."""

    @classmethod
    def setUpClass(cls) -> None:
        d = os.environ.get("HAMH_GOAL_BUNDLE_DIR")
        if not d:
            raise unittest.SkipTest("HAMH_GOAL_BUNDLE_DIR is not set: the bundle is fetched, run `scripts/engine-fetch build goal-plugin --dest DIR` and set it to DIR")
        cls.dir = Path(d)

    def test_the_folder_equals_the_lock(self) -> None:
        self.assertEqual(er.verify_goal_plugin(ROOT, self.dir)["files"], 5)

    def test_the_bundle_has_no_network_or_telemetry_call(self) -> None:
        text = (self.dir / "goal" / "index.js").read_text()
        for forbidden in ("fetch(", "node:http", "node:https", "node:net", "child_process", "XMLHttpRequest", "WebSocket"):
            self.assertNotIn(forbidden, text)

    def test_the_bundle_passes_the_14_checks_of_the_plugins_state_machine(self) -> None:
        if shutil.which("docker") is None:
            self.skipTest("docker is not installed: scripts/node.sh cannot run Node")
        out = node("engine/plugins/build/plugin-unit.mjs", str(self.dir / "goal" / "index.js"))
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        res = unit_results(out)
        self.assertEqual([r for r in res["results"] if not r["ok"]], [])
        self.assertEqual(len(res["results"]), 14)
        self.assertTrue(res["allOk"])


class NodeBehaviourCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if shutil.which("docker") is None:
            raise unittest.SkipTest("docker is not installed: scripts/node.sh cannot run Node")

    def test_the_observer_writes_one_line_per_start_and_per_end_and_never_throws(self) -> None:
        out = node("engine/plugins/build/observer-unit.mjs", str(OBSERVER / "index.js"))
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)
        res = unit_results(out)
        failed = [r for r in res["results"] if not r["ok"]]
        self.assertEqual(failed, [])
        self.assertTrue(res["allOk"])
        self.assertGreaterEqual(len(res["results"]), 7)


class ImageCase(unittest.TestCase):
    def test_the_dockerfile_copies_the_fetched_goal_folder_the_observer_and_the_lock_beside_the_profiles(self) -> None:
        self.assertRegex(DOCKERFILE, r"(?m)^COPY build/plugins/goal/ /opt/harness/engine/plugins/goal/$")
        self.assertRegex(DOCKERFILE, r"(?m)^COPY engine/plugins/harness-rounds/ /opt/harness/engine/plugins/harness-rounds/$")
        self.assertRegex(DOCKERFILE, r"(?m)^COPY engine/plugins/lock\.json /opt/harness/engine/plugins/lock\.json$")
        # nothing from build/ (the scripts) and no node_modules reach the image
        self.assertNotRegex(DOCKERFILE, r"(?m)^COPY engine/plugins/build")
        self.assertNotRegex(DOCKERFILE, r"(?m)^COPY engine/plugins/?\s")
        self.assertNotRegex(DOCKERFILE, r"(?m)^COPY engine/plugins/goal")

    def test_the_build_spec_fetches_and_verifies_the_goal_folder_before_the_image_is_built(self) -> None:
        lines = [ln.strip() for ln in BUILDSPEC.splitlines()]
        build = next(i for i, ln in enumerate(lines) if "docker build" in ln)
        fetch = next(i for i, ln in enumerate(lines) if "scripts/engine-fetch build goal-plugin" in ln)
        verify = next(i for i, ln in enumerate(lines) if "scripts/engine-fetch verify goal-plugin" in ln)
        self.assertLess(fetch, verify)
        self.assertLess(verify, build)
        self.assertIn("--dest build/plugins", lines[fetch])
        self.assertIn("--dir build/plugins", lines[verify])
        self.assertIn("--platform linux-arm64", lines[fetch])
        self.assertNotIn("npx", BUILDSPEC)

    def test_the_build_spec_runs_the_14_checks_and_the_observer_checks_in_the_built_image(self) -> None:
        self.assertIn("plugin-unit.mjs", BUILDSPEC)
        self.assertIn("observer-unit.mjs", BUILDSPEC)
        run = next(ln for ln in BUILDSPEC.splitlines() if "plugin-unit.mjs" in ln)
        self.assertIn("/opt/harness/engine/plugins/goal/index.js", run, "the shipped file in the image is the one that is checked")
        self.assertIn("allOk", BUILDSPEC)

    def test_the_apache_licence_texts_and_the_notice_travel_in_the_image(self) -> None:
        # The Cline fork binary and the goal bundle are Apache-2.0 software of Cline Bot Inc.; the image carries the licence text and the repository NOTICE.
        self.assertRegex(DOCKERFILE, r"(?m)^COPY --chmod=0444 LICENSE NOTICE /opt/harness/licenses/$")
        self.assertRegex(DOCKERFILE, r"(?m)^COPY --chmod=0444 build/plugins/goal/LICENSE /opt/harness/licenses/Apache-2\.0-Cline\.txt$")

    def test_the_notice_and_the_licence_are_inputs_of_the_worker_image_key(self) -> None:
        sys.path.insert(0, str(ROOT / "scripts" / "lib"))
        try:
            import digests  # type: ignore[import-not-found]
        finally:
            sys.path.pop(0)
        for rel in ("LICENSE", "NOTICE"):
            self.assertIn(rel, digests.IMAGE_PATHS, "a change of %s changes the image, so it changes the key" % rel)

    def test_the_image_size_ceilings_are_the_rebased_ones(self) -> None:
        for name, want in (("IMAGE_MAX_COMPRESSED_MB", 1800), ("IMAGE_MAX_UNPACKED_MIB", 3200)):
            m = re.search(r'^\s+%s:\s*"?(\d+)"?\s*$' % name, BUILDSPEC, re.M)
            self.assertIsNotNone(m, name)
            self.assertEqual(int(m.group(1)), want, name)

    def test_the_plugins_and_their_sources_are_inputs_of_the_worker_image_key(self) -> None:
        # scripts/lib/digests.py hashes engine/ except engine/catalog/ and engine/policy/: the source files, the recipe and the lock move the key, as the
        # decision record says, and are not on the excluded list.
        sys.path.insert(0, str(ROOT / "scripts" / "lib"))
        try:
            import digests  # type: ignore[import-not-found]
        finally:
            sys.path.pop(0)
        self.assertIn("engine", digests.IMAGE_PATHS)
        self.assertFalse(any(e.startswith(("engine/plugins", "engine/sources")) for e in digests.IMAGE_EXCLUDED), digests.IMAGE_EXCLUDED)


if __name__ == "__main__":
    unittest.main()
