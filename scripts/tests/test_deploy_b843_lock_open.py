"""A deploy that runs under the caller's lock opens the stage as soon as it has finished.

The stage's API service starts no new runs while the deploy lock exists without the line `deploys=no`. Setup's runner build deploys under the lock setup holds
(HAMH_DEPLOY_LOCK_TOKEN) and then makes the client, and only setup wrote the line, when CodeBuild said that the whole build had succeeded: the stage stayed closed
for the minutes the client took to build. The deploy now writes the line itself, right after the settings record says `deployed`:

- order      the write comes after the record and is the last write of the lock; the lock stays (the caller releases it)
- text       every line of the caller's lock as it was read, in its order, without the `renewed=` line, and `deploys=no` last
- renewals   they stop first; a renewal and the open take turns, and a renewal that is under way for too long keeps the open from writing
- race       a write of the caller made from a read before the open replaces the line (SSM has no conditional write): the read-back writes it once more
- ownership  a lock that another deploy took, or that is gone, is left alone
- failure    a write that fails is said and the deploy still passes; a deploy that fails never opens the lock; a deploy by hand deletes its own lock

The deploys run the real scripts/deploy.sh against the fakes of test_deploy.py through the wrappers of test_setup_deploy_settings.py.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import subprocess  # nosec B404: tests start the script under test with argument lists, never a shell string
import sys
import threading
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import setup_fake_aws as setup_fake  # noqa: E402
import test_deploy as td  # noqa: E402
import test_setup_deploy_settings as ds  # noqa: E402

LOCK = "/hamh/direct/deploy-lock"
SETTINGS = "/hamh/direct/settings"
TOKEN = "b" * 32
OPEN = "deploys=no"

# The AWS CLI stand-in of these cases: the wrapper of test_setup_deploy_settings.py with switches for the write that opens the lock (a put of the lock whose
# value has the line deploys=no). FAKE_OPEN_FAILS=1 refuses it. FAKE_STALE_AFTER_OPEN=<text> lets the first one through and then puts that text in the lock's place:
# a renewal of the caller that read the lock before the open and wrote after it. FAKE_READ_BACK_FAILS=1 refuses every read of the lock after the first such write
# went through, and FAKE_REPAIR_FAILS=1 refuses the write that puts the line back after the caller's write replaced it.
OPEN_WRAPPER = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
st = os.environ["FAKE_STATE"]
def arg(name):
    return args[args.index(name) + 1] if name in args else None
opens = args[:2] == ["ssm", "put-parameter"] and arg("--name") == "/hamh/direct/deploy-lock" and "deploys=no" in (arg("--value") or "").splitlines()
reads = args[:2] == ["ssm", "get-parameter"] and arg("--name") == "/hamh/direct/deploy-lock"
if opens and os.environ.get("FAKE_OPEN_FAILS") == "1":
    sys.stderr.write("An error occurred (ThrottlingException) when calling the PutParameter operation: Rate exceeded\n"); sys.exit(254)
marker = os.path.join(st, "stale-write-done")
opened = os.path.join(st, "open-write-done")
if reads and os.environ.get("FAKE_READ_BACK_FAILS") == "1" and os.path.exists(opened):
    sys.stderr.write("An error occurred (ThrottlingException) when calling the GetParameter operation: Rate exceeded\n"); sys.exit(254)
if opens and os.environ.get("FAKE_REPAIR_FAILS") == "1" and os.path.exists(marker):
    sys.stderr.write("An error occurred (ThrottlingException) when calling the PutParameter operation: Rate exceeded\n"); sys.exit(254)
if opens and os.environ.get("FAKE_READ_BACK_FAILS") == "1":
    rc = subprocess.call([sys.executable, os.environ["FAKE_WRAPPED_AWS"], *args])
    if rc == 0:
        open(opened, "w").write("1")
    sys.exit(rc)
if opens and os.environ.get("FAKE_STALE_AFTER_OPEN") and not os.path.exists(marker):
    rc = subprocess.call([sys.executable, os.environ["FAKE_WRAPPED_AWS"], *args])
    p = os.path.join(st, "ssm.json")
    d = json.load(open(p))
    d["/hamh/direct/deploy-lock"] = os.environ["FAKE_STALE_AFTER_OPEN"]
    with open(p, "w") as f:
        json.dump(d, f)
    open(marker, "w").write("1")
    sys.exit(rc)
os.execv(sys.executable, [sys.executable, os.environ["FAKE_WRAPPED_AWS"], *args])
'''


class OpenBase(ds.SettingsBase):
    """The deploy rig of the settings cases with the switches of the open, and helpers to read the lock."""

    def setUp(self) -> None:
        super().setUp()
        self.wrapped = self.bins["aws"]
        p = self.tmp / "aws-open-wrapper"
        p.write_text(OPEN_WRAPPER.replace("/usr/bin/env python3", sys.executable, 1))
        p.chmod(0o755)
        self.bins["aws"] = str(p)

    def run_deploy(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        env.setdefault("DEPLOY_LOCK_SETTLE_S", "0")
        return super().run_deploy(*args, FAKE_WRAPPED_AWS=self.wrapped, **env)

    def lock_puts(self, since: int = 0) -> list:
        return self.puts(LOCK, since)

    def put_lines(self, put: list) -> list:
        return put[put.index("--value") + 1].splitlines()

    def lock_now(self):
        return self.ssm().get(LOCK)

    def set_lock(self, value) -> None:
        store = self.ssm()
        store.pop(LOCK, None)
        if value is not None:
            store[LOCK] = value
        (self.state / "ssm.json").write_text(json.dumps(store))

    def said(self, proc: subprocess.CompletedProcess, words: str) -> list:
        return [ln for ln in proc.stdout.splitlines() if ln.startswith("deploy: ") and LOCK in ln and words in ln]

    def caller_deployer(self):
        """A Deployer of this process that runs under the caller's lock, with its keeper driven by hand and not from a thread."""
        with mock.patch.dict(os.environ, {"FAKE_WRAPPED_AWS": self.wrapped, "DEPLOY_LOCK_SETTLE_S": "0"}):
            dp, d = self.deployer()
        with mock.patch.dict(os.environ, {"HAMH_DEPLOY_LOCK_TOKEN": TOKEN}), mock.patch.object(dp.Deployer, "start_lock_keeper"), \
                contextlib.redirect_stdout(io.StringIO()):
            d.take_lock()
        return dp, d


