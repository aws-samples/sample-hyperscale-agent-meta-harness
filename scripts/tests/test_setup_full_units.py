"""Unit tests of the helper files of the full setup, run as bash functions.

Each case sources the helpers it needs in a fresh bash and calls one function: the JSON reader (`json_flat`), the generator of the account
file (`acct_generate`), the receipt (`st_*`) and the client platform (`cl_platform`). The helpers are sourced by `setup.sh`; they hold definitions
only and run nothing when sourced.
"""

from __future__ import annotations

import json
import re
import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from input
import sys
import unittest
from pathlib import Path

from setup_full_support import FullBase, golden_accounts
from setup_waits import limit
from test_setup_script import BASH, REPO

sys.path.insert(0, str(REPO / "scripts" / "lib"))
import aws_target  # noqa: E402  the loader of accounts.json that every script uses

EAST = FullBase.ACCOUNT
AZ_EAST = "us-east-1a\tuse1-az6\nus-east-1b\tuse1-az1\nus-east-1c\tuse1-az2\nus-east-1d\tuse1-az4\nus-east-1e\tuse1-az3\nus-east-1f\tuse1-az5\n"
AZ_WEST = "us-west-2a\tusw2-az2\nus-west-2b\tusw2-az1\nus-west-2c\tusw2-az3\nus-west-2d\tusw2-az4\n"


class HelperCase(FullBase):
    """Run a snippet of bash with the helper files sourced, in the checkout, with the same PATH setup.sh gets."""

    def sh(self, body: str, *files: str, **env: str) -> subprocess.CompletedProcess:
        # setup.sh sources names.sh before the helpers, so a helper reads HAMH_CLIENT_BIN and HAMH_CONFIG_DIR_NAME without a default of its own
        sources = ". scripts/setup/names.sh\n" + "".join(f". scripts/setup/{f}.sh\n" for f in files)
        return subprocess.run([BASH, "-c", "set -u\n" + sources + body], capture_output=True, text=True, env=self.env(**env), cwd=str(self.repo),  # nosec B603: argument list, test snippets
                              timeout=limit(60), stdin=subprocess.DEVNULL)


class JsonReaderCase(HelperCase):
    def flat(self, doc: str) -> dict:
        p = self.sh('printf "%s" "$DOC" | json_flat', "json", DOC=doc)
        self.assertEqual(p.returncode, 0, p.stderr)
        return dict(line.split("\t", 1) for line in p.stdout.splitlines() if line)

    def test_nested_objects_and_arrays_become_dotted_paths(self) -> None:
        doc = json.dumps({"state": "SETUP-PASS", "client": {"darwin-arm64": {"key": "client/b/darwin-arm64/ham", "sha256": "ab" * 32, "bytes": 12}},
                          "phases": [{"name": "toolchain", "state": "done", "seconds": 4}, {"name": "deploy", "state": "done", "seconds": 9}],
                          "ok": True, "gone": None})
        got = self.flat(doc[:-1] + ', "ratio": -1.5e2}')
        self.assertEqual(got["state"], "SETUP-PASS")
        self.assertEqual(got["client.darwin-arm64.sha256"], "ab" * 32)
        self.assertEqual(got["client.darwin-arm64.bytes"], "12")
        self.assertEqual(got["phases.0.name"], "toolchain")
        self.assertEqual(got["phases.1.seconds"], "9")
        self.assertEqual(got["ok"], "true")
        self.assertEqual(got["gone"], "null")
        self.assertEqual(got["ratio"], "-1.5e2")

    def test_the_layout_of_the_document_does_not_matter(self) -> None:
        doc = {"a": {"b": [1, 2, {"c": "x y"}]}, "d": "e"}
        want = self.flat(json.dumps(doc))
        for text in (json.dumps(doc, indent=4), json.dumps(doc, indent="\t"), json.dumps(doc, separators=(",", ":")), "\n\n " + json.dumps(doc) + " \n"):
            self.assertEqual(self.flat(text), want)

    def test_string_escapes_are_decoded_and_stay_on_one_line(self) -> None:
        got = self.flat(json.dumps({"detail": 'say "hi"\\ now\nnext\tcol é A', "plain": "a/b"}))
        self.assertEqual(got["plain"], "a/b")
        self.assertIn('say "hi"\\ now', got["detail"])
        self.assertNotIn("\n", got["detail"])
        self.assertIn("\\n", got["detail"], "a newline inside a value is written as the two characters \\n")
        self.assertTrue(got["detail"].endswith(" A"), got["detail"])

    def test_braces_commas_and_colons_inside_strings_are_not_structure(self) -> None:
        got = self.flat(json.dumps({"detail": '{"a": [1, 2], "b": "c"}, done: yes', "next": "n"}))
        self.assertEqual(got["detail"], '{"a": [1, 2], "b": "c"}, done: yes')
        self.assertEqual(got["next"], "n")

    def test_empty_containers_and_empty_input_print_nothing(self) -> None:
        self.assertEqual(self.flat("{}"), {})
        self.assertEqual(self.flat("[]"), {})
        self.assertEqual(self.flat(""), {})
        self.assertEqual(self.flat('{"a": {}, "b": [], "c": ""}'), {"c": ""})

    def test_json_value_prints_the_first_value_of_a_path_and_nothing_for_a_missing_one(self) -> None:
        p = self.sh('printf "%s" "$DOC" | json_value client.linux-amd64.sha256; printf "[%s]\\n" "$(printf "%s" "$DOC" | json_value nope.nothing)"', "json",
                    DOC=json.dumps({"client": {"linux-amd64": {"sha256": "cd" * 32}}}))
        self.assertEqual(p.stdout, "cd" * 32 + "\n[]\n", p.stderr)


