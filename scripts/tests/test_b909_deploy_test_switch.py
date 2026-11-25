"""The half of the deploy engine, `scripts/lib/deploy.py`, of the test switch of the storage notice, `HAMH_TEST_STORAGE_REFUSAL`.

The switch is a row of the settings registry for a setting the stage does not keep. What these cases hold, on the fakes of `test_deploy.py`
through `SettingsBase` and `StageShapeBase` of `test_setup_deploy_settings.py`:
- The kind rule. `on` is taken only when the deploy states the kind `test` for the stage: the exported `HAMH_STAGE_KIND`, else the kind the
  stage's settings record keeps. The role of the account entry, the live read of a stage without a record and the word `default` state no
  kind, and `on` is then refused with `DEPLOY-REFUSED reason=test-switch` before the guard and before anything is built. `off`, `default`
  and no word are never refused by it.
- The guard. A word other than a live worker runtime's makes a new runtime version: the deploy says so in one line for each such runtime
  and asks its guard before the Lambda build. The same word again changes nothing and asks no guard.
- A move to the kind `official` always ends with the switch off, and no deploy records the kind `official` while it asked the CDK app for
  the word `on`.
- The row is not kept: the next deploy without the word takes the switch off, also after a deploy that did not finish.

These cases prove the engine and its fakes: what the engine asks the CDK app for, and what it reads back from the fake runtimes. They do
not prove that a real runtime gets or loses the variable: the synthesis test of the CDK app (`infra/test/b909-test-switch-kind.test.ts`) and
a read of the live runtime do. The case of the earlier spelling of the variable stands in `test_setup_deploy_settings.py`.
"""

from __future__ import annotations

import json
import unittest

import test_setup_deploy_settings as ds

WORD = "HAMH_TEST_STORAGE_REFUSAL"
VAR = "AGENTD_TEST_STORAGE_REFUSAL"
KIND = "HAMH_STAGE_KIND"
TOKEN = "b" * 32
NO_CHANGE = {"old": None, "new": None, "class": None}
TURNS_ON = {"value": "on", "source": "explicit", "old": "off", "new": "on", "class": "free"}


class SwitchMixin:
    """Helpers of the cases, and one rule held over every deploy a case makes (`deploy`)."""

    def setUp(self) -> None:
        super().setUp()
        self.seen: list = []

    def deploy(self, *args: str, **env: str):
        mark = self.mark()
        proc = super().deploy(*args, **env)
        asked_on = any(c.get(WORD) == "on" for c in self.cdk_calls(mark))
        kind = (self.record() or {}).get(f"set.{KIND}")
        self.assertFalse(asked_on and kind == "official", "a stage is never recorded as official by a deploy that asked the CDK app for the word on")
        self.seen.append((kind, asked_on))
        return proc

    def runtime_env(self) -> dict:
        """The environment of each fake runtime, by the name of its file."""
        return {p.name: json.loads(p.read_text())["environmentVariables"] for p in sorted(self.state.glob("runtime-*"))}

    def carried(self) -> list:
        """The value of the variable on each fake runtime; None for a runtime without it."""
        return [env.get(VAR) for env in self.runtime_env().values()]

    def line(self, proc) -> dict:
        return self.settings(proc)[WORD]

    def switch_lines(self, proc) -> list:
        return [ln for ln in proc.stdout.splitlines() if ln.startswith("deploy: test switch ")]

    def guard_looks(self) -> int:
        return len([c for c in self.log("aws") if c["args"][:2] == ["stepfunctions", "list-executions"]])

    def built(self) -> tuple:
        """What a deploy that goes on leaves: the cargo calls and the stack deploys so far."""
        return len(self.log("cargo")), self.stack_deploys()

    def switch_record(self) -> dict:
        found = sorted(self.records.glob("*-deploy-direct-*.json"), key=lambda p: p.stat().st_mtime_ns)
        self.assertTrue(found)
        return json.loads(found[-1].read_text())["inputs"]["testStorageRefusal"]

    def refused_switch(self, proc, kind: str) -> str:
        """The refusal of the kind rule, with the kind the deploy names."""
        reason, wait, text = self.refused(proc)
        self.assertEqual((reason, wait), ("test-switch", "no"), text)
        self.assertEqual(text, f"{WORD}=on is for a test stage, and stage direct is not one (its kind: {kind}). Unset {WORD}, then deploy again.")
        return text