class DeployOpensTheCallersLockCase(OpenBase):
    def test_b843_a_deploy_under_the_callers_lock_opens_it_right_after_its_record_says_deployed(self) -> None:
        text = self.seed_lock(TOKEN)
        proc = self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN)
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")
        puts = self.lock_puts()
        self.assertEqual(len(puts), 1, "one write of the lock: the one that opens it")
        self.assertIn("--overwrite", puts[0], "it writes over the caller's lock and never creates one")
        self.assertEqual(self.put_lines(puts[0]), text.splitlines() + [OPEN], "every line of the caller's lock in its order, and deploys=no last")
        self.assertEqual(self.lock_now(), text + "\n" + OPEN, "the lock stays, and it no longer closes the stage")
        calls = self.aws_calls()
        deployed = next(i for i, a in enumerate(calls) if a[:2] == ["ssm", "put-parameter"] and SETTINGS in a and "status=deployed" in " ".join(a))
        opened = next(i for i, a in enumerate(calls) if a[:2] == ["ssm", "put-parameter"] and LOCK in a)
        self.assertGreater(opened, deployed, "the lock is opened after the record says deployed")
        self.assertEqual([a for a in calls[opened + 1:] if a[:2] == ["ssm", "put-parameter"]], [], "nothing of the stage is written after the open")
        self.assertEqual([a for a in calls if a[:2] in (["ssm", "delete-parameter"], ["ssm", "add-tags-to-resource"]) and LOCK in a], [],
                         "the lock stays the caller's: the deploy neither deletes nor tags it")
        self.assertEqual(len(self.said(proc, "the stage starts runs again")), 1, proc.stdout)

    def test_b843_the_open_drops_the_renewal_line_and_no_renewal_follows_it(self) -> None:
        text = self.seed_lock(TOKEN)
        proc = self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, DEPLOY_LOCK_RENEW_S="0.2", FAKE_NODE_SLEEP_S="0.3")
        puts = self.lock_puts()
        self.assertGreaterEqual(len(puts), 3, "TRIGGER PRESENT: the keeper renewed the lock while the nine stacks deployed")
        for put in puts[:-1]:
            lines = self.put_lines(put)
            self.assertEqual(lines[:-1], text.splitlines())
            self.assertTrue(lines[-1].startswith("renewed="), lines[-1])
        self.assertEqual(self.put_lines(puts[-1]), text.splitlines() + [OPEN], "the last write is the open: no renewed= line, deploys=no last")
        self.assertEqual(self.lock_now(), text + "\n" + OPEN)
        self.assertEqual(self.said(proc, "stops renewing"), [], "the keeper stopped without a word")
        self.assertEqual(self.record()["status"], "deployed")

    def test_b843_the_stand_in_build_of_the_setup_cases_writes_the_lock_as_the_deploy_does(self) -> None:
        # test_setup_b843_stage_opens.py holds setup's reading of the opened lock against a stand-in build, which writes the line in the deploy's place with
        # opened_lock_text of setup_fake_aws.py. This case holds that function to the bytes of the real deploy, for a lock that was renewed before the open.
        self.seed_lock(TOKEN)
        self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, DEPLOY_LOCK_RENEW_S="0.2", FAKE_NODE_SLEEP_S="0.3")
        puts = self.lock_puts()
        self.assertGreaterEqual(len(puts), 2, "TRIGGER PRESENT: the keeper renewed the lock before the deploy opened it")
        before = "\n".join(self.put_lines(puts[-2]))
        self.assertIn("renewed=", before, "TRIGGER PRESENT: the lock the open read carried a renewal line")
        self.assertEqual("\n".join(self.put_lines(puts[-1])), setup_fake.opened_lock_text(before), "the write of the real deploy is the write of the stand-in build")
        self.assertEqual(self.lock_now(), setup_fake.opened_lock_text(before))

    def test_b843_a_write_of_the_caller_that_replaces_the_line_is_repaired_once(self) -> None:
        text = self.seed_lock(TOKEN)
        # setup renewed its lock from a read before the open: the same token, a later start time, no deploys=no
        stale = text.replace("started=2026-11-10T16:00:00Z", "started=2026-11-10T16:10:00Z")
        self.assertNotEqual(stale, text)
        proc = self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, FAKE_STALE_AFTER_OPEN=stale)
        self.assertTrue((self.state / "stale-write-done").is_file(), "TRIGGER PRESENT: the caller's write replaced the lock after the open")
        puts = self.lock_puts()
        self.assertEqual(len(puts), 2, "the open and one repair")
        self.assertEqual(self.put_lines(puts[1]), stale.splitlines() + [OPEN], "the repair keeps what the caller wrote and adds the line again")
        self.assertEqual(self.lock_now(), stale + "\n" + OPEN)
        self.assertEqual(len(self.said(proc, "it is written again")), 1, proc.stdout)
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")

    def test_b843_a_lock_that_another_deploy_took_is_left_alone(self) -> None:
        self.seed_lock(TOKEN)
        other = "\n".join(("token=" + "e" * 32, "holder=other-host:77", f"identity=arn:aws:sts::{td.ACCOUNT}:assumed-role/Admin/bob", "route=local",
                           "started=2026-11-10T16:40:00Z", "commit=" + "e" * 40, "inputs=" + "f" * 64, "build="))
        proc = self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, FAKE_LOCK_REPLACED_AT_PUT=SETTINGS, FAKE_LOCK_OTHER=other)
        self.assertEqual(self.lock_now(), other, "the lock of the deploy that replaced it stays as it is")
        self.assertEqual(self.lock_puts(), [])
        said = self.said(proc, "left alone")
        self.assertEqual(len(said), 1, proc.stdout)
        self.assertIn("other-host:77", said[0])
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")

    def test_b843_an_open_that_fails_is_said_and_the_deploy_still_passes(self) -> None:
        text = self.seed_lock(TOKEN)
        proc = self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, FAKE_OPEN_FAILS="1")
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")
        self.assertEqual(self.record()["status"], "deployed")
        self.assertEqual(self.lock_now(), text, "TRIGGER PRESENT: the write that opens the lock was refused, so the lock still closes the stage")
        said = self.said(proc, "could not be written to say that the deploy has ended")
        self.assertEqual(len(said), 1, proc.stdout)
        self.assertIn("The caller writes that when it sees its build end", said[0])

    def test_b843_a_read_back_that_fails_does_not_say_that_the_write_failed(self) -> None:
        text = self.seed_lock(TOKEN)
        proc = self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, FAKE_READ_BACK_FAILS="1")
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")
        self.assertTrue((self.state / "open-write-done").is_file(), "TRIGGER PRESENT: the write that opens the lock went through")
        self.assertEqual(self.lock_now(), text + "\n" + OPEN, "TRIGGER PRESENT: the lock says deploys=no")
        self.assertEqual(len(self.lock_puts()), 1, "one write of the lock: the one that opens it")
        self.assertEqual(len(self.said(proc, "the stage starts runs again")), 1, proc.stdout)
        self.assertEqual(self.said(proc, "could not be written"), [], "the write went through, so no line says that it could not be written")
        said = self.said(proc, "could not be read back")
        self.assertEqual(len(said), 1, proc.stdout)
        self.assertIn("ThrottlingException", said[0])
        self.assertIn(f"the line {OPEN} was written", said[0])
        self.assertIn("The caller writes it again when it sees its build end", said[0])

    def test_b843_a_repair_that_fails_says_that_the_line_was_replaced_and_not_written_again(self) -> None:
        text = self.seed_lock(TOKEN)
        stale = text.replace("started=2026-11-10T16:00:00Z", "started=2026-11-10T16:10:00Z")
        proc = self.ok(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, FAKE_STALE_AFTER_OPEN=stale, FAKE_REPAIR_FAILS="1")
        self.assertEqual(self.last(proc)[:11], "DEPLOY-PASS")
        self.assertTrue((self.state / "stale-write-done").is_file(), "TRIGGER PRESENT: the caller's write replaced the lock after the open")
        self.assertEqual(self.lock_now(), stale, "TRIGGER PRESENT: the write that puts the line back was refused, so the lock closes the stage")
        self.assertEqual(self.said(proc, "could not be written to say that the deploy has ended"), [], "the first write went through")
        said = self.said(proc, "could not be written again")
        self.assertEqual(len(said), 1, proc.stdout)
        self.assertIn(f"had replaced the line {OPEN}", said[0])
        self.assertIn("ThrottlingException", said[0])
        self.assertIn("The caller writes it when it sees its build end", said[0])

    def test_b843_a_deploy_that_fails_leaves_the_callers_lock_closed(self) -> None:
        text = self.seed_lock(TOKEN)
        proc = self.deploy(HAMH_DEPLOY_LOCK_TOKEN=TOKEN, FAKE_FAIL_STACK="hamh-direct-control")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(self.lock_now(), text, "a deploy that did not finish never opens the stage")
        self.assertEqual(self.lock_puts(), [])

    def test_b843_a_deploy_by_hand_deletes_its_own_lock_and_never_writes_the_line(self) -> None:
        self.ok()
        self.assertEqual([p for p in self.lock_puts() if OPEN in self.put_lines(p)], [])
        self.assertIsNone(self.lock_now(), "the lock the deploy took itself is gone at its end")


