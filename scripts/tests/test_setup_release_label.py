"""A release that changes no file the stage runs still updates the version the backend reports.

A release whose deployed files are unchanged (no file under the paths of `digests.DEPLOYABLE_PATHS` differs: `VERSION`, the docs, the scripts and the changelog may) has the source tree the stage already runs, so
`./setup.sh --update` deploys nothing (CONTRACT C7). The version the backend reports is the `VERSION` that control-api was compiled with, so the client became the
new release and the backend kept the old one: `ham doctor` warned "backend older than this ham" and `ham update` ended with exit 3.

The release label is the stage's SSM parameter `/hamh/<stage>/release-version` (`N.NN <source tree>`). A deploy writes it with the digests. When the stage already
runs the target's source tree, setup writes it alone (`scripts/deploy.sh --stage direct --relabel`: no build, no CloudFormation, no new image), and control-api reports
it when it is a valid version, no older than the version compiled into it and bound to the source tree the stage runs. What these cases hold:

- the deploy writes the label with the digests, tagged, and only when the deploy finished; a change of `VERSION` alone moves neither the source tree, the worker image
  key nor the deployment digest;
- the relabel writes one tagged parameter and nothing else, never lowers it, repairs the tags of a label that is already right, refuses a stage that does not run this
  source tree or has no finished record, runs under the lock of the caller, and a retry finishes what a failure left;
- the runner's client-only build calls it after its own check of the account (a deploy build never does), and a refusal or a failure is the build's;
- setup's plan names the change of label before the question, on the runner route and on the local route.

The deploys run the real scripts/deploy.sh against the fakes of test_deploy.py through the wrappers of test_setup_deploy_settings.py. The runner half runs
runner-build.sh in the rig of test_setup_runner_build.py, and setup.sh runs against the stand-in AWS CLI of setup_fake_aws.py.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import subprocess  # nosec B404: tests start the script under test with argument lists, never a shell string
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(REPO / "scripts" / "lib"))
sys.path.insert(0, str(HERE))
import deploy as dp  # noqa: E402
import digests  # noqa: E402
import setup_fake_aws as fake  # noqa: E402
import test_deploy as td  # noqa: E402
import test_setup_deploy_settings as ds  # noqa: E402
import test_setup_runner_build as rb  # noqa: E402
import test_setup_update_integration as integration  # noqa: E402
import test_setup_update_runner as ur  # noqa: E402

LABEL = "/hamh/direct/release-version"
LOCK = "/hamh/direct/deploy-lock"
TREE_PARAM = "/hamh/direct/deployed-source-tree"
LABEL_LINE = re.compile(r"^DEPLOY-LABEL stage=direct version=(\d+\.\d\d)(?: was=(\d+\.\d\d)| (unchanged)| (kept):.*)$")


# The AWS CLI stand-in of these cases: the wrapper of test_setup_deploy_settings.py with one more switch, FAKE_TAG_FAILS=<parameter name>, which makes the call that
# tags that parameter fail (a deploy that is stopped between the put of a parameter and its tags).
TAG_WRAPPER = r'''#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
if args[:2] == ["ssm", "add-tags-to-resource"] and args[args.index("--resource-id") + 1] == os.environ.get("FAKE_TAG_FAILS"):
    sys.stderr.write("An error occurred (ThrottlingException) when calling the AddTagsToResource operation: Rate exceeded\n"); sys.exit(254)
os.execv(sys.executable, [sys.executable, os.environ["FAKE_WRAPPED_AWS"], *args])
'''


class LabelBase(ds.SettingsBase):
    """The deploy rig of the settings cases, with the tag switch, the stage deployed at VERSION 5.13 and helpers to read the label."""

    def setUp(self) -> None:
        super().setUp()
        wrapped = self.bins["aws"]
        p = self.tmp / "aws-tag-wrapper"
        p.write_text(TAG_WRAPPER.replace("/usr/bin/env python3", sys.executable, 1))
        p.chmod(0o755)
        self.bins["aws"] = str(p)
        self.wrapped = wrapped

    def run_deploy(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        return super().run_deploy(*args, FAKE_WRAPPED_AWS=self.wrapped, **env)

    def version_only(self, version: str, also: dict | None = None) -> str:
        """A commit that changes `VERSION` (and the files in `also`) and nothing of the deployed paths unless `also` names one."""
        (self.root / "VERSION").write_text(version + "\n")
        for rel, text in (also or {}).items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        td.git(self.root, "add", ".")
        td.git(self.root, "commit", "-q", "-m", f"release {version}")
        return td.git(self.root, "rev-parse", "HEAD")

    def tree(self) -> str:
        return self.ssm()[TREE_PARAM]

    def label(self) -> tuple[str, str]:
        version, tree = self.ssm()[LABEL].split(" ")
        return version, tree

    def label_puts(self, since: int = 0) -> list[list[str]]:
        return self.puts(LABEL, since)

    def label_tags(self, since: int = 0) -> list[list[str]]:
        return [a for a in self.aws_calls(since) if a[:2] == ["ssm", "add-tags-to-resource"] and a[a.index("--resource-id") + 1] == LABEL]

    def deployed_at(self, version: str = "5.13") -> None:
        """The stage after a real deploy of the tree at `version` (VERSION is 5.13 in the rig)."""
        if version != "5.13":
            self.version_only(version)
        self.ok()

    def relabel(self, **env: str) -> subprocess.CompletedProcess:
        return self.deploy("--relabel", **env)

    def label_line(self, proc: subprocess.CompletedProcess) -> re.Match:
        lines = [ln for ln in proc.stdout.splitlines() if ln.startswith("DEPLOY-LABEL ")]
        self.assertEqual(len(lines), 1, proc.stdout + proc.stderr)
        self.assertEqual(self.last(proc), lines[0], "the DEPLOY-LABEL line is the last line")
        m = LABEL_LINE.match(lines[0])
        self.assertIsNotNone(m, lines[0])
        return m

    def stage_changes(self) -> tuple:
        """What a build or a deploy would have left behind: the stack deploys, the cargo calls and the CodeBuild calls."""
        return (len(self.deploys()), len(self.log("cargo")), len(self.log("codebuild")))


# ---- the deploy writes the label -------------------------------------------------------------------------------------------------------

class DeployWritesTheLabelCase(LabelBase):
    def test_b839_a_deploy_writes_the_release_label_with_the_digests_and_tags_it(self) -> None:
        self.ok()
        version, tree = self.label()
        self.assertEqual((version, tree), ("5.13", self.tree()), "the label is the VERSION and the source tree the deploy recorded")
        puts = [a[a.index("--name") + 1] for a in self.aws_calls() if a[:2] == ["ssm", "put-parameter"]]
        self.assertLess(puts.index(TREE_PARAM), puts.index(LABEL), "the label follows the digests it is bound to")
        tags = self.label_tags()
        self.assertEqual(len(tags), 1)
        self.assertIn("Key=project,Value=hyperscale-agent-meta-harness", tags[0])
        self.assertIn("Key=stage,Value=direct", tags[0])

    def test_b839_a_deploy_that_ends_before_its_digests_writes_no_label(self) -> None:
        proc = self.deploy(FAKE_NO_V1_API_URL="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertNotIn(LABEL, self.ssm(), "a deploy that did not finish leaves the label of the last one that did")
        self.ok()
        before = self.ssm()[LABEL]
        self.version_only("5.14", {"crates/agentd/src/main.rs": "fn main() { println!(); }\n"})
        proc = self.deploy(FAKE_NO_V1_API_URL="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.ssm()[LABEL], before)

    def test_b839_a_deploy_of_a_changed_tree_replaces_the_label_of_the_tree_it_replaces(self) -> None:
        self.ok()
        old_tree = self.tree()
        self.version_only("5.14", {"infra/lib/app.ts": "// app 2\n"})
        self.ok()
        self.assertNotEqual(self.tree(), old_tree)
        self.assertEqual(self.label(), ("5.14", self.tree()))

    def test_b839_a_deploy_that_goes_back_to_an_older_version_lowers_the_label_before_its_first_stack_change(self) -> None:
        # the stage runs the tree T2 at 5.14 (label 5.14 T2). A rollback deploy of another tree at 5.12 changes its stacks and then fails before its digests are written:
        # control-api, built with 5.12, must not report the 5.14 of the deploy it replaced, which would show a backend that runs 5.12 code as 5.14
        self.deployed_at("5.13")
        self.version_only("5.14", {"infra/lib/app.ts": "// app 2\n"})
        self.ok()
        live = self.tree()
        self.assertEqual(self.label(), ("5.14", live))
        self.version_only("5.12", {"infra/lib/app.ts": "// app 3\n"})
        stacks = len(self.deploys())
        proc = self.deploy(FAKE_NO_V1_API_URL="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertGreater(len(self.deploys()), stacks, "TRIGGER PRESENT: a stack deploy ran before the failure")
        self.assertEqual(self.tree(), live, "the digests are the last finished deploy's")
        self.assertEqual(self.label(), ("5.12", live), "the label does not name a version above the code that is being deployed")
        # the retry that finishes writes the label of the new tree
        self.ok()
        self.assertEqual(self.label(), ("5.12", self.tree()))
        self.assertNotEqual(self.tree(), live)

    def test_b839_a_deploy_of_a_newer_version_leaves_the_label_alone_before_its_first_stack_change(self) -> None:
        self.deployed_at("5.13")
        before = self.ssm()[LABEL]
        self.version_only("5.14", {"infra/lib/app.ts": "// app 2\n"})
        mark = len(self.aws_calls())
        proc = self.deploy(FAKE_NO_V1_API_URL="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.ssm()[LABEL], before, "a label that is not above this deploy's version is not written")
        self.assertEqual(self.label_puts(mark), [])

    def test_b839_a_version_only_change_leaves_the_source_tree_the_image_key_and_the_deployment_digest_alone(self) -> None:
        # a release that changes nothing under the deployed paths (here VERSION, the README, a guide and the changelog): the three values the accepted records and the image tag pin do not move
        self.ok()
        crates = ["crates/agentd", "crates/harness-protocol", "crates/harness-core"]
        tree0, key0, digest0 = digests.source_tree(self.root), digests.image_key(self.root, crates), self.ssm()["/hamh/direct/deployment-digest"]
        self.version_only("5.14", {"README.md": "x\nmore\n", "docs/guide.md": "a guide\n", "CHANGELOG.md": "# changes\n"})
        self.assertEqual(digests.source_tree(self.root), tree0)
        self.assertEqual(digests.image_key(self.root, crates), key0)
        self.ok()
        self.assertEqual(self.ssm()["/hamh/direct/deployment-digest"], digest0, "the same stacks, the same image: the same deployment digest")
        self.assertEqual(self.label(), ("5.14", tree0), "only the label follows the release")


class SharedCasesCase(LabelBase):
    """`fixtures/release-label/cases.json` is read by this module and by the control-api tests: the deploy writes the form the file describes, and the relabel reads a label as
    the file says (a label that is well formed there is one here), so the writer, the relabel and the reader cannot drift apart."""

    def cases(self) -> dict:
        return json.loads((REPO / "fixtures" / "release-label" / "cases.json").read_text())

    def test_b839_the_label_a_deploy_writes_has_the_form_of_the_shared_cases(self) -> None:
        doc = self.cases()
        self.ok()
        written = self.ssm()[LABEL]
        self.assertRegex(written, doc["shape"], "the bytes of the deploy's own label")
        well_formed = [c for c in doc["cases"] if c["well_formed"] and c["label"] == c["label"].strip()]
        self.assertTrue(well_formed)
        for c in doc["cases"]:
            self.assertEqual(bool(re.fullmatch(doc["shape"], c["label"].strip())), c["well_formed"], c["what"])
            self.assertEqual(bool(dp.LABEL_VALUE.fullmatch(c["label"].strip())), c["well_formed"], f"the relabel reads {c['what']} as the file says")

# ---- the relabel -----------------------------------------------------------------------------------------------------------------------

class RelabelCase(LabelBase):
    def test_b839_a_relabel_writes_the_label_of_a_version_only_release_and_nothing_else(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14", {"README.md": "x\nmore\n"})
        stable = {k: v for k, v in self.ssm().items() if k not in (LABEL, LOCK)}
        record = self.ssm()[ds.SETTINGS]
        changes = self.stage_changes()
        mark = len(self.aws_calls())
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        m = self.label_line(proc)
        self.assertEqual((m.group(1), m.group(2)), ("5.14", "5.13"), proc.stdout)
        self.assertEqual(self.label(), ("5.14", self.tree()))
        self.assertEqual({k: v for k, v in self.ssm().items() if k not in (LABEL, LOCK)}, stable, "every digest and the record are as the deploy left them")
        self.assertEqual(self.ssm()[ds.SETTINGS], record)
        self.assertEqual(self.stage_changes(), changes, "no stack deploy, no cargo call and no image build")
        puts = self.label_puts(mark)
        self.assertEqual(len(puts), 1)
        self.assertEqual(len(self.label_tags(mark)), 1, "the parameter is tagged as the digests are")
        self.assertNotIn(LOCK, self.ssm(), "the lock the relabel took is gone")
        (lock_put,) = self.puts(LOCK, mark)
        self.assertIn("route=manual", lock_put[lock_put.index("--value") + 1].splitlines())
        self.assertNotIn("DEPLOY-FAIL", proc.stdout)

    def test_b839_a_relabel_never_lowers_the_label(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14")
        self.assertEqual(self.relabel().returncode, 0)
        self.version_only("5.12")
        mark = len(self.aws_calls())
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        m = self.label_line(proc)
        self.assertEqual((m.group(1), m.group(4)), ("5.14", "kept"), "a clone that is older than the label leaves it as it is: " + proc.stdout)
        self.assertEqual(self.label()[0], "5.14")
        self.assertEqual(self.label_puts(mark), [], "nothing was written")

    def test_b839_a_label_that_is_already_right_is_not_written_again_and_its_tags_are_repaired(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14")
        self.assertEqual(self.relabel().returncode, 0)
        mark = len(self.aws_calls())
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.label_line(proc).group(3), "unchanged")
        self.assertEqual(self.label_puts(mark), [])
        self.assertEqual(len(self.label_tags(mark)), 1, "the tags are put again, so a label that an earlier run left untagged is found by the teardown")

    def test_b839_a_relabel_that_stopped_after_the_put_is_finished_by_the_next_run(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14")
        mark = len(self.aws_calls())
        proc = self.relabel(FAKE_TAG_FAILS=LABEL)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("DEPLOY-FAIL", self.last(proc))
        self.assertNotIn("DEPLOY-STAGE unchanged", proc.stdout, "the label was written: the stage is not unchanged")
        self.assertEqual(self.label()[0], "5.14", "TRIGGER PRESENT: the put happened and the tags did not")
        self.assertEqual(self.label_tags(mark), [], "the call that tags the label failed before it reached the stand-in")
        self.assertNotIn(LOCK, self.ssm(), "the lock is released after a failure too")
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.label_line(proc).group(3), "unchanged")
        self.assertEqual(len(self.label_tags(mark)), 1, "the next run tags the parameter")

    def test_b839_a_stage_that_runs_another_source_tree_is_refused_and_nothing_is_written(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14", {"crates/agentd/src/main.rs": "fn main() { println!(); }\n"})
        before = self.ssm()[LABEL]
        mark = len(self.aws_calls())
        proc = self.relabel()
        reason, wait, text = self.refused(proc)
        self.assertEqual((reason, wait), ("not-deployed", "no"))
        self.assertIn("source tree", text)
        self.assertIn("./setup.sh --update --force-deploy", text, "the line names the way out")
        self.assertEqual(self.ssm()[LABEL], before)
        self.assertEqual(self.label_puts(mark), [])

    def test_b839_a_stage_without_a_finished_record_is_refused(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14")
        self.edit_record(status="deploying")
        reason, _, text = self.refused(self.relabel())
        self.assertEqual(reason, "not-deployed")
        self.assertIn("deploying", text)
        self.drop_record()
        reason, _, text = self.refused(self.relabel())
        self.assertEqual(reason, "not-deployed")
        self.assertIn("no record", text)
        self.assertEqual(self.label()[0], "5.13", "nothing was written")

    def test_b839_a_stage_whose_digest_parameters_differ_from_its_record_is_refused(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14")
        d = self.ssm()
        d[TREE_PARAM] = "e" * 64  # a deploy changed the stage after the record was written
        (self.state / "ssm.json").write_text(json.dumps(d))
        reason, _, _ = self.refused(self.relabel())
        self.assertEqual(reason, "not-deployed")
        d = self.ssm()
        d[TREE_PARAM] = self.label()[1]
        d["/hamh/direct/deployment-config-digest"] = "f" * 64
        (self.state / "ssm.json").write_text(json.dumps(d))
        reason, _, text = self.refused(self.relabel())
        self.assertEqual(reason, "not-deployed")
        self.assertIn("account file", text)

    def test_b839_a_relabel_runs_under_the_lock_of_its_caller_and_leaves_it_alone(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14")
        token = "a" * 32
        self.seed_lock(token)
        mark = len(self.aws_calls())
        proc = self.relabel(HAMH_DEPLOY_LOCK_TOKEN=token)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertIn(LOCK, self.ssm(), "the lock of setup is setup's to delete")
        self.assertEqual(self.puts(LOCK, mark), [], "no second lock was made")
        self.assertEqual(self.label()[0], "5.14")
        self.version_only("5.15")
        reason, wait, _ = self.refused(self.relabel(HAMH_DEPLOY_LOCK_TOKEN="b" * 32))
        self.assertEqual((reason, wait), ("lock", "yes"))
        self.assertEqual(self.label()[0], "5.14")

    def test_b839_b804_a_relabel_runs_under_a_caller_lock_that_says_it_deploys_nothing(self) -> None:
        # An update of a commit the stage already runs holds its lock with the line deploys=no, so the stage keeps starting runs, and the runner's client-only
        # build hands the relabel the token of that lock. The label changes nothing the stage runs, so it is written under such a lock. A deploy is still refused
        # under it (test_b804_wait_a_deploy_never_runs_under_a_lock_that_says_it_deploys_nothing).
        self.deployed_at("5.13")
        self.version_only("5.14")
        token = "a" * 32
        text = self.seed_lock(token) + "\ndeploys=no"
        d = self.ssm()
        d[LOCK] = text
        (self.state / "ssm.json").write_text(json.dumps(d))
        print("B839-B804 TRIGGER PRESENT: the caller's lock says deploys=no:", "deploys=no" in self.ssm()[LOCK].splitlines())
        mark = len(self.aws_calls())
        proc = self.relabel(HAMH_DEPLOY_LOCK_TOKEN=token)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        m = self.label_line(proc)
        self.assertEqual((m.group(1), m.group(2)), ("5.14", "5.13"), proc.stdout)
        self.assertEqual(self.label(), ("5.14", self.tree()))
        self.assertEqual(self.ssm()[LOCK], text, "the caller's lock is left as it is: not written again, not closed, not deleted")
        self.assertEqual(self.puts(LOCK, mark), [], "the relabel wrote no lock")
        # another token is refused under such a lock too
        self.version_only("5.15")
        reason, wait, _ = self.refused(self.relabel(HAMH_DEPLOY_LOCK_TOKEN="b" * 32))
        self.assertEqual((reason, wait), ("lock", "yes"))
        self.assertEqual(self.label()[0], "5.14")

    def test_b839_b804_the_lock_a_relabel_takes_itself_says_that_it_deploys_nothing(self) -> None:
        # A relabel without a caller's lock (the local route of setup, a run by hand) takes the lock as a deploy does. Its hold changes nothing the stage runs, so the
        # lock carries the line deploys=no and the stage's API service keeps starting runs while the label is written.
        self.deployed_at("5.13")
        deploy_locks = self.puts(LOCK)
        self.assertEqual(len(deploy_locks), 1, deploy_locks)
        self.assertNotIn("deploys=no", deploy_locks[0][deploy_locks[0].index("--value") + 1].splitlines(), "the lock of a deploy closes the stage")
        self.version_only("5.14")
        mark = len(self.aws_calls())
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        locks = self.puts(LOCK, mark)
        self.assertEqual(len(locks), 1, locks)
        lines = locks[0][locks[0].index("--value") + 1].splitlines()
        self.assertIn("deploys=no", lines, lines)
        self.assertIn("route=manual", lines, lines)
        self.assertNotIn("--overwrite", locks[0], "create-only, as a deploy takes it")
        self.assertNotIn(LOCK, self.ssm(), "the relabel deleted the lock it took")
        self.assertEqual(self.label()[0], "5.14")

    def test_b839_without_a_label_the_version_of_the_record_is_the_one_to_beat(self) -> None:
        # a stage whose deploy wrote no label (the parameter was removed): control-api reports the VERSION it was compiled with, which the record names
        self.deployed_at("5.13")
        d = self.ssm()
        d.pop(LABEL)
        (self.state / "ssm.json").write_text(json.dumps(d))
        mark = len(self.aws_calls())
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.label_line(proc).group(3), "unchanged", "the version the backend reports already is 5.13")
        self.assertEqual(self.label_puts(mark), [])
        self.version_only("5.14")
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.label(), ("5.14", self.tree()))

    def test_b839_a_label_of_another_tree_does_not_count_and_is_replaced(self) -> None:
        # what an older release left behind: the label names a tree the stage no longer runs, control-api ignores it, and the relabel compares with the record
        self.deployed_at("5.13")
        d = self.ssm()
        d[LABEL] = "5.99 " + "c" * 64
        (self.state / "ssm.json").write_text(json.dumps(d))
        self.version_only("5.14")
        proc = self.relabel()
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        m = self.label_line(proc)
        self.assertEqual((m.group(1), m.group(2)), ("5.14", "5.13"), proc.stdout)
        self.assertEqual(self.label(), ("5.14", self.tree()))

    def test_b839_a_version_that_is_not_n_dot_nn_stops_the_relabel_before_it_writes(self) -> None:
        self.deployed_at("5.13")
        (self.root / "VERSION").write_text("latest\n")
        td.git(self.root, "add", ".")
        td.git(self.root, "commit", "-q", "-m", "bad")
        mark = len(self.aws_calls())
        proc = self.relabel()
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("DEPLOY-FAIL version", self.last(proc))
        self.assertEqual(self.label_puts(mark), [])

    def test_b839_the_relabel_works_in_the_environment_the_client_only_build_gives_it(self) -> None:
        # runner-build.sh runs deploy.sh from a repository it made of the archive, with the commit the user asked for in HAMH_SOURCE_COMMIT, the token of the lock setup holds
        # and none of the variables that phase_deploy exports for a deploy (the folders of the deploy record and of the criteria)
        self.deployed_at("5.13")
        self.version_only("5.14")
        token = "ab" * 16
        # the lock of the runner route: setup holds it with the line deploys=no for an update that deploys nothing, so this case holds the runner's
        # environment and the runner's lock together
        text = self.seed_lock(token) + "\ndeploys=no"
        d = self.ssm()
        d[LOCK] = text
        (self.state / "ssm.json").write_text(json.dumps(d))
        mark = len(self.aws_calls())
        records = sorted(p.name for p in self.records.iterdir())
        proc = self.relabel(HAMH_SOURCE_COMMIT="d" * 40, HAMH_DEPLOY_LOCK_TOKEN=token, DEPLOY_RECORD_DIR=None, DEPLOY_CRITERIA=None)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        m = self.label_line(proc)
        self.assertEqual((m.group(1), m.group(2)), ("5.14", "5.13"), proc.stdout)
        self.assertEqual(self.label(), ("5.14", self.tree()))
        self.assertEqual(sorted(p.name for p in self.records.iterdir()), records, "the relabel writes no deploy record")
        self.assertEqual(self.ssm()[LOCK], text, "the lock of setup is setup's to delete: not written again, not closed, not deleted")
        self.assertEqual(self.puts(LOCK, mark), [], "the relabel wrote no lock")

    def test_b839_relabel_goes_with_no_other_mode(self) -> None:
        for other in ("--bump-worker-version", "--check-guard", "--print-settings"):
            proc = self.deploy("--relabel", other)
            self.assertEqual(proc.returncode, 2, f"{other}: {proc.stdout}{proc.stderr}")


class RelabelLineCase(LabelBase):
    """The lines the real `deploy.sh --relabel` prints are the lines the client-only build of the runner reads (its stand-in deploy.sh prints what it is told to)."""

    def test_b839_every_line_the_real_relabel_prints_is_read_by_the_client_only_build(self) -> None:
        self.deployed_at("5.13")
        self.version_only("5.14")
        lines = [self.last(self.relabel())]
        lines.append(self.last(self.relabel()))
        self.version_only("5.12")
        lines.append(self.last(self.relabel()))
        self.assertIn(" was=5.13", lines[0])
        self.assertTrue(lines[1].endswith(" unchanged"), lines[1])
        self.assertIn(" kept: ", lines[2])
        for n, line in enumerate(lines, start=2):
            rig = rb.Rig(self)
            self.assertEqual(rig.run().returncode, 0, "the first build deploys")
            rig.mark_deployed()
            rig.build_id = f"hamh-setup-runner:99999999-2222-3333-4444-55555555555{n}"
            proc = rig.run(CODEBUILD_BUILD_ID=rig.build_id, SETUP_CLIENT_ONLY="1", FAKE_RELABEL="line", FAKE_RELABEL_LINE=line)
            self.assertEqual(proc.returncode, 0, f"{line}: {proc.stdout}{proc.stderr}")
            detail = {p["name"]: p for p in rig.phase_lines(rig.build_id)}["deploy"]["detail"]
            version = line.split("version=")[1].split(" ")[0]
            self.assertIn(f"release label {version}", detail, line)


# ---- the runner's client-only build ----------------------------------------------------------------------------------------------------

class RunnerBuildCase(unittest.TestCase):
    """runner-build.sh with SETUP_CLIENT_ONLY=1 writes the label through `deploy.sh --relabel`; a deploy build never does."""

    def deployed_rig(self) -> rb.Rig:
        rig = rb.Rig(self)
        self.assertEqual(rig.run().returncode, 0, "the first build deploys")
        rig.mark_deployed()
        return rig

    def client_only(self, rig: rb.Rig, n: int = 2, **env: str):
        rig.build_id = f"hamh-setup-runner:99999999-2222-3333-4444-55555555555{n}"
        return rig.run(CODEBUILD_BUILD_ID=rig.build_id, SETUP_CLIENT_ONLY="1", **env)

    def test_b839_a_client_only_build_writes_the_label_after_its_own_check_and_deploys_nothing(self) -> None:
        rig = self.deployed_rig()
        deploys = len(rig.calls("deploy"))
        token = "ab" * 16
        proc = self.client_only(rig, SETUP_DEPLOY_LOCK_TOKEN=token, FAKE_LABEL_VERSION="5.14")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(rig.calls("deploy")), deploys, "the build deployed nothing")
        (call,) = rig.calls("relabel")
        self.assertEqual(call["argv"], ["--stage", "direct", "--relabel"])
        self.assertEqual(call["env"].get("HAMH_DEPLOY_LOCK_TOKEN"), token, "the relabel runs under the lock setup holds")
        phases = {line["name"]: line for line in rig.phase_lines(rig.build_id)}
        self.assertEqual(phases["deploy"]["state"], "skipped")
        self.assertIn("release label 5.14", phases["deploy"]["detail"], "the phase says what it did")
        self.assertEqual(phases["client"]["state"], "ok")

    def test_b839_a_deploy_build_never_calls_the_relabel(self) -> None:
        rig = rb.Rig(self)
        self.assertEqual(rig.run().returncode, 0)
        self.assertEqual(rig.calls("relabel"), [])

    def test_b839_a_relabel_that_the_stage_refuses_ends_the_build_as_refused_and_the_client_is_not_built(self) -> None:
        rig = self.deployed_rig()
        clients = len(rig.calls("client"))
        proc = self.client_only(rig, FAKE_RELABEL="refused")
        self.assertNotEqual(proc.returncode, 0)
        res = rig.result()
        self.assertEqual(res["failedPhase"], "refused")
        self.assertTrue(res["detail"].startswith("reason=not-deployed wait=no stage=unchanged :: "), res["detail"])
        self.assertTrue(res.get("stageUnchanged"))
        self.assertEqual(len(rig.calls("client")), clients, "no second client build")

    def test_b839_a_relabel_that_fails_ends_the_build_in_the_deploy_phase_naming_the_step(self) -> None:
        rig = self.deployed_rig()
        proc = self.client_only(rig, FAKE_RELABEL="fail")
        self.assertNotEqual(proc.returncode, 0)
        res = rig.result()
        self.assertEqual(res["failedPhase"], "deploy")
        self.assertIn("release-label", res["detail"])
        self.assertFalse(res.get("stageUnchanged"), "the label may be written: the build does not say the stage is unchanged")

    def test_b839_the_account_is_checked_before_the_label_is_written(self) -> None:
        rig = rb.Rig(self)
        proc = rig.run(SETUP_CLIENT_ONLY="1")
        self.assertNotEqual(proc.returncode, 0, "a client-only build on a stage that does not run this tree refuses")
        self.assertEqual(rig.result()["failedPhase"], "refused")
        self.assertEqual(rig.calls("relabel"), [], "no label is written for a stage that does not run this source tree")


# ---- setup.sh: the plan and the local route --------------------------------------------------------------------------------------------

def source_tree_of(repo: Path) -> str:
    return digests.source_tree(repo)


# The stand-in client of the setup tests prints `ham v9.99`, and setup installs a client only when it prints the VERSION of the commit it was built from: the release
# of these cases is 9.99, and the stage records an earlier version of the same source tree.
RELEASE = "9.99"


class RunnerRoutePlanCase(ur.RunnerUpdateBase):
    """The runner route: the plan names the change of the label before the question; the build is the client-only build of C7."""

    def docs_only_commit(self) -> str:
        """A commit that changes a file no stage runs (the source tree of the stage does not move)."""
        (self.repo / "docs").mkdir(exist_ok=True)
        (self.repo / "docs" / "note.md").write_text("a release note\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "docs of the release")
        return self.commit()

    def stage_at(self, recorded: str, label: str | None = None) -> str:
        """A stage that runs the source tree of the checkout (VERSION 9.99), whose record names `recorded`; `label` is the value of its label parameter."""
        self.set_scenario(nine_stacks=True)
        (self.repo / "VERSION").write_text(RELEASE + "\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", f"VERSION {RELEASE}")
        head = self.deployed()
        cfg = self.deploy_starts()[0]["SETUP_CONFIG_SHA256"]
        self.seed_record(status="deployed", commit=head, tree=source_tree_of(self.repo), version=recorded, accounts=cfg, operator="x", azs="use1-az1",
                         updated=ur.NOW)
        if label is not None:
            w = self.open_world()
            w.put_param(LABEL, label, dict(fake.PROJECT_TAGS))
            w.save()
        return head

    def test_b839_u55_the_plan_of_a_version_only_release_names_the_label_and_the_build_is_client_only(self) -> None:
        self.stage_at("9.98")
        self.docs_only_commit()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        said = self.said(p)
        self.assertIn("the stage already runs this commit", said)
        self.assertRegex(said, r"label: the backend reports v9\.98 and will report v9\.99")
        self.assertIn("nothing is deployed", said)
        self.assertIn("only the release label of the backend is written", said, "the sentence of the build says what is written")
        self.assertNotIn("nothing on the stage changes", said, "the label is a change of the stage, so the sentence does not say that nothing changes")
        env = self.deploy_starts()[-1]
        self.assertEqual(env.get("SETUP_CLIENT_ONLY"), "1")
        self.assertEqual(self.world().get("n_deploys", 0), 1, "TRIGGER PRESENT: only the first deploy of the case ran")

    def test_b839_u55_the_plan_of_a_version_only_release_names_no_deploy_and_no_stop_for_runs(self) -> None:
        # The plan of an update that deploys nothing says so in every line: no new revision, no image or deploy duration, and no stop for
        # active runs, with and without --wait-for-runs.
        self.stage_at("9.98")
        self.docs_only_commit()
        p = self.run_full(*self.UPDATE, "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = " ".join((p.stdout + " " + p.stderr).split())
        self.assertRegex(plan, r"label: the backend reports v9\.98 and will report v9\.99", "TRIGGER PRESENT: the update only writes the label")
        self.assertIn("guard: this update deploys nothing, so active runs do not stop it and the stage keeps starting runs.", plan)
        self.assertNotIn("Active runs stop ./setup.sh --update with exit 7", plan)
        self.assertIn("update: nothing is deployed; the runner is installed again, builds only the client and is removed afterwards", plan)
        self.assertNotIn("gets the new revision", plan)
        self.assertNotIn("duration:", plan)
        self.assertNotIn(" image: ", plan)

    def test_b839_u55_a_kept_client_is_not_installed_alone_while_the_label_is_older(self) -> None:
        # An update of a commit the stage already runs installs the client that an earlier update of this commit downloaded and checked, without
        # a build. While the backend still reports an older version that is one build too few: the client-only build is what writes the label.
        self.stage_at("9.98")
        self.docs_only_commit()
        # the client is downloaded and checked, then `ham models` fails here, so the new client is not installed
        p = self.run_full(*self.UPDATE, FAKE_MODELS_RC="1")
        self.assertNotEqual(p.returncode, 0, self.out(p))
        kept = self.receipt_path().parent / "download" / "verified" / "SHA256SUMS"
        self.assertTrue(kept.is_file(), "TRIGGER PRESENT: the checked client is kept in the receipt folder")
        # the stage reports the older version again: a deploy of an older commit with the same source tree lowered its label
        w = self.open_world()
        w.put_param(LABEL, f"9.98 {source_tree_of(self.repo)}", dict(fake.PROJECT_TAGS))
        w.save()
        starts = len(self.deploy_starts())
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        said = self.said(q)
        self.assertRegex(said, r"label: the backend reports v9\.98 and will report v9\.99", "TRIGGER PRESENT: the update has the label to write")
        self.assertNotIn("downloaded and checked earlier for this commit", said, "the kept client is not installed alone")
        self.assertEqual(len(self.deploy_starts()), starts + 1, "the client-only build runs, because it writes the label")
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")

    def test_b839_u55_the_label_the_stage_holds_is_the_one_the_plan_starts_from(self) -> None:
        self.stage_at("9.90", label=f"9.95 {source_tree_of(self.repo)}")
        self.docs_only_commit()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(self.said(p), r"label: the backend reports v9\.95 and will report v9\.99")

    def test_b839_u55_a_label_of_another_tree_is_not_the_one_the_plan_starts_from(self) -> None:
        self.stage_at("9.90", label="9.97 " + "c" * 64)
        self.docs_only_commit()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(self.said(p), r"label: the backend reports v9\.90 and will report v9\.99")

    def test_b839_u55_a_release_with_the_label_already_in_place_names_nothing(self) -> None:
        self.stage_at("9.98", label=f"{RELEASE} {source_tree_of(self.repo)}")
        self.docs_only_commit()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("label:", self.said(p))
        self.assertIn("nothing on the stage changes", self.said(p), "no label is written: the earlier sentence stands")

    def test_b839_u55_a_clone_older_than_the_label_says_the_label_stays(self) -> None:
        self.stage_at("9.98", label=f"10.00 {source_tree_of(self.repo)}")
        self.docs_only_commit()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(self.said(p), r"label: the backend reports v10\.00 and keeps it \(this clone's VERSION is 9\.99, which is older\)")
        self.assertIn("nothing on the stage changes", self.said(p), "the label stays: the earlier sentence stands")

    def test_b839_u55_a_label_the_build_could_not_write_says_that_only_the_label_is_in_question(self) -> None:
        # the build wrote the label through deploy.sh --relabel, which failed (a throttled put, say): the stage runs the source tree it ran before, so the message must not
        # offer the teardown of a stage with "partial deployment changes"
        self.stage_at("9.98")
        self.docs_only_commit()
        self.set_scenario(build={"result": "fail", "failed_phase": "deploy", "detail": "release-label release label: DEPLOY-FAIL could not write /hamh/direct/release-version",
                                 "result_extra": {"failedStep": "release-label"}})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        said = self.said(p)
        self.assertIn("nothing was deployed, and only its version label is in question", said)
        self.assertNotIn("partial deployment changes", said, "TRIGGER PRESENT: the generic text of a failed deploy says it")
        self.assertNotIn("./setup.sh --teardown", said)
        self.assertIn("You run ./setup.sh --update to try again", said)

    def test_b839_u55_a_release_that_changes_a_deployed_file_deploys_and_names_no_label_step(self) -> None:
        self.stage_at("9.98")
        (self.repo / "infra").mkdir(exist_ok=True)
        (self.repo / "infra" / "change.yaml").write_text("a: 1\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "a change the stage deploys")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("label:", self.said(p), "the deploy writes the label itself")
        self.assertNotEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")


class LocalRouteCase(integration.IntBase):
    """`--build-on local`: the client-only update writes the label through scripts/deploy.sh, a full update does not."""

    def deploy_calls(self) -> list:
        return [ln.split("|")[0] for ln in self.log_lines("deploy.log")]

    def same_tree_stage(self, recorded: str) -> None:
        """A stage that runs the source tree of the checkout (VERSION 9.99, the version the stand-in client prints), recorded at `recorded`: a local deploy first, then
        the record and the two parameters the local route reads live, and a commit that changes no deployed file."""
        self.stage(nine_stacks=True)
        self.with_registry()
        self.local_checkout()
        self.ham_on_path()
        (self.repo / "VERSION").write_text(RELEASE + "\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", f"VERSION {RELEASE}")
        first = self.run_local()
        self.assertEqual(first.returncode, 0, self.out(first))
        tree = source_tree_of(self.repo)
        config = hashlib.sha256((self.receipt_dir() / "accounts-local.json").read_bytes()).hexdigest()
        ur.RunnerUpdateBase.seed_record(self, status="deployed", commit=self.commit(), tree=tree, version=recorded, accounts=config, operator="x", azs="use1-az1",
                                        updated=ur.NOW)
        w = self.open_world()
        w.put_param(fake.SOURCE_TREE_PARAM, tree, dict(fake.PROJECT_TAGS))
        w.put_param(fake.CONFIG_DIGEST_PARAM, config, dict(fake.PROJECT_TAGS))
        w.save()
        self.write_commit({"docs/note.md": "a release note\n"}, "docs of the release")

    def test_b839_u55_the_local_client_only_update_writes_the_label_with_the_relabel_mode_of_the_deploy_script(self) -> None:
        self.same_tree_stage("9.98")
        before = len(self.deploy_calls())
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        said = " ".join(self.out(p).split())
        self.assertIn("the stage already runs this commit", said)
        self.assertRegex(said, r"label: the backend reports v9\.98 and will report v9\.99")
        calls = self.deploy_calls()[before:]
        self.assertEqual(len(calls), 1, calls)
        self.assertIn("--relabel", calls[0])
        self.assertNotIn("--azs", calls[0], "the label step is not a deploy")
        self.assertIn("DEPLOY-LABEL stage=direct", self.out(p))

    def test_b839_u55_a_relabel_that_fails_stops_the_local_update_before_the_client_is_built(self) -> None:
        self.same_tree_stage("9.98")
        cargo = len(self.log_lines("cargo.log"))
        p = self.run_local(FAKE_RELABEL="fail")
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("release label", " ".join(p.stderr.split()))
        self.assertEqual(len(self.log_lines("cargo.log")), cargo, "no client was built")
        p = self.run_local(FAKE_RELABEL="refused")
        self.assertEqual(p.returncode, 5, self.out(p))
        self.assertIn("The stage is unchanged.", " ".join(p.stderr.split()))
        self.assertEqual(len(self.log_lines("cargo.log")), cargo, "no client was built")

    def test_b839_u55_a_full_local_update_deploys_and_does_not_call_the_relabel(self) -> None:
        self.same_tree_stage("9.98")
        self.write_commit({"infra/change.yaml": "a: 1\n"})
        before = len(self.deploy_calls())
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        calls = self.deploy_calls()[before:]
        self.assertTrue(calls and all("--relabel" not in c for c in calls), calls)


# ---- the row of the matrix --------------------------------------------------------------------------------------------------------------

class ScenarioRowCase(unittest.TestCase):
    """The row of the update matrix for these cases is U55: U45 to U54 are the rows of the item before, so a test named for another row pairs this change with it."""

    def test_b839_the_cases_name_the_row_u55_and_no_other_row(self) -> None:
        ids = set(re.findall(r"b839_u(\d+)_", (HERE / "test_setup_release_label.py").read_text()))
        ids |= set(re.findall(r"b839_u(\d+)_", (REPO / "crates" / "ham" / "tests" / "b804_update.rs").read_text()))
        self.assertEqual(ids, {"55"}, "the cases of the Python module and of the ham update tests name one row")
        self.assertIn("is U55", ScenarioRowCase.__doc__, "the module names its row in the text of this class")


if __name__ == "__main__":
    unittest.main()
