"""What the helper files of the full setup must be, what `setup.sh --help` says about them, and how the options fit together.

The helpers under `scripts/setup/` are sourced by `setup.sh`, so they are held to the same rules as the script: bash 3.2 (the bash of macOS), no delete,
no network fetch other than through the AWS CLI, no root, no account id, endpoint or checksum written into the text, and nothing in them or in what they
print that names a tool, host, account or profile of the authors. The client name comes from one definition (`names.sh`), so the planned rename is one change.
"""

from __future__ import annotations

import re
import shutil
import subprocess  # nosec B404: tests start bash and shellcheck with argument lists, never a shell
import unittest

from setup_full_support import FullBase
from setup_waits import limit
from test_setup_script import BASH, BASH3, REPO, STUB_HAM, STUB_CARGO, SetupBase, internal_names_in, sha256_of, tree_state

SETUP_DIR = REPO / "scripts" / "setup"
# The files this work package owns. The others of the folder (names.sh, runner-build.sh, build-client.sh) belong to other parts of the program.
HELPERS = ("lib", "json", "accounts", "state", "client", "runner", "preflight", "full", "status", "teardown")
FILES = [SETUP_DIR / f"{n}.sh" for n in HELPERS]


class HelperFilesCase(unittest.TestCase):
    def test_every_helper_exists(self) -> None:
        for f in FILES:
            self.assertTrue(f.is_file(), f"{f.relative_to(REPO)} is missing")

    def test_each_helper_says_it_is_sourced_and_by_whom(self) -> None:
        for f in FILES:
            first = f.read_text().splitlines()[:3]
            self.assertTrue(any("sourced by setup.sh" in ln.lower() for ln in first), f"{f.name}: its first lines must say that setup.sh sources it")

    def test_the_syntax_parses_with_the_bash_here_and_with_bash_3(self) -> None:
        for f in FILES:
            p = subprocess.run([BASH, "-n", str(f)], capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell
            self.assertEqual(p.returncode, 0, f"{f.name}: {p.stderr}")
            if BASH3:
                q = subprocess.run([BASH3, "-n", str(f)], capture_output=True, text=True, timeout=limit(60))  # nosec B603: argument list, no shell
                self.assertEqual(q.returncode, 0, f"{f.name} under bash 3: {q.stderr}")

    def test_no_bash_4_features(self) -> None:
        banned = [r"declare\s+-A", r"\bmapfile\b", r"\breadarray\b", r"\$\{[A-Za-z_]+(,,|\^\^|,|\^)\}", r"&>>", r"\|&", r"\bcoproc\b", r"\bwait\s+-n\b",
                  r"\bdeclare\s+-n\b", r"\blocal\s+-n\b", r"\[\[.*=~", r"\$\{!", r"\bdeclare\s+-g\b", r";;&", r";&\s*$"]
        for f in FILES:
            text = f.read_text()
            for pat in banned:
                self.assertIsNone(re.search(pat, text, re.M), f"{f.name}: {pat}")

    def test_no_delete_no_root_no_download_tool_no_eval(self) -> None:
        for f in FILES:
            code = "\n".join(ln for ln in f.read_text().splitlines() if not ln.lstrip().startswith("#"))
            for pat in (r"(^|[^\w.-])rm\s", r"\bsudo\b", r"\bcurl\b", r"\bwget\b", r"\|\s*(ba)?sh\b", r"\beval\b", r"\bsu\s", r"\bchown\b", r"\bsafe_rm\b",
                        r"\bfind\b.*-delete", r"\brmdir\b", r"\bunlink\b", r"\btruncate\b"):
                self.assertIsNone(re.search(pat, code, re.M), f"{f.name}: {pat}")

    def test_no_credential_value_is_read_printed_or_stored(self) -> None:
        for f in FILES:
            code = "\n".join(ln for ln in f.read_text().splitlines() if not ln.lstrip().startswith("#"))
            for pat in (r"AWS_SECRET_ACCESS_KEY", r"AWS_SESSION_TOKEN", r"AWS_ACCESS_KEY_ID", r"\.\w+/cookie", r">>?\s*\S*\.aws", r"export-credentials",
                        r"get-session-token", r"assume-role", r"create-access-key", r"secretsmanager(?!\s+(describe-secret|delete-secret)\b)", r"GITHUB_TOKEN", r"\bgh\s+auth"):
                self.assertIsNone(re.search(pat, code), f"{f.name}: {pat}")
        # `--teardown` is the one helper that calls Secrets Manager, and it makes two calls: `describe-secret` answers the name and the dates of a secret and never its value (it lists the
        # secrets that CloudFormation scheduled for deletion), and `delete-secret --force-delete-without-recovery` removes the stage's research secret, whose fixed name a recovery window
        # would keep from a new deploy. Every other operation (get-secret-value, put-secret-value, ...) is still refused, and no other helper calls the service.
        teardown_code = "\n".join(ln for ln in (SETUP_DIR / "teardown.sh").read_text().splitlines() if not ln.lstrip().startswith("#"))
        for m in re.finditer(r"secretsmanager\s+(\S+)", teardown_code):
            self.assertIn(m.group(1), ("describe-secret", "delete-secret"))
        for m in re.finditer(r"secretsmanager delete-secret[^\n]*", teardown_code):
            self.assertIn("--force-delete-without-recovery", m.group(0))
        for f in FILES:
            if f.name != "teardown.sh":
                self.assertNotIn("secretsmanager", "\n".join(ln for ln in f.read_text().splitlines() if not ln.lstrip().startswith("#")), f.name)

    def test_no_persistent_password_and_no_github_clone(self) -> None:
        for f in FILES:
            code = f.read_text()
            self.assertIsNone(re.search(r"(?i)git\s+clone\s+\S*github|github\.com", code.replace("a git clone", "")), f.name)
            self.assertIsNone(re.search(r"(?i)\bpassword\s*=", code), f.name)

    def test_no_account_id_endpoint_or_checksum_is_written_into_the_text(self) -> None:
        for f in FILES:
            text = f.read_text()
            self.assertIsNone(re.search(r"\b\d{12}\b", text), f"{f.name}: a 12-digit number")
            self.assertIsNone(re.search(r"\b[0-9a-f]{64}\b", text), f"{f.name}: a 64-digit hex number")
            self.assertIsNone(re.search(r"execute-api\.[a-z0-9-]+\.amazonaws\.com/[a-z]", text), f.name)
            self.assertIsNone(re.search(r"https?://(?!docs\.aws\.amazon\.com|127\.0\.0\.1|localhost)[a-z0-9.-]+\.[a-z]{2,}/\S*\.(tar|tgz|zip|gz)", text), f.name)

    def test_nothing_names_an_internal_tool_host_account_or_profile(self) -> None:
        for f in FILES + [SETUP_DIR / "AGENTS.md"]:
            if not f.is_file():
                continue
            text = f.read_text()
            self.assertEqual(internal_names_in(text), [], f.name)

    @unittest.skipUnless(shutil.which("shellcheck"), "shellcheck is not installed")
    def test_shellcheck_is_clean(self) -> None:
        p = subprocess.run(["shellcheck", "-s", "bash", "-x", *[str(f) for f in FILES]], capture_output=True, text=True, timeout=limit(120))  # nosec B603: argument list, no shell; B607: bare name of a standard tool looked up on PATH
        self.assertEqual(p.returncode, 0, p.stdout)

    def test_setup_sh_sources_exactly_these_helpers_in_this_order(self) -> None:
        text = (REPO / "setup.sh").read_text()
        found = re.findall(r"for helper in ([a-z ]+); do", text)
        self.assertEqual(len(found), 2, "setup.sh checks for the helpers and then sources them")
        for names in found:
            self.assertEqual(tuple(names.split()), tuple(h for h in HELPERS), names)

    def test_the_folder_documents_itself_and_names_every_file(self) -> None:
        agents = SETUP_DIR / "AGENTS.md"
        self.assertTrue(agents.is_file(), "scripts/setup/AGENTS.md is missing")
        text = agents.read_text()
        for section in ("Purpose", "Ownership", "Local Contracts", "Work Guidance", "Verification", "Child DOX Index"):
            self.assertRegex(text, rf"(?m)^## {section}\s*$")
        for f in sorted(SETUP_DIR.glob("*.sh")):
            self.assertIn(f.name, text, f"scripts/setup/AGENTS.md must name {f.name}")
        parent = (REPO / "scripts" / "AGENTS.md").read_text()
        self.assertIn("`setup/AGENTS.md`", parent, "scripts/AGENTS.md must list scripts/setup/AGENTS.md in its Child DOX Index")

    def test_the_exit_codes_and_the_contract_names_are_written_down_in_the_folders_doc(self) -> None:
        text = (SETUP_DIR / "AGENTS.md").read_text()
        for needle in ("hamh-setup-runner", "SetupBucket", "BuildProject", "LogGroup", "RunnerRoleArn", "SETUP_SOURCE_KEY", "SETUP_CONFIG_KEY", "SETUP_STAGE",
                       "SETUP_AZS", "SETUP_BINARY_NAME", "result.json", "phases.jsonl", "lease/", "HAMH_CLIENT_BIN"):
            self.assertIn(needle, text, needle)
        for code in "45678":
            self.assertRegex(text, rf"(?m)^\s*[-|]?\s*`?{code}`?\b", f"exit code {code}")


class HelpTextCase(SetupBase):
    def test_help_lists_every_new_option_and_every_exit_code(self) -> None:
        p = self.run_setup("--help")
        self.assertEqual(p.returncode, 0, self.out(p))
        for flag in ("--full", "--client-only", "--status", "--doctor", "--update", "--teardown", "--keep-runner", "--acknowledge-costs",
                     "--acknowledge-teardown", "--preflight-only", "--build-on", "--account-id", "--revision", "--stage", "--azs", "--invocation-logging"):
            self.assertIn(flag, p.stdout, flag)
        flat = " ".join(p.stdout.split())
        for code in ("4 ", "5 ", "6 ", "7 ", "8 "):
            self.assertRegex(flat, rf"\b{code}\w+", code)

    def test_help_states_the_prerequisite_in_the_words_of_the_decision(self) -> None:
        flat = " ".join(self.run_setup("--help").stdout.split())
        self.assertIn("The only tool you must already have is the AWS CLI, version 2 (plus git, or a zip download of this repository, to get setup.sh).", flat)
        self.assertIn("shell tools that ship with macOS and Linux", flat)

    def test_help_says_what_the_runner_is_and_what_authority_it_has(self) -> None:
        flat = " ".join(self.run_setup("--help").stdout.split())
        for needle in ("runner", "CodeBuild", "sandbox", "removed", "--keep-runner"):
            self.assertIn(needle, flat, needle)

    def test_help_says_yes_never_chooses_the_full_setup_and_costs_need_their_own_flag(self) -> None:
        flat = " ".join(self.run_setup("--help").stdout.split())
        self.assertIn("--yes never chooses the full setup", flat)
        self.assertIn("--yes --acknowledge-costs", flat)

    def test_the_help_text_holds_no_internal_name(self) -> None:
        self.assertEqual(internal_names_in(self.run_setup("--help").stdout), [])

    def test_the_old_sentence_that_setup_deploys_nothing_is_gone(self) -> None:
        text = (REPO / "setup.sh").read_text()
        self.assertNotIn("deploys nothing", text)
        self.assertNotIn("edits no accounts.json", text)
        self.assertNotIn("does not do it", text)


class NoInternalNamesInTheFullSetupCase(FullBase):
    """Everything the full setup prints, on every path, and the log it writes, holds no tool, host, account or profile of the authors."""

    def extra_check(self, label: str, text: str) -> None:
        """A hook for a check that needs a file the public tree does not hold; a subclass fills it."""

    def test_every_screen_and_every_log_is_clean(self) -> None:
        seen = []

        def run(label: str, *args: str, **env: str) -> None:
            p = self.run_full(*args, **env)
            # the user's own account id is printed on purpose, and the temporary folder of the test is not setup's text
            text = self.out(p).replace(self.ACCOUNT, "<the user's account>").replace(str(self.tmp), "<tmp>")
            seen.append(text)
            self.assertEqual(internal_names_in(text), [], f"{label}:\n{text}")
            self.extra_check(label, text)

        run("dry run", "--full", "--profile", "work", "--region", "us-east-1", "--dry-run")
        run("preflight", "--full", "--profile", "work", "--region", "us-east-1", "--preflight-only")
        run("refused", "--full", "--profile", "work", "--region", "us-east-1", "--yes")
        run("success", *self.ARGS, "--keep-runner")
        run("status", "--status", "--profile", "work", "--region", "us-east-1")
        run("doctor", "--doctor", "--profile", "work", "--region", "us-east-1")
        run("update", "--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.set_scenario(build={"final": "FAILED", "result": "fail"})
        self.commit_more()
        run("failed update", "--update", "--profile", "work", "--region", "us-east-1", "--yes")
        self.set_scenario(build={}, eni_stuck=True)
        run("teardown", "--teardown", "--profile", "work", "--region", "us-east-1", "--yes", "--acknowledge-teardown", "--account-id", "210987654321")
        logs = sorted((self.receipt_path().parent / "logs").glob("setup-*.log"))
        self.assertTrue(logs)
        for f in logs:
            self.assertEqual(internal_names_in(f.read_text().replace(self.ACCOUNT, "<the user's account>")), [], f.name)
        self.assertGreater(len("".join(seen)), 6000, "the scan should have read a lot of text")


class OptionConflictCase(SetupBase):
    def usage(self, *args: str):
        p = self.run_setup(*args)
        self.assertEqual(p.returncode, 2, self.out(p))
        return p

    def test_two_modes_or_two_commands_are_a_usage_error(self) -> None:
        self.usage("--full", "--client-only")
        for a, b in (("--status", "--update"), ("--doctor", "--teardown"), ("--update", "--teardown"), ("--status", "--doctor")):
            with self.subTest(a=a, b=b):
                self.usage(a, b)

    def test_a_command_does_not_go_with_a_mode(self) -> None:
        for cmd in ("--status", "--doctor", "--update", "--teardown"):
            with self.subTest(cmd=cmd):
                self.usage(cmd, "--client-only")

    def test_the_values_of_the_new_options_are_checked(self) -> None:
        for args in (("--build-on", "moon"), ("--stage", "other"), ("--azs", "2"), ("--azs", "x"), ("--invocation-logging", "loud"), ("--account-id", "abc"),
                     ("--revision", "main")):
            with self.subTest(args=args):
                self.usage("--full", *args)

    def test_a_value_is_required(self) -> None:
        for flag in ("--build-on", "--stage", "--azs", "--invocation-logging", "--account-id", "--revision"):
            with self.subTest(flag=flag):
                self.assertIn(flag, self.usage(flag).stderr)

    def test_equals_form_works_for_the_new_options(self) -> None:
        p = self.run_setup("--full", "--build-on=moon")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--build-on", p.stderr)


class FullWithAClientOfYourOwnCase(FullBase):
    """The full setup builds the client in the user's own account and makes its own stage, so the options of the client-only setup (--from, --sha256, --build:
    a client of the user's own; --endpoint: a stage that exists) do not go with it. Every way into the full setup refuses one with exit 2, before any AWS call and
    before anything is written: --full, each option that only the full setup takes (it turns the full setup on by itself), and each command of the later life of the system. The same refusal on a terminal, after the answer
    "deploy one for me" to question 1, is held by OwnClientVersusFullCase in test_setup_full_wizard.py. Each refusal is judged against the stand-in AWS CLI, so
    "no AWS call" is a fact read from its log; the control case shows the log is not empty when nothing is refused."""

    BASE = ("--profile", "work", "--region", "us-east-1", "--yes")
    # The options that turn the full setup on, each with the arguments it needs.
    FULL_ON = {"--full": ("--full",), "--keep-runner": ("--keep-runner",), "--acknowledge-costs": ("--acknowledge-costs",),
               "--preflight-only": ("--preflight-only",), "--build-on": ("--build-on", "runner"), "--invocation-logging": ("--invocation-logging", "read-only")}

    def own_clients(self) -> dict:
        art = self.make_artifact()
        return {"--from DIR": ("--from", str(art)), "--from FILE --sha256": ("--from", str(art / "ham"), "--sha256", sha256_of(art / "ham")),
                "--build": ("--build",), "--endpoint": ("--endpoint", self.ENDPOINT)}

    def refused(self, args: tuple, fragment: str) -> None:
        before = tree_state(self.home)
        p = self.run_full(*args)
        self.assertEqual(p.returncode, 2, f"{args}\n{self.out(p)}")
        self.assertIn(fragment, p.stderr, args)
        self.assertIn("Try: setup.sh --help", p.stderr, args)
        self.assertEqual(self.calls(), [], f"{args}: the refusal comes before any AWS call")
        self.assertEqual(before, tree_state(self.home), f"{args}: nothing is written")
        self.assertFalse(self.installed().exists(), args)
        self.assertEqual(self.log_lines(), [], f"{args}: the client is not run")

    def test_a_client_of_your_own_is_refused_on_every_way_into_the_full_setup(self) -> None:
        own = self.own_clients()
        for full_label, full_on in self.FULL_ON.items():
            fragment = self.refusal_text(full_label)
            for own_label, own_args in own.items():
                for order, args in (("full first", (*full_on, *own_args)), ("client first", (*own_args, *full_on))):
                    with self.subTest(full=full_label, own=own_label, order=order):
                        self.refused((*args, *self.BASE), fragment)

    def test_the_refusal_names_the_option_you_typed(self) -> None:
        """A person who never typed --full is not told about --full: the message names the option that turned the full setup on (the first one typed), and --full wins
        when it was typed as well."""
        art = self.make_artifact()
        own = ("--from", str(art))
        tail = " builds the client in your AWS account, so --from, --sha256, --build and --endpoint do not go with it (they belong to --client-only)"
        self.refused(("--keep-runner", *own, *self.BASE), "--keep-runner turns on the full setup, which" + tail)
        self.refused((*own, "--preflight-only", "--keep-runner", *self.BASE), "--preflight-only turns on the full setup, which" + tail)
        self.refused(("--build-on=runner", *own, *self.BASE), "--build-on turns on the full setup, which" + tail)
        self.refused(("--keep-runner", "--full", *own, *self.BASE), "--full" + tail)
        self.refused(("--full", "--keep-runner", *own, *self.BASE), "--full" + tail)
        p = self.run_full("--keep-runner", *own, *self.BASE)
        self.assertNotIn("--full builds", p.stderr, "the person never typed --full")

    @staticmethod
    def refusal_text(label: str) -> str:
        tail = "builds the client in your AWS account, so --from, --sha256, --build and --endpoint do not go with it (they belong to --client-only)"
        return f"--full {tail}" if label == "--full" else f"{label} turns on the full setup, which {tail}"

    def test_the_same_arguments_without_a_client_of_your_own_do_reach_aws(self) -> None:
        # TRIGGER PRESENT: the refusals above prove nothing unless the stand-in's log does fill when the full setup is not refused.
        for full_label, full_on in self.FULL_ON.items():
            with self.subTest(full=full_label):
                before = len(self.calls())
                read_only = () if full_label == "--preflight-only" else ("--preflight-only",)
                p = self.run_full(*full_on, *read_only, *self.BASE)
                self.assertEqual(p.returncode, 0, self.out(p))
                self.assertGreater(len(self.calls()), before, "the full setup reads the account before it stops")
                self.assertGreaterEqual(self.count_calls("sts", "get-caller-identity"), 1)
        self.assertEqual(self.mutations(), [])

    def test_a_command_does_not_go_with_a_client_of_your_own(self) -> None:
        own = self.own_clients()
        for cmd in ("--status", "--doctor", "--update", "--teardown"):
            for own_label, own_args in own.items():
                with self.subTest(cmd=cmd, own=own_label):
                    self.refused((cmd, *own_args, *self.BASE),
                                 f"{cmd} works on the deployment in your AWS account; --from, --sha256, --build and --endpoint belong to the client-only setup")

    def test_client_only_does_not_go_with_an_option_of_the_full_setup(self) -> None:
        art = self.make_artifact()
        fragment = "--client-only does not take --keep-runner, --acknowledge-costs, --preflight-only, --build-on or --invocation-logging: they belong to the full setup"
        for full_label, full_on in self.FULL_ON.items():
            if full_label == "--full":
                continue
            with self.subTest(option=full_label):
                self.refused(("--client-only", *full_on, "--from", str(art), *self.BASE), fragment)

    def test_a_client_of_your_own_still_installs_in_the_client_only_setup(self) -> None:
        art = self.make_artifact()
        sha = sha256_of(art / "ham")
        cases = (("--client-only --from DIR", ("--client-only", "--from", str(art))),
                 ("--from DIR, client-only by default", ("--from", str(art))),
                 ("--client-only --from FILE --sha256", ("--client-only", "--from", str(art / "ham"), "--sha256", sha)),
                 ("--client-only --from DIR --endpoint", ("--client-only", "--from", str(art), "--endpoint", self.ENDPOINT)))
        for label, args in cases:
            with self.subTest(args=label):
                p = self.run_full(*args, *self.BASE)
                self.assertEqual(p.returncode, 0, self.out(p))
                self.assertTrue(self.installed().is_file())
                self.assertEqual(self.calls(), [], "the client-only setup makes no AWS call")

    def test_a_build_on_this_machine_still_goes_with_the_client_only_setup(self) -> None:
        (self.repo / "crates" / "ham").mkdir(parents=True)
        (self.repo / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/ham"]\n')
        (self.repo / "crates" / "ham" / "Cargo.toml").write_text('[package]\nname = "ham"\n')
        self.write_stub("cargo", STUB_CARGO)
        p = self.run_full("--client-only", "--build", *self.BASE, STUB_HAM_SOURCE=str(self.client_file))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(self.log_lines("cargo.log"), [f"{self.repo}|build --release --locked -p ham --bin ham"])
        self.assertTrue(self.installed().is_file())
        self.assertEqual(self.calls(), [], "the client-only setup makes no AWS call")


class NamesCase(FullBase):
    """The client's name, the product's name and the config directory come from scripts/setup/names.sh; setup.sh and its helpers read them from there."""

    def test_a_renamed_client_is_installed_and_run_under_its_new_name_with_one_changed_file(self) -> None:
        # a probe name that is not the default: one file (names.sh) gives the name, and nothing is installed under the default name
        (self.repo / "scripts" / "setup").mkdir(parents=True, exist_ok=True)
        (self.repo / "scripts" / "setup" / "names.sh").write_text('HAMH_CLIENT_BIN=widget\nHAMH_PRODUCT_NAME="Widget"\nHAMH_CONFIG_DIR_NAME=widget\n')
        art = self.tmp / "widget-artifact"
        art.mkdir()
        (art / "widget").write_text(STUB_HAM)
        (art / "widget").chmod(0o755)
        (art / "SHA256SUMS").write_text(f"{sha256_of(art / 'widget')}  widget\n")
        p = self.run_full("--client-only", "--yes", "--from", str(art))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((self.prefix / "bin" / "widget").is_file())
        self.assertFalse((self.prefix / "bin" / "ham").exists())
        self.assertIn("models", self.log_lines())

    def test_without_names_sh_the_client_is_called_ham(self) -> None:
        names = self.repo / "scripts" / "setup" / "names.sh"
        if names.exists():
            names.unlink()
        art = self.make_artifact()
        p = self.run_full("--client-only", "--yes", "--from", str(art))
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((self.prefix / "bin" / "ham").is_file())

    def test_no_command_or_path_in_the_code_names_the_client_by_its_literal_name(self) -> None:
        literal = ("command -v ham", "BINDIR/ham", "FROM/ham", "--bin ham", "release/ham", "$HAMH_CONFIG_DIR_NAME/ham", ".config}/hamh/")
        for f in FILES + [REPO / "setup.sh"]:
            in_text = False
            for n, ln in enumerate(f.read_text().splitlines(), 1):
                if in_text:
                    in_text = ln != "EOF"  # a here-document is text for a person (the help text), not a command
                    continue
                if re.search(r"<<-?\s*'?EOF'?\s*$", ln):
                    in_text = True
                if ln.lstrip().startswith("#"):
                    continue
                for frag in literal:
                    self.assertNotIn(frag, ln, f"{f.name}:{n}: use $HAMH_CLIENT_BIN / $HAMH_CONFIG_DIR_NAME, not the literal name: {ln.strip()}")


if __name__ == "__main__":
    unittest.main()