class B909EngineCase(SwitchMixin, ds.SettingsBase):
    def a_test_stage(self) -> None:
        """A stage whose settings record keeps the kind test: the role of the fixture's entry gave the word at the first deploy, and every deploy records it."""
        self.ok()
        self.assertEqual(self.record()[f"set.{KIND}"], "test", "TRIGGER PRESENT: the record keeps the kind test")
        self.assertEqual(self.carried(), [None], "TRIGGER PRESENT: one worker runtime, without the variable")

    def hold(self, extra: str = "") -> str:
        """The caller's lock of a deploy, as setup writes it, with `extra` lines after it."""
        text = self.seed_lock(TOKEN) + extra
        d = self.ssm()
        d[ds.LOCK] = text
        (self.state / "ssm.json").write_text(json.dumps(d))
        return text

    # ---- E1 ----
    def test_b909_e1_on_at_a_test_stage_reaches_every_cdk_call_and_the_worker_runtime(self) -> None:
        self.a_test_stage()
        mark, looks = self.mark(), self.guard_looks()
        proc = self.ok(**{WORD: " On "})
        self.assertEqual(self.line(proc), TURNS_ON)
        self.assertIn(f"DEPLOY-SETTING {WORD}=on source=explicit change=off -> on (free)", proc.stdout.splitlines())
        calls = self.cdk_calls(mark)
        self.assertTrue(calls and all(c.get(WORD) == "on" for c in calls), calls)
        self.assertEqual(self.carried(), ["1"], "the fake runtime carries the variable")
        self.assertGreater(self.guard_looks(), looks, "the guard is asked: the word differs from the live runtime")
        self.assertEqual(self.switch_lines(proc), [f"deploy: test switch on ({WORD}=on); the live WorkerPython runtime has no {VAR}, so this deploy changes its "
                                                   "environment and makes a new runtime version"])
        record = self.record()
        self.assertNotIn(f"set.{WORD}", record, "a setting the stage does not keep gets no set. line")
        self.assertIn(WORD, record["explicit"].split(","), "the explicit= line names what this deploy got from its environment")
        self.assertEqual(self.switch_record(), {"setting": "on", "word": "on", "stageKind": "test", "live": {"WorkerPython": None}, "differs": ["WorkerPython"],
                                                "change": True})

    # ---- E2 ----
    def test_b909_e2_a_change_of_the_switch_is_refused_while_runs_are_active(self) -> None:
        self.a_test_stage()
        self.running("01K5RUNA")
        before = self.built()
        reason, wait, text = self.refused(self.deploy(**{WORD: "on"}))
        self.assertEqual((reason, wait), ("guard", "yes"), text)
        self.assertIn("1 running run(s)", text)
        self.assertEqual(self.built(), before, "nothing is built and no stack is deployed")
        self.assertEqual(self.carried(), [None])
        # a stage that carries the switch: off, default and no word change the runtime too
        self.running()
        self.ok(**{WORD: "on"})
        self.assertEqual(self.carried(), ["1"], "TRIGGER PRESENT: the stage carries the switch")
        self.running("01K5RUNA")
        for env in ({WORD: "off"}, {WORD: "default"}, {}):
            with self.subTest(env=env):
                before = self.built()
                reason, wait, text = self.refused(self.deploy(**env))
                self.assertEqual((reason, wait), ("guard", "yes"), text)
                self.assertEqual(self.built(), before)
                self.assertEqual(self.carried(), ["1"])
        # under a lock that lets the stage start runs, a look that finds no run decides nothing
        self.running()
        self.hold("\nstarts=yes")
        before = self.built()
        reason, wait, text = self.refused(self.deploy(HAMH_DEPLOY_LOCK_TOKEN=TOKEN))
        self.assertEqual((reason, wait), ("new-runs", "no"), text)
        self.assertEqual(self.built(), before)
        self.assertEqual(self.carried(), ["1"])

    # ---- E3 ----
    def test_b909_e3_on_given_again_changes_nothing_and_asks_no_guard(self) -> None:
        self.a_test_stage()
        self.ok(**{WORD: "on"})
        self.running("01K5RUNA")
        looks = self.guard_looks()
        proc = self.ok(**{WORD: "on"})
        self.assertEqual(self.line(proc), {"value": "on", "source": "explicit", **NO_CHANGE})
        self.assertEqual(self.guard_looks(), looks, "the guard is not asked: the deploy passes with a running run")
        self.assertEqual(self.carried(), ["1"], "the runtime keeps the variable")
        self.assertEqual(self.switch_lines(proc), [f"deploy: test switch on ({WORD}=on); every live worker runtime has {VAR}=1, so this deploy does not change it"])
        self.assertEqual(self.switch_record(), {"setting": "on", "word": "on", "stageKind": "test", "live": {"WorkerPython": "1"}, "differs": [], "change": False})

    # ---- E4 ----
    def test_b909_e4_a_move_to_the_kind_official_always_ends_with_the_switch_off(self) -> None:
        self.a_test_stage()
        self.ok(**{WORD: "on"})
        # with the word: refused, whatever the kind word says but test
        for kind_word, named in (("official", "official"), ("default", "not stated")):
            with self.subTest(kind=kind_word):
                before = self.built()
                self.refused_switch(self.deploy(**{WORD: "on", KIND: kind_word}), named)
                self.assertEqual(self.built(), before, "nothing is built and no stack is called")
                self.assertEqual(self.record()[f"set.{KIND}"], "test", "the record still says test")
                self.assertEqual(self.carried(), ["1"])
        # without the word, while a run is active: the guard refuses, and the stage stays a test stage with the switch
        self.running("01K5RUNA")
        reason, wait, text = self.refused(self.deploy(**{KIND: "official"}))
        self.assertEqual((reason, wait), ("guard", "yes"), text)
        self.assertEqual(self.record()[f"set.{KIND}"], "test")
        self.assertEqual(self.carried(), ["1"])
        # without the word and without a run: the deploy asks for no variable, and only then records official
        self.running()
        mark = self.mark()
        proc = self.ok(**{KIND: "official"})
        self.assertEqual(self.line(proc), {"value": "default", "source": "default", "old": "on", "new": "off", "class": "free"})
        calls = self.cdk_calls(mark)
        self.assertTrue(calls and all(WORD not in c for c in calls), "no cdk call gets the word")
        self.assertEqual(self.record()[f"set.{KIND}"], "official")
        self.assertEqual(self.carried(), [None], "the fake runtime has no variable")
        # the rule of `deploy` above was held over deploys of both classes
        self.assertIn(("test", True), self.seen)
        self.assertEqual(self.seen[-1], ("official", False))

    def test_b909_e4_a_move_to_official_that_does_not_finish_is_ended_by_the_next_deploy(self) -> None:
        """A move to the kind official that fails after its first stack change leaves a record that says official (status deploying) beside a live runtime
        that may still carry the variable. No cdk call of that deploy got the word, the stage refuses `on` from then on, and the next deploy takes the variable
        out of the runtime."""
        self.a_test_stage()
        self.ok(**{WORD: "on"})
        mark = self.mark()
        failed = self.deploy(FAKE_FAIL_STACK="hamh-direct-control", **{KIND: "official"})
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        record = self.record()
        self.assertEqual((record["status"], record[f"set.{KIND}"]), ("deploying", "official"), "TRIGGER PRESENT: the move did not finish, and the record names the new kind")
        calls = self.cdk_calls(mark)
        self.assertTrue(calls and all(WORD not in c for c in calls), "the deploy that did not finish asked for no variable")
        # the stage refuses the word now: the record's kind is official
        before = self.built()
        self.refused_switch(self.deploy(**{WORD: "on"}), "official")
        self.assertEqual(self.built(), before)
        # the next deploy ends the move, and the runtime has no variable
        proc = self.ok()
        self.assertEqual((self.line(proc)["value"], self.line(proc)["source"]), ("default", "default"))
        self.assertEqual((self.record()["status"], self.record()[f"set.{KIND}"]), ("deployed", "official"))
        self.assertEqual(self.carried(), [None])

    # ---- E5 ----
    def test_b909_e5_with_the_instances_part_both_worker_runtimes_get_and_lose_the_variable(self) -> None:
        self.ok(HAMH_COMPUTE_INSTANCES="on")
        self.assertEqual(self.carried(), [None, None], "TRIGGER PRESENT: two worker runtimes, without the variable")
        on = self.ok(**{WORD: "on"})
        self.assertEqual(self.carried(), ["1", "1"])
        state = self.switch_record()
        self.assertEqual((state["live"], state["differs"], state["change"]),
                         ({"WorkerPython": None, "WorkerPythonInstances": None}, ["WorkerPython", "WorkerPythonInstances"], True))
        self.assertEqual(len(self.switch_lines(on)), 2, "one line for each runtime that differs")
        off = self.ok(**{WORD: "off"})
        self.assertEqual(self.line(off), {"value": "off", "source": "explicit", "old": "on", "new": "off", "class": "free"})
        self.assertEqual(self.carried(), [None, None])
        state = self.switch_record()
        self.assertEqual((state["word"], state["live"], state["differs"]),
                         ("off", {"WorkerPython": "1", "WorkerPythonInstances": "1"}, ["WorkerPython", "WorkerPythonInstances"]))
        self.assertEqual(len(self.switch_lines(off)), 2)

    # ---- E6 ----
    def test_b909_e6_only_a_stated_kind_test_takes_on(self) -> None:
        # a new stage whose entry's role gives the kind test: the role states no kind, so on is refused before anything is built
        before = self.built()
        proc = self.deploy(**{WORD: "on"})
        self.refused_switch(proc, "not stated")
        self.assertEqual(self.settings(proc)[KIND], {"value": "default", "source": "default", **NO_CHANGE}, "TRIGGER PRESENT: no kind word, no record")
        self.assertEqual(self.built(), before, "no cargo lambda build and no stack call")
        self.assertIsNone(self.record())
        # the same new stage with the kind word: taken
        self.ok(**{WORD: "on", KIND: "test"})
        self.assertEqual(self.carried(), ["1"])
        # the record keeps the kind now, so the word alone is taken (every deploy records the kind)
        self.assertEqual(self.record()[f"set.{KIND}"], "test")
        self.assertEqual(self.line(self.ok(**{WORD: "on"})), {"value": "on", "source": "explicit", **NO_CHANGE})
        # the same stage without its record: the role's word is the kind again, and it states none
        self.drop_record()
        before = self.built()
        proc = self.deploy(**{WORD: "on"})
        self.refused_switch(proc, "not stated")
        self.assertEqual(self.settings(proc)[KIND]["source"], "default", "TRIGGER PRESENT: no record, and the live kind is the role's word")
        self.assertEqual(self.built(), before)
        self.ok(**{WORD: "on", KIND: "test"})
        self.ok(**{KIND: "official"})
        self.assertEqual(self.carried(), [None])
        for word in ("off", "default", "OFF"):
            with self.subTest(word=word):
                proc = self.ok(**{WORD: word})
                self.assertEqual(self.line(proc), {"value": word.lower(), "source": "explicit", **NO_CHANGE})
                self.assertEqual(self.carried(), [None])
        before = self.built()
        self.refused_switch(self.deploy(**{WORD: "on"}), "official")
        self.assertEqual(self.built(), before)
        bad = self.deploy(**{WORD: "1"})
        self.assertEqual(bad.returncode, 2, bad.stdout + bad.stderr)
        self.assertIn("The test switch of the storage notice on a test stage", bad.stdout + bad.stderr)
        self.assertEqual(self.built(), before)

    # ---- E8 ----
    def test_b909_e8_the_next_deploy_without_the_word_takes_the_switch_off(self) -> None:
        self.a_test_stage()
        self.ok(**{WORD: "on"})
        mark, looks = self.mark(), self.guard_looks()
        proc = self.ok()
        self.assertIn(f"DEPLOY-SETTING {WORD}=default source=default change=on -> off (free)", proc.stdout.splitlines())
        self.assertGreater(self.guard_looks(), looks, "the guard is asked")
        self.assertTrue(all(WORD not in c for c in self.cdk_calls(mark)))
        self.assertEqual(self.carried(), [None], "the runtime has no variable")
        self.assertEqual(self.switch_lines(proc), [f"deploy: test switch off ({WORD} not set, the default); the live WorkerPython runtime has {VAR}=1, so this deploy changes "
                                                   "its environment and makes a new runtime version"])
        # an ordinary deploy of a stage without the switch prints the row's line and no other line about it
        again = self.ok()
        self.assertEqual(self.line(again), {"value": "default", "source": "default", **NO_CHANGE})
        self.assertIn(f"DEPLOY-SETTING {WORD}=default source=default", again.stdout.splitlines())
        self.assertNotIn("test switch", again.stdout)
        self.assertEqual(self.switch_record(), {"setting": None, "word": "off", "stageKind": "test", "live": {"WorkerPython": None}, "differs": [], "change": False})

    # ---- E9 ----
    def test_b909_e9_print_settings_refuses_as_the_deploy_does_and_writes_nothing(self) -> None:
        self.ok(**{KIND: "official"})
        before = (self.ssm(), self.built(), self.carried(), sorted(p.name for p in self.records.glob("*.json")))
        self.refused_switch(self.deploy("--print-settings", **{WORD: "on"}), "official")
        proc = self.deploy("--print-settings", **{WORD: "on", KIND: "test"})
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(self.line(proc), TURNS_ON)
        self.assertEqual((self.ssm(), self.built(), self.carried(), sorted(p.name for p in self.records.glob("*.json"))), before, "the mode writes nothing")

    # ---- E10 ----
    def test_b909_e10_a_deploy_with_on_that_did_not_finish_leaves_no_intent_behind(self) -> None:
        self.a_test_stage()
        failed = self.deploy(FAKE_FAIL_STACK="hamh-direct-control", **{WORD: "on"})
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        record = self.record()
        self.assertEqual(record["status"], "deploying", "TRIGGER PRESENT: the deploy did not finish")
        self.assertIn(WORD, record["explicit"].split(","), "TRIGGER PRESENT: the record names the word among what that deploy was given")
        mark = self.mark()
        proc = self.ok()
        self.assertEqual((self.line(proc)["value"], self.line(proc)["source"]), ("default", "default"), "the word is not kept from the deploy that did not finish")
        self.assertTrue(all(WORD not in c for c in self.cdk_calls(mark)))
        self.assertEqual(self.carried(), [None], "the switch is off")

    # ---- the letter case of the word ----
    def test_b909_the_word_on_in_upper_and_mixed_case_is_refused_on_an_official_stage(self) -> None:
        """The engine judges the word it resolved, in lower case, never the raw text of its environment: `ON`, `On` and `oN` are refused on an official
        stage as `on` is, before anything is built."""
        self.a_test_stage()
        self.ok(**{KIND: "official"})
        self.assertEqual(self.record()[f"set.{KIND}"], "official", "TRIGGER PRESENT: the record keeps the kind official")
        for word in ("ON", "On", "oN"):
            with self.subTest(word=word):
                before = self.built()
                self.refused_switch(self.deploy(**{WORD: word}), "official")
                self.assertEqual(self.built(), before, "nothing is built and no stack is called")
                self.assertEqual(self.carried(), [None], "no worker runtime carries the variable")