class OpenByHandCase(OpenBase):
    """The open as a method of a Deployer of this process, with the lock changed under it."""

    def test_b843_a_lock_that_is_gone_is_not_written(self) -> None:
        self.seed_lock(TOKEN)
        dp, d = self.caller_deployer()
        self.set_lock(None)
        calls = len(self.aws_calls())
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            d.open_caller_lock()
        self.assertEqual([a for a in self.aws_calls(calls) if a[:2] in (["ssm", "put-parameter"], ["ssm", "delete-parameter"])], [])
        self.assertIsNone(self.lock_now())
        self.assertIn("is gone", out.getvalue())

    def test_b843_a_lock_that_already_says_the_line_is_not_written_again(self) -> None:
        text = self.seed_lock(TOKEN)
        dp, d = self.caller_deployer()
        self.set_lock(text + "\n" + OPEN)
        calls = len(self.aws_calls())
        with contextlib.redirect_stdout(io.StringIO()):
            d.open_caller_lock()
        self.assertEqual([a for a in self.aws_calls(calls) if a[:2] == ["ssm", "put-parameter"]], [])

    def test_b843_a_renewal_that_is_under_way_keeps_the_open_from_writing(self) -> None:
        text = self.seed_lock(TOKEN)
        dp, d = self.caller_deployer()
        calls = len(self.aws_calls())
        out = io.StringIO()
        held = threading.Event()
        done = threading.Event()

        def renewal() -> None:
            with d.lock_cycle:
                held.set()
                done.wait(30)

        t = threading.Thread(target=renewal, daemon=True)
        t.start()
        self.assertTrue(held.wait(10))
        try:
            with mock.patch.object(dp, "LOCK_JOIN_S", 0.2), contextlib.redirect_stdout(out):
                d.open_caller_lock()
        finally:
            done.set()
            t.join(10)
        self.assertEqual([a for a in self.aws_calls(calls) if a[:2] == ["ssm", "put-parameter"]], [], "nothing is written while a renewal holds its turn")
        self.assertEqual(self.lock_now(), text)
        self.assertIn("a renewal of it is still under way", out.getvalue())
        self.assertTrue(d.lock_stop.is_set(), "the renewals are told to stop all the same")

    def test_b843_after_the_open_the_keeper_writes_nothing(self) -> None:
        text = self.seed_lock(TOKEN)
        dp, d = self.caller_deployer()
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            d.open_caller_lock()
        self.assertEqual(self.lock_now(), text + "\n" + OPEN)
        calls = len(self.aws_calls())
        clock = ds.FakeClock(turns=3)
        d.clock = clock.now
        with contextlib.redirect_stdout(out):
            d.keep_lock(clock.wait)
        self.assertEqual(len(clock.waits), 1, "the keeper ends at its first turn")
        self.assertEqual(self.aws_calls(calls), [], "it neither reads nor writes the lock")
        self.assertNotIn("stops renewing", out.getvalue())

    def test_b843_a_lock_the_deploy_took_itself_is_not_opened(self) -> None:
        with mock.patch.dict(os.environ, {"FAKE_WRAPPED_AWS": self.wrapped}):
            dp, d = self.deployer()
        with mock.patch.object(dp.Deployer, "start_lock_keeper"), contextlib.redirect_stdout(io.StringIO()):
            d.take_lock()
            calls = len(self.aws_calls())
            d.open_caller_lock()
        self.assertEqual(self.aws_calls(calls), [], "release_lock deletes that lock a moment later")
        self.assertNotIn(OPEN, (self.lock_now() or "").splitlines())

    def test_b843_the_pause_before_the_read_back_can_only_be_shortened(self) -> None:
        with mock.patch.dict(os.environ, {"FAKE_WRAPPED_AWS": self.wrapped}):
            dp, _ = self.deployer()
        self.assertEqual(dp.LOCK_SETTLE_S, 5)
        for given, want in (("0", 0.0), ("0.5", 0.5), ("5", 5.0), ("9", 5.0), ("-1", 5.0), ("soon", 5.0), ("", 5.0)):
            with self.subTest(given=given), mock.patch.dict(os.environ, {"DEPLOY_LOCK_SETTLE_S": given}):
                self.assertEqual(dp.Deployer.settle_interval(), want)


if __name__ == "__main__":
    unittest.main()
