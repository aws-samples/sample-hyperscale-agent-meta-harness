"""The published and deployed artifacts say what the product does in plain words.

A stranger reads `schemas/`, the Step Functions `Comment` that lands in their account, the crate descriptions and the comments of the infra app, and
none of it may use a name of the authors' own product line or advertise a delivery mode the product does not serve.

  label   the working name of the product line ("cline" + "-direct") is not a word a reader knows. Direct run is the plain word.
  target  `gh:` and `gl:` parse but are refused: GitHub and GitLab delivery are not supported, so no description may offer them.
  status  a goal status is described by its own words, not by the status set of another product.
"""

from __future__ import annotations

import json
import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
LABEL = "cline" + "-direct"

# The product files whose words a stranger reads: the generated contract, the deployed state machine, the infra app, the images and the service crates.
LABEL_FREE = (
    "statemachines/*.json",
    "schemas/*.json",
    "infra/lib/**/*.ts",
    "infra/test/*.ts",
    "images/worker-python/Dockerfile",
    "images/worker-python/ham-*",
    "images/worker-python/containers/etc/*",
    "images/tools/lang/install.sh",
    "engine/lock.toml",
    "scripts/lib/deploy.py",
    "scripts/tests/test_build_cline_fork.py",
    "scripts/tests/test_deploy_direct.py",
    "scripts/tests/test_teardown_direct.py",
    "crates/agentd/**/*.rs",
    "crates/agentd/**/*.toml",
    "crates/control-api/**/*.rs",
    "crates/control-api/**/*.toml",
    "crates/delivery/**/*.rs",
    "crates/delivery/**/*.toml",
    "crates/dispatcher/**/*.rs",
    "crates/dispatcher/**/*.toml",
    "crates/harness-core/**/*.rs",
    "crates/harness-core/**/*.toml",
    "crates/harness-protocol/src/**/*.rs",
    "crates/harness-protocol/tests/*.rs",
    "crates/harness-protocol/tests/snapshots/*.snap",
    "crates/harness-protocol/Cargo.toml",
    "crates/pkgproxy/**/*.rs",
    "crates/pkgproxy/**/*.toml",
    "crates/research-api/**/*.rs",
    "crates/research-api/**/*.toml",
    "crates/run-session/**/*.rs",
    "crates/run-session/**/*.toml",
    "crates/worker-api/**/*.rs",
    "crates/worker-api/**/*.toml",
)


def tracked(patterns) -> list:
    out = []
    for pat in patterns:
        out.extend(p for p in ROOT.glob(pat) if p.is_file())
    return sorted(set(out))


class LabelCase(unittest.TestCase):
    def test_the_working_name_of_the_product_line_is_in_no_product_file(self) -> None:
        hits = []
        for path in tracked(LABEL_FREE):
            for n, line in enumerate(path.read_text(encoding="utf-8", errors="replace").splitlines(), 1):
                if LABEL in line.lower():
                    hits.append("%s:%d: %s" % (path.relative_to(ROOT), n, line.strip()[:120]))
        self.assertEqual(hits, [], "say direct run, not the working name of the line")

    def test_the_run_machine_comment_that_lands_in_the_account_names_no_product_line(self) -> None:
        for path in sorted((ROOT / "statemachines").glob("*.asl.json")):
            comment = json.loads(path.read_text(encoding="utf-8")).get("Comment", "")
            self.assertNotIn(LABEL, comment.lower(), path.name)

    def test_the_crate_descriptions_name_no_product_line(self) -> None:
        for path in sorted((ROOT / "crates").glob("*/Cargo.toml")):
            m = re.search(r'(?m)^description\s*=\s*"([^"]*)"', path.read_text(encoding="utf-8"))
            if m:
                self.assertNotIn(LABEL, m.group(1).lower(), str(path.relative_to(ROOT)))


class DeliveryModesCase(unittest.TestCase):
    def schema_texts(self) -> dict:
        return {p.name: p.read_text(encoding="utf-8") for p in sorted((ROOT / "schemas").glob("*.json"))}

    def test_no_schema_offers_github_or_gitlab_delivery(self) -> None:
        for name, text in self.schema_texts().items():
            for phrase in ("(GitHub App)", "(GitLab CI delivery)"):
                self.assertNotIn(phrase, text, "%s offers a delivery mode that is not served" % name)

    def test_the_repository_target_description_says_only_cc_is_served(self) -> None:
        for name in ("openapi-v1.json", "items.schema.json"):
            doc = json.loads((ROOT / "schemas" / name).read_text(encoding="utf-8"))
            found = []

            def walk(node) -> None:
                if isinstance(node, dict):
                    d = node.get("description")
                    if isinstance(d, str) and d.startswith("Repository target"):
                        found.append(d)
                    for v in node.values():
                        walk(v)
                elif isinstance(node, list):
                    for v in node:
                        walk(v)

            walk(doc)
            self.assertTrue(found, name)
            for d in found:
                self.assertIn("Only `cc:` targets are served", d, name)
                self.assertIn("not supported", d, name)


class GoalStatusCase(unittest.TestCase):
    def test_the_goal_status_description_does_not_name_another_products_status_set(self) -> None:
        for p in sorted((ROOT / "schemas").glob("*.json")):
            self.assertNotIn("the Codex set", p.read_text(encoding="utf-8"), p.name)


if __name__ == "__main__":
    unittest.main()