class B909SetupMadeStageCase(SwitchMixin, ds.StageShapeBase):
    """The account file setup generates has one entry with the role active. The kind of its stage is what decides, never that role."""

    def test_b909_e6_a_stage_of_an_active_entry_takes_on_when_its_kind_is_test(self) -> None:
        # a new stage of the generated entry (role active): no kind word, so on is refused
        proc = self.deploy(**{WORD: "on"})
        self.refused_switch(proc, "not stated")
        self.assertIsNone(self.record())
        # the same entry with the kind word test: a test stage that setup made, and it takes the word
        self.ok(**{KIND: "test"})
        self.assertEqual(self.record()[f"set.{KIND}"], "test")
        self.assertEqual(self.carried(), [None])
        on = self.ok(**{WORD: "on"})
        self.assertEqual(self.line(on), TURNS_ON)
        self.assertEqual(self.carried(), ["1"])
        self.assertEqual(self.switch_record()["stageKind"], "test")
        self.ok(**{WORD: "off"})
        self.assertEqual(self.carried(), [None])
        # the same stage as official: refused
        self.ok(**{KIND: "official"})
        self.refused_switch(self.deploy(**{WORD: "on"}), "official")

    def test_b909_e6_the_live_read_of_a_stage_without_a_record_states_no_kind(self) -> None:
        self.ok(as_test=True)
        self.drop_record()
        # the generated entry (role active) meets a stage without a record whose live kind is test
        before = self.built()
        proc = self.deploy(**{WORD: "on"})
        self.refused_switch(proc, "not stated")
        self.assertEqual((self.settings(proc)[KIND]["value"], self.settings(proc)[KIND]["source"]), ("test", "live"), "TRIGGER PRESENT: the kind is read from the live stage")
        self.assertEqual(self.built(), before)
        # with the kind word exported the same stage takes the word
        self.assertEqual(self.line(self.ok(**{WORD: "on", KIND: "test"})), TURNS_ON)
        self.assertEqual(self.carried(), ["1"])

    def test_b909_e6_the_role_test_states_no_kind(self) -> None:
        # a new stage of the fixture's entry with the role test: the role gives the kind test and states none
        proc = self.deploy(as_test=True, **{WORD: "on"})
        self.refused_switch(proc, "not stated")
        self.assertEqual(self.settings(proc)[KIND]["source"], "default", "TRIGGER PRESENT: the kind comes from the role alone")
        # after its first deploy the record keeps the kind test (every deploy records the kind), and the word is taken
        self.ok(as_test=True)
        self.assertEqual(self.record()[f"set.{KIND}"], "test")
        self.assertEqual(self.line(self.ok(as_test=True, **{WORD: "on"})), TURNS_ON)
        self.assertEqual(self.carried(), ["1"])


if __name__ == "__main__":
    unittest.main()
