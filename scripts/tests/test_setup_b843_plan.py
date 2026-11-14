"""What the plan of `./setup.sh --update` says about the version label and about the commit the stage runs.

Two lines of the plan said less than is true, or something that is not:

- label     an update that deploys named no label. The deploy writes the label of the version it deploys, also when that version is older than the label the stage
            holds: `./setup.sh --update --force-deploy` from a clone at 5.19 on a stage whose label read 5.20 lowered the label, and only the build log said so. The
            plan now has the line `label: the backend reports vA and will report vB after the deploy` whenever the two versions differ, and says that the number
            goes down when it does. The label the backend reports now is the stage's label when it names the source tree the stage's record names, else the
            VERSION of the record. An update that deploys the version the backend already reports names no label.
- commit    an update that only writes the label (a release that changes no file the stage runs) said "the stage already runs this commit (its record names this
            commit, ...)" while it installed another commit with the same source tree. It now says "the stage already runs this source tree" and names the commit
            of the record; the earlier sentence stays for an update of the commit the record names.
- after it  four lines that follow that sentence said "the stage already runs this commit" for such a record all the same: the `build:` line of the plan and the
            result line of an update that installs the client it kept from an earlier update of this commit (no build), the way on after a build whose client
            could not be installed, and the way on after a build that failed in a step after its deploy phase. They now say "this source tree" when the update
            deploys nothing and the record names another commit, and the last one no longer says that a deploy finished when the build deployed nothing.

The cases run setup.sh against the stand-in AWS CLI of setup_fake_aws.py, with the rigs of the release label cases (test_setup_release_label.py), on the runner route
and with `--build-on local`.
"""

from __future__ import annotations

import os
import subprocess  # nosec B404: one case starts bash with an argument list and a script file, never a shell string
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
sys.path.insert(0, str(HERE))
import setup_fake_aws as fake  # noqa: E402
import test_setup_release_label as rl  # noqa: E402
from setup_waits import limit  # noqa: E402
import test_setup_update_integration as integration  # noqa: E402
import test_setup_update_runner as ur  # noqa: E402

RELEASE = rl.RELEASE
SAME_COMMIT = "the stage already runs this commit (its record names this commit, the same account file and the same settings)"
LOWER = "this clone's VERSION is older, and a deploy sets the version label of the stage to the version it deploys"


