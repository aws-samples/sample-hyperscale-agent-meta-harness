"""Tests for scripts/dox_lint.py: the package-manifest rule, fixture repositories and shipped content.

A directory with a package manifest is a durable boundary and needs an AGENTS.md, except a
fixture repository nested under the top-level fixtures/ data root: its files are test data that
agents receive byte for byte, and fixtures/AGENTS.md owns them (criteria doxRequiredDirs lists
"fixtures", not the repos inside it).

lifecycle/pack/ holds content that ships to the repositories of coding runs (the lifecycle contract is a file named
AGENTS.md in that pack). It is the shipped file, not a DOX document of this repository, so the lint leaves its
AGENTS.md alone; lifecycle/AGENTS.md owns the directory.
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO / "scripts"))

ROOT_DOC = "# DOX framework\n\n## Child DOX Index\n\n| Path | Covers |\n| --- | --- |\n{rows}"
CHILD = ("# {name}\n\n## Purpose\n\n## Ownership\n\n## Local Contracts\n\n## Work Guidance\n\n"
         "## Verification\n\n## Child DOX Index\n\nNone.\n")


class DoxLintCase(unittest.TestCase):
    def setUp(self) -> None:
        import dox_lint  # noqa: PLC0415 (imported after sys.path setup)
        self.lint = dox_lint.lint
        self.root = Path(tempfile.mkdtemp(prefix="dox-lint-"))

    def tree(self, children: list[str]) -> None:
        rows = "".join(f"| `{c}/AGENTS.md` | x |\n" for c in children)
        (self.root / "AGENTS.md").write_text(ROOT_DOC.format(rows=rows))
        for c in children:
            (self.root / c).mkdir(parents=True, exist_ok=True)
            (self.root / c / "AGENTS.md").write_text(CHILD.format(name=c))

    def test_fixture_repository_with_a_manifest_needs_no_agents_md(self) -> None:
        self.tree(["fixtures"])
        repo = self.root / "fixtures" / "token-bench" / "F2"
        repo.mkdir(parents=True)
        (repo / "pyproject.toml").write_text("[project]\nname = \"f2\"\n")
        result = self.lint(self.root)
        self.assertTrue(result.ok, result.errors)

    def test_manifest_outside_fixtures_still_needs_agents_md(self) -> None:
        self.tree(["crates"])
        pkg = self.root / "crates" / "demo"
        pkg.mkdir(parents=True)
        (pkg / "Cargo.toml").write_text("[package]\nname = \"demo\"\n")
        result = self.lint(self.root)
        self.assertFalse(result.ok)
        self.assertIn("crates/demo: missing AGENTS.md (package manifest Cargo.toml)", result.errors)

    def test_fixtures_itself_still_needs_agents_md(self) -> None:
        self.tree([])
        (self.root / "fixtures" / "x").mkdir(parents=True)
        (self.root / "fixtures" / "x" / "pyproject.toml").write_text("")
        result = self.lint(self.root)
        self.assertFalse(result.ok)
        self.assertIn("fixtures: missing AGENTS.md (top-level directory)", result.errors)

    def test_a_prose_mention_of_the_index_heading_does_not_start_the_index_section(self) -> None:
        # The index starts at the heading line. The words quoted in a bullet above it (a test contract that describes a DOX tree and
        # names `ghost/AGENTS.md` as a dangling entry) are prose: they must not turn every later `x/AGENTS.md` into an index entry.
        self.tree(["api"])
        child = self.root / "api" / "AGENTS.md"
        text = child.read_text()
        child.write_text(text.replace(
            "## Local Contracts\n",
            "## Local Contracts\n\n- The starter ends in the one `## Child DOX Index`; a variant lists `ghost/AGENTS.md`.\n",
        ))
        result = self.lint(self.root)
        self.assertTrue(result.ok, result.errors)

    def test_shipped_pack_agents_md_is_not_a_dox_document(self) -> None:
        self.tree(["lifecycle"])
        pack = self.root / "lifecycle" / "pack"
        pack.mkdir(parents=True)
        (pack / "AGENTS.md").write_text("# Development lifecycle\n\n1. Goal card.\n2. Research.\n")
        result = self.lint(self.root)
        self.assertTrue(result.ok, result.errors)

    def test_the_same_file_elsewhere_under_lifecycle_is_still_checked(self) -> None:
        self.tree(["lifecycle"])
        other = self.root / "lifecycle" / "other"
        other.mkdir(parents=True)
        (other / "AGENTS.md").write_text("# Development lifecycle\n")
        result = self.lint(self.root)
        self.assertFalse(result.ok)
        self.assertIn("lifecycle/other/AGENTS.md: missing section '## Purpose'", result.errors)
        self.assertIn("lifecycle/other/AGENTS.md: not listed in the Child DOX Index of lifecycle/AGENTS.md", result.errors)

    def test_only_lifecycle_pack_is_shipped_content(self) -> None:
        self.tree(["crates"])
        pack = self.root / "crates" / "pack"
        pack.mkdir(parents=True)
        (pack / "AGENTS.md").write_text("# Development lifecycle\n")
        result = self.lint(self.root)
        self.assertFalse(result.ok)
        self.assertIn("crates/pack/AGENTS.md: missing section '## Purpose'", result.errors)

    def test_a_directory_holding_only_shipped_content_still_needs_its_own_agents_md(self) -> None:
        self.tree([])
        pack = self.root / "lifecycle" / "pack"
        pack.mkdir(parents=True)
        (pack / "AGENTS.md").write_text("# Development lifecycle\n")
        result = self.lint(self.root)
        self.assertFalse(result.ok)
        self.assertIn("lifecycle: missing AGENTS.md (top-level directory)", result.errors)

    def test_a_manifest_inside_shipped_content_needs_no_agents_md(self) -> None:
        self.tree(["lifecycle"])
        scripts = self.root / "lifecycle" / "pack" / "tools"
        scripts.mkdir(parents=True)
        (scripts / "pyproject.toml").write_text("[project]\nname = \"x\"\n")
        result = self.lint(self.root)
        self.assertTrue(result.ok, result.errors)




class ShippedPluginCase(unittest.TestCase):
    """engine/plugins/harness-rounds ships into the worker image as a plugin folder: its package.json is the manifest the engine
    reads, not a package of this repository, and engine/plugins/AGENTS.md owns it. The goal plugin folder is fetched at build and is not in the
    tree, so a goal folder with a manifest is an ordinary folder that needs its own AGENTS.md."""

    def setUp(self) -> None:
        import dox_lint  # noqa: PLC0415 (imported after sys.path setup)
        self.lint = dox_lint.lint
        self.root = Path(tempfile.mkdtemp(prefix="dox-lint-plugins-"))
        (self.root / "AGENTS.md").write_text(ROOT_DOC.format(rows="| `engine/AGENTS.md` | x |\n"))
        (self.root / "engine").mkdir()
        (self.root / "engine" / "AGENTS.md").write_text(CHILD.format(name="engine").replace("None.\n", "| `plugins/AGENTS.md` | x |\n"))
        (self.root / "engine" / "plugins").mkdir()
        (self.root / "engine" / "plugins" / "AGENTS.md").write_text(CHILD.format(name="plugins"))

    def test_the_observer_plugin_folder_needs_no_agents_md_for_its_package_json(self) -> None:
        for name in ("harness-rounds",):
            (self.root / "engine" / "plugins" / name).mkdir()
            (self.root / "engine" / "plugins" / name / "package.json").write_text("{}\n")
        result = self.lint(self.root)
        self.assertTrue(result.ok, result.errors)

    def test_a_goal_folder_with_a_manifest_is_no_longer_exempt(self) -> None:
        goal = self.root / "engine" / "plugins" / "goal"
        goal.mkdir()
        (goal / "package.json").write_text("{}\n")
        result = self.lint(self.root)
        self.assertFalse(result.ok)
        self.assertIn("engine/plugins/goal: missing AGENTS.md (package manifest package.json)", result.errors)

    def test_another_folder_under_engine_plugins_with_a_manifest_still_needs_one(self) -> None:
        other = self.root / "engine" / "plugins" / "other"
        other.mkdir()
        (other / "package.json").write_text("{}\n")
        result = self.lint(self.root)
        self.assertFalse(result.ok)
        self.assertIn("engine/plugins/other: missing AGENTS.md (package manifest package.json)", result.errors)


if __name__ == "__main__":
    unittest.main()
