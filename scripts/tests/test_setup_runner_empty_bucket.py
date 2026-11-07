"""Tests for scripts/setup/empty-bucket.sh, the step that empties a versioned bucket before `setup.sh --teardown` deletes the stack that owns it.

CloudFormation cannot delete a bucket that holds objects, and these buckets are versioned, so every version and every delete marker has to go. The templates have no
custom resource that would do it; `setup.sh` runs this script with the caller's own credentials. The script accepts the setup bucket (`hamh-setup-<account>-<Region>`)
and the two buckets of the stage (`hamh-direct-harness-<account>` and `hamh-direct-gomod-<account>`, whose Region comes from AWS_REGION because the name has none), each
also under the first word of the previous names (the stand-in's leg() spells them), and nothing else, passes the account of the name as the expected bucket owner on every call, sends the governance bypass for the Go module store alone, lists one page of at most 1,000
entries, deletes exactly that page (at most 1,000 keys per call), lists again after every batch (so it needs no page token), and ends with
`EMPTY-BUCKET PASS bucket=<name> deleted=<n>` or `EMPTY-BUCKET FAIL <detail>`.

The fake aws of these cases does not know the script's query. It keeps a versioned bucket in a JSON file and answers with the rules of S3 and of the AWS CLI that
s3_versions_model.py keeps: a listing page holds at most 1,000 entries, versions and delete markers together; `--max-items` counts the versions only and adds the delete
markers of every page it read; DeleteObjects refuses more than 1,000 keys. The first test class runs the shapes of buckets that an independent review found the earlier
script (`--max-items 1000`) to fail on, and a test below runs that form of the script to show that the model reproduces the failure.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess  # nosec B404: tests start the scripts under test with argument lists, never a shell
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any, Dict, List

import s3_versions_model as model
import setup_runner_support as sup
from setup_fake_aws import leg
from setup_waits import limit

SCRIPT = sup.REPO / "scripts" / "setup" / "empty-bucket.sh"
ACCOUNT = "111122223333"
BUCKET = f"hamh-setup-{ACCOUNT}-us-east-1"
ARN = f"arn:aws:iam::{ACCOUNT}:user/tester"

FAKE_AWS = r'''#!__PY__
import json, os, sys
sys.dont_write_bytecode = True  # a child writes no .pyc beside the modules it imports
sys.path.insert(0, os.environ["FAKE_TESTS_DIR"])
import s3_versions_model as m
import setup_fake_aws as fa

args = sys.argv[1:]
glob = {}
while args and args[0] in ("--profile", "--region"):
    glob[args[0]] = args[1]
    args = args[2:]
path = os.environ["FAKE_BUCKET"]
doc = json.load(open(path))
with open(os.environ["FAKE_LOG"], "a") as f:
    f.write(json.dumps({"args": args, "global": glob, "env_profile": os.environ.get("AWS_PROFILE", "")}) + "\n")


def opt(name):
    return args[args.index(name) + 1] if name in args else None


def fail(text, code=254):
    sys.stderr.write(text + "\n")
    sys.exit(code)


def counter(name):
    """How many times this kind of call was made before (kept in the bucket file)."""
    n = doc.setdefault("seen", {}).get(name, 0)
    doc["seen"][name] = n + 1
    json.dump(doc, open(path, "w"))
    return n


def emit(data):
    q = opt("--query")
    if q:
        data = fa.jmespath(q, data)
    if opt("--output") == "text":
        sys.stdout.write(fa.as_text(data))
    else:
        print(json.dumps(data, indent=4))


if args[:2] == ["sts", "get-caller-identity"]:
    if os.environ.get("FAKE_IDENTITY_ERROR"):
        fail("An error occurred (ExpiredToken) when calling the GetCallerIdentity operation: The security token included in the request is expired")
    emit({"Account": os.environ["FAKE_OWNER"], "Arn": os.environ.get("FAKE_ARN", "__ARN__"), "UserId": "AIDATEST"})
    sys.exit(0)
if "--expected-bucket-owner" not in args or opt("--expected-bucket-owner") != os.environ["FAKE_OWNER"]:
    fail("An error occurred (AccessDenied) when calling the operation: Access Denied")
entries = doc["entries"]
if args[:2] == ["s3api", "list-object-versions"]:
    if os.environ.get("FAKE_THROTTLE_LISTS") and counter("list") < int(os.environ["FAKE_THROTTLE_LISTS"]):
        fail("An error occurred (SlowDown) when calling the ListObjectVersions operation (reached max retries: 2): Please reduce your request rate.")
    if os.environ.get("FAKE_NO_BUCKET"):
        fail("An error occurred (NoSuchBucket) when calling the ListObjectVersions operation: The specified bucket does not exist")
    if os.environ.get("FAKE_JUNK"):
        # answers that have the key of a listing and are none: a null, a list without version ids, entries of another shape
        junk = {"objects-null": {"Objects": None}, "keys-only": {"Objects": [{"Key": "still-present"}]}, "no-array": {"Objects": {"Key": "x", "VersionId": "v1"}}}
        if os.environ["FAKE_JUNK"] in junk:
            print(json.dumps(junk[os.environ["FAKE_JUNK"]], indent=4))
        else:
            print("null")
        sys.exit(0)
    if os.environ.get("FAKE_LIST_DENIED"):
        counter("list-denied")
        if os.environ.get("FAKE_DENIED_TEXT"):
            fail("An error occurred (AccessDenied) when calling the ListObjectVersions operation: User is not authorized to perform s3:ListBucketVersions on resource "
                 "arn:aws:s3:::NoSuchBucket-guard")
        fail("An error occurred (AccessDenied) when calling the ListObjectVersions operation: Access Denied")
    try:
        data = m.cli_list(entries, bucket=args[args.index("--bucket") + 1], no_paginate="--no-paginate" in args,
                          max_keys=int(opt("--max-keys")) if opt("--max-keys") else None, page_size=int(opt("--page-size")) if opt("--page-size") else None,
                          max_items=int(opt("--max-items")) if opt("--max-items") else None, starting_token=opt("--starting-token"))
    except m.UsageError as exc:
        fail("\n" + str(exc) + "\n", 252)
    if os.environ.get("FAKE_OVERSIZE"):
        # a service that answers more than it may: the script must not send these as one delete
        data = dict(data, Versions=data.get("Versions", []) + [dict(data["Versions"][0], VersionId="x%05d" % i) for i in range(1100)]) if data.get("Versions") else data
    emit(data)
elif args[:2] == ["s3api", "delete-objects"]:
    body = json.loads(open(opt("--delete")[len("file://"):]).read())
    with open(os.environ["FAKE_LOG"] + ".sizes", "a") as f:
        f.write("%d\n" % len(body["Objects"]))
    if os.environ.get("FAKE_DELETE_ERRORS"):
        emit({"Errors": [{"Key": body["Objects"][0]["Key"], "VersionId": body["Objects"][0]["VersionId"], "Code": "AccessDenied", "Message": "Access Denied"}]})
        sys.exit(0)
    gone = set()

    def protected(key, vid):
        return bool(os.environ.get("FAKE_LOCKED"))

    def remove(key, vid):
        gone.add((key, vid))
    try:
        out = m.delete_objects(body["Objects"], quiet=bool(body.get("Quiet")), bypass="--bypass-governance-retention" in args, protected=protected, remove=remove)
    except m.ServiceError as exc:
        fail(exc.text(), exc.exit_code)
    if not os.environ.get("FAKE_NO_DELETE"):
        doc["entries"] = [e for e in entries if (e["Key"], e["VersionId"]) not in gone]
        doc["deleted"] = doc.get("deleted", 0) + len(gone)
        json.dump(doc, open(path, "w"))
    emit(out)
else:
    sys.stderr.write("fake aws: unsupported\n")
    sys.exit(252)
'''

TESTS_DIR = Path(__file__).resolve().parent


class Rig:
    def __init__(self, test: unittest.TestCase, versions: int = 3, markers: int = 1, layout: str = "markers-first", script: Path = SCRIPT) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="empty-bucket-"))
        test.addCleanup(shutil.rmtree, self.tmp, True)
        self.script = script
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        (self.bin / "aws").write_text(FAKE_AWS.replace("__PY__", sys.executable).replace("__ARN__", ARN))
        (self.bin / "aws").chmod(0o755)
        self.bucket_file = self.tmp / "bucket.json"
        self.bucket_file.write_text(json.dumps({"entries": model.shape(versions, markers, layout)}))
        self.log = self.tmp / "log"

    def set_entries(self, entries: List[Dict[str, Any]]) -> None:
        self.bucket_file.write_text(json.dumps({"entries": entries}))

    def run(self, *args: str, timeout: float = limit(300), **extra: str) -> subprocess.CompletedProcess:
        env = {"PATH": f"{self.bin}:{os.environ.get('PATH', '/usr/bin:/bin')}", "HOME": str(self.tmp), "TMPDIR": str(self.tmp), "HAMH_PYTHON": sys.executable,
               "FAKE_BUCKET": str(self.bucket_file), "FAKE_LOG": str(self.log), "FAKE_OWNER": ACCOUNT, "FAKE_TESTS_DIR": str(TESTS_DIR), "HAMH_SETUP_RETRY_SECONDS": "0"}
        env.update(extra)
        return subprocess.run(["bash", str(self.script), *args], env=env, capture_output=True, text=True, timeout=timeout)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do

    def state(self) -> Dict[str, Any]:
        return json.loads(self.bucket_file.read_text())

    def left(self) -> int:
        return len(self.state()["entries"])

    def all_calls(self) -> List[Dict[str, Any]]:
        return [json.loads(ln) for ln in self.log.read_text().splitlines()] if self.log.is_file() else []

    def calls(self) -> List[List[str]]:
        """The S3 calls (the identity call is not one)."""
        return [c["args"] for c in self.all_calls() if c["args"][0] == "s3api"]

    def delete_sizes(self) -> List[int]:
        p = Path(str(self.log) + ".sizes")
        return [int(x) for x in p.read_text().split()] if p.is_file() else []


def last_line(proc: subprocess.CompletedProcess) -> str:
    return [ln for ln in proc.stdout.splitlines() if ln.strip()][-1]


# (versions, delete markers, layout): the shapes an independent review ran against the real AWS CLI v2.37.2 and a loopback S3, and more of the same kind.
SHAPES = [
    (500, 600, "markers-first"),   # 1,100 entries, 500 of them versions: the CLI reads the whole bucket and sends 1,100 keys
    (999, 2, "markers-first"),
    (10, 1500, "markers-first"),
    (0, 2500, "markers-first"),
    (2500, 0, "markers-first"),
    (1200, 1200, "interleaved"),
    (100, 3000, "markers-first"),
    (1000, 0, "markers-first"),
    (1001, 0, "markers-first"),
    (300, 1500, "versions-first"),
    (1, 1, "interleaved"),
    (1000, 1000, "interleaved"),
    (2999, 1, "versions-first"),
]


class ShapesCase(unittest.TestCase):
    """The buckets whose aggregated listing is longer than 1,000 entries: the earlier script sent more keys than DeleteObjects takes, S3 refused, the bucket stayed full
    and the stack that owns it stayed too."""

    def test_every_shape_is_emptied_in_batches_of_at_most_a_thousand_keys(self) -> None:
        for versions, markers, layout in SHAPES:
            with self.subTest(versions=versions, markers=markers, layout=layout):
                rig = Rig(self, versions=versions, markers=markers, layout=layout)
                proc = rig.run(BUCKET)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={BUCKET} deleted={versions + markers}")
                self.assertEqual(rig.left(), 0)
                sizes = rig.delete_sizes()
                self.assertEqual(sum(sizes), versions + markers)
                self.assertTrue(all(0 < s <= 1000 for s in sizes), sizes)

    def test_the_form_of_the_earlier_script_fails_on_these_shapes_in_the_model(self) -> None:
        """The earlier script listed with `--max-items 1000`. The model must reproduce its failure, or it proves nothing about the new one."""
        old = Path(tempfile.mkdtemp(prefix="empty-bucket-old-"))
        self.addCleanup(shutil.rmtree, old, True)
        text = SCRIPT.read_text()
        self.assertIn("--no-paginate --max-keys 1000", text, "the script lists one page at a time")
        earlier = old / "empty-bucket-earlier.sh"
        earlier.write_text(text.replace("--no-paginate --max-keys 1000", "--max-items 1000"))
        for versions, markers, layout in ((500, 600, "markers-first"), (999, 2, "markers-first"), (10, 1500, "markers-first"), (0, 2500, "markers-first"), (300, 1500, "versions-first")):
            with self.subTest(versions=versions, markers=markers):
                rig = Rig(self, versions=versions, markers=markers, layout=layout, script=earlier)
                proc = rig.run(BUCKET)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                # the earlier form gave more than 1,000 keys in one payload; the guard of the script stops before S3 refuses it with MalformedXML (see the red record)
                self.assertRegex(last_line(proc), r"MalformedXML|entries in one page")
                self.assertEqual(rig.left(), versions + markers, "nothing was deleted")
        rig = Rig(self, versions=2500, markers=0, script=earlier)
        proc = rig.run(BUCKET)
        self.assertEqual(proc.returncode, 0, "versions alone were never a problem: " + proc.stdout + proc.stderr)

    def test_a_page_is_never_sent_as_a_delete_when_it_holds_more_than_a_thousand_entries(self) -> None:
        """A service that answered with more than 1,000 entries in one page would give a delete S3 refuses; the script stops before it sends that."""
        rig = Rig(self, versions=3, markers=0)
        proc = rig.run(BUCKET, FAKE_OVERSIZE="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("EMPTY-BUCKET FAIL", last_line(proc))
        self.assertIn("1000", last_line(proc))
        self.assertEqual(rig.delete_sizes(), [])
        self.assertEqual(rig.left(), 3)

    def test_the_model_lists_a_page_the_way_the_real_cli_does(self) -> None:
        """The numbers below were measured with AWS CLI v2.37.2 against a loopback S3 (pages of 1,000 combined entries)."""
        entries = model.shape(500, 600, "markers-first")
        out = model.cli_list(entries, max_items=1000)
        self.assertEqual((len(out.get("Versions", [])), len(out.get("DeleteMarkers", []))), (500, 600), "the CLI read the whole bucket: 500 versions never reach the limit")
        out = model.cli_list(model.shape(2500, 0), max_items=1000)
        self.assertEqual((len(out["Versions"]), "DeleteMarkers" in out, "NextToken" in out), (1000, False, True))
        out = model.cli_list(entries, no_paginate=True, max_keys=1000)
        self.assertEqual(len(out.get("Versions", [])) + len(out.get("DeleteMarkers", [])), 1000)
        self.assertTrue(out["IsTruncated"])
        with self.assertRaises(model.UsageError):
            model.cli_list(entries, no_paginate=True, max_items=1000)
        with self.assertRaises(model.ServiceError):
            model.delete_objects([{"Key": f"k{i}", "VersionId": "v"} for i in range(1001)])


class EmptyBucketCase(unittest.TestCase):
    def setUp(self) -> None:
        if not SCRIPT.is_file():
            self.fail("scripts/setup/empty-bucket.sh does not exist")

    def test_deletes_every_version_and_marker(self) -> None:
        rig = Rig(self, versions=3, markers=2)
        proc = rig.run(BUCKET)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={BUCKET} deleted=5")
        self.assertEqual(rig.left(), 0)

    def test_lists_again_after_each_batch_and_deletes_in_batches_of_a_thousand_or_less(self) -> None:
        rig = Rig(self, versions=2500, markers=0)
        proc = rig.run(BUCKET)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={BUCKET} deleted=2500")
        self.assertEqual(rig.delete_sizes(), [1000, 1000, 500])
        self.assertEqual(len([c for c in rig.calls() if c[1] == "list-object-versions"]), 4, "a listing after each of the three batches finds the next, the last finds none")

    def test_the_listing_is_one_page_of_at_most_a_thousand_entries(self) -> None:
        rig = Rig(self, versions=3, markers=1)
        rig.run(BUCKET)
        lists = [c for c in rig.calls() if c[1] == "list-object-versions"]
        self.assertTrue(lists)
        for call in lists:
            self.assertIn("--no-paginate", call)
            self.assertEqual(call[call.index("--max-keys") + 1], "1000")
            self.assertNotIn("--max-items", call, "the CLI counts the versions only against --max-items and adds every delete marker")

    def test_an_empty_bucket_passes_with_nothing_deleted(self) -> None:
        rig = Rig(self, versions=0, markers=0)
        proc = rig.run(BUCKET)
        self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={BUCKET} deleted=0")
        self.assertFalse([c for c in rig.calls() if c[1] == "delete-objects"])

    def test_every_call_names_the_owner_of_the_bucket_from_its_name(self) -> None:
        rig = Rig(self)
        rig.run(BUCKET)
        self.assertTrue(rig.calls())
        for call in rig.calls():
            self.assertEqual(call[call.index("--expected-bucket-owner") + 1], ACCOUNT)
            self.assertEqual(call[call.index("--bucket") + 1], BUCKET)

    def test_only_a_setup_bucket_or_a_bucket_of_the_stage_is_accepted(self) -> None:
        refused = ("my-bucket", "hamh-direct-other-111122223333", "hamh-direct-harness-1234", "hamh-direct-harness-111122223333-us-east-1", "hamh-direct-gomod-", "hamh-direct-harness",
                   "hamh-direct-harness-111122223333x", "hamh-direct-harness-111122223333/prefix", "xhamh-direct-gomod-111122223333", "hamh-live-harness-111122223333",
                   "hamh-setup-1234-us-east-1", "hamh-setup-111122223333-", f"{BUCKET}/prefix", f"s3://{BUCKET}", f"{BUCKET}\nextra", f"{BUCKET};other", f"{BUCKET} other", f"{BUCKET}&", "")
        for name in refused:
            rig = Rig(self)
            proc = rig.run(name, AWS_REGION="us-east-1")
            self.assertEqual(proc.returncode, 2, f"{name!r}: {proc.stdout}{proc.stderr}")
            self.assertEqual(rig.all_calls(), [], name)
            self.assertEqual(rig.left(), 4)

    def test_a_bucket_of_the_stage_is_emptied_in_the_region_of_the_environment(self) -> None:
        for kind in ("harness", "gomod"):
            bucket = f"hamh-direct-{kind}-{ACCOUNT}"
            rig = Rig(self, versions=3, markers=1)
            proc = rig.run(bucket, AWS_REGION="us-west-2")
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={bucket} deleted=4")
            for call in rig.all_calls():
                self.assertEqual(call["global"]["--region"], "us-west-2")
            for call in rig.calls():
                self.assertEqual(call[call.index("--bucket") + 1], bucket)
                self.assertEqual(call[call.index("--expected-bucket-owner") + 1], ACCOUNT)

    def test_a_bucket_of_the_stage_without_a_region_is_refused_before_any_call(self) -> None:
        rig = Rig(self)
        proc = rig.run(f"hamh-direct-harness-{ACCOUNT}")
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertEqual(rig.all_calls(), [])
        self.assertIn("AWS_REGION", proc.stderr)

    def test_the_governance_bypass_is_sent_for_the_go_module_store_and_for_nothing_else(self) -> None:
        for bucket, region in ((f"hamh-direct-gomod-{ACCOUNT}", "us-east-1"), (f"hamh-direct-harness-{ACCOUNT}", "us-east-1"), (BUCKET, "")):
            rig = Rig(self, versions=2, markers=0)
            proc = rig.run(bucket, **({"AWS_REGION": region} if region else {}))
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            deletes = [c for c in rig.calls() if c[1] == "delete-objects"]
            self.assertTrue(deletes)
            for call in deletes:
                self.assertEqual("--bypass-governance-retention" in call, bucket.startswith("hamh-direct-gomod-"), (bucket, call))
            for call in rig.calls():
                if call[1] == "list-object-versions":
                    self.assertNotIn("--bypass-governance-retention", call)

    def test_a_locked_object_is_a_failure_without_the_bypass_and_goes_with_it(self) -> None:
        rig = Rig(self, versions=3, markers=0)
        proc = rig.run(f"hamh-direct-harness-{ACCOUNT}", AWS_REGION="us-east-1", FAKE_LOCKED="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("refused a delete", last_line(proc))
        self.assertEqual(rig.left(), 3)
        rig = Rig(self, versions=3, markers=0)
        proc = rig.run(f"hamh-direct-gomod-{ACCOUNT}", AWS_REGION="us-east-1", FAKE_LOCKED="1")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(rig.left(), 0)

    def test_a_stage_bucket_of_another_account_is_not_touched(self) -> None:
        rig = Rig(self)
        proc = rig.run(f"hamh-direct-harness-{ACCOUNT}", AWS_REGION="us-east-1", FAKE_OWNER="999988887777")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("EMPTY-BUCKET FAIL "), proc.stdout)
        self.assertEqual(rig.left(), 4)

    def test_usage(self) -> None:
        rig = Rig(self)
        self.assertEqual(rig.run().returncode, 2)
        self.assertEqual(rig.run(BUCKET, "extra").returncode, 2)

    def test_a_refused_delete_is_a_failure(self) -> None:
        rig = Rig(self)
        proc = rig.run(BUCKET, FAKE_DELETE_ERRORS="1")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("EMPTY-BUCKET FAIL "), proc.stdout)
        self.assertIn("AccessDenied", last_line(proc))

    def test_a_delete_that_reports_success_and_changes_nothing_is_a_failure_not_a_loop(self) -> None:
        rig = Rig(self, versions=3, markers=1)
        proc = rig.run(BUCKET, timeout=limit(30), FAKE_NO_DELETE="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("still lists", last_line(proc))
        self.assertEqual(len(rig.delete_sizes()), 1, "one delete, then the same page again: the script stops")

    def test_a_bucket_that_is_not_there_is_its_own_answer_and_a_key_named_after_the_error_is_not_that_answer(self) -> None:
        rig = Rig(self)
        proc = rig.run(BUCKET, FAKE_NO_BUCKET="1")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"EMPTY-BUCKET GONE bucket={BUCKET}")
        rig = Rig(self, versions=0, markers=0)
        rig.set_entries([{"Key": "NoSuchBucket/object.txt", "VersionId": "v1", "marker": False}])
        proc = rig.run(BUCKET, FAKE_DELETE_ERRORS="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertTrue(last_line(proc).startswith("EMPTY-BUCKET FAIL "), proc.stdout)
        self.assertNotIn("GONE", proc.stdout)

    def test_an_answer_that_is_not_a_listing_is_a_failure_not_an_empty_bucket(self) -> None:
        rig = Rig(self, versions=3, markers=0)
        proc = rig.run(BUCKET, FAKE_JUNK="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn("not a listing", last_line(proc))
        self.assertEqual(rig.left(), 3)

    def test_an_object_list_that_is_null_or_holds_no_version_ids_is_not_an_empty_bucket(self) -> None:
        """Only {"Objects": []} is an empty bucket; a null, a list of keys without versions or an object in place of the array is a listing the script cannot use."""
        for junk in ("objects-null", "keys-only", "no-array"):
            with self.subTest(answer=junk):
                rig = Rig(self, versions=3, markers=0)
                proc = rig.run(BUCKET, FAKE_JUNK=junk)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertTrue(last_line(proc).startswith("EMPTY-BUCKET FAIL "), proc.stdout)
                self.assertNotIn("EMPTY-BUCKET PASS", proc.stdout)
                self.assertEqual(rig.delete_sizes(), [])
                self.assertEqual(rig.left(), 3)

    def test_an_empty_bucket_answers_an_empty_array_and_passes_with_nothing_deleted(self) -> None:
        rig = Rig(self, versions=0, markers=0)
        proc = rig.run(BUCKET)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={BUCKET} deleted=0")

    def test_a_refusal_of_the_listing_that_only_names_a_missing_bucket_in_its_text_is_a_failure(self) -> None:
        """AccessDenied for a resource whose name holds NoSuchBucket is a refusal, not a bucket that is gone."""
        rig = Rig(self, versions=3, markers=0)
        proc = rig.run(BUCKET, FAKE_LIST_DENIED="1", FAKE_DENIED_TEXT="1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertNotIn("GONE", proc.stdout)

    def test_a_throttled_listing_is_repeated_and_a_refused_one_is_not(self) -> None:
        rig = Rig(self, versions=3, markers=1)
        proc = rig.run(BUCKET, FAKE_THROTTLE_LISTS="3")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={BUCKET} deleted=4")
        rig = Rig(self, versions=3, markers=1)
        proc = rig.run(BUCKET, FAKE_THROTTLE_LISTS="9")
        self.assertEqual(proc.returncode, 1, "5 tries and then a failure that says why")
        self.assertIn("SlowDown", last_line(proc))
        self.assertEqual(len([c for c in rig.calls() if c[1] == "list-object-versions"]), 5)
        rig = Rig(self, versions=3, markers=1)
        proc = rig.run(BUCKET, FAKE_LIST_DENIED="1")
        self.assertEqual(proc.returncode, 1)
        self.assertEqual(len([c for c in rig.calls() if c[1] == "list-object-versions"]), 1, "AccessDenied is not repeated")

    def test_a_key_that_holds_the_word_versionid_is_counted_once(self) -> None:
        """The count of a batch is the number of objects, not the number of lines that hold the text."""
        rig = Rig(self, versions=0, markers=0)
        rig.set_entries([{"Key": 'odd "VersionId": key', "VersionId": "v1", "marker": False}, {"Key": "plain", "VersionId": "v2", "marker": False}])
        proc = rig.run(BUCKET)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={BUCKET} deleted=2")

    def test_the_first_line_names_the_identity_and_a_different_expected_identity_stops_the_step(self) -> None:
        rig = Rig(self)
        proc = rig.run(BUCKET)
        self.assertEqual(proc.stdout.splitlines()[0], f"EMPTY-BUCKET identity {ARN}")
        rig = Rig(self)
        proc = rig.run(BUCKET, HAMH_EXPECT_CALLER_ARN=ARN)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        rig = Rig(self)
        other = f"arn:aws:iam::{ACCOUNT}:user/someone-else"
        proc = rig.run(BUCKET, HAMH_EXPECT_CALLER_ARN=other)
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertIn(ARN, last_line(proc))
        self.assertIn(other, last_line(proc))
        self.assertEqual(rig.calls(), [], "no bucket call was made")
        self.assertEqual(rig.left(), 4)
        # the same role under another session name is the same identity (a credential_process helper makes a new session for each call); another role is not
        role = f"arn:aws:sts::{ACCOUNT}:assumed-role/Admin/session-1"
        rig = Rig(self)
        proc = rig.run(BUCKET, HAMH_EXPECT_CALLER_ARN=role, FAKE_ARN=f"arn:aws:sts::{ACCOUNT}:assumed-role/Admin/session-2")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        rig = Rig(self)
        proc = rig.run(BUCKET, HAMH_EXPECT_CALLER_ARN=role, FAKE_ARN=f"arn:aws:sts::{ACCOUNT}:assumed-role/Other/session-1")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(rig.calls(), [])
        rig = Rig(self)
        proc = rig.run(BUCKET, FAKE_IDENTITY_ERROR="1")
        self.assertEqual(proc.returncode, 1)
        self.assertIn("ExpiredToken", last_line(proc))
        self.assertEqual(rig.calls(), [])

    def test_the_aws_program_and_the_profile_can_be_named_as_setup_sh_names_them(self) -> None:
        """setup.sh runs this script with AWS_CLI_PROG (HAMH_AWS_CLI of the user, else aws) and the profile of its run as --profile, which beats exported access keys."""
        rig = Rig(self, versions=2, markers=0)
        named = rig.bin / "named-aws"
        named.write_text('#!/bin/sh\necho "$0" >> "$FAKE_LOG.named"\nexec "$(dirname "$0")/aws" "$@"\n')
        named.chmod(0o755)
        proc = rig.run(BUCKET, AWS_CLI_PROG=str(named), HAMH_EMPTY_BUCKET_PROFILE="work")
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        self.assertEqual(len(Path(str(rig.log) + ".named").read_text().split()), len(rig.all_calls()))
        for call in rig.all_calls():
            self.assertEqual(call["global"]["--profile"], "work", call)
        self.assertEqual(len(rig.calls()), 3, "one listing, one delete, one listing that finds the bucket empty")

    def test_without_a_named_profile_no_profile_option_is_sent(self) -> None:
        rig = Rig(self, versions=1, markers=0)
        rig.run(BUCKET, AWS_PROFILE="from-the-environment")
        for call in rig.all_calls():
            self.assertNotIn("--profile", call["global"])
            self.assertEqual(call["env_profile"], "from-the-environment")

    def test_a_bucket_of_another_account_is_not_touched(self) -> None:
        rig = Rig(self)
        proc = rig.run(BUCKET, FAKE_OWNER="999988887777")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("EMPTY-BUCKET FAIL "), proc.stdout)
        self.assertEqual(rig.left(), 4)


class PreviousNamesCase(unittest.TestCase):
    """A stage deployed before the rename has its buckets under the first word of the previous names. The script empties them too, with the same guards,
    and a name that only looks like one is refused before any call."""

    def test_the_buckets_of_the_previous_names_are_accepted_and_emptied_in_the_region_of_the_run(self) -> None:
        for bucket, region in ((leg(BUCKET), ""), (leg(f"hamh-direct-harness-{ACCOUNT}"), "us-west-2"), (leg(f"hamh-direct-gomod-{ACCOUNT}"), "us-west-2")):
            with self.subTest(bucket=bucket):
                rig = Rig(self, versions=3, markers=1)
                proc = rig.run(bucket, **({"AWS_REGION": region} if region else {}))
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                self.assertEqual(last_line(proc), f"EMPTY-BUCKET PASS bucket={bucket} deleted=4")
                for call in rig.calls():
                    self.assertEqual(call[call.index("--bucket") + 1], bucket)
                    self.assertEqual(call[call.index("--expected-bucket-owner") + 1], ACCOUNT)
                if region:
                    for call in rig.all_calls():
                        self.assertEqual(call["global"]["--region"], region)

    def test_the_governance_bypass_goes_to_the_go_module_store_of_either_generation_alone(self) -> None:
        for bucket, region in ((leg(f"hamh-direct-gomod-{ACCOUNT}"), "us-east-1"), (leg(f"hamh-direct-harness-{ACCOUNT}"), "us-east-1"), (leg(BUCKET), "")):
            rig = Rig(self, versions=2, markers=0)
            proc = rig.run(bucket, **({"AWS_REGION": region} if region else {}))
            self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
            deletes = [c for c in rig.calls() if c[1] == "delete-objects"]
            self.assertTrue(deletes)
            for call in deletes:
                self.assertEqual("--bypass-governance-retention" in call, "gomod" in bucket, (bucket, call))

    def test_a_name_that_only_looks_like_a_bucket_of_the_previous_names_is_refused_before_any_call(self) -> None:
        refused = (leg(f"hamh-live-harness-{ACCOUNT}"), leg(f"hamh-drill-gomod-{ACCOUNT}"), leg(f"hamh-direct-other-{ACCOUNT}"), "x" + leg(f"hamh-direct-gomod-{ACCOUNT}"),
                   leg(f"hamh-direct-harness-{ACCOUNT}") + "x", leg("hamh-setup-1234-us-east-1"), leg(f"hamh-setup-{ACCOUNT}-"), leg(BUCKET) + "/prefix", leg(BUCKET) + ";other",
                   "hamh-" + leg(f"hamh-direct-harness-{ACCOUNT}"), leg(f"hamh-direct-harness-{ACCOUNT}") + "-us-east-1", leg("hamh-setup-runner"))
        for name in refused:
            rig = Rig(self)
            proc = rig.run(name, AWS_REGION="us-east-1")
            self.assertEqual(proc.returncode, 2, f"{name!r}: {proc.stdout}{proc.stderr}")
            self.assertEqual(rig.all_calls(), [], name)
            self.assertEqual(rig.left(), 4)

    def test_a_stage_bucket_of_the_previous_names_without_a_region_is_refused_before_any_call(self) -> None:
        rig = Rig(self)
        proc = rig.run(leg(f"hamh-direct-gomod-{ACCOUNT}"))
        self.assertEqual(proc.returncode, 2, proc.stdout + proc.stderr)
        self.assertEqual(rig.all_calls(), [])
        self.assertIn("AWS_REGION", proc.stderr)

    def test_a_bucket_of_the_previous_names_of_another_account_is_not_touched(self) -> None:
        rig = Rig(self)
        proc = rig.run(leg(f"hamh-direct-harness-{ACCOUNT}"), AWS_REGION="us-east-1", FAKE_OWNER="999988887777")
        self.assertEqual(proc.returncode, 1)
        self.assertTrue(last_line(proc).startswith("EMPTY-BUCKET FAIL "), proc.stdout)
        self.assertEqual(rig.left(), 4)


if __name__ == "__main__":
    unittest.main()