OPERATOR_TABLE = json.loads((REPO / "infra" / "test" / "fixtures" / "operator-identity.json").read_text())
OTHER_ACCOUNT = "4444" + "5555" + "6666"


class AccountsGeneratorCase(HelperCase):
    def generate(self, account: str, region: str, azlines: str, profile: str = "hamh-setup", azs: str = "1", logging: str = "read-only", caller: str | None = None):
        """`caller` is the seventh argument of acct_generate, the Arn of `sts get-caller-identity`; None passes no seventh argument."""
        env = {} if caller is None else {"CALLER": caller}
        extra = "" if caller is None else ' "$CALLER"'
        p = self.sh('acct_generate "$ACCT" "$REG" "$PROF" "$AZ" "$N" "$LOG"' + extra + ' || exit $?\nprintf "%s" "$ACCT_JSON" > out.json\n'
                    'printf "%s\\n%s\\n%s\\n" "$ACCT_DIGEST" "$ACCT_KEY" "$ACCT_DIRECT_AZ"', "lib", "accounts", ACCT=account, REG=region, PROF=profile, AZ=azlines, N=azs, LOG=logging, **env)
        return p

    @staticmethod
    def arn(row: dict) -> str:
        return row["arn"].replace("{account}", EAST).replace("{other}", OTHER_ACCOUNT)

    def test_the_caller_identity_becomes_the_operator_of_the_entry_for_each_row_of_the_shared_table(self) -> None:
        rows = [r for r in OPERATOR_TABLE["generate"] if "operator" in r]
        self.assertGreaterEqual(len(rows), 6)
        for row in rows:
            with self.subTest(self.arn(row)):
                out = self.repo / "out.json"
                if out.exists():
                    out.unlink()
                p = self.generate(EAST, "us-east-1", AZ_EAST, caller=self.arn(row))
                self.assertEqual(p.returncode, 0, p.stderr)
                t = aws_target.load(out)[0]
                self.assertEqual((t.operator.kind, t.operator.name), (row["operator"]["kind"], row["operator"]["name"]))
                self.assertEqual(json.loads(out.read_text())["accounts"][0]["operator"], row["operator"])
                # the rest of the file is the file of a generator that was given no caller
                self.assertEqual(out.read_text(), golden_accounts("us-east-1", operator=(row["operator"]["kind"], row["operator"]["name"])).decode())

    def test_an_identity_that_cannot_be_the_operator_is_refused_with_the_reason_of_the_table_and_nothing_is_written(self) -> None:
        rows = [r for r in OPERATOR_TABLE["generate"] if "refuse" in r]
        self.assertGreaterEqual(len(rows), 10)
        for row in rows:
            with self.subTest(self.arn(row)):
                out = self.repo / "out.json"
                if out.exists():
                    out.unlink()
                p = self.generate(EAST, "us-east-1", AZ_EAST, caller=self.arn(row))
                self.assertNotEqual(p.returncode, 0)
                self.assertFalse(out.exists())
                self.assertIn(row["refuse"], p.stderr)

    def test_the_root_user_and_a_federated_user_are_told_the_way_out(self) -> None:
        for who in ("arn:aws:iam::{account}:root", "arn:aws:sts::{account}:federated-user/bob"):
            p = self.generate(EAST, "us-east-1", AZ_EAST, caller=who.replace("{account}", EAST))
            self.assertNotEqual(p.returncode, 0)
            self.assertIn("IAM user", p.stderr)
            self.assertIn("aws sso login", p.stderr)
            self.assertIn("Nothing was created", p.stderr)

    def test_without_a_seventh_argument_the_entry_names_no_operator_and_an_empty_one_is_refused(self) -> None:
        self.assertEqual(self.generate(EAST, "us-east-1", AZ_EAST).returncode, 0)
        self.assertNotIn("operator", (self.repo / "out.json").read_text())
        out = self.repo / "out.json"
        out.unlink()
        p = self.generate(EAST, "us-east-1", AZ_EAST, caller="")
        self.assertNotEqual(p.returncode, 0)
        self.assertFalse(out.exists())
        self.assertIn("cannot name", p.stderr)

    def test_the_operator_changes_the_digest_so_a_second_person_is_a_second_account_file(self) -> None:
        a = self.generate(EAST, "us-east-1", AZ_EAST, caller=f"arn:aws:iam::{EAST}:user/alice").stdout.splitlines()[0]
        b = self.generate(EAST, "us-east-1", AZ_EAST, caller=f"arn:aws:iam::{EAST}:user/bob").stdout.splitlines()[0]
        c = self.generate(EAST, "us-east-1", AZ_EAST, caller=f"arn:aws:iam::{EAST}:user/alice").stdout.splitlines()[0]
        d = self.generate(EAST, "us-east-1", AZ_EAST).stdout.splitlines()[0]
        self.assertEqual(a, c)
        self.assertEqual(len({a, b, d}), 3)

    def test_the_file_has_exactly_the_shape_of_the_committed_accounts_json_and_loads_through_the_real_loader(self) -> None:
        p = self.generate(EAST, "us-east-1", AZ_EAST)
        self.assertEqual(p.returncode, 0, p.stderr)
        text = (self.repo / "out.json").read_text()
        want = golden_accounts("us-east-1", operator=None).decode()
        self.assertEqual(text, want)
        entries = aws_target.load(self.repo / "out.json")
        self.assertEqual(len(entries), 1)
        t = entries[0]
        self.assertEqual((t.account, t.region, t.profile, t.role, t.invocation_logging), (EAST, "us-east-1", "hamh-setup", "active", "read-only"))
        self.assertEqual(t.azs["direct"], ["us-east-1b"])
        self.assertEqual(t.az_ids["direct"], ["use1-az1"])

    def test_west_picks_the_lowest_agentcore_zone_id_whatever_the_account_calls_it(self) -> None:
        p = self.generate(EAST, "us-west-2", AZ_WEST)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual((self.repo / "out.json").read_text(), golden_accounts("us-west-2", operator=None).decode())
        t = aws_target.load(self.repo / "out.json")[0]
        self.assertEqual((t.azs["direct"], t.az_ids["direct"]), (["us-west-2b"], ["usw2-az1"]))

    def test_the_digest_and_key_name_the_file(self) -> None:
        p = self.generate(EAST, "us-east-1", AZ_EAST)
        digest, key, direct = p.stdout.splitlines()
        import hashlib
        self.assertEqual(digest, hashlib.sha256((self.repo / "out.json").read_bytes()).hexdigest())
        self.assertEqual(key, f"config/accounts-{digest[:12]}.json")
        self.assertEqual(direct, "us-east-1b use1-az1")

    def test_the_same_inputs_give_the_same_file_and_other_inputs_another(self) -> None:
        a = self.generate(EAST, "us-east-1", AZ_EAST).stdout
        b = self.generate(EAST, "us-east-1", AZ_EAST).stdout
        c = self.generate(str(int(EAST) + 1), "us-east-1", AZ_EAST).stdout
        d = self.generate(EAST, "us-east-1", AZ_EAST, profile="work").stdout
        e = self.generate(EAST, "us-east-1", AZ_EAST, logging="enable-and-restore").stdout
        self.assertEqual(a, b)
        self.assertEqual(len({a.splitlines()[0], c.splitlines()[0], d.splitlines()[0], e.splitlines()[0]}), 4)

    def test_the_profile_and_the_logging_mode_are_written_as_given(self) -> None:
        self.generate(EAST, "us-east-1", AZ_EAST, profile="work", logging="enable-and-restore")
        t = aws_target.load(self.repo / "out.json")[0]
        self.assertEqual((t.profile, t.invocation_logging), ("work", "enable-and-restore"))

    def test_placeholders_are_refused_and_nothing_is_written(self) -> None:
        for label, account, region in (("documentation account", "111122223333", "us-east-1"), ("example account", "123456789012", "us-east-1"),
                                       ("zeros", "000000000000", "us-east-1"), ("angle brackets", "<account id>", "us-east-1"),
                                       ("short", "12345", "us-east-1"), ("not digits", "21098765432x", "us-east-1"), ("region placeholder", EAST, "<region>"),
                                       ("empty region", EAST, "")):
            with self.subTest(label):
                out = self.repo / "out.json"
                if out.exists():
                    out.unlink()
                p = self.generate(account, region, AZ_EAST)
                self.assertNotEqual(p.returncode, 0, label)
                self.assertFalse(out.exists(), label)
                self.assertTrue(p.stderr.strip(), label)

    def test_an_unsupported_region_is_refused_with_the_supported_ones_named(self) -> None:
        p = self.generate(EAST, "eu-west-1", "eu-west-1a\teuw1-az3\neu-west-1b\teuw1-az1\n")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("us-east-1", p.stderr)
        self.assertIn("us-west-2", p.stderr)

    def test_too_few_supported_zones_is_refused(self) -> None:
        p = self.generate(EAST, "us-east-1", "us-east-1a\tuse1-az6\nus-east-1e\tuse1-az3\n")
        self.assertNotEqual(p.returncode, 0)
        self.assertIn("Availability Zone", p.stderr)

    def test_names_and_ids_must_have_the_shape_of_the_region(self) -> None:
        for label, lines in (("name from another Region", "us-west-2a\tuse1-az1\n"), ("id without a number", "us-east-1a\tuse1-azx\n"),
                             ("one column", "us-east-1a\n"), ("placeholder", "<zone>\t<id>\n")):
            with self.subTest(label):
                self.assertNotEqual(self.generate(EAST, "us-east-1", lines).returncode, 0, label)

    def test_the_generator_runs_no_aws_call_and_no_tool_outside_the_path_setup_uses(self) -> None:
        self.generate(EAST, "us-east-1", AZ_EAST)
        self.assertEqual(self.calls(), [])