class RunnerRouteLabelCase(ur.RunnerUpdateBase):
    """The runner route: the plan of an update that deploys names the label the deploy writes."""

    stage_at = rl.RunnerRoutePlanCase.stage_at
    docs_only_commit = rl.RunnerRoutePlanCase.docs_only_commit

    def plan(self, *extra: str) -> str:
        p = self.run_full(*self.UPDATE, "--dry-run", *extra)
        self.assertEqual(p.returncode, 0, self.out(p))
        return " ".join((p.stdout + " " + p.stderr).split())

    def deployed_file_commit(self) -> str:
        (self.repo / "infra").mkdir(exist_ok=True)
        (self.repo / "infra" / "change.yaml").write_text("a: 1\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "a change the stage deploys")
        return self.commit()

    def test_b843_the_plan_of_a_forced_deploy_names_the_label_it_lowers(self) -> None:
        # the live case: the stage's label is above the VERSION of the clone (a release that only wrote the label came first), and --force-deploy deploys the clone
        self.stage_at(RELEASE, label=f"10.00 {rl.source_tree_of(self.repo)}")
        plan = self.plan("--force-deploy")
        self.assertIn("deploy: --force-deploy", plan, "TRIGGER PRESENT: the update deploys")
        self.assertIn(f"label: the backend reports v10.00 and will report v{RELEASE} after the deploy ({LOWER})", plan)
        self.assertNotIn("and keeps it (", plan, "a deploy does not keep a label above the version it deploys")

    def test_b843_the_plan_of_an_update_that_deploys_a_newer_version_names_the_label(self) -> None:
        self.stage_at("9.98")
        self.deployed_file_commit()
        plan = self.plan()
        self.assertIn("gets the new revision", plan, "TRIGGER PRESENT: the update deploys")
        self.assertIn(f"label: the backend reports v9.98 and will report v{RELEASE} after the deploy", plan)
        self.assertNotIn(LOWER, plan)
        self.assertNotIn("only its version label is written", plan, "that is the sentence of an update that deploys nothing")

    def test_b843_the_label_of_the_tree_the_stage_runs_is_the_one_the_plan_starts_from(self) -> None:
        # the stage's label names the source tree of its record, so the backend reports it; the target of the update has another tree
        self.stage_at("9.90", label=f"9.95 {rl.source_tree_of(self.repo)}")
        self.deployed_file_commit()
        self.assertIn(f"label: the backend reports v9.95 and will report v{RELEASE} after the deploy", self.plan())

    def test_b843_a_label_of_another_tree_is_not_the_one_the_plan_starts_from(self) -> None:
        self.stage_at("9.90", label="9.97 " + "c" * 64)
        self.deployed_file_commit()
        self.assertIn(f"label: the backend reports v9.90 and will report v{RELEASE} after the deploy", self.plan())

    def test_b843_a_deploy_of_the_version_the_backend_reports_names_no_label(self) -> None:
        self.stage_at(RELEASE, label=f"{RELEASE} {rl.source_tree_of(self.repo)}")
        plan = self.plan("--force-deploy")
        self.assertIn("deploy: --force-deploy", plan, "TRIGGER PRESENT: the update deploys")
        self.assertNotIn("label:", plan)

    def test_b843_the_run_says_the_same_line_as_the_dry_run(self) -> None:
        self.stage_at(RELEASE, label=f"10.00 {rl.source_tree_of(self.repo)}")
        p = self.run_full(*self.UPDATE, "--force-deploy")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(f"label: the backend reports v10.00 and will report v{RELEASE} after the deploy", self.said(p))
        self.assertNotEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1", "TRIGGER PRESENT: the build deploys")


class RunnerRouteCommitCase(ur.RunnerUpdateBase):
    """The runner route: the sentence of an update that deploys nothing says which commit the stage records."""

    stage_at = rl.RunnerRoutePlanCase.stage_at
    docs_only_commit = rl.RunnerRoutePlanCase.docs_only_commit

    def test_b843_a_label_only_update_says_that_the_stage_runs_this_source_tree_and_names_the_recorded_commit(self) -> None:
        recorded = self.stage_at("9.98")
        target = self.docs_only_commit()
        self.assertNotEqual(recorded, target, "TRIGGER PRESENT: the update installs another commit than the one the stage records")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        said = self.said(p)
        self.assertIn(f"the stage already runs this source tree (its record names commit {recorded[:12]}, which has the same source tree as this commit, "
                      "the same account file and the same settings)", said)
        self.assertNotIn("its record names this commit", said, "the record names another commit")
        self.assertIn("only the release label of the backend is written", said)
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1")

    def test_b843_a_label_only_update_with_the_label_in_place_says_the_same_about_the_commit(self) -> None:
        recorded = self.stage_at("9.98", label=f"{RELEASE} {rl.source_tree_of(self.repo)}")
        self.docs_only_commit()
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        said = self.said(p)
        self.assertIn(f"the stage already runs this source tree (its record names commit {recorded[:12]}", said)
        self.assertIn("nothing on the stage changes", said)

    def test_b843_an_update_of_the_commit_the_record_names_keeps_its_sentence(self) -> None:
        self.stage_at(RELEASE, label=f"{RELEASE} {rl.source_tree_of(self.repo)}")
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn(SAME_COMMIT, self.said(p))
        self.assertNotIn("this source tree (its record names commit", self.said(p))


class RunnerRouteAfterTheSentenceCase(ur.RunnerUpdateBase):
    """The runner route: the lines that follow the sentence of an update that deploys nothing name what that sentence named, the commit or the source tree."""

    stage_at = rl.RunnerRoutePlanCase.stage_at
    docs_only_commit = rl.RunnerRoutePlanCase.docs_only_commit

    THIS_COMMIT = "the stage already runs this commit"
    THIS_TREE = "the stage already runs this source tree"

    def kept_client(self) -> None:
        """An update that deploys nothing downloads and checks the client and ends before it is installed (`ham models` fails here). Its build wrote the release
        label; the stand-in build writes none, so the label is put in its place here."""
        p = self.run_full(*self.UPDATE, FAKE_MODELS_RC="1")
        self.assertNotEqual(p.returncode, 0, self.out(p))
        kept = self.receipt_path().parent / "download" / "verified" / "SHA256SUMS"
        self.assertTrue(kept.is_file(), "TRIGGER PRESENT: the checked client is kept in the receipt folder")
        w = self.open_world()
        w.put_param(rl.LABEL, f"{RELEASE} {rl.source_tree_of(self.repo)}", dict(fake.PROJECT_TAGS))
        w.save()

    def test_b843_an_update_that_installs_the_kept_client_names_the_source_tree_in_its_plan_and_its_result(self) -> None:
        # the live order: the first update of a release that changes no file the stage runs writes the label and downloads the client, and stops before the install
        recorded = self.stage_at("9.98")
        target = self.docs_only_commit()
        self.assertNotEqual(recorded, target, "TRIGGER PRESENT: the stage records another commit than the one the update installs")
        self.kept_client()
        starts = len(self.deploy_starts())
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        said = self.said(q)
        self.assertEqual(len(self.deploy_starts()), starts, "TRIGGER PRESENT: no build ran: the client kept on this machine is installed")
        self.assertIn("downloaded and checked earlier for this commit", said, "TRIGGER PRESENT: the update installs the kept client")
        self.assertIn(f"{self.THIS_TREE} (its record names commit {recorded[:12]}, which has the same source tree as this commit", said)
        self.assertIn(f"build: none: {self.THIS_TREE}, and the client of an earlier update of this commit is kept on this machine", said)
        self.assertIn(f"ok {self.THIS_TREE}: {self.ENDPOINT}", said)
        self.assertNotIn(self.THIS_COMMIT, said, "the record names another commit, so no line says that the stage runs this one")

    def test_b843_the_dry_run_of_that_update_says_the_same_build_line(self) -> None:
        self.stage_at("9.98")
        self.docs_only_commit()
        self.kept_client()
        d = self.run_full("--update", "--profile", "work", "--region", "us-east-1", "--dry-run")
        self.assertEqual(d.returncode, 0, self.out(d))
        plan = self.said(d)
        self.assertIn("downloaded and checked earlier for this commit", plan, "TRIGGER PRESENT: the plan is the one of the kept client")
        self.assertIn(f"build: none: {self.THIS_TREE}, and the client of an earlier update of this commit is kept on this machine", plan)
        self.assertNotIn(self.THIS_COMMIT, plan)

    def test_b843_an_update_of_the_recorded_commit_that_installs_the_kept_client_keeps_its_lines(self) -> None:
        # the record names the commit of the checkout: the two lines say "this commit", as before
        self.stage_at(RELEASE, label=f"{RELEASE} {rl.source_tree_of(self.repo)}")
        self.kept_client()
        starts = len(self.deploy_starts())
        q = self.run_full(*self.UPDATE)
        self.assertEqual(q.returncode, 0, self.out(q))
        said = self.said(q)
        self.assertEqual(len(self.deploy_starts()), starts, "TRIGGER PRESENT: no build ran: the client kept on this machine is installed")
        self.assertIn(f"build: none: {self.THIS_COMMIT}, and the client of an earlier update of it is kept on this machine", said)
        self.assertIn(f"ok {self.THIS_COMMIT}: {self.ENDPOINT}", said)
        self.assertNotIn(self.THIS_TREE, said)

    def test_b843_a_client_that_is_not_installed_after_a_label_only_build_names_the_source_tree_as_the_way_on(self) -> None:
        recorded = self.stage_at("9.98")
        self.docs_only_commit()
        self.set_scenario(build={"client": "tamper"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        said = self.said(p)
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1", "TRIGGER PRESENT: the build deployed nothing")
        self.assertEqual(self.open_world().stage_record()["commit"], recorded, "TRIGGER PRESENT: the record still names the earlier commit")
        self.assertIn("the client from your bucket was not installed", said)
        self.assertIn(f"({self.THIS_TREE}, so the update builds only the client)", said)
        self.assertNotIn(self.THIS_COMMIT, said)

    def test_b843_a_client_that_is_not_installed_after_a_build_that_deployed_keeps_its_way_on(self) -> None:
        self.stage_at("9.98")
        (self.repo / "infra").mkdir(exist_ok=True)
        (self.repo / "infra" / "change.yaml").write_text("a: 1\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "a change the stage deploys")
        self.set_scenario(build={"client": "tamper"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        said = self.said(p)
        self.assertNotEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1", "TRIGGER PRESENT: the build deployed")
        self.assertIn(f"({self.THIS_COMMIT}, so the update builds only the client)", said)

    def test_b843_a_label_only_build_that_fails_in_a_later_step_does_not_say_that_a_deploy_finished(self) -> None:
        recorded = self.stage_at("9.98")
        target = self.docs_only_commit()
        self.set_scenario(build={"result": "fail", "final": "FAILED", "failed_phase": "client", "detail": "build-client.sh exit 101: cargo failed"})
        p = self.run_full(*self.UPDATE)
        self.assertEqual(p.returncode, 5, self.out(p))
        said = self.said(p)
        self.assertEqual(self.deploy_starts()[-1].get("SETUP_CLIENT_ONLY"), "1", "TRIGGER PRESENT: the build deployed nothing")
        self.assertEqual(self.open_world().stage_record()["commit"], recorded, "TRIGGER PRESENT: the record still names the earlier commit")
        self.assertIn(f"Nothing was deployed: {self.THIS_TREE}, and only the step client failed.", said)
        self.assertIn("You run ./setup.sh --update to finish; the retry deploys nothing.", said)
        self.assertNotIn(f"The stage runs commit {target}", said, "the stage records another commit")
        self.assertNotIn("its deploy finished", said, "the build deployed nothing")
        self.assertNotIn(self.THIS_COMMIT, said)


class StageRunsWordsCase(unittest.TestCase):
    """`rn_stage_runs_words` of scripts/setup/runner.sh by itself, with the commit of the stage's record given by hand: the rule of the lines above for every way a
    run can come to them, the run that attaches to or reads the build of another run included."""

    TARGET = "a" * 40
    OTHER = "b" * 40

    def words(self, recorded: str, client_only: str, mode: str) -> str:
        text = (REPO / "scripts" / "setup" / "runner.sh").read_text(encoding="utf-8")
        start = text.index("\nrn_stage_runs_words() {\n")
        body = text[start:text.index("\n}\n", start) + 3]
        with tempfile.TemporaryDirectory(prefix="b843-words-") as tmp:
            script = Path(tmp) / "words.sh"
            # the record reader of preflight.sh, with the one value this function asks for
            script.write_text('pf_rec_get() { printf "%s" "$REC_COMMIT"; }\n' + body + "\nrn_stage_runs_words\n", encoding="utf-8")
            env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "REC_COMMIT": recorded, "SRC_COMMIT": self.TARGET, "SETUP_CLIENT_ONLY": client_only, "RN_MODE": mode}
            p = subprocess.run(["bash", "-u", str(script)], capture_output=True, text=True, env=env, timeout=limit(60))  # nosec B603: argument list, no shell string; B607: bash of the host
        self.assertEqual(p.returncode, 0, p.stderr)
        return p.stdout

    def test_b843_the_words_follow_the_record_and_what_this_run_saw(self) -> None:
        commit, tree = "the stage already runs this commit", "the stage already runs this source tree"
        rows = (
            # the commit of the record, SETUP_CLIENT_ONLY, RN_MODE, the words
            ("", "0", "new", commit),               # a first setup: the stage has no record
            (self.TARGET, "1", "new", commit),      # the record names this commit
            (self.TARGET, "0", "finish", commit),
            (self.OTHER, "1", "new", tree),         # the update deploys nothing, and the record names another commit
            (self.OTHER, "1", "stale", tree),
            (self.OTHER, "0", "new", commit),       # the build of this run deployed this commit and wrote it into the record
            (self.OTHER, "0", "stale", commit),
            (self.OTHER, "0", "attach", tree),      # the build of another run: this run did not see whether it deployed
            (self.OTHER, "0", "finish", tree),
        )
        for recorded, client_only, mode, want in rows:
            with self.subTest(recorded=recorded[:4] or "none", client_only=client_only, mode=mode):
                self.assertEqual(self.words(recorded, client_only, mode), want)


class LocalRouteCase(integration.IntBase):
    """`--build-on local`: the same two lines."""

    same_tree_stage = rl.LocalRouteCase.same_tree_stage
    deploy_calls = rl.LocalRouteCase.deploy_calls

    def test_b843_the_local_plan_of_an_update_that_deploys_names_the_label(self) -> None:
        self.same_tree_stage("9.98")
        self.write_commit({"infra/change.yaml": "a: 1\n"})
        p = self.run_local("--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = " ".join(self.out(p).split())
        self.assertIn(f"label: the backend reports v9.98 and will report v{RELEASE} after the deploy", plan)

    def test_b843_the_local_plan_of_a_forced_deploy_names_the_label_it_lowers(self) -> None:
        self.same_tree_stage(RELEASE)
        w = self.open_world()
        w.put_param(rl.LABEL, f"10.00 {rl.source_tree_of(self.repo)}", dict(fake.PROJECT_TAGS))
        w.save()
        p = self.run_local("--dry-run", "--force-deploy")
        self.assertEqual(p.returncode, 0, self.out(p))
        plan = " ".join(self.out(p).split())
        self.assertIn(f"label: the backend reports v10.00 and will report v{RELEASE} after the deploy ({LOWER})", plan)

    def test_b843_the_local_label_only_update_names_the_recorded_commit(self) -> None:
        self.same_tree_stage("9.98")
        # the rig records the commit of the deploy and then adds a commit that changes no deployed file
        recorded = self.git("rev-parse", "HEAD~1")
        p = self.run_local()
        self.assertEqual(p.returncode, 0, self.out(p))
        said = " ".join(self.out(p).split())
        self.assertIn(f"the stage already runs this source tree (its record names commit {recorded[:12]}, which has the same source tree as this commit, "
                      "the same account file and the same settings)", said)
        self.assertNotIn("its record names this commit", said)


if __name__ == "__main__":
    unittest.main()