class ReceiptCase(HelperCase):
    def test_the_receipt_lives_under_the_ham_config_directory_by_account_and_region(self) -> None:
        p = self.sh('st_init "$ACCT" us-east-1 direct\nprintf "%s" "$ST_FILE"', "lib", "json", "state", ACCT=EAST)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(p.stdout, str(self.home / ".config" / "ham" / "setup" / EAST / "us-east-1" / "direct.json"))

    def test_xdg_config_home_is_honoured(self) -> None:
        p = self.sh('st_init "$ACCT" us-east-1 direct\nprintf "%s" "$ST_FILE"', "lib", "json", "state", ACCT=EAST, XDG_CONFIG_HOME=str(self.tmp / "xdg"))
        self.assertEqual(p.stdout, str(self.tmp / "xdg" / "ham" / "setup" / EAST / "us-east-1" / "direct.json"))

    def test_a_receipt_round_trips_through_a_new_process_and_is_valid_json(self) -> None:
        body = ('st_init "$ACCT" us-east-1 direct\nst_set buildId "hamh-setup-runner-build:0000-1"\nst_set sourceCommit abc123def456\nst_set state started\n'
                'st_set profile work\nst_save || exit 3\n')
        p = self.sh(body, "lib", "json", "state", ACCT=EAST)
        self.assertEqual(p.returncode, 0, p.stderr)
        path = self.receipt_path()
        data = json.loads(path.read_text())
        self.assertEqual(data["buildId"], "hamh-setup-runner-build:0000-1")
        self.assertEqual(data["account"], EAST)
        self.assertEqual(data["region"], "us-east-1")
        self.assertEqual(data["stage"], "direct")
        self.assertEqual(data["schema"], "1")
        q = self.sh('st_init "$ACCT" us-east-1 direct\nst_load\nprintf "%s|%s" "$(st_get state)" "$(st_get buildId)"', "lib", "json", "state", ACCT=EAST)
        self.assertEqual(q.stdout, "started|hamh-setup-runner-build:0000-1")

    def test_a_value_is_replaced_not_repeated(self) -> None:
        p = self.sh('st_init "$ACCT" us-east-1 direct\nst_set state started\nst_set state built\nst_save\n', "lib", "json", "state", ACCT=EAST)
        self.assertEqual(p.returncode, 0, p.stderr)
        self.assertEqual(self.receipt()["state"], "built")
        self.assertEqual(self.receipt_path().read_text().count('"state"'), 1)

    def test_the_file_is_private_and_holds_no_credential(self) -> None:
        made_up = {"AWS_ACCESS_KEY_ID": "AKIA" + "EXAMPLE" * 2 + "0", "AWS_SECRET_ACCESS_KEY": "not-a-real-secret-value", "AWS_SESSION_TOKEN": "not-a-real-token"}
        self.sh('st_init "$ACCT" us-east-1 direct\nst_set profile work\nst_set state started\nst_save\n', "lib", "json", "state", ACCT=EAST, **made_up)
        text = self.receipt_path().read_text()
        for needle in ("AKIA", "not-a-real", "secret", "token", "password"):
            self.assertNotIn(needle, text.lower() if needle.islower() else text)
        mode = self.receipt_path().stat().st_mode & 0o777
        self.assertEqual(mode & 0o077, 0, oct(mode))

    def test_a_value_with_a_quote_or_a_newline_is_refused_instead_of_written(self) -> None:
        for bad in ('a"b', "a\nb", "a\\b"):
            with self.subTest(bad=bad):
                p = self.sh('st_init "$ACCT" us-east-1 direct\nst_set detail "$V" && echo accepted', "lib", "json", "state", ACCT=EAST, V=bad)
                self.assertNotIn("accepted", p.stdout)

    def test_a_missing_receipt_loads_as_empty(self) -> None:
        p = self.sh('st_init "$ACCT" us-east-1 direct\nst_load\nprintf "[%s]" "$(st_get buildId)"', "lib", "json", "state", ACCT=EAST)
        self.assertEqual(p.stdout, "[]")
        self.assertFalse(self.receipt_path().exists())


class PlatformCase(HelperCase):
    def platform(self, system: str, machine: str):
        shim = self.tmp / f"uname-{system}-{machine}"
        shim.mkdir()
        (shim / "uname").write_text('#!/bin/sh\ncase "$1" in -s) echo "%s";; -m) echo "%s";; *) echo "%s";; esac\n' % (system, machine, system))
        (shim / "uname").chmod(0o755)
        return self.sh('cl_platform', "lib", "client", PATH=f"{shim}:{self.stub}:{self.sysbin}")

    def test_the_three_platforms_the_runner_builds(self) -> None:
        for system, machine, want in (("Darwin", "arm64", "darwin-arm64"), ("Linux", "x86_64", "linux-amd64"), ("Linux", "aarch64", "linux-arm64"),
                                      ("Linux", "arm64", "linux-arm64"), ("Linux", "amd64", "linux-amd64")):
            with self.subTest(system=system, machine=machine):
                p = self.platform(system, machine)
                self.assertEqual((p.returncode, p.stdout.strip()), (0, want), p.stderr)

    def test_any_other_machine_is_refused_with_the_ways_to_get_a_client(self) -> None:
        for system, machine in (("Darwin", "x86_64"), ("FreeBSD", "amd64"), ("Linux", "ppc64le")):
            with self.subTest(system=system, machine=machine):
                p = self.platform(system, machine)
                self.assertNotEqual(p.returncode, 0)
                self.assertEqual(p.stdout.strip(), "")
                self.assertIn("--from", p.stderr)


class SourcingCase(HelperCase):
    def test_sourcing_a_helper_runs_nothing_and_prints_nothing(self) -> None:
        # The files `setup.sh` sources. The other scripts of the folder (names.sh is sourced too but defines names only; runner-build.sh, runner-toolchain.sh,
        # build-client.sh and empty-bucket.sh) are programs that do their work when started, not definitions.
        names = ["lib", "json", "accounts", "state", "client", "runner", "preflight", "full", "status", "teardown"]
        missing = [n for n in names if not (self.repo / "scripts" / "setup" / f"{n}.sh").is_file()]
        self.assertEqual(missing, [])
        for name in names:
            with self.subTest(name=name):
                p = self.sh("true", "lib", "json", name)
                self.assertEqual((p.returncode, p.stdout, p.stderr), (0, "", ""))
                self.assertEqual(self.calls(), [])


if __name__ == "__main__":
    unittest.main()
